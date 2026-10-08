pub mod pyrowave_protocol;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};

use crate::session::authorization::MediaStream;
use crate::session::compositor::frame::HdrModeState;
use crate::session::lifecycle::{StartLatch, StartWaiter, WorkerGuard};
use crate::session::manager::SessionShutdownReason;
use crate::session::stream::MediaDemand;
use crate::session::{AuthorizationReceiver, SessionKeysReceiver};

mod diagnostics;
pub(crate) mod fec;
mod format;
mod gso_socket;
mod pacing_timer;
mod packetizer;
mod pipeline;
pub(crate) mod pyrowave;
mod rtp_clock;
pub use pipeline::ConversionQueueMode;
pub use pyrowave::PyroWaveQueueMode;
mod shard_batch;
pub use fec::FecMode;
use fec::FrameFecStatus;
pub use format::{
	BitDepth, ChromaFormat, ColorPrimaries, ColorRange, MatrixCoefficients, NegotiatedVideoFormat, TransferFunction,
	VideoChromaSampling, VideoCodec, VideoDynamicRange, VideoFormat,
};
use gso_socket::{UdpGsoSocket, duration_micros_u64};
use pipeline::VideoPipeline;
use shard_batch::ShardBatch;

/// Configuration for the video stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub port: u16,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,

	/// Whether FEC is disabled, fixed, or driven by client feedback.
	pub fec_mode: FecMode,

	/// Lower bound for automatic FEC.
	pub fec_min_percentage: u8,

	/// Upper bound for automatic FEC.
	pub fec_max_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	#[serde(default)]
	pub encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	/// packetize than the frame budget.
	#[serde(default)]
	pub log_frame_spikes: bool,

	/// Emit periodic capture, pipeline, transport, and runtime statistics.
	/// Disabling this also skips diagnostic accumulation and process sampling;
	/// benchmark frame statistics and operational warnings remain available.
	pub log_stats: bool,
	/// GPU scheduling preference for cross-process PyroWave encoding; auto prefers graphics.
	pub pyrowave_queue: pyrowave::PyroWaveQueueMode,
	/// Queue for H.264/HEVC/AV1 RGB-to-YCbCr conversion; auto prefers a
	/// dedicated compute family so a GPU-bound game cannot delay it.
	pub conversion_queue: pipeline::ConversionQueueMode,

	/// Upper bound for the client-requested video packet size, in bytes.
	///
	/// Moonlight's default packet size (1392 bytes) can exceed the path MTU
	/// over VPNs and tunnels, causing fragmented, dropped video. When non-zero,
	/// the client's `x-nv-video[0].packetSize` is clamped to this value; a
	/// smaller client request is honored. `0` disables the cap.
	///
	/// This is the stream's packet size, not a raw interface MTU: the on-wire
	/// UDP payload is `max_packet_size + 16` bytes. For example, to fit a
	/// 1420-byte WireGuard MTU over IPv4, use 1376 (1420 minus the 20-byte IP
	/// and 8-byte UDP headers and the 16-byte stream overhead).
	#[serde(default)]
	pub max_packet_size: usize,
}

/// Smallest accepted packet size cap. Lower values would leave almost no room
/// for payload after the 16-byte NV video header, so they are ignored.
const MIN_PACKET_SIZE: usize = 200;

impl VideoStreamConfig {
	/// Clamp a client-requested video packet size to `max_packet_size`.
	///
	/// A client only asks for what it believes it can receive, so a request
	/// smaller than the cap is honored; the cap only lowers oversized requests.
	/// A cap of `0` disables the limit.
	///
	/// The cap bounds the on-wire datagram at `max_packet_size + 16` bytes. An
	/// encrypted stream's packet size excludes its 32-byte per-shard prefix
	/// (Moonlight subtracts it before announcing), so the cap does as well.
	pub(crate) fn clamp_packet_size(&self, requested: usize, encrypted: bool) -> usize {
		if self.max_packet_size == 0 {
			return requested;
		}
		if self.max_packet_size < MIN_PACKET_SIZE {
			tracing::warn!(
				"max_packet_size {} is below the minimum of {MIN_PACKET_SIZE}, ignoring the cap.",
				self.max_packet_size
			);
			return requested;
		}
		let cap = if encrypted {
			self.max_packet_size - packetizer::ENC_PREFIX_SIZE
		} else {
			self.max_packet_size
		};
		requested.min(cap)
	}
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			// Fixed preserves the behavior of configurations written before the
			// explicit policy fields existed. Users opt into feedback with `auto`.
			fec_mode: FecMode::Fixed,
			fec_min_percentage: 0,
			fec_max_percentage: 25,
			encrypt: false,
			log_frame_spikes: false,
			log_stats: true,
			pyrowave_queue: pyrowave::PyroWaveQueueMode::Auto,
			conversion_queue: pipeline::ConversionQueueMode::Auto,
			max_packet_size: 0,
		}
	}
}

/// Per-frame encoding statistics emitted by the video pipeline.
///
/// Sent via `broadcast` channel, receivable through `SessionManager::bench_stats_receiver()`.
#[derive(Clone, Debug)]
pub struct FrameStats {
	/// Time the frame spent waiting in the compositor's output channel.
	pub channel_wait: std::time::Duration,
	/// Time spent importing the DMA-BUF into Vulkan.
	pub import: std::time::Duration,
	/// Time spent on GPU color conversion.
	pub convert: std::time::Duration,
	/// Time spent submitting the frame to the asynchronous encoder.
	pub submit: std::time::Duration,
	/// Time between submit completion and the packet consumer awaiting the encode future.
	pub consumer_queue: std::time::Duration,
	/// Time from submit completion until the asynchronous encode/readback future has resolved.
	pub encode_wait: std::time::Duration,
	/// Time spent packetizing the encoded data.
	pub packetize: std::time::Duration,
	/// Queue handoff/residence measured separately from actual socket work.
	pub enqueue: std::time::Duration,
	/// Actual socket work through final submission or failure.
	pub send: std::time::Duration,
	/// Total end-to-end latency for this frame.
	pub total: std::time::Duration,
	/// Number of bytes encoded for this frame.
	pub encoded_bytes: usize,
	/// Successfully submitted UDP payload bytes including FEC and encryption;
	/// kernel acceptance does not confirm receiver delivery.
	pub wire_bytes: usize,
	/// Number of UDP video shards successfully submitted to the kernel.
	pub packet_count: usize,
	/// Logical UDP attempts; readiness retries/fallback duplication are excluded.
	pub attempted_packet_count: usize,
	pub failed_packet_count: usize,
	pub discarded_packet_count: usize,
	/// Stale compositor frames discarded before this frame was encoded.
	pub stale_frames_dropped: u32,
	/// Whether this frame is a key (IDR) frame.
	pub is_key_frame: bool,
	/// Content-time interval since the previous frame with new content (the
	/// source's frame time as carried in RTP); zero for replays and the first
	/// frame of an epoch.
	pub source_interval: std::time::Duration,
	/// Time from the content becoming current to its capture.
	pub content_age: std::time::Duration,
	/// Captured with VRR (presentation-driven) pacing.
	pub vrr_capture: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoStreamContext {
	/// Explicit setup-time PyroWave dialect; conventional codecs use None.
	pub pyrowave_dialect: Option<pyrowave_protocol::PyroWaveDialect>,
	/// Width of the video stream in pixels.
	pub width: u32,

	/// Height of the video stream in pixels.
	pub height: u32,

	/// Frames per second of the video stream.
	pub fps: u32,

	/// Size of each encoded packet in bytes.
	pub packet_size: usize,

	/// Target bitrate for the video stream in bits per second.
	pub bitrate: usize,

	/// Minimum number of FEC packets to include for each frame.
	pub minimum_fec_packets: u32,

	/// Whether to apply QoS markings to video stream packets.
	pub qos: bool,

	/// Fully negotiated codec, chroma, bit depth, and color representation.
	pub format: NegotiatedVideoFormat,

	/// Maximum number of reference frames for the video encoder.
	pub max_reference_frames: u32,

	/// Whether the client has enabled video encryption.
	pub encrypt_video: bool,
}

impl VideoStreamContext {
	/// Check every value that timing, allocation, encoder and transport code
	/// consume. Callers validate before pausing or reconfiguring a live epoch.
	pub(crate) fn validate(&self) -> Result<(), String> {
		crate::session::negotiation::validate_display_mode(self.width, self.height, self.fps)?;
		self.format.validate().map_err(str::to_string)?;
		if self.packet_size < packetizer::MIN_VIDEO_PACKET_SIZE {
			return Err(format!(
				"video packet size {} is below the {}-byte protocol minimum",
				self.packet_size,
				packetizer::MIN_VIDEO_PACKET_SIZE
			));
		}
		match packetizer::wire_shard_size(self.packet_size, self.encrypt_video) {
			Some(shard) if shard <= gso_socket::MAX_UDP_PAYLOAD => {},
			_ => {
				return Err(format!(
					"video packet size {} does not fit one UDP datagram{}",
					self.packet_size,
					if self.encrypt_video { " with encryption" } else { "" }
				));
			},
		}
		if self.bitrate == 0 {
			return Err("video bitrate must be non-zero".to_string());
		}
		// Vulkan Video rate control takes a 32-bit bit rate; PyroWave budgets
		// with checked `usize` arithmetic and has no such limit.
		if self.format.codec != VideoCodec::PyroWave && u32::try_from(self.bitrate).is_err() {
			return Err(format!(
				"video bitrate {} bps exceeds the {} encoder's 32-bit rate-control range",
				self.bitrate, self.format.codec
			));
		}
		Ok(())
	}

	/// Names of negotiated properties whose change requires a new stream epoch.
	///
	/// Keep this list next to the context definition so newly-added negotiated
	/// fields cannot silently fall through the reconnect fast path.
	/// The virtual-output properties this stream requires from the compositor.
	pub(crate) fn output_mode(
		&self,
		capture_pacing: crate::session::compositor::CapturePacing,
	) -> crate::session::compositor::OutputMode {
		crate::session::compositor::OutputMode {
			width: self.width,
			height: self.height,
			refresh_rate: self.fps,
			hdr: self.format.hdr,
			capture_pacing,
		}
	}

	pub(crate) fn changed_fields(&self, requested: &Self) -> Vec<&'static str> {
		let mut changed = Vec::new();
		macro_rules! changed {
			($field:ident, $name:literal) => {
				if self.$field != requested.$field {
					changed.push($name);
				}
			};
		}
		changed!(pyrowave_dialect, "PyroWave dialect");
		changed!(width, "width");
		changed!(height, "height");
		changed!(fps, "fps");
		changed!(packet_size, "packet size");
		changed!(bitrate, "bitrate");
		changed!(minimum_fec_packets, "minimum FEC packets");
		changed!(qos, "QoS");
		changed!(max_reference_frames, "max reference frames");
		changed!(encrypt_video, "video encryption");
		if self.format.codec != requested.format.codec {
			changed.push("codec");
		}
		if self.format.chroma != requested.format.chroma {
			changed.push("chroma sampling");
		}
		if self.format.bit_depth != requested.format.bit_depth {
			changed.push("bit depth");
		}
		if self.format.hdr != requested.format.hdr {
			changed.push("dynamic range");
		}
		if self.format.primaries != requested.format.primaries {
			changed.push("color primaries");
		}
		if self.format.transfer != requested.format.transfer {
			changed.push("transfer function");
		}
		if self.format.matrix != requested.format.matrix {
			changed.push("matrix coefficients");
		}
		if self.format.range != requested.format.range {
			changed.push("color range");
		}
		let known_format_change = self.format.codec != requested.format.codec
			|| self.format.chroma != requested.format.chroma
			|| self.format.bit_depth != requested.format.bit_depth
			|| self.format.hdr != requested.format.hdr
			|| self.format.primaries != requested.format.primaries
			|| self.format.transfer != requested.format.transfer
			|| self.format.matrix != requested.format.matrix
			|| self.format.range != requested.format.range;
		if self.format != requested.format && !known_format_change {
			changed.push("other video format property");
		}
		changed
	}
}

/// Handle returned by `VideoStream::start` that gates the pipeline and packet handler.
///
/// The pipeline and packet handler are registered with the session and spawned
/// immediately, then wait on a persistent [`StartLatch`] until `StartB`.
#[derive(Clone)]
pub(crate) struct VideoStreamHandle {
	start: StartLatch,
	idr_tx: broadcast::Sender<()>,
	/// Reference frame invalidation requests, carrying the inclusive
	/// `[first, last]` client frame-index range the client could not decode.
	invalidate_tx: broadcast::Sender<(u32, u32)>,
	reset_tx: std::sync::mpsc::Sender<pipeline::EpochActivation>,
	fec_feedback_tx: watch::Sender<FrameFecStatus>,
	packet_tx: mpsc::Sender<VideoPacketMessage>,
	pause_tx: watch::Sender<u64>,
	reconfigure_tx: std::sync::mpsc::Sender<VideoReconfigureCommand>,
}

pub(super) struct VideoReconfigureCommand {
	pub context: VideoStreamContext,
	/// The authorization generation the reconfigured epoch is activated for.
	pub generation: u64,
	pub applied: tokio::sync::oneshot::Sender<Result<(), ()>>,
}

pub(super) enum VideoPacketMessage {
	Batch(ShardBatch),
	Pause(tokio::sync::oneshot::Sender<()>),
	/// Ordered after every batch of the previous epoch. Delivery resumes only
	/// for `generation`, and only while it is the current authorization.
	BeginEpoch {
		context: VideoStreamContext,
		generation: u64,
		ready: tokio::sync::oneshot::Sender<()>,
	},
}

impl VideoStreamHandle {
	/// Signal the video pipeline and packet handler to begin processing.
	/// Idempotent: a duplicate `StartB` has no further effect.
	pub fn trigger(&self) {
		if !self.start.open() {
			tracing::debug!("Ignoring duplicate video start signal");
		}
	}

	/// Request an IDR (key) frame from the encoder.
	pub fn request_idr_frame(&self) {
		let _ = self.idr_tx.send(());
	}

	/// Request reference frame invalidation for the inclusive client frame-index
	/// range `[first, last]` the client reported it could not decode.
	///
	/// The encoder drops the affected references and recovers by predicting from
	/// a surviving reference where possible, falling back to an IDR only when no
	/// reference survives — much cheaper than always re-sending a keyframe.
	pub fn invalidate_reference_frames(&self, first: u32, last: u32) {
		let _ = self.invalidate_tx.send((first, last));
	}

	/// Reset the stream's frame/sequence counters for a resuming client.
	///
	/// Called when a client reconnects to an already-running session. The pipeline
	/// keeps incrementing `frame_number` for the lifetime of the session, but a fresh
	/// Moonlight session expects frame numbers to start at 1; without a reset it counts
	/// the jump as massive frame loss and reports a poor connection. This also forces an
	/// IDR so the resumed client has a decodable starting frame.
	pub async fn request_reset(&self, generation: u64) -> Result<(), ()> {
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reset_tx
			.send(pipeline::EpochActivation { generation, applied })
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())?
	}

	/// Stop delivering packets until the encoder activates the next client epoch.
	pub async fn pause_for_reconfigure(&self) -> Result<(), ()> {
		// Interrupt socket waits before ordering the pause barrier behind old work.
		self.pause_tx
			.send_modify(|generation| *generation = generation.wrapping_add(1));
		let (ready, waiting) = tokio::sync::oneshot::channel();
		self.packet_tx
			.send(VideoPacketMessage::Pause(ready))
			.await
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())
	}

	/// Replace the encoder/packetizer epoch and wait until it is ready.
	pub async fn reconfigure(&self, context: VideoStreamContext, generation: u64) -> Result<(), ()> {
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reconfigure_tx
			.send(VideoReconfigureCommand {
				context,
				generation,
				applied,
			})
			.map_err(|_| ())?;
		waiting.await.map_err(|_| ())?
	}

	pub(crate) fn report_fec_status(&self, mut status: FrameFecStatus) {
		tracing::trace!(
			frame_index = status.frame_index,
			highest_sequence = status.highest_received_sequence_number,
			next_contiguous_sequence = status.next_contiguous_sequence_number,
			missing_before_highest = status.missing_packets_before_highest,
			data_packets = status.total_data_packets,
			parity_packets = status.total_parity_packets,
			received_data_packets = status.received_data_packets,
			received_parity_packets = status.received_parity_packets,
			fec_percentage = status.fec_percentage,
			block_index = status.block_index,
			block_count = status.block_count,
			"Received Moonlight frame FEC status"
		);
		let serial = self.fec_feedback_tx.borrow().serial.wrapping_add(1).max(1);
		status.serial = serial;
		self.fec_feedback_tx.send_replace(status);
	}

	/// The stream's start latch, for external triggering (e.g. bench binary).
	pub(crate) fn start_latch(&self) -> StartLatch {
		self.start.clone()
	}
}

/// Receivers behind a [`VideoStreamHandle::for_test`] handle.
#[cfg(test)]
pub(super) struct VideoHandleProbe {
	pub(super) idr_rx: broadcast::Receiver<()>,
	pub(super) packet_rx: mpsc::Receiver<VideoPacketMessage>,
}

#[cfg(test)]
impl VideoStreamHandle {
	/// A handle not connected to a pipeline, for control-stream tests.
	pub(super) fn for_test() -> (Self, VideoHandleProbe) {
		let (idr_tx, idr_rx) = broadcast::channel(16);
		let (packet_tx, packet_rx) = mpsc::channel(16);
		let handle = Self {
			pause_tx: watch::channel(0u64).0,
			start: StartLatch::new(),
			idr_tx,
			invalidate_tx: broadcast::channel(16).0,
			reset_tx: std::sync::mpsc::channel().0,
			fec_feedback_tx: watch::channel(FrameFecStatus::default()).0,
			packet_tx,
			reconfigure_tx: std::sync::mpsc::channel().0,
		};
		(handle, VideoHandleProbe { idr_rx, packet_rx })
	}
}

pub(crate) struct VideoStream {
	socket: UdpGsoSocket,
	frame_rx: crate::session::compositor::admission::CaptureReceiver,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
	stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
}

impl VideoStream {
	pub async fn new(
		config: VideoStreamConfig,
		address: String,
		frame_rx: crate::session::compositor::admission::CaptureReceiver,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		_stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: tokio::sync::broadcast::Sender<FrameStats>,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpGsoSocket::new(&address, config.port).await?;

		tracing::debug!(
			"Listening for video messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::warn!("Failed to get local address associated with video socket: {e}"))?
		);

		Ok(Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		})
	}

	#[allow(clippy::too_many_arguments)]
	pub fn start(
		self,
		config: VideoStreamConfig,
		context: VideoStreamContext,
		keys_rx: SessionKeysReceiver,
		generation: u64,
		authorization_rx: AuthorizationReceiver,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<VideoStreamHandle, ()> {
		let Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		} = self;

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = socket.set_tos_v4(160);
		}

		// Persistent gate for pipeline + packet handler.
		let start = StartLatch::new();

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, reset_rx) = std::sync::mpsc::channel();
		let (fec_feedback_tx, fec_feedback_rx) = watch::channel(FrameFecStatus::default());

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<VideoPacketMessage>(128);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		if config.log_stats {
			diagnostics::spawn_watchdog(stop.clone(), packet_tx.downgrade());
		}
		let (reconfigure_tx, reconfigure_rx) = std::sync::mpsc::channel();
		let pacing_bitrate =
			(context.format.codec == VideoCodec::PyroWave).then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));

		// Spawn packet handler — registered now, gated behind the start latch.
		let demand = MediaDemand::new();
		let worker = WorkerGuard::register(&stop, SessionShutdownReason::VideoPacketHandlerStopped)?;
		spawn_handle_video_packets(
			packet_rx,
			pause_rx,
			socket,
			authorization_rx,
			generation,
			demand.clone(),
			start.waiter(),
			stop.clone(),
			worker,
			pacing_bitrate,
			context.fps,
			config.log_stats,
		);

		// Spawn pipeline thread — registered now, gated behind the start latch.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx.clone(),
			idr_tx.clone(),
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
			reset_rx,
			stop.clone(),
			hdr_metadata_tx,
			start.waiter(),
			stats_tx,
			fec_feedback_rx,
			reconfigure_rx,
			demand,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			start,
			idr_tx,
			invalidate_tx,
			reset_tx,
			fec_feedback_tx,
			packet_tx,
			pause_tx,
			reconfigure_tx,
		})
	}
}

/// `worker` was registered by the caller before spawning, so a stop before
/// `StartB` still waits for this task to drop its socket.
///
/// Delivery is bound to one authorization generation: the epoch the last
/// `BeginEpoch` (or the initial start) activated. A batch is sent only after
/// `StartB`, to an endpoint that generation discovered, while it is still the
/// current authorization. A `/resume` therefore ends delivery to the replaced
/// client at once, including a paced send in progress; datagrams already
/// handed to the kernel cannot be recalled. Pause/epoch commands and endpoint
/// discovery are served before `StartB` so a reconnect never waits on it.
#[allow(clippy::too_many_arguments)]
fn spawn_handle_video_packets(
	packet_rx: mpsc::Receiver<VideoPacketMessage>,
	mut pause_rx: watch::Receiver<u64>,
	socket: UdpGsoSocket,
	mut authorization: AuthorizationReceiver,
	initial_generation: u64,
	demand: MediaDemand,
	start: StartWaiter,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	worker: WorkerGuard,
	mut pacing_bitrate: Option<u64>,
	mut fps: u32,
	log_stats: bool,
) {
	tokio::spawn(async move {
		// Declared first so it is released after the socket and channel below.
		let _worker = worker;
		let mut socket = socket;
		let mut packet_rx = packet_rx;
		if stop_session_manager.is_shutdown_triggered() {
			tracing::debug!("Video packet handler stopped before start signal.");
			return;
		}
		let start = start.wait(&stop_session_manager);
		tokio::pin!(start);
		let mut started = false;

		let mut buf = [0; 1024];
		// The discovered endpoint and the generation that discovered it.
		let mut client_address: Option<(u64, std::net::SocketAddr)> = None;
		let mut paused = false;
		let mut active_generation = initial_generation;
		// Rate-limits the GSO-fallback warning.
		let mut last_send_warn: Option<std::time::Instant> = None;
		let mut transport_window = diagnostics::TransportWindow::new(log_stats);

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				result = &mut start, if !started => {
					if result.is_err() {
						tracing::debug!("Video packet handler stopped before start signal.");
						break;
					}
					started = true;
				},
				Ok(()) = pause_rx.changed() => {
					if !paused { client_address = None; }
					paused = true;
					demand.set(false);
				},
				message = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
					match message {
						Ok(Some(VideoPacketMessage::Pause(ready))) => {
							// The FIFO barrier can win before changed(). Consume
							// its urgent notification before acknowledging; otherwise
							// that old notification could pause the next BeginEpoch.
							pause_rx.borrow_and_update();
							if !paused {
								client_address = None;
							}
							// PING may discover the next endpoint, but only an ordered
							// BeginEpoch from the producer can enable delivery again.
							paused = true;
							demand.set(false);
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::BeginEpoch { context, generation, ready })) => {
							pacing_bitrate = (context.format.codec == VideoCodec::PyroWave)
								.then(|| u64::try_from(context.bitrate).unwrap_or(u64::MAX));
							fps = context.fps;
							active_generation = generation;
							paused = false;
							demand.set(true);
							let tos = if context.qos { 160 } else { 0 };
							let _ = socket.set_tos_v4(tos);
							let _ = ready.send(());
						},
						Ok(Some(VideoPacketMessage::Batch(mut batch))) => {
							let current = authorization.borrow().generation();
							if let Some(addr) = deliverable(client_address, started && !paused, active_generation, current) {
								if batch.shard_count() == 0 {
									continue;
								}

								batch.mark_send_started();
								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown. A pause
								// or a replaced authorization stops the remaining shards.
								let interrupted = {
									let send = stop_session_manager.wrap_cancel(socket.send_batch(&mut batch, addr, pacing_bitrate));
									tokio::pin!(send);
									loop {
										tokio::select! {
											biased;
											Ok(()) = pause_rx.changed() => {
												paused = true;
												demand.set(false);
												client_address = None;
												break None;
											},
											Ok(()) = authorization.changed() => {
												if authorization.borrow_and_update().generation() != active_generation {
													break None;
												}
											},
											result = &mut send => break Some(result),
										}
									}
								};
								let Some(result) = interrupted else {
									let completion = batch.finish(shard_batch::CompletionDisposition::Discarded);
									transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len());
									continue;
								};
								match result
								{
									Ok(send_stats) => {
										transport_window.record(&send_stats, packet_rx.len());
										if (send_stats.fallback_chunks > 0 || send_stats.outcome.failed_datagrams > 0)
											&& last_send_warn
												.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
										{
											tracing::warn!(
												last_error = ?send_stats.outcome.last_error,
												"Video transport: {} fallback chunks, {} failed datagrams",
												send_stats.fallback_chunks, send_stats.outcome.failed_datagrams
											);
											last_send_warn = Some(std::time::Instant::now());
										}
										if tracing::enabled!(tracing::Level::TRACE) {
											let effective_wire_bitrate = if send_stats.elapsed.is_zero() {
												0
											} else {
												(u128::from(send_stats.wire_bytes)
													.saturating_mul(8)
													.saturating_mul(1_000_000)
													/ send_stats.elapsed.as_micros().max(1))
													.min(u128::from(u64::MAX)) as u64
											};
											tracing::trace!(
												frame_number = batch.frame_number(),
												fps,
												shard_size = batch.shard_size(),
												encoded_bytes = batch.encoded_size(),
												wire_bytes = send_stats.wire_bytes,
												data_shards = batch.data_shards(),
												parity_shards = batch.parity_shards(),
												total_shards = batch.shard_count(),
												fec_blocks = batch.fec_blocks(),
												gso_segments_per_send = send_stats.gso_segments_per_send,
												gso_sends = send_stats.gso_sends,
												final_chunk_segments = send_stats.final_chunk_segments,
												final_chunk_bytes = send_stats.final_chunk_bytes,
												per_shard_sends = send_stats.per_shard_sends,
												would_block_events = send_stats.would_block_events,
												gso_fallback_chunks = send_stats.fallback_chunks,
												send_us = duration_micros_u64(send_stats.elapsed),
												pacing_window_us = duration_micros_u64(send_stats.pacing_duration),
												scheduled_pacing_us = duration_micros_u64(send_stats.scheduled_pacing_duration),
												initial_pacing_lateness_us = duration_micros_u64(send_stats.initial_pacing_lateness),
												max_pacing_lateness_us = duration_micros_u64(send_stats.max_pacing_lateness),
												pacing_rebased = send_stats.pacing_rebased,
												backpressure_rebases = send_stats.backpressure_rebases,
												effective_wire_bitrate,
												"Video frame transport"
											);
										}
									},
									Err(_) => {
										let completion = batch.finish(shard_batch::CompletionDisposition::Cancelled);
										transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len());
										break;
									},
								}
							}
							let current = authorization.borrow().generation();
							if deliverable(client_address, started && !paused, active_generation, current).is_some() {
								batch.notify_sent();
							} else {
								let completion = batch.finish(shard_batch::CompletionDisposition::Discarded);
								transport_window.record(&gso_socket::SendStats::released(completion), packet_rx.len());
							}
						},
						Ok(None) => {
							tracing::debug!("Video packet channel closed.");
							break;
						},
						Err(_) => break,
					}
				},

				message = stop_session_manager.wrap_cancel(socket.recv_from(&mut buf)) => {
					let (len, address) = match message {
						Ok(Ok((len, address))) => (len, address),
						Ok(Err(e)) => {
							tracing::warn!("Failed to receive message: {e}");
							break;
						},
						Err(_) => break,
					};

					// Only the current launch/resume generation may (re)discover the
					// destination; the source port may change (NAT, client sockets).
					if authorization.borrow().admits_media_ping(MediaStream::Video, address, &buf[..len]) {
						tracing::trace!("Received video stream PING message from {address}.");
						client_address = Some((authorization.borrow().generation(), address));
					} else {
						tracing::debug!(%address, len, "Ignoring unauthorized video endpoint discovery datagram");
					}
				},
			}
		}

		tracing::debug!("Video packet stream stopped.");
	});
}

/// The endpoint a batch may be sent to: only once started and not paused, only
/// to the endpoint the active epoch's generation discovered, and only while
/// that generation is the current authorization.
fn deliverable(
	client_address: Option<(u64, std::net::SocketAddr)>,
	enabled: bool,
	active_generation: u64,
	current_generation: u64,
) -> Option<std::net::SocketAddr> {
	client_address
		.filter(|(discovered, _)| {
			enabled && *discovered == active_generation && current_generation == active_generation
		})
		.map(|(_, address)| address)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::compositor::CapturePacing;
	use crate::session::stream::test_support::SocketId;

	#[tokio::test]
	async fn pause_and_stop_interrupt_blocked_network_work_and_release_all_credits() {
		use std::sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		};
		use std::time::Duration;
		for pause in [false, true] {
			let mut socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
			socket.force_no_gso_for_test();
			socket.faults.stall_after = Some(1);
			let server = socket.local_addr().unwrap();
			let stop = ShutdownManager::new();
			let (mut handle, _probe) = VideoStreamHandle::for_test();
			let (tx, rx) = mpsc::channel(16);
			handle.packet_tx = tx.clone();
			let (pause_tx, pause_rx) = watch::channel(0u64);
			handle.pause_tx = pause_tx;
			let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
			spawn_handle_video_packets(
				rx,
				pause_rx,
				socket,
				authorization_rx,
				1,
				MediaDemand::new(),
				handle.start.waiter(),
				stop.clone(),
				worker(&stop),
				None,
				120,
				false,
			);
			handle.trigger();
			let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
			client.send_to(b"PING", server).await.unwrap();
			tokio::time::sleep(Duration::from_millis(10)).await;
			let credits = Arc::new(AtomicUsize::new(3));
			let mut completions = Vec::new();
			for _ in 0..3 {
				let mut batch = shard_batch::ShardBuf::new(3, 64, 0).into_batch();
				batch.hold_until_release(shard_batch::NetworkCredit(credits.clone()));
				let (sent, completed) = std::sync::mpsc::sync_channel(1);
				batch.set_send_completion(sent);
				completions.push(completed);
				tx.send(VideoPacketMessage::Batch(batch)).await.unwrap();
			}
			// The first datagram was submitted; the second is permanently blocked.
			tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut [0; 64]))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(credits.load(Ordering::Relaxed), 3);
			if pause {
				tokio::time::timeout(Duration::from_secs(1), handle.pause_for_reconfigure())
					.await
					.unwrap()
					.unwrap();
				let (ready, waiting) = tokio::sync::oneshot::channel();
				tx.send(VideoPacketMessage::BeginEpoch {
					generation: 1,
					context: VideoStreamContext {
						fps: 120,
						bitrate: 750_000_000,
						..Default::default()
					},
					ready,
				})
				.await
				.unwrap();
				waiting.await.unwrap();
			} else {
				let _ = stop.trigger_shutdown(SessionShutdownReason::VideoPacketHandlerStopped);
			}
			if pause {
				let _ = stop.trigger_shutdown(SessionShutdownReason::VideoPacketHandlerStopped);
			}
			tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
				.await
				.unwrap();
			assert_eq!(credits.load(Ordering::Relaxed), 0);
			let completion = completions[0].try_recv().unwrap();
			assert_eq!(completion.outcome.submitted_datagrams, 1);
			assert_eq!(
				completion.disposition,
				if pause {
					shard_batch::CompletionDisposition::Discarded
				} else {
					shard_batch::CompletionDisposition::Cancelled
				}
			);
			for c in &completions[1..] {
				assert_eq!(c.try_recv().unwrap().outcome.submitted_datagrams, 0);
			}
			assert!(
				tokio::time::timeout(Duration::from_millis(20), client.recv_from(&mut [0; 64]))
					.await
					.is_err()
			);
		}
	}

	#[tokio::test(flavor = "current_thread")]
	async fn queued_pause_notification_cannot_pause_the_following_epoch() {
		use std::time::Duration;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		start.open();
		for _ in 0..32 {
			// Queue both ordered barriers and the urgent notification before
			// yielding. Either notification or FIFO reception may win the select.
			let (paused, pause_ack) = tokio::sync::oneshot::channel();
			let (ready, epoch_ack) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(paused)).await.unwrap();
			tx.send(VideoPacketMessage::BeginEpoch {
				generation: 1,
				context: VideoStreamContext::default(),
				ready,
			})
			.await
			.unwrap();
			pause_ack.await.unwrap();
			epoch_ack.await.unwrap();
			// Any unread urgent notification is now ready to run. It must not
			// disable the newly acknowledged epoch.
			tokio::time::sleep(Duration::from_millis(1)).await;
			client.send_to(b"PING", server).await.unwrap();
			tokio::time::sleep(Duration::from_millis(2)).await;
			let mut fresh = shard_batch::ShardBuf::new(1, 64, 0);
			fresh.shard_mut(0).fill(0x11);
			tx.send(VideoPacketMessage::Batch(fresh.into_batch())).await.unwrap();
			let mut buf = [0; 64];
			let (len, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(&buf[..len], &[0x11; 64]);
		}
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}

	#[tokio::test]
	async fn reconnect_ping_cannot_deliver_old_batches_before_epoch_activation() {
		use std::time::Duration;
		use tokio::net::UdpSocket;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			Some(650_000_000),
			120,
			false,
		);
		start.open();
		let mut buf = [0u8; 64];
		for codec in [VideoCodec::PyroWave, VideoCodec::Hevc, VideoCodec::PyroWave] {
			let (ready, waiting) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(ready)).await.unwrap();
			waiting.await.unwrap();
			let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
			client.send_to(b"PING", server).await.unwrap();
			// Let the endpoint discovery arrive while negotiation is paused.
			tokio::time::sleep(Duration::from_millis(10)).await;
			let (ready, waiting) = tokio::sync::oneshot::channel();
			pause_tx.send_modify(|g| *g += 1);
			tx.send(VideoPacketMessage::Pause(ready)).await.unwrap();
			waiting.await.unwrap(); // Duplicate disconnect/ANNOUNCE pauses retain the new PING.
			let mut old = shard_batch::ShardBuf::new(1, 64, 0);
			old.shard_mut(0).fill(0xee);
			let mut old = old.into_batch();
			let (sent, completed) = std::sync::mpsc::sync_channel(1);
			old.set_send_completion(sent);
			tx.send(VideoPacketMessage::Batch(old)).await.unwrap();
			let (ready, waiting) = tokio::sync::oneshot::channel();
			tx.send(VideoPacketMessage::BeginEpoch {
				generation: 1,
				context: VideoStreamContext {
					fps: 120,
					bitrate: 650_000_000,
					format: NegotiatedVideoFormat {
						codec,
						..Default::default()
					},
					..Default::default()
				},
				ready,
			})
			.await
			.unwrap();
			waiting.await.unwrap();
			// Dropped old PyroWave batches still release their synchronous producer.
			assert!(completed.try_recv().is_ok());
			let mut fresh = shard_batch::ShardBuf::new(1, 64, 0);
			fresh.shard_mut(0).fill(0x11);
			tx.send(VideoPacketMessage::Batch(fresh.into_batch())).await.unwrap();
			let (len, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
				.await
				.unwrap()
				.unwrap();
			assert_eq!(&buf[..len], &[0x11; 64]);
			assert!(
				tokio::time::timeout(Duration::from_millis(10), client.recv_from(&mut buf))
					.await
					.is_err()
			);
		}
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	}

	fn worker(stop: &ShutdownManager<SessionShutdownReason>) -> WorkerGuard {
		WorkerGuard::register(stop, SessionShutdownReason::VideoPacketHandlerStopped).unwrap()
	}

	/// Spawn a packet handler on an ephemeral port and return its address
	/// and the identity of the socket it owns.
	async fn spawn_unstarted(
		stop: &ShutdownManager<SessionShutdownReason>,
		start: &StartLatch,
	) -> (std::net::SocketAddr, SocketId, mpsc::Sender<VideoPacketMessage>) {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let address = socket.local_addr().unwrap();
		let id = SocketId::of(&socket);
		let (tx, rx) = mpsc::channel(16);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(stop),
			None,
			60,
			false,
		);
		(address, id, tx)
	}

	/// STAB-001: a stop before `StartB` completes only after the packet
	/// handler released its socket, so the next session can bind the port.
	#[tokio::test]
	async fn stop_before_start_releases_the_socket_before_completion() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (_address, socket, _tx) = spawn_unstarted(&stop, &start).await;
		tokio::task::yield_now().await;
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(
			!socket.is_open(),
			"completed shutdown must imply the video port is free"
		);
		// A late StartB cannot resurrect the stopped handler.
		start.open();
	}

	/// STAB-001: the stop can also arrive before the spawned task is first polled.
	#[tokio::test(flavor = "current_thread")]
	async fn stop_before_first_poll_is_still_joined() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (_address, socket, _tx) = spawn_unstarted(&stop, &start).await;
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(!socket.is_open());
	}

	/// STAB-001: `StartB` before the handler polls its gate, and duplicate
	/// `StartB`, both leave exactly one running handler.
	#[tokio::test(flavor = "current_thread")]
	async fn early_and_duplicate_start_signals_start_the_handler() {
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (address, socket, tx) = spawn_unstarted(&stop, &start).await;
		assert!(start.open());
		assert!(!start.open());
		let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		client.send_to(b"PING", address).await.unwrap();
		let mut delivered = false;
		for _ in 0..50 {
			send_frame(&tx, 0x5a).await;
			if receives(&client, 0x5a).await {
				delivered = true;
				break;
			}
		}
		assert!(delivered, "an early StartB must not be lost");
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
		assert!(!socket.is_open());
	}

	/// A port still owned by another session makes stream construction fail
	/// cleanly (the manager then tears the partial session down); it never
	/// starts a stream on a different port.
	#[tokio::test]
	async fn busy_udp_port_fails_stream_construction() {
		let occupied = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let port = occupied.local_addr().unwrap().port();
		let (_capture_tx, capture_rx) = crate::session::compositor::admission::capture_channel();
		let config = VideoStreamConfig {
			port,
			..Default::default()
		};
		assert!(
			VideoStream::new(
				config,
				"127.0.0.1".into(),
				capture_rx,
				watch::channel(HdrModeState::new(false)).0,
				ShutdownManager::new(),
				broadcast::channel(1).0,
			)
			.await
			.is_err()
		);
		let audio = crate::session::stream::audio::AudioStreamConfig { port };
		assert!(
			crate::session::stream::audio::AudioStream::new(audio, "127.0.0.1".into(), ShutdownManager::new())
				.await
				.is_err()
		);
	}

	/// TEST-001 fault injection on the real owners: `VideoStream::start` spawns
	/// the registered UDP packet handler and then fails to construct the
	/// pipeline (no verified capture device). Over 100 sessions on one fixed
	/// port, the failed start must still be joined by the session's completion
	/// and must release the port for the next session.
	#[tokio::test]
	async fn failed_pipeline_construction_releases_the_started_packet_handler() {
		use crate::session::stream::test_support::{construct_on_fixed_port, fixed_udp_port, udp_port_open_here};
		let port = fixed_udp_port();
		let config = VideoStreamConfig {
			port,
			..Default::default()
		};
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		for cycle in 0..100 {
			let stop = ShutdownManager::new();
			// Each attempt gets its own capture channel; keep the senders open.
			let mut capture_senders = Vec::new();
			let stream = construct_on_fixed_port(port, async || {
				let (capture_tx, capture_rx) = crate::session::compositor::admission::capture_channel();
				capture_senders.push(capture_tx);
				VideoStream::new(
					config.clone(),
					"127.0.0.1".into(),
					capture_rx,
					watch::channel(HdrModeState::new(false)).0,
					stop.clone(),
					broadcast::channel(1).0,
				)
				.await
			})
			.await
			.unwrap_or_else(|e| panic!("cycle {cycle}: {e}"));
			let keys = watch::channel(crate::session::keys::KeyLedger::default().publish(
				crate::session::SessionKeyData::new(
					crate::session::RemoteInputKey::from_bytes([1; 16]),
					crate::session::RemoteInputKeyId::new(1),
				),
			));
			let started = stream.start(
				config.clone(),
				VideoStreamContext {
					width: 1920,
					height: 1080,
					fps: 60,
					packet_size: 1392,
					bitrate: 20_000_000,
					..Default::default()
				},
				keys.1,
				1,
				authorization_rx.clone(),
				stop.clone(),
			);
			assert!(started.is_err(), "cycle {cycle}: no capture device was verified");
			// The manager's failed-transition path stops the session.
			let _ = stop.trigger_shutdown(SessionShutdownReason::TransitionFailed);
			tokio::time::timeout(std::time::Duration::from_secs(5), stop.wait_shutdown_complete())
				.await
				.unwrap_or_else(|_| panic!("cycle {cycle}: started packet handler was not joined"));
			assert!(
				!udp_port_open_here(port),
				"cycle {cycle}: completed stop must release the port"
			);
		}
	}

	fn test_authorization(
		client: &str,
	) -> (
		watch::Sender<crate::session::authorization::StreamAuthorization>,
		AuthorizationReceiver,
	) {
		watch::channel(crate::session::authorization::StreamAuthorization::new(1, client.parse().unwrap()).unwrap())
	}

	fn session_ping(authorization: &crate::session::authorization::StreamAuthorization, counter: u32) -> Vec<u8> {
		let mut ping = authorization.ping_payload(MediaStream::Video).as_bytes().to_vec();
		ping.extend(counter.to_be_bytes());
		ping
	}

	async fn send_frame(tx: &mpsc::Sender<VideoPacketMessage>, byte: u8) {
		let mut shard = shard_batch::ShardBuf::new(1, 64, 0);
		shard.shard_mut(0).fill(byte);
		tx.send(VideoPacketMessage::Batch(shard.into_batch())).await.unwrap();
	}

	async fn receives(socket: &tokio::net::UdpSocket, byte: u8) -> bool {
		let mut buf = [0u8; 128];
		match tokio::time::timeout(std::time::Duration::from_millis(200), socket.recv_from(&mut buf)).await {
			Ok(Ok((len, _))) => buf[..len] == [byte; 64],
			_ => false,
		}
	}

	/// Unauthorized hosts, forged/stale payloads and legacy PINGs from session-ID
	/// clients cannot redirect video; the authorized client may change ports.
	#[tokio::test]
	async fn endpoint_discovery_is_bound_to_the_authorized_generation() {
		use tokio::net::UdpSocket;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (authorization_tx, authorization_rx) = test_authorization("127.0.0.1");
		authorization_tx.send_modify(crate::session::authorization::StreamAuthorization::require_session_id);
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		start.open();
		let current = authorization_tx.borrow().clone();

		// Another host knowing the payload, and the client with a wrong payload or
		// a legacy PING after announcing session-ID support.
		let attacker = UdpSocket::bind("127.0.0.2:0").await.unwrap();
		attacker.send_to(&session_ping(&current, 1), server).await.unwrap();
		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut forged = session_ping(&current, 1);
		forged[3] ^= 0x20;
		client.send_to(&forged, server).await.unwrap();
		client.send_to(b"PING", server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x11).await;
		assert!(!receives(&attacker, 0x11).await);
		assert!(!receives(&client, 0x11).await);

		// The authorized client discovers the endpoint, then moves to a new port.
		client.send_to(&session_ping(&current, 2), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x22).await;
		assert!(receives(&client, 0x22).await);
		let rebound = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		rebound.send_to(&session_ping(&current, 3), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x33).await;
		assert!(receives(&rebound, 0x33).await);
		assert!(!receives(&client, 0x33).await);
		// An attacker cannot steal it back.
		attacker.send_to(&session_ping(&current, 4), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x44).await;
		assert!(receives(&rebound, 0x44).await);
		assert!(!receives(&attacker, 0x44).await);

		// After a resume, the previous generation's payload is stale, and
		// nothing reaches either endpoint until the new generation discovers its
		// endpoint and its epoch is activated (see
		// `authorization_replacement_stops_old_endpoint_delivery`).
		let next = crate::session::authorization::StreamAuthorization::new(2, "127.0.0.1".parse().unwrap()).unwrap();
		authorization_tx.send_replace(next.clone());
		client.send_to(&session_ping(&current, 5), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x55).await;
		assert!(!receives(&client, 0x55).await);
		assert!(!receives(&rebound, 0x55).await);
		client.send_to(&session_ping(&next, 1), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		let (ready, activated) = tokio::sync::oneshot::channel();
		tx.send(VideoPacketMessage::BeginEpoch {
			context: VideoStreamContext::default(),
			generation: next.generation(),
			ready,
		})
		.await
		.unwrap();
		activated.await.unwrap();
		send_frame(&tx, 0x66).await;
		assert!(receives(&client, 0x66).await);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	}

	/// Review 2026-10-05 SEC-001: admitting a discovery PING is not authority
	/// to keep transmitting. Once the grant is replaced (an HTTP `/resume`,
	/// before any ANNOUNCE pause), batches admitted afterwards must not reach
	/// the old generation's endpoint, and a PING of the new generation alone
	/// must not resume delivery before an ordered epoch activation.
	#[tokio::test]
	async fn authorization_replacement_stops_old_endpoint_delivery() {
		use tokio::net::UdpSocket;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (authorization_tx, authorization_rx) = test_authorization("127.0.0.1");
		authorization_tx.send_modify(crate::session::authorization::StreamAuthorization::require_session_id);
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		start.open();
		let old = authorization_tx.borrow().clone();
		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		client.send_to(&session_ping(&old, 1), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x11).await;
		assert!(
			receives(&client, 0x11).await,
			"fixture: the current generation receives video"
		);

		let mut next =
			crate::session::authorization::StreamAuthorization::new(2, "127.0.0.1".parse().unwrap()).unwrap();
		next.require_session_id();
		authorization_tx.send_replace(next.clone());
		send_frame(&tx, 0x22).await;
		assert!(
			!receives(&client, 0x22).await,
			"review 2026-10-05 SEC-001: the replaced generation's endpoint received a batch admitted after the grant changed"
		);

		// The new client (same host, new port) discovers the endpoint; that
		// alone must not activate media for the new generation.
		let resumed = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		resumed.send_to(&session_ping(&next, 1), server).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		send_frame(&tx, 0x33).await;
		assert!(
			!receives(&resumed, 0x33).await,
			"review 2026-10-05 SEC-001: a new-generation PING resumed delivery before epoch activation"
		);
		assert!(!receives(&client, 0x33).await);

		let (ready, activated) = tokio::sync::oneshot::channel();
		tx.send(VideoPacketMessage::BeginEpoch {
			generation: next.generation(),
			context: VideoStreamContext::default(),
			ready,
		})
		.await
		.unwrap();
		activated.await.unwrap();
		send_frame(&tx, 0x44).await;
		assert!(
			receives(&resumed, 0x44).await,
			"the activated current generation receives video"
		);
		assert!(!receives(&client, 0x44).await);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}

	/// Review 2026-10-05 SEC-001 under load: a resume that replaces the grant
	/// while a paced send is blocked stops it, discards the batch and releases
	/// its network credit; queued batches of the old epoch are discarded too.
	/// A client on another address then needs its own discovery and epoch.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn authorization_replacement_interrupts_a_blocked_send() {
		use std::sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		};
		use std::time::Duration;
		use tokio::net::UdpSocket;
		let mut socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		socket.force_no_gso_for_test();
		socket.faults.stall_after = Some(1);
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (authorization_tx, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			watch::channel(0u64).1,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			120,
			false,
		);
		start.open();
		let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		client.send_to(b"PING", server).await.unwrap();
		tokio::time::sleep(Duration::from_millis(20)).await;
		let credits = Arc::new(AtomicUsize::new(3));
		let mut completions = Vec::new();
		for _ in 0..3 {
			let mut batch = shard_batch::ShardBuf::new(3, 64, 0).into_batch();
			batch.hold_until_release(shard_batch::NetworkCredit(credits.clone()));
			let (sent, completed) = std::sync::mpsc::sync_channel(1);
			batch.set_send_completion(sent);
			completions.push(completed);
			tx.send(VideoPacketMessage::Batch(batch)).await.unwrap();
		}
		// The first datagram was submitted; the rest of the batch is blocked.
		tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut [0; 64]))
			.await
			.unwrap()
			.unwrap();
		// A client on another address resumes.
		let resumed_authorization =
			crate::session::authorization::StreamAuthorization::new(2, "127.0.0.2".parse().unwrap()).unwrap();
		authorization_tx.send_replace(resumed_authorization);
		for completion in &completions {
			let completion = completion.recv_timeout(Duration::from_secs(1)).unwrap();
			assert_eq!(completion.disposition, shard_batch::CompletionDisposition::Discarded);
		}
		assert_eq!(credits.load(Ordering::Relaxed), 0, "every old-epoch credit is released");
		assert!(
			tokio::time::timeout(Duration::from_millis(50), client.recv_from(&mut [0; 64]))
				.await
				.is_err()
		);
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
	}

	/// Review 2026-10-05 PERF-001: every pause (urgent notification or the
	/// ordered barrier) clears media demand, and only an epoch activation
	/// restores it, so producers stop while no client can receive media.
	#[tokio::test]
	async fn pauses_clear_media_demand_until_an_epoch_activates() {
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let stop = ShutdownManager::new();
		let start = StartLatch::new();
		let (tx, rx) = mpsc::channel(16);
		let (pause_tx, pause_rx) = watch::channel(0u64);
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		let demand = MediaDemand::new();
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			1,
			demand.clone(),
			start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		start.open();
		assert!(demand.wanted());
		let activate = || async {
			let (ready, activated) = tokio::sync::oneshot::channel();
			tx.send(VideoPacketMessage::BeginEpoch {
				context: VideoStreamContext::default(),
				generation: 1,
				ready,
			})
			.await
			.unwrap();
			activated.await.unwrap();
		};
		// Ordered barrier.
		let (paused, acknowledged) = tokio::sync::oneshot::channel();
		tx.send(VideoPacketMessage::Pause(paused)).await.unwrap();
		acknowledged.await.unwrap();
		assert!(!demand.wanted());
		activate().await;
		assert!(demand.wanted());
		// Urgent notification alone.
		pause_tx.send_modify(|g| *g += 1);
		tokio::time::timeout(std::time::Duration::from_secs(1), async {
			while demand.wanted() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		activate().await;
		assert!(demand.wanted());
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		stop.wait_shutdown_complete().await;
	}

	/// Review 2026-10-05 STAB-002: the one-shot media latch gates media, not
	/// lifecycle commands. A client that completes PLAY and disappears before
	/// `StartB` leaves workers that a reconnect ANNOUNCE must still be able to
	/// pause; acknowledging must not open delivery or the latch.
	#[tokio::test]
	async fn pause_is_acknowledged_before_start() {
		use std::time::Duration;
		let socket = UdpGsoSocket::new("127.0.0.1", 0).await.unwrap();
		let server = socket.local_addr().unwrap();
		let stop = ShutdownManager::new();
		let (mut handle, _probe) = VideoStreamHandle::for_test();
		let (tx, rx) = mpsc::channel(16);
		handle.packet_tx = tx.clone();
		let (pause_tx, pause_rx) = watch::channel(0u64);
		handle.pause_tx = pause_tx;
		let (_authorization, authorization_rx) = test_authorization("127.0.0.1");
		spawn_handle_video_packets(
			rx,
			pause_rx,
			socket,
			authorization_rx,
			1,
			MediaDemand::new(),
			handle.start.waiter(),
			stop.clone(),
			worker(&stop),
			None,
			60,
			false,
		);
		let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		client.send_to(b"PING", server).await.unwrap();
		let paused = tokio::time::timeout(Duration::from_secs(5), handle.pause_for_reconfigure()).await;
		assert!(
			matches!(paused, Ok(Ok(()))),
			"review 2026-10-05 STAB-002: video pause was not acknowledged before StartB ({paused:?})"
		);
		assert!(!handle.start.is_open(), "pausing must not open the media latch");
		send_frame(&tx, 0x5a).await;
		assert!(!receives(&client, 0x5a).await, "no media before StartB");
		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(Duration::from_secs(1), stop.wait_shutdown_complete())
			.await
			.unwrap();
	}

	#[test]
	fn dialect_changes_require_a_new_stream_epoch() {
		use pyrowave_protocol::PyroWaveDialect;
		let active = VideoStreamContext {
			pyrowave_dialect: Some(PyroWaveDialect::NativeWireV1),
			..Default::default()
		};
		assert!(active.changed_fields(&active).is_empty());
		let records = VideoStreamContext {
			pyrowave_dialect: Some(PyroWaveDialect::RecordFramed),
			..active.clone()
		};
		assert_eq!(active.changed_fields(&records), vec!["PyroWave dialect"]);
		assert_eq!(records.changed_fields(&active), vec!["PyroWave dialect"]);
	}

	#[test]
	fn only_display_properties_change_the_compositor_output_mode() {
		use pyrowave_protocol::PyroWaveDialect;
		let active = valid_context();
		let media_only = [
			VideoStreamContext {
				pyrowave_dialect: Some(PyroWaveDialect::NativeWireV1),
				format: NegotiatedVideoFormat::sdr(
					VideoCodec::PyroWave,
					ChromaFormat::Yuv444,
					BitDepth::Ten,
					ColorRange::Full,
				),
				..active.clone()
			},
			VideoStreamContext {
				format: NegotiatedVideoFormat::sdr(
					VideoCodec::Av1,
					ChromaFormat::Yuv420,
					BitDepth::Ten,
					ColorRange::Full,
				),
				..active.clone()
			},
			VideoStreamContext {
				bitrate: 150_000_000,
				..active.clone()
			},
			VideoStreamContext {
				packet_size: 1024,
				minimum_fec_packets: 4,
				qos: !active.qos,
				encrypt_video: !active.encrypt_video,
				max_reference_frames: 4,
				..active.clone()
			},
		];
		for requested in media_only {
			assert!(!active.changed_fields(&requested).is_empty());
			assert_eq!(
				requested.output_mode(CapturePacing::Fixed),
				active.output_mode(CapturePacing::Fixed),
				"{requested:?}"
			);
		}
		let display = [
			VideoStreamContext {
				width: 2560,
				height: 1440,
				..active.clone()
			},
			VideoStreamContext {
				fps: 60,
				..active.clone()
			},
			VideoStreamContext {
				format: NegotiatedVideoFormat::hdr10(VideoCodec::Hevc, ChromaFormat::Yuv420, ColorRange::Limited),
				..active.clone()
			},
		];
		for requested in display {
			assert_ne!(
				requested.output_mode(CapturePacing::Fixed),
				active.output_mode(CapturePacing::Fixed),
				"{requested:?}"
			);
		}
	}

	#[test]
	fn stats_logging_defaults_on_and_roundtrips_explicit_choice() {
		let config: VideoStreamConfig = toml::from_str("").unwrap();
		assert!(config.log_stats);
		for enabled in [false, true] {
			let root: crate::config::Config =
				toml::from_str(&format!("[stream.video]\nlog_stats = {enabled}")).unwrap();
			let config = root.stream.video;
			assert_eq!(config.log_stats, enabled);
			let roundtrip: VideoStreamConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
			assert_eq!(roundtrip.log_stats, enabled);
		}
	}

	fn config(max_packet_size: usize) -> VideoStreamConfig {
		VideoStreamConfig {
			max_packet_size,
			..Default::default()
		}
	}

	#[test]
	fn no_cap_honors_requested() {
		assert_eq!(config(0).clamp_packet_size(1392, false), 1392);
	}

	#[test]
	fn smaller_client_request_is_honored() {
		assert_eq!(config(1200).clamp_packet_size(1024, false), 1024);
	}

	#[test]
	fn larger_client_request_is_capped() {
		assert_eq!(config(1200).clamp_packet_size(1392, false), 1200);
	}

	#[test]
	fn encrypted_cap_preserves_the_configured_wire_size() {
		// Moonlight announces 1392 - 32 for an encrypted 1392-byte stream.
		assert_eq!(config(1376).clamp_packet_size(1360, true), 1344);
		assert_eq!(config(1376).clamp_packet_size(1300, true), 1300);
		let wire = |size, encrypted| packetizer::wire_shard_size(size, encrypted).unwrap();
		assert_eq!(wire(config(1376).clamp_packet_size(1360, true), true), 1376 + 16);
		assert_eq!(wire(config(1376).clamp_packet_size(1392, false), false), 1376 + 16);
	}

	fn valid_context() -> VideoStreamContext {
		VideoStreamContext {
			width: 3840,
			height: 2160,
			fps: 120,
			packet_size: 1392,
			bitrate: 900_000_000,
			format: NegotiatedVideoFormat::sdr(
				VideoCodec::Hevc,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Limited,
			),
			max_reference_frames: 1,
			..Default::default()
		}
	}

	#[test]
	fn high_end_contexts_validate() {
		for (width, height, fps, bitrate) in [
			(3840, 2160, 120, 650_000_000),
			(3840, 2160, 144, 900_000_000),
			(3840, 2160, 240, 900_000_000),
			(7680, 4320, 60, 900_000_000),
			(2560, 1440, 500, 150_000_000),
		] {
			for encrypt_video in [false, true] {
				let context = VideoStreamContext {
					width,
					height,
					fps,
					bitrate,
					encrypt_video,
					..valid_context()
				};
				assert_eq!(context.validate(), Ok(()), "{width}x{height}@{fps} {bitrate}");
			}
		}
		// PyroWave budgets beyond the conventional encoder's 32-bit range.
		let pyrowave = VideoStreamContext {
			bitrate: u32::MAX as usize + 1,
			format: NegotiatedVideoFormat::sdr(
				VideoCodec::PyroWave,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Full,
			),
			..valid_context()
		};
		assert_eq!(pyrowave.validate(), Ok(()));
	}

	#[test]
	fn degenerate_contexts_are_rejected() {
		let cases: [fn(&mut VideoStreamContext); 11] = [
			|c| c.fps = 0,
			|c| c.width = 0,
			|c| c.height = 0,
			|c| c.width = 16_385,
			|c| c.fps = u32::MAX,
			|c| c.bitrate = 0,
			|c| c.bitrate = u32::MAX as usize + 1,
			|c| c.packet_size = 0,
			|c| c.packet_size = 23,
			|c| c.packet_size = gso_socket::MAX_UDP_PAYLOAD,
			|c| c.packet_size = usize::MAX,
		];
		for (index, mutate) in cases.iter().enumerate() {
			let mut context = valid_context();
			mutate(&mut context);
			assert!(context.validate().is_err(), "case {index}: {context:?}");
		}
		// Largest datagram-sized packets: encryption costs exactly its prefix.
		let largest = gso_socket::MAX_UDP_PAYLOAD - 16;
		let plain = VideoStreamContext {
			packet_size: largest,
			..valid_context()
		};
		assert_eq!(plain.validate(), Ok(()));
		let encrypted = VideoStreamContext {
			encrypt_video: true,
			..plain.clone()
		};
		assert!(encrypted.validate().is_err());
		let encrypted = VideoStreamContext {
			packet_size: largest - packetizer::ENC_PREFIX_SIZE,
			..encrypted
		};
		assert_eq!(encrypted.validate(), Ok(()));
		let minimum = VideoStreamContext {
			packet_size: packetizer::MIN_VIDEO_PACKET_SIZE,
			..valid_context()
		};
		assert_eq!(minimum.validate(), Ok(()));
	}

	#[test]
	fn undersized_cap_is_ignored() {
		assert_eq!(config(50).clamp_packet_size(1392, false), 1392);
	}
}
