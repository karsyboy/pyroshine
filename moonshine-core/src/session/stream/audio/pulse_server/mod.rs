mod commands;
mod dyn_buffer;

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time;

use async_shutdown::ShutdownManager;
use bytes::{Buf, BytesMut};
use mio::net::UnixListener;
use pulseaudio::protocol::{self as pulse};

use dyn_buffer::DynPlaybackBuffer;

use crate::session::manager::SessionShutdownReason;
use crate::session::stream::audio::PulseReconfigure;

type Error = Box<dyn std::error::Error + Send + Sync>;

const LISTENER: mio::Token = mio::Token(0);
const CLOCK: mio::Token = mio::Token(1);

/// Cap on per-client pending outgoing data. A buffer exceeding this means the
/// client has stopped reading; the stuck client is dropped rather than letting
/// the buffer grow unboundedly, and the session survives.
const MAX_OUTGOING_BUFFER: usize = 64 * 1024 * 1024;
/// Bytes read from one client per dispatch before other clients, capture ticks
/// and lifecycle commands get a turn. The client is rescheduled explicitly
/// because its edge-triggered readiness does not fire again for unread data.
const READ_BUDGET: usize = 256 * 1024;

/// How a receive pass ended.
#[derive(Debug, PartialEq, Eq)]
enum Received {
	/// Everything available was read.
	Drained,
	/// The read budget ran out; more may be buffered in the socket.
	Budget,
	/// The client ended its stream; complete requests before it were handled.
	Closed,
}

/// The server emits samples at this rate to the encoder.
pub(crate) const CAPTURE_SAMPLE_RATE: u32 = 48000;

/// Clock tick rate for a negotiated packet duration. Determines the audio
/// frame size sent to the encoder: 200 Hz for 5 ms frames, 100 Hz for 10 ms.
/// Negotiation rejects other durations; reaching here with one is an error
/// rather than a silent change of the client's requested timing.
fn clock_rate_hz(packet_duration_ms: u32) -> Result<u32, Error> {
	crate::session::negotiation::validate_audio_packet_duration(packet_duration_ms)?;
	Ok(1000 / packet_duration_ms)
}

const SINK_NAME: &str = "moonshine";

/// Pre-allocated zero-volume slice for muted streams (up to 8 channels).
const ZERO_VOL: [f32; 8] = [0.0; 8];

/// A buffer of interleaved f32 samples ready for Opus encoding.
pub(crate) struct AudioFrame {
	/// Interleaved f32 samples for the negotiated channel count.
	pub buf: Vec<f32>,
	pub generation: u64,

	/// Capture timestamp in milliseconds since process start.
	pub capture_ts_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
	Prebuffering(u64),
	Corked,
	Playing,
	Draining(u32),
}

struct PlaybackStream {
	stream_index: u32,
	state: StreamState,
	buffer_attr: pulse::stream::BufferAttr,
	buffer: DynPlaybackBuffer,
	volume: Vec<f32>,
	muted: bool,
	/// Bytes consumed by the audio clock since the last REQUEST was sent.
	/// Grows on every successful drain. Zeroed atomically when a REQUEST is issued.
	/// Mirrors `pa_memblockq::missing` (signed to allow temporary negative values
	/// from flush/seek, though in practice it never goes negative here).
	missing: i64,

	/// Bytes we have sent a REQUEST for but have not yet received from the client.
	/// Grows when a REQUEST is sent. Shrinks when the client writes data.
	/// Mirrors `pa_memblockq::requested`.
	requested: usize,
	played_bytes: u64,
	write_offset: u64,
	read_offset: u64,
}

struct Client {
	id: u32,
	socket: mio::net::UnixStream,
	protocol_version: u16,
	props: Option<pulse::Props>,
	incoming: BytesMut,
	outgoing: BytesMut,
	writable_registered: bool,
	playback_streams: BTreeMap<u32, PlaybackStream>,
}

/// Outcome of attempting to flush a client's outgoing buffer.
enum FlushResult {
	/// Buffer fully drained; socket is idle.
	Done,
	/// Socket send buffer is full; retry when the socket becomes writable.
	Blocked,
	/// Socket write failed (peer gone, etc.); the client should be dropped.
	Dead,
}

impl Client {
	/// Write as much of the outgoing buffer to the socket as possible.
	///
	/// `WouldBlock` is not an error: the remainder stays buffered and the
	/// caller arms `WRITABLE` interest. A real write error only ever kills the
	/// offending client, never the server.
	fn flush(&mut self) -> FlushResult {
		loop {
			if self.outgoing.is_empty() {
				return FlushResult::Done;
			}
			match self.socket.write(&self.outgoing) {
				Ok(0) => return FlushResult::Dead,
				Ok(n) => self.outgoing.advance(n),
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return FlushResult::Blocked,
				Err(_) => return FlushResult::Dead,
			}
		}
	}
}

/// Adapter that lets the `pulseaudio` protocol writers append directly into a
/// client's outgoing buffer instead of hitting a nonblocking socket.
struct ClientWriter<'a>(&'a mut BytesMut);

impl std::io::Write for ClientWriter<'_> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.0.extend_from_slice(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

struct ServerState {
	server_info: pulse::ServerInfo,
	sinks: Vec<pulse::SinkInfo>,
	default_format_info: pulse::FormatInfo,
	next_playback_channel_index: u32,
	next_stream_index: u32,
	sink_volume: Vec<f32>,
	sink_muted: bool,
	capture_channels: u8,
	capture_spec: pulse::SampleSpec,
}

pub(crate) struct PulseServer {
	generation: u64,
	listener: UnixListener,
	poll: mio::Poll,
	clock: mio_timerfd::TimerFd,
	clock_rate_hz: u32,

	frame_tx: crossbeam_channel::Sender<AudioFrame>,
	frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
	spare_frame: Option<AudioFrame>,

	clients: BTreeMap<mio::Token, Client>,
	server_state: ServerState,
	reconfigure_rx: crossbeam_channel::Receiver<PulseReconfigure>,

	epoch: time::Instant,
}

/// Mirrors `pa_memblockq_pop_missing`.
///
/// Transfers accumulated demand (`missing`) into in-flight credit (`requested`)
/// and returns the number of bytes to include in the next REQUEST message.
/// Returns 0 if there is nothing to request yet.
///
/// The `in_prebuf` flag bypasses the `min_req` gate — when re-filling after
/// an underrun the server should not wait for a full `min_req` chunk to
/// accumulate before asking; any positive demand should be requested immediately.
fn pop_missing(missing: &mut i64, requested: &mut usize, min_req: usize, in_prebuf: bool) -> usize {
	if *missing <= 0 {
		return 0;
	}
	if (*missing as usize) < min_req && !in_prebuf {
		return 0;
	}
	let l = *missing as usize;
	*requested += l;
	*missing = 0;
	l
}

impl PulseServer {
	#[allow(clippy::too_many_arguments)]
	pub fn spawn(
		listener: std::os::unix::net::UnixListener,
		_socket_path: PathBuf,
		channels: u8,
		packet_duration_ms: u32,
		frame_tx: crossbeam_channel::Sender<AudioFrame>,
		frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
		stop: ShutdownManager<SessionShutdownReason>,
		reconfigure_rx: crossbeam_channel::Receiver<PulseReconfigure>,
		generation: u64,
	) -> Result<(), Error> {
		listener.set_nonblocking(true)?;
		let listener = UnixListener::from_std(listener);
		let poll = mio::Poll::new()?;

		let clock_rate_hz = clock_rate_hz(packet_duration_ms)?;

		let mut clock = mio_timerfd::TimerFd::new(mio_timerfd::ClockId::Monotonic)?;
		clock.set_timeout_interval(&time::Duration::from_nanos(1_000_000_000 / clock_rate_hz as u64))?;

		let sink_name = std::ffi::CString::new(SINK_NAME).unwrap();

		let capture_spec = pulse::SampleSpec {
			format: pulse::SampleFormat::Float32Le,
			channels,
			sample_rate: CAPTURE_SAMPLE_RATE,
		};

		let channel_map = match channels {
			6 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
			]),
			8 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
				pulse::ChannelPosition::SideLeft,
				pulse::ChannelPosition::SideRight,
			]),
			_ => pulse::ChannelMap::stereo(),
		};

		let port_name = match channels {
			6 => "Surround 5.1 Output",
			8 => "Surround 7.1 Output",
			_ => "Stereo Output",
		};

		let mut dummy_sink = pulse::SinkInfo::new_dummy(1);
		dummy_sink.name = sink_name.clone();
		dummy_sink.description = Some(std::ffi::CString::new("Moonshine virtual output").unwrap());
		dummy_sink.sample_spec = capture_spec;
		dummy_sink.channel_map = channel_map;
		dummy_sink.cvolume = pulse::ChannelVolume::norm(channels);

		let server_info = pulse::ServerInfo {
			server_name: Some(std::ffi::CString::new("Moonshine").unwrap()),
			server_version: Some(std::ffi::CString::new(env!("CARGO_PKG_VERSION")).unwrap()),
			host_name: Some(std::ffi::CString::new("moonshine").unwrap()),
			default_sink_name: Some(sink_name.clone()),
			default_source_name: Some(std::ffi::CString::new("").unwrap()),
			sample_spec: capture_spec,
			channel_map,
			..Default::default()
		};

		dummy_sink.ports[0].name = std::ffi::CString::new(port_name).unwrap();
		dummy_sink.ports[0].port_type = pulse::port_info::PortType::Network;
		dummy_sink.ports[0].description = Some(std::ffi::CString::new("virtual output").unwrap());

		let channel_map_str = match channels {
			6 => "front-left,front-right,front-center,lfe,rear-left,rear-right",
			8 => "front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right",
			_ => "front-left,front-right",
		};

		let mut format_props = pulse::Props::new();
		format_props.set(
			pulse::Prop::FormatChannels,
			std::ffi::CString::new(channels.to_string()).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatChannelMap,
			std::ffi::CString::new(channel_map_str).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatSampleFormat,
			std::ffi::CString::new("float32le").unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatRate,
			std::ffi::CString::new(CAPTURE_SAMPLE_RATE.to_string()).unwrap(),
		);

		let default_format_info = pulse::FormatInfo {
			encoding: pulse::FormatEncoding::Pcm,
			props: format_props,
		};

		dummy_sink.formats[0] = default_format_info.clone();

		let server = Self {
			generation,
			listener,
			poll,
			clock,
			clock_rate_hz,
			frame_tx,
			frame_recycle_rx,
			spare_frame: None,
			clients: BTreeMap::new(),
			server_state: ServerState {
				server_info,
				sinks: vec![dummy_sink],
				default_format_info,
				next_playback_channel_index: 0,
				next_stream_index: 0,
				sink_volume: vec![1.0; channels as usize],
				sink_muted: false,
				capture_channels: channels,
				capture_spec,
			},
			reconfigure_rx,
			epoch: time::Instant::now(),
		};

		// The server owns the fixed-name Pulse socket; the next session may bind
		// it only after this worker has exited.
		let worker = crate::session::lifecycle::WorkerGuard::register(&stop, SessionShutdownReason::PulseServerStopped)
			.map_err(|()| std::io::Error::other("session stopped before the PulseAudio server started"))?;
		std::thread::Builder::new()
			.name("pulse-server".to_string())
			.spawn(move || {
				let _worker = worker;
				if let Err(e) = server.run(stop) {
					tracing::error!("PulseServer error: {e}");
				}
			})?;

		Ok(())
	}

	fn run(mut self, stop: ShutdownManager<SessionShutdownReason>) -> Result<(), Error> {
		let mut next_client_token = 1024u64;

		self.poll.registry().register(
			&mut mio::unix::SourceFd(&self.clock.as_raw_fd()),
			CLOCK,
			mio::Interest::READABLE,
		)?;
		self.poll
			.registry()
			.register(&mut self.listener, LISTENER, mio::Interest::READABLE)?;

		let mut events = mio::Events::with_capacity(1024);
		// Clients whose last receive pass stopped at the budget.
		let mut rescheduled: Vec<mio::Token> = Vec::new();

		loop {
			while let Ok(request) = self.reconfigure_rx.try_recv() {
				let result = if request.reconfigure_capture {
					self.reconfigure(request.channels, request.packet_duration_ms)
				} else {
					Ok(())
				};
				if result.is_ok() {
					// Retain Pulse sockets, but discard buffered PCM and resampler history.
					for client in self.clients.values_mut() {
						for stream in client.playback_streams.values_mut() {
							stream.missing += stream.buffer.len_bytes() as i64;
							stream.read_offset = stream.write_offset;
							stream.buffer.clear();
						}
					}
					self.spare_frame = None;
					self.generation = request.generation;
				}
				let _ = request.applied.send(result.map_err(|error| error.to_string()));
			}

			let timeout = if rescheduled.is_empty() {
				time::Duration::from_millis(50)
			} else {
				time::Duration::ZERO
			};
			match self.poll.poll(&mut events, Some(timeout)) {
				Ok(_) => (),
				Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
				Err(e) => return Err(e.into()),
			}

			if stop.is_shutdown_triggered() {
				return Ok(());
			}

			for event in events.iter() {
				match event.token() {
					CLOCK => {
						// A wakeup can race its own drain and find no expirations
						// left to read (EAGAIN); skip the tick, don't kill the session.
						match self.clock.read() {
							Ok(_) => self.clock_tick()?,
							Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
							Err(e) => return Err(e.into()),
						}
					},
					LISTENER => loop {
						let (mut socket, _) = match self.listener.accept() {
							Ok(conn) => conn,
							Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
							Err(e) => return Err(e.into()),
						};
						let id = next_client_token as u32;
						let token = mio::Token(next_client_token as usize);
						next_client_token += 1;

						tracing::debug!("PulseAudio client connected (id={})", id);

						self.poll
							.registry()
							.register(&mut socket, token, mio::Interest::READABLE)?;

						self.clients.insert(
							token,
							Client {
								id,
								socket,
								protocol_version: pulse::MAX_VERSION,
								props: None,
								incoming: BytesMut::new(),
								outgoing: BytesMut::new(),
								writable_registered: false,
								playback_streams: BTreeMap::new(),
							},
						);
					},
					// A closed peer may still have complete requests buffered:
					// read them, then remove the client at end of stream.
					client_token
						if (event.is_readable() || event.is_read_closed())
							&& self.clients.contains_key(&client_token) =>
					{
						if self.service_client(client_token) && !rescheduled.contains(&client_token) {
							rescheduled.push(client_token);
						}
						if event.is_writable() {
							self.flush_client(client_token);
						}
					},
					client_token if event.is_writable() => {
						self.flush_client(client_token);
					},
					_ => (),
				}
			}

			// Continue clients that used their budget, after this round's events.
			for client_token in std::mem::take(&mut rescheduled) {
				if self.clients.contains_key(&client_token) && self.service_client(client_token) {
					rescheduled.push(client_token);
				}
			}
		}
	}

	/// One receive pass for a readable client. Removes it at end of stream or
	/// on error; returns whether it must be continued (budget exhausted).
	fn service_client(&mut self, client_token: mio::Token) -> bool {
		let outcome = self.recv(client_token);
		let remove = match &outcome {
			Ok(Received::Budget) => return true,
			Ok(Received::Drained) => false,
			Ok(Received::Closed) => {
				tracing::debug!(token = client_token.0, "PulseAudio client ended its stream");
				true
			},
			Err(e) => {
				tracing::error!("PulseAudio client error: {:#}", e);
				true
			},
		};
		if remove && let Some(mut client) = self.clients.remove(&client_token) {
			tracing::debug!("PulseAudio client disconnected (id={})", client.id);
			let _ = self.poll.registry().deregister(&mut client.socket);
		}
		false
	}

	fn reconfigure(&mut self, channels: u8, packet_duration_ms: u32) -> Result<(), Error> {
		let channels = match channels {
			6 | 8 => channels,
			_ => 2,
		};
		let clock_rate_hz = clock_rate_hz(packet_duration_ms)?;
		self.clock
			.set_timeout_interval(&time::Duration::from_nanos(1_000_000_000 / u64::from(clock_rate_hz)))?;
		self.clock_rate_hz = clock_rate_hz;
		if channels == self.server_state.capture_channels {
			return Ok(());
		}

		let channel_map = match channels {
			6 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
			]),
			8 => pulse::ChannelMap::new([
				pulse::ChannelPosition::FrontLeft,
				pulse::ChannelPosition::FrontRight,
				pulse::ChannelPosition::FrontCenter,
				pulse::ChannelPosition::Lfe,
				pulse::ChannelPosition::RearLeft,
				pulse::ChannelPosition::RearRight,
				pulse::ChannelPosition::SideLeft,
				pulse::ChannelPosition::SideRight,
			]),
			_ => pulse::ChannelMap::stereo(),
		};
		let capture_spec = pulse::SampleSpec {
			format: pulse::SampleFormat::Float32Le,
			channels,
			sample_rate: CAPTURE_SAMPLE_RATE,
		};
		// Playback wire formats remain valid: rebuild their output conversion,
		// retaining Pulse sockets and per-client negotiated source formats.
		for client in self.clients.values_mut() {
			for stream in client.playback_streams.values_mut() {
				stream.missing += stream.buffer.len_bytes() as i64;
				stream.read_offset = stream.write_offset;
				stream.buffer.reconfigure_output(capture_spec);
				stream.volume.resize(channels as usize, 1.0);
			}
		}
		self.server_state.capture_channels = channels;
		self.server_state.capture_spec = capture_spec;
		self.server_state.server_info.sample_spec = capture_spec;
		self.server_state.server_info.channel_map = channel_map;
		self.server_state.sink_volume = vec![1.0; channels as usize];
		let channel_map_str = match channels {
			6 => "front-left,front-right,front-center,lfe,rear-left,rear-right",
			8 => "front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right",
			_ => "front-left,front-right",
		};
		let mut format_props = pulse::Props::new();
		format_props.set(
			pulse::Prop::FormatChannels,
			std::ffi::CString::new(channels.to_string()).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatChannelMap,
			std::ffi::CString::new(channel_map_str).unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatSampleFormat,
			std::ffi::CString::new("float32le").unwrap(),
		);
		format_props.set(
			pulse::Prop::FormatRate,
			std::ffi::CString::new(CAPTURE_SAMPLE_RATE.to_string()).unwrap(),
		);
		self.server_state.default_format_info = pulse::FormatInfo {
			encoding: pulse::FormatEncoding::Pcm,
			props: format_props,
		};
		if let Some(sink) = self.server_state.sinks.first_mut() {
			sink.sample_spec = capture_spec;
			sink.channel_map = channel_map;
			sink.cvolume = pulse::ChannelVolume::norm(channels);
			sink.formats[0] = self.server_state.default_format_info.clone();
		}
		self.spare_frame = None;
		self.epoch = time::Instant::now();
		tracing::info!(channels, packet_duration_ms, "Reconfigured live PulseAudio capture");
		Ok(())
	}

	/// Read and handle a client's requests until its socket would block, its
	/// stream ends or the read budget is used. A zero-byte read is the end of
	/// stream: complete requests already buffered are handled, an incomplete
	/// one is discarded, and the caller removes the client.
	fn recv(&mut self, client_token: mio::Token) -> Result<Received, Error> {
		let result = (|| -> Result<Received, Error> {
			let client = self.clients.get_mut(&client_token).unwrap();

			let mut read_size;
			let mut budget = READ_BUDGET;

			loop {
				// Handle every complete request already buffered.
				loop {
					if client.incoming.len() < pulse::DESCRIPTOR_SIZE {
						read_size = 8192;
						break;
					}

					let desc = pulse::read_descriptor(&mut Cursor::new(&client.incoming[..pulse::DESCRIPTOR_SIZE]))?;

					// Guard against excessively large payloads (max 4 MiB).
					const MAX_PAYLOAD_SIZE: u32 = 4 * 1024 * 1024;
					if desc.length > MAX_PAYLOAD_SIZE {
						return Err(format!("payload too large: {} bytes", desc.length).into());
					}

					if client.incoming.len() < (desc.length as usize + pulse::DESCRIPTOR_SIZE) {
						read_size = desc.length as usize + pulse::DESCRIPTOR_SIZE - client.incoming.len();
						break;
					}

					let _desc_bytes = client.incoming.split_to(pulse::DESCRIPTOR_SIZE);
					let payload = client.incoming.split_to(desc.length as usize).freeze();

					if desc.channel == u32::MAX {
						let (seq, cmd) =
							match pulse::Command::read_tag_prefixed(&mut Cursor::new(payload), client.protocol_version)
							{
								Err(pulse::ProtocolError::Unimplemented(seq, cmd)) => {
									tracing::warn!("Unimplemented PA command: {:?}", cmd);
									pulse::write_error(
										&mut ClientWriter(&mut client.outgoing),
										seq,
										&pulse::PulseError::NotImplemented,
									)?;
									continue;
								},
								v => v.map_err(|e| -> Error { format!("decoding command: {e}").into() })?,
							};

						match commands::handle_command(client, &mut self.server_state, seq, cmd) {
							Ok(()) => (),
							Err(e) => {
								let _ = pulse::write_error(
									&mut ClientWriter(&mut client.outgoing),
									seq,
									&pulse::PulseError::Internal,
								);
								return Err(e);
							},
						}
					} else {
						commands::handle_stream_write(client, desc, &payload)?;
					}

					// Replies a client does not read are bounded while it is being
					// served, not only after its socket drains.
					if client.outgoing.len() > MAX_OUTGOING_BUFFER {
						return Err(format!(
							"client {} is not reading its replies (outgoing buffer exceeds {MAX_OUTGOING_BUFFER} B)",
							client.id
						)
						.into());
					}
				}

				if budget == 0 {
					return Ok(Received::Budget);
				}
				let size = read_size.min(budget);
				let off = client.incoming.len();
				client.incoming.resize(off + size, 0);
				let n = match client.socket.read(&mut client.incoming[off..]) {
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
						client.incoming.truncate(off);
						return Ok(Received::Drained);
					},
					Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
						client.incoming.truncate(off);
						continue;
					},
					v => v.map_err(|e| -> Error { format!("recv error: {e}").into() })?,
				};
				client.incoming.truncate(off + n);
				if n == 0 {
					if !client.incoming.is_empty() {
						tracing::debug!(
							bytes = client.incoming.len(),
							"PulseAudio client ended its stream inside a request"
						);
					}
					return Ok(Received::Closed);
				}
				budget -= n;
			}
		})();

		if result.is_ok() {
			self.flush_client(client_token);
		}

		result
	}

	/// Flush a client's pending outgoing data, managing `WRITABLE` interest and
	/// dropping clients that are stuck or whose socket has failed.
	fn flush_client(&mut self, client_token: mio::Token) {
		let remove = {
			let client = match self.clients.get_mut(&client_token) {
				Some(client) => client,
				None => return,
			};

			if client.outgoing.len() > MAX_OUTGOING_BUFFER {
				tracing::warn!(
					"PulseAudio client {} is stuck (outgoing buffer {} B exceeds cap), dropping",
					client.id,
					MAX_OUTGOING_BUFFER,
				);
				true
			} else {
				match client.flush() {
					FlushResult::Done => {
						if client.writable_registered {
							client.writable_registered = false;
							let _ = self.poll.registry().reregister(
								&mut client.socket,
								client_token,
								mio::Interest::READABLE,
							);
						}
						false
					},
					FlushResult::Blocked => {
						if !client.writable_registered {
							client.writable_registered = true;
							let _ = self.poll.registry().reregister(
								&mut client.socket,
								client_token,
								mio::Interest::READABLE | mio::Interest::WRITABLE,
							);
						}
						false
					},
					FlushResult::Dead => true,
				}
			}
		};

		if remove && let Some(mut client) = self.clients.remove(&client_token) {
			tracing::debug!("PulseAudio client disconnected (id={})", client.id);
			let _ = self.poll.registry().deregister(&mut client.socket);
		}
	}

	fn clock_tick(&mut self) -> Result<(), Error> {
		let mut done_draining = Vec::new();

		let capture_ts = self.epoch.elapsed().as_millis() as u64;
		let channels = self.server_state.capture_channels as u32;
		let num_frames = CAPTURE_SAMPLE_RATE / self.clock_rate_hz;
		let encode_len = num_frames * channels;

		let mut frame = match self.frame_recycle_rx.try_recv() {
			Ok(mut frame) => {
				frame.buf.resize(encode_len as usize, 0.0);
				frame.buf.fill(0.0);
				frame
			},
			Err(crossbeam_channel::TryRecvError::Empty) => {
				if let Some(mut frame) = self.spare_frame.take() {
					frame.buf.resize(encode_len as usize, 0.0);
					frame.buf.fill(0.0);
					frame
				} else {
					// Recycle pool temporarily exhausted; allocate a fresh frame to avoid
					// deadlocking the encoder which is blocked on frame_rx.recv().
					AudioFrame {
						buf: vec![0.0; encode_len as usize],
						generation: self.generation,
						capture_ts_ms: 0,
					}
				}
			},
			Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
		};

		for client in self.clients.values_mut() {
			done_draining.clear();
			for (id, stream) in client.playback_streams.iter_mut() {
				// ── Drain phase ─────────────────────────────────────────────────────
				// Only Playing and Draining streams consume audio data each tick.
				// Prebuffering and Corked streams do not drain.
				if matches!(stream.state, StreamState::Playing | StreamState::Draining(_)) {
					let buffer_len = stream.buffer.len_bytes();

					let drained = if stream.muted {
						stream.buffer.drain_and_mix(
							num_frames as usize,
							&mut frame.buf,
							&ZERO_VOL[..stream.volume.len()],
						)
					} else {
						stream
							.buffer
							.drain_and_mix(num_frames as usize, &mut frame.buf, &stream.volume)
					};

					if !drained {
						// Buffer underrun: notify client and (re-)enter prebuffering.
						tracing::warn!("Buffer underrun for stream {}", id);
						pulse::write_command_message(
							&mut ClientWriter(&mut client.outgoing),
							u32::MAX,
							&pulse::Command::Underflow(pulse::Underflow {
								channel: *id,
								offset: 0,
							}),
							client.protocol_version,
						)?;

						if stream.buffer_attr.pre_buffering > 0 && matches!(stream.state, StreamState::Playing) {
							stream.state = StreamState::Prebuffering(stream.buffer_attr.pre_buffering as u64);
							// Seed missing = pre_buffering so pop_missing fires immediately
							// in the REQUEST phase below (same tick, no one-tick delay).
							// Reset requested — any stale in-flight credit is discarded.
							stream.missing = stream.buffer_attr.pre_buffering as i64;
							stream.requested = 0;
						}
						// Fall through to REQUEST phase — do NOT continue.
					} else {
						// Successful drain: accumulate demand.
						// Mirrors pa_memblockq::read_index_changed → missing += delta.
						let read_len = buffer_len - stream.buffer.len_bytes();
						stream.read_offset += read_len as u64;
						stream.played_bytes += read_len as u64;
						stream.missing += read_len as i64;

						if matches!(stream.state, StreamState::Draining(_)) && stream.buffer.is_empty() {
							done_draining.push(*id);
						}
					}
				}

				// ── Request phase ────────────────────────────────────────────────────
				// Issue a REQUEST if enough demand has accumulated.
				// Draining streams are excluded — they are emptying, not refilling.
				if matches!(
					stream.state,
					StreamState::Playing | StreamState::Corked | StreamState::Prebuffering(_)
				) {
					let min_req = stream.buffer_attr.minimum_request_length as usize;
					let in_prebuf = matches!(stream.state, StreamState::Prebuffering(_));
					let req = pop_missing(&mut stream.missing, &mut stream.requested, min_req, in_prebuf);
					if req > 0 {
						pulse::write_command_message(
							&mut ClientWriter(&mut client.outgoing),
							u32::MAX,
							&pulse::Command::Request(pulse::Request {
								channel: *id,
								length: req as u32,
							}),
							client.protocol_version,
						)?;
					}
				}
			}

			for id in done_draining.iter() {
				let stream = client.playback_streams.remove(id).unwrap();
				if let StreamState::Draining(drain_seq) = stream.state {
					pulse::write_ack_message(&mut ClientWriter(&mut client.outgoing), drain_seq)?;
				}
			}
		}

		// Flush pending output for all clients. WouldBlock is handled gracefully
		// (buffered + WRITABLE interest); a stuck client is dropped, never the
		// whole server.
		let tokens: Vec<mio::Token> = self.clients.keys().copied().collect();
		for token in tokens {
			self.flush_client(token);
		}

		// Apply sink-level volume.
		if self.server_state.sink_muted {
			frame.buf.fill(0.0);
		} else if self.server_state.sink_volume.iter().any(|&v| v != 1.0) {
			let vol = &self.server_state.sink_volume;
			for (i, sample) in frame.buf.iter_mut().enumerate() {
				*sample *= vol[i % vol.len()];
			}
		}

		frame.capture_ts_ms = capture_ts;
		frame.generation = self.generation;
		match self.frame_tx.try_send(frame) {
			Ok(()) => {},
			Err(crossbeam_channel::TrySendError::Full(frame)) => {
				// Encoder is behind; stash the frame so we don't lose the allocation.
				self.spare_frame = Some(frame);
			},
			Err(crossbeam_channel::TrySendError::Disconnected(_)) => return Ok(()),
		}

		Ok(())
	}
}

#[cfg(test)]
mod epoch_tests {
	use super::*;
	#[tokio::test]
	async fn same_mode_duration_and_layout_resumes_retain_pulse_connection() {
		use std::io::Read;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("native");
		let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
		let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
		let (_recycle_tx, recycle_rx) = crossbeam_channel::bounded(3);
		let (commands, command_rx) = crossbeam_channel::unbounded();
		let stop = ShutdownManager::new();
		PulseServer::spawn(
			listener,
			path.clone(),
			2,
			5,
			frame_tx,
			recycle_rx,
			stop.clone(),
			command_rx,
			1,
		)
		.unwrap();
		let mut client = std::os::unix::net::UnixStream::connect(&path).unwrap();
		client.set_nonblocking(true).unwrap();
		tokio::time::sleep(time::Duration::from_millis(30)).await;
		for (index, (channels, duration, changed)) in
			[(2, 5, false), (2, 10, true), (6, 10, true), (8, 5, true), (2, 5, true)]
				.into_iter()
				.enumerate()
		{
			let generation = index as u64 + 2;
			let (applied, waiting) = tokio::sync::oneshot::channel();
			commands
				.send(PulseReconfigure {
					generation,
					reconfigure_capture: changed,
					channels,
					packet_duration_ms: duration,
					applied,
				})
				.unwrap();
			tokio::time::timeout(time::Duration::from_secs(1), waiting)
				.await
				.unwrap()
				.unwrap()
				.unwrap();
			let error = client.read(&mut [0; 1]).unwrap_err();
			assert_eq!(
				error.kind(),
				std::io::ErrorKind::WouldBlock,
				"Pulse socket remains connected"
			);
			// The capture producer stamps its generation and negotiated frame size.
			let frame = tokio::task::spawn_blocking({
				let rx = frame_rx.clone();
				move || {
					loop {
						let f = rx.recv_timeout(time::Duration::from_secs(1)).unwrap();
						if f.generation == generation {
							break f;
						}
					}
				}
			})
			.await
			.unwrap();
			assert_eq!(frame.buf.len(), 48 * duration as usize * channels as usize);
		}
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}
}

/// Review 2026-10-05 STAB-006: a client's end of stream and a client that never
/// stops writing must not trap the server's single thread. Each case is judged
/// by whether the server still serves reconfiguration, capture ticks and stop.
#[cfg(test)]
mod receive_tests {
	use super::*;
	use std::io::{Read, Write};
	use std::os::unix::net::UnixStream;

	struct Server {
		_dir: tempfile::TempDir,
		path: PathBuf,
		frames: crossbeam_channel::Receiver<AudioFrame>,
		_recycle: crossbeam_channel::Sender<AudioFrame>,
		commands: crossbeam_channel::Sender<PulseReconfigure>,
		stop: ShutdownManager<SessionShutdownReason>,
		generation: u64,
	}

	impl Server {
		fn spawn() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let path = dir.path().join("native");
			let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
			let (frame_tx, frames) = crossbeam_channel::bounded(3);
			let (recycle, recycle_rx) = crossbeam_channel::bounded(3);
			let (commands, command_rx) = crossbeam_channel::unbounded();
			let stop = ShutdownManager::new();
			PulseServer::spawn(
				listener,
				path.clone(),
				2,
				5,
				frame_tx,
				recycle_rx,
				stop.clone(),
				command_rx,
				1,
			)
			.unwrap();
			Self {
				_dir: dir,
				path,
				frames,
				_recycle: recycle,
				commands,
				stop,
				generation: 1,
			}
		}

		/// The event loop still answers a reconfiguration and still produces
		/// capture frames for it.
		async fn responsive(&mut self) -> bool {
			self.generation += 1;
			let generation = self.generation;
			let (applied, waiting) = tokio::sync::oneshot::channel();
			self.commands
				.send(PulseReconfigure {
					generation,
					reconfigure_capture: false,
					channels: 2,
					packet_duration_ms: 5,
					applied,
				})
				.unwrap();
			if !matches!(
				tokio::time::timeout(time::Duration::from_secs(2), waiting).await,
				Ok(Ok(Ok(())))
			) {
				return false;
			}
			let frames = self.frames.clone();
			tokio::task::spawn_blocking(move || {
				let deadline = time::Instant::now() + time::Duration::from_secs(2);
				while let Some(left) = deadline.checked_duration_since(time::Instant::now()) {
					match frames.recv_timeout(left) {
						Ok(frame) if frame.generation == generation => return true,
						Ok(_) => {},
						Err(_) => return false,
					}
				}
				false
			})
			.await
			.unwrap()
		}

		async fn stop(self) -> bool {
			self.stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
			tokio::time::timeout(time::Duration::from_secs(2), self.stop.wait_shutdown_complete())
				.await
				.is_ok()
		}
	}

	fn command(seq: u32, command: &pulse::Command) -> Vec<u8> {
		let mut message = Vec::new();
		pulse::write_command_message(&mut message, seq, command, pulse::MAX_VERSION).unwrap();
		message
	}

	fn descriptor(length: u32, channel: u32) -> Vec<u8> {
		let mut bytes = [0u8; pulse::DESCRIPTOR_SIZE];
		pulse::encode_descriptor(
			&mut bytes,
			&pulse::Descriptor {
				length,
				channel,
				offset: 0,
				flags: pulse::DescriptorFlags::empty(),
			},
		);
		bytes.to_vec()
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn end_of_stream_in_any_receive_state_removes_the_client() {
		let complete = command(1, &pulse::Command::GetServerInfo);
		let cases: [(&str, Vec<u8>); 4] = [
			("empty", Vec::new()),
			("partial descriptor", descriptor(100, u32::MAX)[..5].to_vec()),
			("partial payload", [descriptor(100, u32::MAX), vec![0; 10]].concat()),
			("complete command", complete),
		];
		for (name, bytes) in cases {
			let mut server = Server::spawn();
			let mut client = UnixStream::connect(&server.path).unwrap();
			assert!(server.responsive().await, "{name}: fixture");
			client.write_all(&bytes).unwrap();
			client.shutdown(std::net::Shutdown::Write).unwrap();
			assert!(
				server.responsive().await,
				"review 2026-10-05 STAB-006: {name} followed by EOF stalled the PulseAudio server"
			);
			// A complete request before the EOF is still answered, then the
			// server closes its side.
			client.set_read_timeout(Some(time::Duration::from_secs(2))).unwrap();
			let mut reply = Vec::new();
			let closed = client.read_to_end(&mut reply).is_ok();
			assert!(closed, "{name}: the server closes a client that ended its stream");
			assert_eq!(
				!reply.is_empty(),
				name == "complete command",
				"{name}: {} reply bytes",
				reply.len()
			);
			assert!(server.stop().await, "{name}: stop");
		}
	}

	/// A client that keeps sending requests without reading the replies is
	/// served in bounded slices: capture and reconfiguration keep running, and
	/// the client is dropped once its unread replies exceed the cap.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn a_flooding_non_reading_client_cannot_starve_the_server() {
		let mut server = Server::spawn();
		let mut client = UnixStream::connect(&server.path).unwrap();
		let request = command(1, &pulse::Command::GetServerInfo);
		let batch = request.repeat(1024);
		let writer = std::thread::spawn(move || {
			let mut written = 0usize;
			while client.write_all(&batch).is_ok() {
				written += batch.len();
				if written > 1 << 30 {
					break;
				}
			}
			written
		});
		for _ in 0..3 {
			assert!(server.responsive().await, "the server kept serving while flooded");
		}
		assert!(server.stop().await);
		// The server dropped the client (or stopped): the writer ends.
		assert!(writer.join().unwrap() > 0);
	}

	/// Real libpulse clients (`pacat`) play stereo, 5.1 and 7.1 into the
	/// server; every channel reaches the capture frames. A second client is
	/// killed mid-stream (its socket ends without any shutdown handshake) while
	/// the first keeps playing, and the server stays responsive.
	/// `MOONSHINE_TEST_PULSE_CLIENT=1 cargo test -p moonshine-core real_pulse_clients -- --ignored`
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	#[ignore = "needs pacat (libpulse): MOONSHINE_TEST_PULSE_CLIENT=1"]
	async fn real_pulse_clients_play_every_layout_and_may_vanish() {
		use std::process::{Command, Stdio};
		assert!(
			std::env::var_os("MOONSHINE_TEST_PULSE_CLIENT").is_some(),
			"set MOONSHINE_TEST_PULSE_CLIENT=1"
		);
		for channels in [2u8, 6, 8] {
			let dir = tempfile::tempdir().unwrap();
			let path = dir.path().join("native");
			let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
			let (frame_tx, frames) = crossbeam_channel::bounded(3);
			let (_recycle, recycle_rx) = crossbeam_channel::bounded(3);
			let (_commands, command_rx) = crossbeam_channel::unbounded();
			let stop = ShutdownManager::new();
			PulseServer::spawn(
				listener,
				path.clone(),
				channels,
				5,
				frame_tx,
				recycle_rx,
				stop.clone(),
				command_rx,
				1,
			)
			.unwrap();
			let pacat = |seconds: usize| {
				let mut child = Command::new("pacat")
					.args([
						&format!("--server=unix:{}", path.display()),
						"--playback",
						"--raw",
						"--format=float32le",
						"--rate=48000",
						&format!("--channels={channels}"),
						"--latency-msec=20",
					])
					.stdin(Stdio::piped())
					.stderr(Stdio::null())
					.spawn()
					.expect("pacat");
				let mut stdin = child.stdin.take().unwrap();
				let writer = std::thread::spawn(move || {
					// A distinct level per channel, so a dropped channel shows.
					let frame: Vec<u8> = (0..channels)
						.flat_map(|channel| (0.1 + 0.1 * f32::from(channel)).to_le_bytes())
						.collect();
					let block = frame.repeat(480);
					for _ in 0..seconds * 100 {
						if std::io::Write::write_all(&mut stdin, &block).is_err() {
							break;
						}
					}
				});
				(child, writer)
			};
			let (mut player, player_writer) = pacat(3);
			let (mut victim, _victim_writer) = pacat(30);
			// Mixed output carries every channel of the playing client.
			let levels = tokio::task::spawn_blocking({
				let frames = frames.clone();
				move || {
					let deadline = time::Instant::now() + time::Duration::from_secs(3);
					while time::Instant::now() < deadline {
						let frame = frames.recv_timeout(time::Duration::from_secs(1)).unwrap();
						let levels: Vec<f32> = (0..usize::from(channels))
							.map(|channel| {
								frame
									.buf
									.iter()
									.skip(channel)
									.step_by(usize::from(channels))
									.fold(0f32, |m, v| m.max(v.abs()))
							})
							.collect();
						if levels.iter().all(|level| *level > 0.05) {
							return Some(levels);
						}
					}
					None
				}
			})
			.await
			.unwrap();
			assert!(levels.is_some(), "{channels} channels: every channel reaches capture");
			// End one client abruptly while the other plays.
			victim.kill().unwrap();
			victim.wait().unwrap();
			let still_capturing = tokio::task::spawn_blocking({
				let frames = frames.clone();
				move || (0..20).all(|_| frames.recv_timeout(time::Duration::from_secs(1)).is_ok())
			})
			.await
			.unwrap();
			assert!(
				still_capturing,
				"{channels} channels: capture continues after a client vanished"
			);
			player_writer.join().unwrap();
			let _ = player.wait();
			stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
			tokio::time::timeout(time::Duration::from_secs(2), stop.wait_shutdown_complete())
				.await
				.expect("stop after real clients");
		}
	}
}
