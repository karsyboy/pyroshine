//! Client microphone: Apollo's microphone passthrough, the wire format Nightjar
//! and Pyrolight send (and ClassicOldSong's moonlight-common-c, which defined
//! it).
//!
//! The client sends one UDP datagram per mono 48 kHz Opus packet to the
//! microphone port it learned from RTSP `SETUP streamid=mic`:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0 | flags, 0 |
//! | 1 | type, `0x61` (Opus) |
//! | 2–3 | sequence, little-endian, +1 per packet |
//! | 4–7 | client milliseconds, little-endian (diagnostics only) |
//! | 8–11 | magic `0x12345678`, little-endian |
//! | 12– | the Opus packet, AES-128-CBC under the launch's `rikey` |
//!
//! The IV is the audio stream's: big-endian `rikeyid + sequence`, zero padded.
//! moonlight-common-c pads twice, first to a whole block with bytes holding the
//! pad length and then a full PKCS#7 block; Android clients pad once. Both are
//! accepted: after the PKCS#7 layer, a plaintext of whole blocks whose tail
//! looks like block padding loses it. A once-padded packet of whole blocks that
//! happens to end like padding is cut by a few bytes; Opus decodes the rest.
//!
//! Nothing goes back to the client. The receiver only accepts datagrams from
//! the authorized client's address that decrypt under the current session key,
//! decodes them in arrival order (late packets are dropped, short gaps are
//! recovered from in-band FEC or concealed), and queues the samples in a
//! [`MicrophoneFeed`]. The session's audio server (the embedded Pulse server
//! in Game Mode, a PipeWire source in Desktop Mode) plays the feed out at its
//! own clock; the feed's bounds keep the microphone's delay small.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_shutdown::{DelayShutdownToken, ShutdownManager};
use tokio::net::UdpSocket;

use super::encoder::audio_iv;
use crate::session::keys::ActiveKeys;
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::audio::CAPTURE_SAMPLE_RATE;
use crate::session::{AuthorizationReceiver, SessionKeysReceiver};

/// Datagram type of an Opus microphone packet.
const PACKET_TYPE_OPUS: u8 = 0x61;
/// Little-endian magic after the timestamp.
const MAGIC: u32 = 0x1234_5678;
const HEADER_LEN: usize = 12;
/// Largest datagram read; clients keep packets within 1400 bytes.
const MAX_DATAGRAM: usize = 2048;

/// The microphone's sample rate; Opus decodes any packet to it.
pub(crate) const MICROPHONE_SAMPLE_RATE: u32 = CAPTURE_SAMPLE_RATE;
/// Samples in the longest Opus packet (120 ms).
const MAX_PACKET_SAMPLES: usize = MICROPHONE_SAMPLE_RATE as usize * 120 / 1000;
/// Lost packets concealed in one gap; a longer gap is a stall, not loss.
const MAX_CONCEALED_PACKETS: u16 = 5;
/// A packet this far behind the newest one is a restarted sender, not a late packet.
const RESTART_DISTANCE: i16 = -32;
/// Silence after which the microphone counts as stopped.
const IDLE_AFTER: Duration = Duration::from_secs(3);

const fn ms(ms: usize) -> usize {
	MICROPHONE_SAMPLE_RATE as usize * ms / 1000
}

/// Queued audio before playback starts or resumes after running dry: absorbs
/// arrival jitter of 10 and 20 ms packets.
const START_SAMPLES: usize = ms(30);
/// Queued audio beyond which the oldest is dropped, down to [`TRIM_TO_SAMPLES`],
/// so a burst or a fast client clock cannot leave the microphone behind.
const MAX_SAMPLES: usize = ms(120);
const TRIM_TO_SAMPLES: usize = ms(60);

/// The header fields and encrypted payload of a microphone datagram.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Packet<'a> {
	pub sequence: u16,
	pub timestamp_ms: u32,
	pub ciphertext: &'a [u8],
}

/// A microphone datagram, or `None` for anything else.
pub(crate) fn parse(datagram: &[u8]) -> Option<Packet<'_>> {
	if datagram.len() <= HEADER_LEN || datagram[0] != 0 || datagram[1] != PACKET_TYPE_OPUS {
		return None;
	}
	let magic = u32::from_le_bytes(datagram[8..12].try_into().ok()?);
	if magic != MAGIC {
		return None;
	}
	Some(Packet {
		sequence: u16::from_le_bytes([datagram[2], datagram[3]]),
		timestamp_ms: u32::from_le_bytes(datagram[4..8].try_into().ok()?),
		ciphertext: &datagram[HEADER_LEN..],
	})
}

/// The Opus packet inside `packet`, decrypted under `keys`.
pub(crate) fn open(keys: &ActiveKeys, packet: &Packet<'_>) -> Result<Vec<u8>, String> {
	let iv = audio_iv(keys.key_id(), packet.sequence);
	let mut opus = crate::crypto::decrypt_cbc(packet.ciphertext, keys.key().as_bytes(), &iv)?;
	// The inner block padding of a twice-padded packet (see the module docs).
	if opus.len().is_multiple_of(16)
		&& let Some(&last) = opus.last()
	{
		let pad = usize::from(last);
		if (1..16).contains(&pad) && opus[opus.len() - pad..].iter().all(|&byte| byte == last) {
			opus.truncate(opus.len() - pad);
		}
	}
	if opus.is_empty() {
		return Err("empty Opus packet".into());
	}
	Ok(opus)
}

/// Decoded microphone audio, mono at [`MICROPHONE_SAMPLE_RATE`], between the
/// receiver and the session's audio server.
#[derive(Clone, Debug, Default)]
pub(crate) struct MicrophoneFeed(Arc<Mutex<FeedState>>);

#[derive(Debug, Default)]
struct FeedState {
	samples: VecDeque<f32>,
	/// Playing out; cleared when the queue runs dry, so it refills to
	/// [`START_SAMPLES`] first.
	playing: bool,
	underruns: u64,
	trimmed_samples: u64,
}

/// Counters of a feed, for the stop summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FeedStats {
	pub underruns: u64,
	pub trimmed_ms: u64,
}

impl MicrophoneFeed {
	fn state(&self) -> std::sync::MutexGuard<'_, FeedState> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Queue decoded samples, dropping the oldest beyond the bound.
	pub(crate) fn push(&self, samples: &[f32]) {
		let mut state = self.state();
		state.samples.extend(samples);
		if state.samples.len() > MAX_SAMPLES {
			let excess = state.samples.len() - TRIM_TO_SAMPLES;
			state.samples.drain(..excess);
			state.trimmed_samples += excess as u64;
		}
	}

	/// Fill `out` with the next samples, silence where there are none. Returns
	/// whether any microphone audio was written.
	pub(crate) fn pull(&self, out: &mut [f32]) -> bool {
		let mut state = self.state();
		if !state.playing {
			if state.samples.len() < START_SAMPLES {
				out.fill(0.0);
				return false;
			}
			state.playing = true;
		}
		let available = state.samples.len().min(out.len());
		for (sample, queued) in out.iter_mut().zip(state.samples.drain(..available)) {
			*sample = queued;
		}
		out[available..].fill(0.0);
		if available < out.len() {
			state.playing = false;
			state.underruns += 1;
		}
		available > 0
	}

	/// Drop queued audio (a new client or sender).
	pub(crate) fn clear(&self) {
		let mut state = self.state();
		state.samples.clear();
		state.playing = false;
	}

	/// Counters so far, reset by the call.
	fn take_stats(&self) -> FeedStats {
		let mut state = self.state();
		let stats = FeedStats {
			underruns: state.underruns,
			trimmed_ms: state.trimmed_samples * 1000 / u64::from(MICROPHONE_SAMPLE_RATE),
		};
		state.underruns = 0;
		state.trimmed_samples = 0;
		stats
	}
}

/// What became of one datagram.
#[derive(Debug, PartialEq, Eq)]
enum Received {
	/// Decoded and queued, after `concealed` lost packets were recovered or concealed.
	Played { concealed: u16 },
	/// Arrived after a newer packet.
	Late,
	/// Not from the authorized client.
	Unauthorized,
	/// Not a microphone packet, or not sealed with the session key.
	Invalid,
}

/// Counters for the microphone's start/stop log lines.
#[derive(Debug, Default)]
struct ReceiveStats {
	packets: u64,
	concealed: u64,
	recovered: u64,
	late: u64,
	invalid: u64,
	decode_errors: u64,
}

/// Decodes datagrams into a feed; socket-free for tests.
struct Receiver {
	decoder: opus::Decoder,
	feed: MicrophoneFeed,
	/// Sequence of the newest decoded packet of the current sender.
	last_sequence: Option<u16>,
	/// Samples per packet of the current sender, for concealment.
	packet_samples: usize,
	/// Key generation and client of the current sender.
	sender: Option<(u64, IpAddr)>,
	last_packet: Option<Instant>,
	pcm: Vec<f32>,
	stats: ReceiveStats,
}

impl Receiver {
	fn new(feed: MicrophoneFeed) -> Result<Self, opus::Error> {
		Ok(Self {
			decoder: opus::Decoder::new(MICROPHONE_SAMPLE_RATE, opus::Channels::Mono)?,
			feed,
			last_sequence: None,
			packet_samples: ms(10),
			sender: None,
			last_packet: None,
			pcm: vec![0.0; MAX_PACKET_SAMPLES],
			stats: ReceiveStats::default(),
		})
	}

	fn receive(
		&mut self,
		datagram: &[u8],
		from: SocketAddr,
		keys: &ActiveKeys,
		authorized: impl Fn(IpAddr) -> bool,
	) -> Received {
		if !authorized(from.ip()) {
			return Received::Unauthorized;
		}
		let Some(packet) = parse(datagram) else {
			self.stats.invalid += 1;
			return Received::Invalid;
		};
		let Ok(opus) = open(keys, &packet) else {
			self.stats.invalid += 1;
			return Received::Invalid;
		};

		// A new key (a resume) or client starts a new sender.
		let sender = (
			keys.material().generation(),
			crate::session::authorization::canonical_ip(from.ip()),
		);
		if self.sender != Some(sender) {
			if self.sender.is_some() {
				tracing::info!(client = %sender.1, "Microphone sender changed");
			}
			self.restart();
			self.sender = Some(sender);
		}

		let mut concealed = 0;
		if let Some(last) = self.last_sequence {
			let distance = packet.sequence.wrapping_sub(last) as i16;
			if distance <= 0 && distance > RESTART_DISTANCE {
				self.stats.late += 1;
				return Received::Late;
			}
			if distance > 1 && distance as u16 - 1 <= MAX_CONCEALED_PACKETS {
				concealed = distance as u16 - 1;
				self.conceal(concealed, &opus);
			}
		}
		self.last_sequence = Some(packet.sequence);
		self.last_packet = Some(Instant::now());
		self.stats.packets += 1;

		if let Ok(samples) = opus::packet::get_nb_samples(&opus, MICROPHONE_SAMPLE_RATE)
			&& (1..=MAX_PACKET_SAMPLES).contains(&samples)
		{
			self.packet_samples = samples;
		}
		match self.decoder.decode_float(&opus, &mut self.pcm, false) {
			Ok(samples) => self.feed.push(&self.pcm[..samples]),
			Err(error) => {
				self.stats.decode_errors += 1;
				if self.stats.decode_errors == 1 {
					tracing::warn!(%error, "Cannot decode a microphone packet");
				}
			},
		}
		Received::Played { concealed }
	}

	/// Fill `lost` missing packets before `next`: the last from `next`'s in-band
	/// FEC, the others by packet loss concealment.
	fn conceal(&mut self, lost: u16, next: &[u8]) {
		let samples = self.packet_samples;
		for index in 0..lost {
			let fec = index + 1 == lost;
			let input: &[u8] = if fec { next } else { &[] };
			match self.decoder.decode_float(input, &mut self.pcm[..samples], fec) {
				Ok(decoded) => {
					self.feed.push(&self.pcm[..decoded]);
					if fec {
						self.stats.recovered += 1;
					} else {
						self.stats.concealed += 1;
					}
				},
				Err(_) => self.stats.decode_errors += 1,
			}
		}
	}

	/// Forget the previous sender's stream position and audio.
	fn restart(&mut self) {
		self.last_sequence = None;
		let _ = self.decoder.reset_state();
		self.feed.clear();
	}

	/// End the current sender after [`IDLE_AFTER`] without packets; returns
	/// whether it just stopped.
	fn expire(&mut self, now: Instant) -> bool {
		match self.last_packet {
			Some(last) if now.duration_since(last) >= IDLE_AFTER => {
				self.last_packet = None;
				self.restart();
				true
			},
			_ => false,
		}
	}
}

/// The microphone's UDP socket, bound with the session's other stream ports.
pub(crate) struct MicrophoneSocket {
	socket: UdpSocket,
	feed: MicrophoneFeed,
}

impl MicrophoneSocket {
	pub(crate) async fn bind(address: &str, port: u16) -> std::io::Result<Self> {
		Ok(Self {
			socket: UdpSocket::bind((address, port)).await?,
			feed: MicrophoneFeed::default(),
		})
	}

	pub(crate) fn feed(&self) -> MicrophoneFeed {
		self.feed.clone()
	}

	/// Receive the client's microphone until the session stops. A failure here
	/// costs only the microphone; the delay token keeps the session's shutdown
	/// from completing before the socket is released.
	pub(crate) fn spawn(
		self,
		keys_rx: SessionKeysReceiver,
		authorization_rx: AuthorizationReceiver,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<(), ()> {
		let delay = stop
			.delay_shutdown_token()
			.map_err(|_| tracing::debug!("Session already stopped; not starting the microphone"))?;
		let receiver = Receiver::new(self.feed)
			.map_err(|error| tracing::error!(%error, "Cannot create the microphone decoder"))?;
		tokio::spawn(receive(self.socket, receiver, keys_rx, authorization_rx, stop, delay));
		Ok(())
	}
}

async fn receive(
	socket: UdpSocket,
	mut receiver: Receiver,
	keys_rx: SessionKeysReceiver,
	authorization_rx: AuthorizationReceiver,
	stop: ShutdownManager<SessionShutdownReason>,
	_delay: DelayShutdownToken<SessionShutdownReason>,
) {
	let mut buf = vec![0u8; MAX_DATAGRAM];
	let mut idle = tokio::time::interval(Duration::from_secs(1));
	idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let mut started_at: Option<Instant> = None;
	let mut unauthorized: u64 = 0;
	loop {
		tokio::select! {
			_ = idle.tick() => {
				if receiver.expire(Instant::now())
					&& let Some(started) = started_at.take()
				{
					log_stopped(&mut receiver, started);
				}
			},
			result = stop.wrap_cancel(socket.recv_from(&mut buf)) => {
				let (len, from) = match result {
					Ok(Ok(received)) => received,
					Ok(Err(error)) => {
						tracing::warn!(%error, "Microphone socket failed; the microphone is off for this session");
						break;
					},
					Err(_) => break,
				};
				let keys = keys_rx.borrow().clone();
				let outcome = {
					let authorization = authorization_rx.borrow();
					receiver.receive(&buf[..len], from, &keys, |ip| authorization.admits_peer(ip))
				};
				match outcome {
					Received::Played { .. } if started_at.is_none() => {
						started_at = Some(Instant::now());
						tracing::info!(client = %from, "Microphone started");
					},
					Received::Unauthorized => {
						unauthorized += 1;
						if unauthorized.is_power_of_two() {
							tracing::debug!(%from, count = unauthorized, "Ignoring microphone datagrams from an unauthorized address");
						}
					},
					_ => {},
				}
			},
		}
	}
	if let Some(started) = started_at {
		log_stopped(&mut receiver, started);
	}
	tracing::debug!("Microphone receiver stopped");
}

fn log_stopped(receiver: &mut Receiver, started: Instant) {
	let stats = std::mem::take(&mut receiver.stats);
	let feed = receiver.feed.take_stats();
	tracing::info!(
		seconds = started.elapsed().as_secs(),
		packets = stats.packets,
		recovered = stats.recovered,
		concealed = stats.concealed,
		late = stats.late,
		invalid = stats.invalid,
		decode_errors = stats.decode_errors,
		underruns = feed.underruns,
		trimmed_ms = feed.trimmed_ms,
		"Microphone stopped"
	);
}

#[cfg(test)]
mod tests {
	use std::net::Ipv4Addr;

	use super::*;

	const KEY: [u8; 16] = *b"mic-key-16-bytes";
	const KEY_ID: u32 = 0xffff_fff0;

	/// A datagram as moonlight-common-c seals it: block padding, then PKCS#7.
	fn seal(sequence: u16, opus: &[u8], twice: bool) -> Vec<u8> {
		let mut padded = opus.to_vec();
		if twice {
			let pad = (16 - opus.len() % 16) % 16;
			padded.resize(opus.len() + pad, pad as u8);
		}
		let iv = audio_iv(crate::session::keys::RemoteInputKeyId::new(KEY_ID), sequence);
		let mut datagram = vec![0, PACKET_TYPE_OPUS];
		datagram.extend_from_slice(&sequence.to_le_bytes());
		datagram.extend_from_slice(&1234u32.to_le_bytes());
		datagram.extend_from_slice(&MAGIC.to_le_bytes());
		datagram.extend_from_slice(&crate::crypto::encrypt_cbc(&padded, &KEY, &iv).unwrap());
		datagram
	}

	fn client() -> SocketAddr {
		SocketAddr::from((Ipv4Addr::new(192, 168, 1, 20), 50000))
	}

	fn tone(encoder: &mut opus::Encoder, frame: usize) -> Vec<u8> {
		let pcm: Vec<f32> = (0..ms(10))
			.map(|i| ((frame * ms(10) + i) as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5)
			.collect();
		encoder.encode_vec_float(&pcm, 1275).unwrap()
	}

	fn encoder() -> opus::Encoder {
		let mut encoder =
			opus::Encoder::new(MICROPHONE_SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
		encoder.set_inband_fec(true).unwrap();
		encoder.set_packet_loss_perc(10).unwrap();
		encoder
	}

	#[test]
	fn datagrams_open_with_either_padding() {
		let keys = ActiveKeys::for_test(KEY, KEY_ID);
		for len in [1, 13, 15, 16, 17, 31, 200, 1360] {
			let opus: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
			for twice in [true, false] {
				let datagram = seal(40, &opus, twice);
				let packet = parse(&datagram).unwrap();
				assert_eq!(packet.sequence, 40);
				assert_eq!(packet.timestamp_ms, 1234);
				assert_eq!(open(&keys, &packet).unwrap(), opus, "len {len}, twice {twice}");
			}
		}
	}

	/// A datagram from Pyrolight's moonlight-common-c `buildMicrophonePacket`
	/// (17 bytes 1..=17, sequence 40, 1234 ms), byte for byte.
	#[test]
	fn a_pyrolight_datagram_opens() {
		const DATAGRAM: &str = "00612800d204000078563412e7ffb8f89cf8cd70ab4e022e529668cfb8d6eea1a6148e3731773d3401dcb1c4dd017bb66bbaa82ce4bf6ac5cf4b4f5a";
		let datagram: Vec<u8> = (0..DATAGRAM.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&DATAGRAM[i..i + 2], 16).unwrap())
			.collect();
		let expected: Vec<u8> = (1..=17).collect();
		assert_eq!(seal(40, &expected, true), datagram);
		let packet = parse(&datagram).unwrap();
		assert_eq!((packet.sequence, packet.timestamp_ms), (40, 1234));
		assert_eq!(open(&ActiveKeys::for_test(KEY, KEY_ID), &packet).unwrap(), expected);
	}

	#[test]
	fn other_datagrams_are_not_microphone_packets() {
		let datagram = seal(1, &[1, 2, 3], true);
		assert!(parse(&datagram[..HEADER_LEN]).is_none());
		for (index, value) in [(0, 1), (1, 0x60), (8, 0)] {
			let mut changed = datagram.clone();
			changed[index] = value;
			assert!(parse(&changed).is_none(), "byte {index}");
		}
		let keys = ActiveKeys::for_test(*b"another-key-16by", KEY_ID);
		assert!(open(&keys, &parse(&datagram).unwrap()).is_err());
	}

	#[test]
	fn gaps_are_recovered_late_packets_dropped_and_restarts_followed() {
		let keys = ActiveKeys::for_test(KEY, KEY_ID);
		let feed = MicrophoneFeed::default();
		let mut receiver = Receiver::new(feed.clone()).unwrap();
		let mut encoder = encoder();
		let mut send = |receiver: &mut Receiver, sequence: u16, frame: usize| {
			let datagram = seal(sequence, &tone(&mut encoder, frame), true);
			receiver.receive(&datagram, client(), &keys, |_| true)
		};
		assert_eq!(send(&mut receiver, 65534, 0), Received::Played { concealed: 0 });
		assert_eq!(send(&mut receiver, 65535, 1), Received::Played { concealed: 0 });
		// Wraps, and two packets were lost on the way.
		assert_eq!(send(&mut receiver, 2, 4), Received::Played { concealed: 2 });
		assert_eq!(receiver.stats.recovered, 1);
		assert_eq!(receiver.stats.concealed, 1);
		assert_eq!(send(&mut receiver, 1, 3), Received::Late);
		assert_eq!(send(&mut receiver, 2, 4), Received::Late);
		// A long stall is not concealed.
		assert_eq!(send(&mut receiver, 100, 5), Received::Played { concealed: 0 });
		// A restarted sender is followed.
		assert_eq!(send(&mut receiver, 0, 6), Received::Played { concealed: 0 });
		assert_eq!(send(&mut receiver, 1, 7), Received::Played { concealed: 0 });
		// Six packets and two lost ones, 10 ms each.
		let queued = feed.state().samples.len();
		assert_eq!(queued, ms(80));

		let unauthorized = seal(2, &tone(&mut encoder, 8), true);
		assert_eq!(
			receiver.receive(&unauthorized, client(), &keys, |_| false),
			Received::Unauthorized
		);
		let other_key = ActiveKeys::for_test(*b"another-key-16by", KEY_ID);
		assert_eq!(
			receiver.receive(&unauthorized, client(), &other_key, |_| true),
			Received::Invalid
		);
	}

	#[test]
	fn a_new_session_key_starts_a_new_sender() {
		let feed = MicrophoneFeed::default();
		let mut receiver = Receiver::new(feed.clone()).unwrap();
		let mut encoder = encoder();
		let keys = || ActiveKeys::with_nonces(KEY, KEY_ID, crate::session::keys::KeyNonces::for_test(0, 0));
		let first = keys();
		let datagram = seal(100, &tone(&mut encoder, 0), true);
		assert_eq!(
			receiver.receive(&datagram, client(), &first, |_| true),
			Received::Played { concealed: 0 }
		);
		// The resumed client starts its sequence again; nothing from before is kept.
		let resumed = keys();
		let datagram = seal(0, &tone(&mut encoder, 1), true);
		assert_eq!(
			receiver.receive(&datagram, client(), &resumed, |_| true),
			Received::Played { concealed: 0 }
		);
		assert_eq!(feed.state().samples.len(), ms(10));
	}

	#[test]
	fn the_feed_starts_after_a_margin_and_stays_bounded() {
		let feed = MicrophoneFeed::default();
		let mut out = vec![1.0; ms(10)];
		feed.push(&vec![0.5; ms(20)]);
		assert!(!feed.pull(&mut out), "waits for the start margin");
		assert!(out.iter().all(|&s| s == 0.0));
		feed.push(&vec![0.5; ms(10)]);
		for _ in 0..3 {
			assert!(feed.pull(&mut out));
			assert!(out.iter().all(|&s| s == 0.5));
		}
		// Ran dry: silence, then a new start margin.
		assert!(!feed.pull(&mut out));
		feed.push(&vec![0.25; ms(10)]);
		assert!(!feed.pull(&mut out));
		assert_eq!(feed.take_stats().underruns, 1);

		feed.push(&vec![0.25; MAX_SAMPLES * 2]);
		assert_eq!(feed.state().samples.len(), TRIM_TO_SAMPLES);
		assert!(feed.take_stats().trimmed_ms > 0);
	}

	/// The spawned receiver plays an authorized client's packets and ignores
	/// other addresses; the session's stop ends it.
	#[tokio::test]
	async fn the_receiver_plays_the_authorized_client() {
		use crate::session::authorization::StreamAuthorization;
		let socket = MicrophoneSocket::bind("127.0.0.1", 0).await.unwrap();
		let target = socket.socket.local_addr().unwrap();
		let feed = socket.feed();
		let keys = ActiveKeys::for_test(KEY, KEY_ID);
		let (_keys_tx, keys_rx) = tokio::sync::watch::channel(keys);
		let authorization = StreamAuthorization::new(1, Ipv4Addr::LOCALHOST.into()).unwrap();
		let (authorization_tx, authorization_rx) = tokio::sync::watch::channel(authorization);
		let stop = ShutdownManager::new();
		socket.spawn(keys_rx, authorization_rx, stop.clone()).unwrap();

		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut encoder = encoder();
		for sequence in 0..5u16 {
			let datagram = seal(sequence, &tone(&mut encoder, usize::from(sequence)), true);
			client.send_to(&datagram, target).await.unwrap();
		}
		let queued = || feed.state().samples.len();
		tokio::time::timeout(Duration::from_secs(2), async {
			while queued() < ms(50) {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("five packets are queued");

		// Another client of the session's address space is not admitted.
		authorization_tx.send_replace(StreamAuthorization::new(2, Ipv4Addr::new(10, 0, 0, 9).into()).unwrap());
		let datagram = seal(5, &tone(&mut encoder, 5), true);
		client.send_to(&datagram, target).await.unwrap();
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(queued(), ms(50));

		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(Duration::from_secs(2), stop.wait_shutdown_complete())
			.await
			.expect("the receiver releases its socket on stop");
		UdpSocket::bind(target)
			.await
			.expect("the microphone port is free again");
	}

	#[test]
	fn a_quiet_sender_expires() {
		let mut receiver = Receiver::new(MicrophoneFeed::default()).unwrap();
		let keys = ActiveKeys::for_test(KEY, KEY_ID);
		let datagram = seal(7, &tone(&mut encoder(), 0), true);
		receiver.receive(&datagram, client(), &keys, |_| true);
		let now = Instant::now();
		assert!(!receiver.expire(now));
		assert!(receiver.expire(now + IDLE_AFTER));
		assert!(!receiver.expire(now + IDLE_AFTER * 2));
		assert_eq!(receiver.last_sequence, None);
	}
}
