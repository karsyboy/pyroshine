//! Desktop Mode records the guard-owned sink through the user's PipeWire.
//! The guard journals and redirects desktop playback; this worker only receives
//! samples and passes them to the existing Opus packet clock. Audio failures
//! stop the session so cleanup restores the saved destinations.
//!
//! Periods enter a buffer bounded to [`MAX_BUFFERED_PACKETS`]. Each packet tick
//! drains it, padding with silence when the desktop is quiet; excess packets
//! are dropped oldest first to prevent accumulated latency.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use pipewire as pw;
use pw::spa::pod::{Object, Pod, Property, Value, ValueArray};
use pw::spa::sys as spa_sys;
use pw::spa::utils::Id;

use super::PulseReconfigure;
use super::pulse_server::{AudioFrame, CAPTURE_SAMPLE_RATE};
use crate::session::manager::SessionShutdownReason;

/// Packets buffered before the oldest are dropped.
const MAX_BUFFERED_PACKETS: usize = 3;

/// PipeWire channel positions in the order the Opus layouts expect
/// (`buffer::output_positions`).
fn positions(channels: u8) -> &'static [u32] {
	match channels {
		6 => &[
			spa_sys::SPA_AUDIO_CHANNEL_FL,
			spa_sys::SPA_AUDIO_CHANNEL_FR,
			spa_sys::SPA_AUDIO_CHANNEL_FC,
			spa_sys::SPA_AUDIO_CHANNEL_LFE,
			spa_sys::SPA_AUDIO_CHANNEL_RL,
			spa_sys::SPA_AUDIO_CHANNEL_RR,
		],
		8 => &[
			spa_sys::SPA_AUDIO_CHANNEL_FL,
			spa_sys::SPA_AUDIO_CHANNEL_FR,
			spa_sys::SPA_AUDIO_CHANNEL_FC,
			spa_sys::SPA_AUDIO_CHANNEL_LFE,
			spa_sys::SPA_AUDIO_CHANNEL_RL,
			spa_sys::SPA_AUDIO_CHANNEL_RR,
			spa_sys::SPA_AUDIO_CHANNEL_SL,
			spa_sys::SPA_AUDIO_CHANNEL_SR,
		],
		_ => &[spa_sys::SPA_AUDIO_CHANNEL_FL, spa_sys::SPA_AUDIO_CHANNEL_FR],
	}
}

fn format_pod(channels: u8) -> Vec<u8> {
	let value = Value::Object(Object {
		type_: spa_sys::SPA_TYPE_OBJECT_Format,
		id: spa_sys::SPA_PARAM_EnumFormat,
		properties: vec![
			Property::new(
				spa_sys::SPA_FORMAT_mediaType,
				Value::Id(Id(spa_sys::SPA_MEDIA_TYPE_audio)),
			),
			Property::new(
				spa_sys::SPA_FORMAT_mediaSubtype,
				Value::Id(Id(spa_sys::SPA_MEDIA_SUBTYPE_raw)),
			),
			Property::new(
				spa_sys::SPA_FORMAT_AUDIO_format,
				Value::Id(Id(spa_sys::SPA_AUDIO_FORMAT_F32_LE)),
			),
			Property::new(spa_sys::SPA_FORMAT_AUDIO_rate, Value::Int(CAPTURE_SAMPLE_RATE as i32)),
			Property::new(spa_sys::SPA_FORMAT_AUDIO_channels, Value::Int(i32::from(channels))),
			Property::new(
				spa_sys::SPA_FORMAT_AUDIO_position,
				Value::ValueArray(ValueArray::Id(positions(channels).iter().map(|&p| Id(p)).collect())),
			),
		],
	});
	pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
		.expect("serializing a well-formed pod")
		.0
		.into_inner()
}

/// Interleaved samples between PipeWire and the packet clock.
#[derive(Debug)]
pub(crate) struct PacketBuffer {
	samples: VecDeque<f32>,
	channels: usize,
	packet_frames: usize,
	pub dropped: u64,
	pub underruns: u64,
}

impl PacketBuffer {
	pub(crate) fn new(channels: u8, packet_frames: usize) -> Self {
		Self {
			samples: VecDeque::new(),
			channels: usize::from(channels),
			packet_frames,
			dropped: 0,
			underruns: 0,
		}
	}

	fn packet_len(&self) -> usize {
		self.packet_frames * self.channels
	}

	/// Append captured samples, dropping the oldest whole frames beyond the bound.
	pub(crate) fn push(&mut self, samples: &[f32]) {
		self.samples.extend(samples);
		let limit = self.packet_len() * MAX_BUFFERED_PACKETS;
		if self.samples.len() > limit {
			let excess = self.samples.len() - limit;
			let excess = excess.div_ceil(self.channels) * self.channels;
			self.samples.drain(..excess.min(self.samples.len()));
			self.dropped += 1;
		}
	}

	/// Fill `out` with one packet, silence where nothing was captured.
	pub(crate) fn pop(&mut self, out: &mut Vec<f32>) {
		let len = self.packet_len();
		out.clear();
		let available = self.samples.len().min(len);
		out.extend(self.samples.drain(..available));
		if available < len {
			if available > 0 || self.underruns == 0 {
				self.underruns += 1;
			}
			out.resize(len, 0.0);
		}
	}

	pub(crate) fn clear(&mut self) {
		self.samples.clear();
	}
}

/// Capture state shared with the PipeWire callbacks.
struct Capture {
	buffer: std::rc::Rc<std::cell::RefCell<PacketBuffer>>,
	_listener: pw::stream::StreamListener<()>,
	stream: pw::stream::StreamRc,
}

fn connect(
	core: &pw::core::CoreRc,
	channels: u8,
	packet_frames: usize,
	target: Option<&str>,
) -> Result<Capture, String> {
	let latency = format!("{packet_frames}/{CAPTURE_SAMPLE_RATE}");
	let stream = pw::stream::StreamRc::new(
		core.clone(),
		"pyroshine-desktop-audio",
		pw::properties::properties! {
			*pw::keys::MEDIA_TYPE => "Audio",
			*pw::keys::MEDIA_CATEGORY => "Capture",
			*pw::keys::MEDIA_ROLE => "Music",
			*pw::keys::STREAM_CAPTURE_SINK => "true",
			*pw::keys::NODE_LATENCY => latency.as_str(),
			*pw::keys::APP_NAME => "Pyroshine",
				"target.object" => target.unwrap_or("@DEFAULT_AUDIO_SINK@"),
			"node.description" => "Pyroshine Desktop Mode",
		},
	)
	.map_err(|e| format!("cannot create the PipeWire audio stream: {e}"))?;
	let buffer = std::rc::Rc::new(std::cell::RefCell::new(PacketBuffer::new(channels, packet_frames)));
	let process_buffer = buffer.clone();
	let listener = stream
		.add_local_listener::<()>()
		.state_changed(|_, _, old, new| tracing::debug!(?old, ?new, "Desktop audio stream state"))
		.process(move |stream, _| {
			let Some(mut buffer) = stream.dequeue_buffer() else {
				return;
			};
			let datas = buffer.datas_mut();
			let Some(data) = datas.first_mut() else {
				return;
			};
			let chunk = data.chunk();
			let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
			if let Some(bytes) = data.data() {
				let end = (offset + size).min(bytes.len());
				let bytes = &bytes[offset.min(end)..end];
				// F32LE interleaved, as negotiated.
				let samples: Vec<f32> = bytes
					.as_chunks::<4>()
					.0
					.iter()
					.map(|b| f32::from_le_bytes(*b))
					.collect();
				process_buffer.borrow_mut().push(&samples);
			}
		})
		.register()
		.map_err(|e| format!("cannot listen to the PipeWire audio stream: {e}"))?;
	let pod = format_pod(channels);
	let mut params = [Pod::from_bytes(&pod).expect("serialized pod")];
	stream
		.connect(
			pw::spa::utils::Direction::Input,
			None,
			pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
			&mut params,
		)
		.map_err(|e| format!("cannot connect the desktop audio capture: {e}"))?;
	Ok(Capture {
		buffer,
		_listener: listener,
		stream,
	})
}

/// The desktop audio capture of a Desktop Mode session.
pub(crate) struct DesktopAudio;

impl DesktopAudio {
	/// Start capturing on the `desktop-audio` thread. `capture` false streams
	/// silence (`[desktop] audio = false`).
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn spawn(
		capture: bool,
		sink: Option<String>,
		channels: u8,
		packet_duration_ms: u32,
		frame_tx: crossbeam_channel::Sender<AudioFrame>,
		frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
		stop: ShutdownManager<SessionShutdownReason>,
		reconfigure_rx: crossbeam_channel::Receiver<PulseReconfigure>,
		generation: u64,
	) -> std::io::Result<()> {
		crate::session::negotiation::validate_audio_packet_duration(packet_duration_ms)
			.map_err(std::io::Error::other)?;
		// The session's audio source, like the Pulse server it replaces.
		let worker = crate::session::lifecycle::WorkerGuard::register(&stop, SessionShutdownReason::PulseServerStopped)
			.map_err(|()| std::io::Error::other("session stopped before desktop audio started"))?;
		std::thread::Builder::new()
			.name("desktop-audio".into())
			.spawn(move || {
				let _worker = worker;
				run(
					capture,
					sink,
					channels,
					packet_duration_ms,
					frame_tx,
					frame_recycle_rx,
					stop,
					reconfigure_rx,
					generation,
				);
			})?;
		Ok(())
	}
}

fn packet_frames(packet_duration_ms: u32) -> usize {
	(CAPTURE_SAMPLE_RATE / (1000 / packet_duration_ms)) as usize
}

#[allow(clippy::too_many_arguments)]
fn run(
	capture: bool,
	sink: Option<String>,
	mut channels: u8,
	mut packet_duration_ms: u32,
	frame_tx: crossbeam_channel::Sender<AudioFrame>,
	frame_recycle_rx: crossbeam_channel::Receiver<AudioFrame>,
	stop: ShutdownManager<SessionShutdownReason>,
	reconfigure_rx: crossbeam_channel::Receiver<PulseReconfigure>,
	mut generation: u64,
) {
	let mut clock = match mio_timerfd::TimerFd::new(mio_timerfd::ClockId::Monotonic) {
		Ok(clock) => clock,
		Err(error) => {
			tracing::error!(%error, "Desktop audio has no clock");
			return;
		},
	};
	let interval = |ms: u32| Duration::from_millis(u64::from(ms));
	if let Err(error) = clock.set_timeout_interval(&interval(packet_duration_ms)) {
		tracing::error!(%error, "Desktop audio has no clock");
		return;
	}

	// PipeWire is optional: without it (or with capture off) the stream is silent.
	let pipewire = capture.then(|| -> Result<_, String> {
		pw::init();
		let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
		let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
		let core = context
			.connect_rc(None)
			.map_err(|e| format!("cannot reach the desktop's PipeWire: {e}"))?;
		Ok((mainloop, context, core))
	});
	let pipewire = match pipewire {
		Some(Ok(pipewire)) => Some(pipewire),
		Some(Err(error)) => {
			tracing::warn!(%error, "Desktop audio is unavailable; streaming silence");
			None
		},
		None => None,
	};
	if capture && pipewire.is_none() {
		return;
	}
	let must_capture = capture;
	let mut capture = pipewire.as_ref().and_then(|(_, _, core)| {
		connect(core, channels, packet_frames(packet_duration_ms), sink.as_deref())
			.inspect_err(|error| tracing::warn!(%error, "Desktop audio is unavailable; streaming silence"))
			.ok()
	});
	if capture.is_some() {
		tracing::info!(channels, packet_duration_ms, "Capturing the desktop's audio output");
	}
	if must_capture && capture.is_none() {
		return;
	}
	let mut silence = PacketBuffer::new(channels, packet_frames(packet_duration_ms));
	let epoch = Instant::now();
	let mut spare: Option<AudioFrame> = None;
	let mut last_report = Instant::now();
	let mut heard = false;

	while !stop.is_shutdown_triggered() {
		if capture
			.as_ref()
			.is_some_and(|c| matches!(c.stream.state(), pw::stream::StreamState::Error(_)))
		{
			break;
		}
		while let Ok(request) = reconfigure_rx.try_recv() {
			if request.reconfigure_capture
				&& (request.channels != channels || request.packet_duration_ms != packet_duration_ms)
			{
				channels = match request.channels {
					6 | 8 => request.channels,
					_ => 2,
				};
				packet_duration_ms = request.packet_duration_ms;
				let _ = clock.set_timeout_interval(&interval(packet_duration_ms));
				// A new format needs a new stream; PipeWire converts the sink's layout.
				drop(capture.take());
				capture = pipewire.as_ref().and_then(|(_, _, core)| {
					connect(core, channels, packet_frames(packet_duration_ms), sink.as_deref())
						.inspect_err(
							|error| tracing::warn!(%error, "Desktop audio reconfiguration failed; streaming silence"),
						)
						.ok()
				});
				silence = PacketBuffer::new(channels, packet_frames(packet_duration_ms));
				tracing::info!(channels, packet_duration_ms, "Reconfigured desktop audio capture");
			} else if let Some(capture) = &capture {
				// A new epoch starts without audio from the previous one.
				capture.buffer.borrow_mut().clear();
			}
			generation = request.generation;
			let _ = request.applied.send(Ok(()));
		}

		let mut fds = vec![libc::pollfd {
			fd: clock.as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		}];
		let loop_fd: Option<BorrowedFd<'_>> = pipewire.as_ref().map(|(mainloop, _, _)| mainloop.loop_().fd());
		if let Some(fd) = loop_fd {
			fds.push(libc::pollfd {
				fd: fd.as_raw_fd(),
				events: libc::POLLIN,
				revents: 0,
			});
		}
		// SAFETY: valid pollfd array for the call.
		let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
		if ready <= 0 {
			continue;
		}
		if fds.get(1).is_some_and(|fd| fd.revents & libc::POLLIN != 0)
			&& let Some((mainloop, _, _)) = &pipewire
		{
			mainloop.loop_().iterate(pw::loop_::Timeout::None);
		}
		if fds[0].revents & libc::POLLIN == 0 {
			continue;
		}
		let ticks = clock.read().unwrap_or(1).max(1);
		for _ in 0..ticks {
			let mut frame = spare
				.take()
				.or_else(|| frame_recycle_rx.try_recv().ok())
				.unwrap_or_else(|| AudioFrame {
					buf: Vec::new(),
					generation,
					capture_ts_ms: 0,
				});
			match &capture {
				Some(capture) => capture.buffer.borrow_mut().pop(&mut frame.buf),
				None => silence.pop(&mut frame.buf),
			}
			if !heard && frame.buf.iter().any(|&sample| sample != 0.0) {
				heard = true;
				let peak = frame.buf.iter().fold(0.0f32, |peak, &s| peak.max(s.abs()));
				tracing::info!(peak, "Desktop audio is playing");
			}
			frame.generation = generation;
			frame.capture_ts_ms = epoch.elapsed().as_millis() as u64;
			match frame_tx.try_send(frame) {
				Ok(()) => {},
				Err(crossbeam_channel::TrySendError::Full(frame)) => spare = Some(frame),
				Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
			}
		}
		if last_report.elapsed() >= Duration::from_secs(30)
			&& let Some(capture) = &capture
		{
			let buffer = capture.buffer.borrow();
			tracing::debug!(
				dropped = buffer.dropped,
				underruns = buffer.underruns,
				"Desktop audio capture"
			);
			last_report = Instant::now();
		}
	}
	if let Some(capture) = capture {
		let _ = capture.stream.disconnect();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn packets_are_padded_with_silence_and_backlog_is_bounded() {
		let mut buffer = PacketBuffer::new(2, 240);
		let mut out = Vec::new();
		buffer.pop(&mut out);
		assert_eq!(out, vec![0.0; 480]);
		// Ten packets arrive at once (a stalled tick): only three remain.
		buffer.push(&vec![0.5; 4800]);
		assert_eq!(buffer.samples.len(), 480 * MAX_BUFFERED_PACKETS);
		for _ in 0..MAX_BUFFERED_PACKETS {
			buffer.pop(&mut out);
			assert!(out.iter().all(|&s| s == 0.5));
		}
		buffer.pop(&mut out);
		assert!(out.iter().all(|&s| s == 0.0));
		// A partial period is kept at frame granularity and padded.
		buffer.push(&[0.25; 6]);
		buffer.pop(&mut out);
		assert_eq!(&out[..6], &[0.25; 6]);
		assert!(out[6..].iter().all(|&s| s == 0.0));
	}

	#[test]
	fn layouts_match_the_opus_channel_order() {
		assert_eq!(positions(2).len(), 2);
		assert_eq!(positions(6)[3], spa_sys::SPA_AUDIO_CHANNEL_LFE);
		assert_eq!(positions(8)[7], spa_sys::SPA_AUDIO_CHANNEL_SR);
		assert_eq!(packet_frames(5), 240);
		assert_eq!(packet_frames(10), 480);
	}
}
