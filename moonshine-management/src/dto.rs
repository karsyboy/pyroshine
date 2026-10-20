//! JSON documents exchanged over the management interface.
//!
//! Every type is a sanitized view built from daemon state. Timestamps are Unix
//! milliseconds; durations carry their unit in the field name.

use serde::{Deserialize, Serialize};

/// Externally visible session lifecycle.
///
/// An application can outlive its client: Pyroshine keeps the session (and
/// the game) running after a disconnect so Moonlight can resume. A session
/// therefore does not imply that media is flowing; only `Streaming` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
	/// No session and no owned resources.
	Idle,
	/// A launch is creating the compositor and application, or the first
	/// client is negotiating and has not connected its control stream yet.
	Starting,
	/// The authenticated client owns the control stream; media is delivered.
	Streaming,
	/// The application keeps running without a client; media is paused until
	/// Moonlight resumes.
	ClientDisconnected,
	/// A client resumed the retained session and is renegotiating.
	Reconnecting,
	/// Teardown is stopping the application and every session worker.
	Stopping,
	/// Teardown exceeded its deadline; the service is shutting down so its
	/// supervisor can restart it.
	Error,
}

impl SessionPhase {
	/// Whether the canonical "end session" operation applies.
	pub fn can_end(self) -> bool {
		matches!(
			self,
			Self::Starting | Self::Streaming | Self::ClientDisconnected | Self::Reconnecting
		)
	}
}

/// Current session state, sent by `GetSession` and `SessionChanged`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
	pub phase: SessionPhase,
	/// Details of the live session; `None` when idle or stopping.
	pub session: Option<SessionDetails>,
	/// How the most recent session ended, if one ended since startup.
	pub last_stop: Option<LastStop>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionDetails {
	/// Session lifetime number (unrelated to client identity).
	pub epoch: u64,
	/// Configured Moonlight launch entry; its identity never follows focus.
	pub application: ApplicationSummary,
	/// Primary application presented by the embedded compositor, if named.
	#[serde(default)]
	pub foreground_application: Option<ForegroundApplication>,
	/// Address of the client authorized by the latest accepted launch/resume.
	pub client_address: String,
	pub started_at_ms: u64,
	/// Values from the client's launch or latest resume request.
	pub requested: RequestedMode,
	/// Negotiated video stream, once RTSP PLAY committed one.
	pub video: Option<VideoDetails>,
	/// Negotiated audio stream, once RTSP PLAY committed one.
	pub audio: Option<AudioDetails>,
	/// What the session streams and how, once capture is running.
	#[serde(default)]
	pub capture: Option<CaptureDetails>,
}

/// The capture backend of a running session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureDetails {
	/// `game` (the isolated compositor) or `desktop` (the running desktop).
	pub mode: String,
	/// `Game Mode` or `Desktop Mode`.
	pub mode_label: String,
	/// `isolated` or `desktop` (XDG Desktop Portal).
	pub backend: String,
	/// DRM render node frames are captured and encoded on.
	pub render_node: String,
	/// `SDR` or `HDR`: what the captured content delivers.
	pub dynamic_range: String,
	/// Desktop Mode: the captured desktop output.
	#[serde(default)]
	pub output: Option<CapturedOutputDetails>,
	/// Desktop Mode: `kde`, `gnome` or `other`, and `XDG_CURRENT_DESKTOP`.
	#[serde(default)]
	pub desktop: Option<String>,
	#[serde(default)]
	pub current_desktop: Option<String>,
	/// Desktop Mode: how remote input reaches the desktop: `libei`, `portal`
	/// or `none` (view-only).
	#[serde(default)]
	pub input: Option<String>,
	/// Desktop Mode: `restored` (no prompt) or `requested` (the desktop asked
	/// or granted it by policy).
	#[serde(default)]
	pub consent: Option<String>,
}

/// A captured desktop output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedOutputDetails {
	pub width: u32,
	pub height: u32,
	/// Declared refresh in millihertz, when the desktop declared one.
	#[serde(default)]
	pub refresh_millihertz: Option<u32>,
}

/// Which encoder backend produces the video stream and on which GPU.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderDetails {
	/// `vulkan_video` (Pixelforge H.264/HEVC/AV1), `vaapi`, `nvenc` or `pyrowave`.
	pub backend: String,
	/// Display name, for example `Vulkan Video`.
	pub backend_label: String,
	/// GPU that encodes; the capture GPU as well (Pyroshine captures and
	/// encodes on one device).
	pub gpu: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationSummary {
	/// Application ID as reported to clients.
	pub id: i32,
	pub title: String,
}

/// Display metadata only: compositor app IDs are not Moonlight entry IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundApplication {
	pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
	pub audio_channels: u8,
	pub audio_channel_mask: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoDetails {
	/// `h264`, `hevc`, `av1` or `pyrowave`.
	pub codec: String,
	/// Display name, for example `HEVC`.
	pub codec_label: String,
	/// `4:2:0` or `4:4:4`.
	pub chroma: String,
	pub bit_depth: u8,
	/// `SDR` or `HDR10`.
	pub dynamic_range: String,
	/// `BT.709` or `PQ`.
	pub transfer: String,
	/// `BT.709` or `BT.2020`.
	pub primaries: String,
	/// `BT.709` or `BT.2020 NCL`.
	pub matrix: String,
	/// `limited` or `full`.
	pub range: String,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	/// Client-requested target bitrate.
	pub bitrate_bps: u64,
	/// Negotiated packet size (after any configured cap).
	pub packet_size: u32,
	pub encrypted: bool,
	/// Client-requested minimum parity shards per frame.
	pub minimum_fec_packets: u32,
	pub max_reference_frames: u32,
	/// PyroWave transport dialect, for PyroWave streams.
	pub pyrowave_dialect: Option<String>,
	/// Encoder backend and GPU. Absent from older daemons.
	#[serde(default)]
	pub encoder: Option<EncoderDetails>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDetails {
	pub channels: u8,
	pub channel_mask: u32,
	/// GameStream high-quality surround layout (one mono Opus stream per channel).
	pub high_quality: bool,
	/// The Opus bitrate encoded, after the packet-size limit of the packet duration.
	pub opus_bitrate_bps: u32,
	pub packet_duration_ms: u32,
	pub encrypted: bool,
	/// Audio quality level: `standard`, `high` or `maximum`. Absent from older daemons.
	#[serde(default)]
	pub quality: Option<String>,
	/// Whether the client requested `quality` (Pyrolight); otherwise the host default applied.
	#[serde(default)]
	pub quality_requested: bool,
	/// Opus streams in the layout, of which `opus_coupled_streams` are stereo pairs.
	/// Zero from older daemons.
	#[serde(default)]
	pub opus_streams: u8,
	#[serde(default)]
	pub opus_coupled_streams: u8,
	/// Capture and encoding sample rate. Zero from older daemons.
	#[serde(default)]
	pub sample_rate_hz: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastStop {
	pub epoch: u64,
	/// Machine-readable reason, for example `user_stopped` or `application_stopped`.
	pub reason: String,
	/// Human-readable explanation.
	pub message: String,
	/// Whether something other than a user or service stop ended the session.
	pub unexpected: bool,
	/// Whether teardown completed: false when it exceeded its deadline or the
	/// application could not be confirmed stopped.
	pub completed: bool,
	pub at_ms: u64,
	/// What ended the session, when the component that ended it said, for
	/// example "Steam exited with status 1, 4 s after launch." Absent from
	/// older daemons.
	#[serde(default)]
	pub detail: Option<String>,
	/// What the operator can do about it.
	#[serde(default)]
	pub hint: Option<String>,
}

/// Static server information, sent by `GetServer`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
	pub api_version: u32,
	pub version: String,
	pub name: String,
	pub pid: u32,
	pub started_at_ms: u64,
	pub config_path: String,
	pub capabilities: Capabilities,
	/// Startup health check, when it ran (it is skipped by `--no-health-check`).
	pub health: Option<HealthReport>,
	pub listeners: Listeners,
	pub pairing_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
	/// Codec profiles verified at startup.
	pub codecs: Vec<CodecCapability>,
	/// HDR is advertised to clients (probe result and `compositor.hdr`).
	pub hdr_advertised: bool,
	pub dma_buf: bool,
	pub gpu: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCapability {
	pub id: String,
	pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
	pub all_fatal_passed: bool,
	pub checks: Vec<HealthCheck>,
	/// When the checks ran. Absent from older daemons.
	#[serde(default)]
	pub checked_at_ms: Option<u64>,
	/// The report comes from `RunHealthCheck` rather than startup.
	#[serde(default)]
	pub rechecked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
	pub name: String,
	/// `passed`, `warning` or `failed`.
	pub outcome: String,
	pub message: String,
	pub duration_ms: u64,
	/// Stable identifier, for example `render_node`. Absent from older daemons.
	#[serde(default)]
	pub id: Option<String>,
	/// What to change when the check did not pass.
	#[serde(default)]
	pub hint: Option<String>,
	/// A failure keeps Pyroshine from starting.
	#[serde(default)]
	pub fatal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listeners {
	pub address: String,
	pub http_port: u16,
	pub https_port: u16,
	pub rtsp_port: u16,
	pub video_port: u16,
	pub audio_port: u16,
	pub control_port: u16,
}

/// Pending pairing requests, sent by `GetPairing` and `PairingChanged`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSnapshot {
	/// `webserver.enable_pairing` of the running configuration.
	pub enabled: bool,
	pub requests: Vec<PendingPairing>,
}

/// A pairing request waiting for the operator.
///
/// `request` identifies this exact request. Approval and rejection must echo
/// it; a request that replaced it under the same client ID has another token,
/// so stale input can never approve the replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
	/// Client ID sent by Moonlight. Many clients send the same ID, so it does
	/// not identify a device; the certificate fingerprint does.
	pub client_id: String,
	pub request: String,
	pub requester: String,
	/// SHA-256 of the client certificate (hex).
	pub fingerprint: Option<String>,
	pub received_at_ms: u64,
	/// Time left to enter the PIN.
	pub approval_expires_in_ms: u64,
	/// A PIN was accepted; the client is completing the protocol.
	pub approved: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingOutcome {
	/// The client completed pairing and is now trusted.
	Paired,
	/// The operator rejected the request.
	Rejected,
	/// The client gave up or the approval wait ended.
	Cancelled,
	/// The request expired.
	Expired,
	/// The client sent a new request under the same client ID.
	Replaced,
	/// The cryptographic exchange failed (for example a wrong PIN).
	Failed,
	/// A revocation or shutdown cleared every pending request.
	Cleared,
}

/// Sent by `PairingResolved` when a pending request leaves the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingResolution {
	pub client_id: String,
	pub request: String,
	pub outcome: PairingOutcome,
}

/// Paired clients, sent by `GetClients` and `ClientsChanged`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientsSnapshot {
	pub clients: Vec<PairedClient>,
	/// Client IDs from older state files without a known certificate. They
	/// cannot authorize HTTPS on their own.
	pub legacy_client_ids: Vec<String>,
}

/// One trusted client certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedClient {
	/// SHA-256 of the client certificate (hex); the credential's identity.
	pub fingerprint: String,
	/// Client IDs associated with this certificate when it was paired.
	pub client_ids: Vec<String>,
	/// Name assigned by the host operator.
	pub label: Option<String>,
	/// Recorded for pairings completed by versions that store it.
	pub paired_at_ms: Option<u64>,
	/// Last authenticated request since the daemon started.
	pub last_seen_ms: Option<u64>,
	pub last_address: Option<String>,
}

/// The configuration file as the daemon reads it, sent by `GetConfig`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument {
	pub path: String,
	/// Opaque content revision; pass it back to `SaveConfig`.
	pub revision: String,
	pub writable: bool,
	pub read_only_reason: Option<String>,
	/// The file differs from the configuration the daemon started with.
	pub restart_required: bool,
	/// The complete configuration, with defaults for omitted settings, using
	/// the same names as `config.toml`.
	pub values: serde_json::Value,
	/// The default configuration.
	pub defaults: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
	/// Dotted setting path, for example `stream.video.port` or `application[2].command`.
	pub path: Option<String>,
	pub message: String,
}

/// Result of `ValidateConfig`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
	pub valid: bool,
	pub issues: Vec<ConfigIssue>,
	/// Settings that differ from the file.
	pub changed_paths: Vec<String>,
	/// Saving would leave the file different from the running configuration.
	pub restart_required: bool,
}

/// Result of `SaveConfig`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveOutcome {
	pub revision: String,
	pub changed_paths: Vec<String>,
	pub restart_required: bool,
}

/// Sent by `ConfigSaved`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSaved {
	pub revision: String,
	pub restart_required: bool,
}

/// Aggregated statistics of the live video stream, sent about once per
/// second by `StatsUpdated` while a client is streaming.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamStats {
	pub epoch: u64,
	pub at_ms: u64,
	/// Length of the aggregation window.
	pub window_ms: u64,
	pub frames: u64,
	pub fps: f64,
	pub key_frames: u64,
	/// Encoder output rate.
	pub encoded_bitrate_bps: f64,
	/// Submitted UDP payload rate, including FEC, headers and encryption.
	pub wire_bitrate_bps: f64,
	/// Wire bytes above encoded bytes, in percent of encoded bytes.
	pub transport_overhead_percent: Option<f64>,
	pub packets: u64,
	pub failed_packets: u64,
	pub discarded_packets: u64,
	pub stale_frames_dropped: u64,
	/// Frame samples the aggregator could not read in time. Telemetry never
	/// slows the stream; it drops samples instead.
	pub samples_dropped: u64,
	pub stages: Vec<StageStats>,
	/// Frame pacing of the captured content; absent when no frame was
	/// captured in the window (or from a daemon that does not report it).
	#[serde(default)]
	pub capture: Option<CaptureStats>,
	/// The daemon's reading of this window: bottlenecks and causes worth
	/// the operator's attention, most important first. Empty when nothing
	/// stands out (and from older daemons).
	#[serde(default)]
	pub findings: Vec<Finding>,
	/// Activity of the capture GPU over the window; absent when its driver
	/// exposes no sensors (and from older daemons).
	#[serde(default)]
	pub gpu: Option<GpuStats>,
}

/// Activity of the capture GPU over one statistics window. Every reading is
/// optional: drivers expose different sensors, and an unreadable sensor is
/// omitted rather than reported as zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuStats {
	/// Kernel driver, for example `amdgpu`, `i915`, `xe` or `nvidia`.
	pub driver: String,
	/// Where the readings come from: `sysfs` (kernel interfaces) or `nvml`.
	pub source: String,
	/// Share of the window the GPU's graphics/compute engines were busy, in percent.
	pub busy_percent: Option<f64>,
	/// Share of the window the video encode engine was busy, in percent.
	pub encoder_busy_percent: Option<f64>,
	pub core_clock_mhz: Option<f64>,
	pub memory_clock_mhz: Option<f64>,
	/// Edge (or the only reported) temperature.
	pub temperature_c: Option<f64>,
	/// Hottest point (junction or hotspot) where the driver reports it.
	pub hotspot_c: Option<f64>,
	pub power_w: Option<f64>,
	pub power_limit_w: Option<f64>,
	pub vram_used_bytes: Option<u64>,
	pub vram_total_bytes: Option<u64>,
	/// GPU time of the streamed application and of Pyroshine itself, where the
	/// driver attributes it to processes.
	pub processes: Vec<GpuProcessStats>,
}

/// GPU use of one group of processes over the window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuProcessStats {
	/// `application` (the launched application's unit) or `pyroshine`
	/// (capture, composition, color conversion and encoding).
	pub role: String,
	pub label: String,
	/// Busy share per engine, busiest first. An engine shared by several
	/// queues can exceed 100 %.
	pub engines: Vec<GpuEngineStats>,
	/// Video memory resident for the group, where reported.
	pub vram_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuEngineStats {
	/// The driver's engine name, for example `gfx`, `enc` or `render`.
	pub engine: String,
	/// `graphics`, `compute`, `video_encode`, `video_decode`, `video`
	/// (shared encode/decode engine), `copy` or `other`.
	pub class: String,
	pub busy_percent: f64,
}

/// One conclusion drawn from a statistics window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
	/// Stable identifier, for example `encoder_bound`.
	pub id: String,
	/// `info` or `warning`.
	pub severity: String,
	/// One line, for example "The encoder limits the frame rate".
	pub title: String,
	/// The numbers behind it.
	pub detail: String,
	/// What the operator can change.
	pub hint: Option<String>,
}

/// How the application's frames reached the encoder over the window: their
/// content-time intervals (the frame times a client sees in RTP timestamps
/// and paces VRR presentation by) and the capture pacing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureStats {
	/// `vrr` when frames were captured as the application presented them
	/// (the client requested VRR presentation), `fixed` when captured on the
	/// stream's refresh clock, `mixed` when the pacing changed in the window.
	pub pacing: String,
	/// New frames per second of content time; zero when the content was static.
	pub source_fps: f64,
	/// Content-time interval between consecutive new frames, in microseconds.
	pub interval_p50_us: f64,
	pub interval_p95_us: f64,
	pub interval_p99_us: f64,
	pub interval_max_us: f64,
	pub interval_stddev_us: f64,
	/// Share of intervals that differ from the previous one by more than 2 ms,
	/// in percent: visible unevenness when a display follows the frame times.
	pub uneven_percent: f64,
	/// Time from content becoming current to its capture, in microseconds.
	pub content_age_p50_us: f64,
	pub content_age_p95_us: f64,
	/// How the frames were produced (direct export or composition). Absent
	/// from older daemons.
	#[serde(default)]
	pub path: Option<CapturePathStats>,
}

/// How the window's frames reached the encoder.
///
/// Direct export hands the application's own DMA-BUF to the encoder;
/// composition renders the scene into a capture buffer first, which costs
/// GPU time on the game's GPU and adds latency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturePathStats {
	/// `direct`, `direct_late`, `composited` or `mixed`: the path of every
	/// frame in the window, or `mixed`.
	pub current: String,
	/// The application's buffer, exported unchanged.
	pub direct_frames: u64,
	/// Direct export with the cursor or a Steam notification drawn by the
	/// encoder's color conversion.
	pub direct_late_frames: u64,
	/// Composited by the compositor.
	pub composited_frames: u64,
	/// Why frames were composited, most frequent first.
	pub composition_reasons: Vec<CompositionReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionReason {
	/// Stable identifier, for example `scaling` or `steam_overlay`.
	pub id: String,
	pub label: String,
	pub hint: Option<String>,
	pub frames: u64,
}

/// Timing of one pipeline stage over the window, in microseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageStats {
	pub id: String,
	pub label: String,
	pub avg_us: f64,
	pub p50_us: f64,
	pub p95_us: f64,
	pub max_us: f64,
}

/// Notable backend event, sent by `ServerEvent` and listed by `GetEvents`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEvent {
	/// `info`, `warning` or `error`.
	pub level: String,
	pub code: String,
	pub message: String,
	pub at_ms: u64,
	/// What the operator can do about it. Absent from older daemons.
	#[serde(default)]
	pub hint: Option<String>,
}

/// Everything needed to diagnose the host in one document, sent by
/// `GetDiagnosticReport` (and the web interface's "Copy diagnostics").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticReport {
	pub generated_at_ms: u64,
	/// Includes the most recent health report.
	pub server: ServerInfo,
	pub session: SessionSnapshot,
	pub stats: Option<StreamStats>,
	/// Recent events, oldest first.
	pub events: Vec<ServerEvent>,
	/// Operating system, kernel, CPU and GPUs. Absent from older daemons.
	#[serde(default)]
	pub host: Option<HostInfo>,
}

/// The host's hardware and software, for support reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
	/// Kernel release, for example `6.17.2-arch1-1`.
	pub kernel: String,
	/// `PRETTY_NAME` from os-release.
	pub os: Option<String>,
	pub cpu: Option<String>,
	pub cpu_threads: u32,
	pub memory_total_bytes: Option<u64>,
	/// The systemd unit Pyroshine runs in, for example `pyroshine@user.service`.
	pub service_unit: Option<String>,
	/// `system` or `user`: which systemd instance runs the unit.
	pub service_scope: Option<String>,
	pub gpus: Vec<GpuDevice>,
}

/// A DRM render node and the device behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
	pub render_node: String,
	pub pci_address: Option<String>,
	/// PCI vendor and device IDs, for example `0x1002` and `0x7550`.
	pub vendor_id: Option<String>,
	pub device_id: Option<String>,
	pub driver: Option<String>,
	/// Version of an out-of-tree kernel driver (NVIDIA); in-tree drivers
	/// follow the kernel.
	pub driver_version: Option<String>,
	pub vbios: Option<String>,
	/// The device Pyroshine captures and encodes on.
	pub capture: bool,
}

/// The application catalog, sent by `GetLibrary` and `LibraryChanged`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySnapshot {
	/// Increments whenever the applications offered to clients change.
	pub generation: u64,
	/// A scan is running.
	pub scanning: bool,
	/// Launcher changes update the catalog automatically (`[library]`).
	pub auto_refresh: bool,
	pub poll_interval_secs: u64,
	/// When the catalog was last reconciled.
	pub last_refresh_ms: Option<u64>,
	/// A problem not tied to one source, for example that visibility choices
	/// cannot be saved.
	pub notice: Option<String>,
	pub sources: Vec<LibrarySource>,
	/// Manual entries first, then discovered ones, in catalog order.
	/// Includes entries clients are not offered (hidden, replaced, duplicate).
	pub applications: Vec<LibraryApplication>,
}

/// One `[[application_scanner]]` and how its last scan went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySource {
	/// Position among the configured scanners.
	pub scanner: u32,
	/// `steam`, `lutris`, `heroic` or `desktop`.
	pub kind: String,
	pub label: String,
	/// The configured location it reads.
	pub root: String,
	/// `pending`, `ok`, `partial` (some locations failed) or `failed`.
	pub status: String,
	pub message: Option<String>,
	pub hint: Option<String>,
	pub applications: u32,
	/// Applications offered from an earlier scan because their location is
	/// currently unreadable.
	pub stale_applications: u32,
	pub last_scan_ms: Option<u64>,
	pub duration_ms: Option<u64>,
	/// Changes are noticed immediately (file notifications); otherwise by
	/// the periodic check only.
	pub watched: bool,
}

/// One catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryApplication {
	/// Stable identity: `manual:<index>` or `<launcher>:<native id>`
	/// (`steam:620`). Pass it to `SetApplicationHidden`.
	pub key: String,
	/// Moonlight application ID; 0 for entries never offered.
	pub id: i32,
	/// Title as clients see it.
	pub title: String,
	/// `manual`, `steam`, `lutris`, `heroic` or `desktop`.
	pub source: String,
	pub source_label: String,
	pub manual: bool,
	/// Index of the scanner that found it.
	pub scanner: Option<u32>,
	/// Launcher-native identity (Steam app ID, Lutris slug, ...).
	pub native_id: Option<String>,
	/// `available`, `stale`, `hidden`, `shadowed` or `duplicate`.
	pub state: String,
	/// Clients see it in their app list.
	pub offered: bool,
	/// Why it is not offered, or why it is stale.
	pub state_detail: Option<String>,
	/// `SetApplicationHidden` applies to it.
	pub can_hide: bool,
	/// Where its box art comes from (`steam`, `steam_custom`, `launcher`,
	/// `desktop_icon`, `manual`, `icon_theme`); `None` without box art.
	pub artwork: Option<String>,
	pub command: Vec<String>,
}

/// Editable configuration settings, sent by `GetConfigSchema`. The daemon
/// builds it next to the typed configuration it describes, and a test
/// requires it to cover every setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigSchema {
	pub sections: Vec<SchemaSection>,
	pub fields: Vec<FieldSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSection {
	pub id: String,
	pub title: String,
	pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
	/// Dotted path in `config.toml` (relative to the item for list items).
	pub path: String,
	pub section: String,
	pub label: String,
	pub help: String,
	pub kind: FieldKind,
	/// Rarely needed or diagnostic; shown under "advanced".
	pub advanced: bool,
	/// Changing it can break streaming or weaken security if misused.
	pub caution: Option<String>,
	/// The setting must be present in an item (list items only).
	pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldKind {
	Text {
		/// `None` is a valid value (the setting is omitted from the file).
		optional: bool,
		placeholder: Option<String>,
		/// Suggested values for free-form settings.
		suggestions: Vec<String>,
	},
	Path {
		optional: bool,
		/// `~` and environment variables are expanded by Pyroshine.
		expands: bool,
		directory: bool,
	},
	Bool,
	Integer {
		min: f64,
		max: f64,
		unit: Option<String>,
		optional: bool,
	},
	Number {
		min: f64,
		max: f64,
		step: f64,
		unit: Option<String>,
		optional: bool,
	},
	Port,
	Choice {
		options: Vec<ChoiceOption>,
		/// Label for "unset" when the setting is optional.
		unset_label: Option<String>,
	},
	/// One argument vector (`["/usr/bin/steam", "-bigpicture"]`).
	Command {
		placeholders: Vec<String>,
	},
	/// A list of argument vectors (pre/post hooks).
	CommandList,
	/// A list of paths.
	PathList {
		expands: bool,
	},
	/// A list of short strings (for example host names).
	TextList {
		placeholder: Option<String>,
	},
	/// `[[application]]` entries.
	Applications {
		item: Vec<FieldSpec>,
	},
	/// `[[application_scanner]]` entries, one field set per `type`.
	Scanners {
		variants: Vec<ScannerVariant>,
	},
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
	pub value: String,
	pub label: String,
	pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScannerVariant {
	/// Value of the `type` key.
	pub id: String,
	pub label: String,
	pub description: String,
	pub fields: Vec<FieldSpec>,
}

/// One session in the persistent history. Every value was measured or
/// negotiated on the host; nothing here is reported by the client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
	/// Row identifier, stable while the entry is kept.
	pub id: i64,
	pub epoch: u64,
	/// The application the client launched.
	pub application: String,
	/// The last foreground application, when it differed (for example a
	/// game started from Steam).
	#[serde(default)]
	pub foreground_application: Option<String>,
	/// Absent when `[history] record_client_address` is off.
	#[serde(default)]
	pub client_address: Option<String>,
	/// The operator's name for the client that streamed from that address,
	/// when it is known.
	#[serde(default)]
	pub client_label: Option<String>,
	pub started_at_ms: u64,
	/// Absent while the session runs.
	#[serde(default)]
	pub ended_at_ms: Option<u64>,
	/// What the client asked for.
	#[serde(default)]
	pub requested: Option<HistoryMode>,
	/// What was negotiated for the first stream of the session.
	#[serde(default)]
	pub video: Option<HistoryVideo>,
	#[serde(default)]
	pub audio_channels: Option<u8>,
	/// Times the client disconnected while the application kept running.
	pub disconnects: u32,
	/// Times a client resumed the session.
	pub reconnects: u32,
	/// Disconnects, resumes and the stop, oldest first (bounded).
	#[serde(default)]
	pub events: Vec<HistoryEvent>,
	#[serde(default)]
	pub stop: Option<HistoryStop>,
	/// Absent when the session never streamed a full statistics window.
	#[serde(default)]
	pub performance: Option<PerformanceSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryVideo {
	pub codec: String,
	pub codec_label: String,
	pub chroma: String,
	pub bit_depth: u8,
	pub hdr: bool,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_bps: u64,
	#[serde(default)]
	pub encoder: Option<String>,
	#[serde(default)]
	pub gpu: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
	pub at_ms: u64,
	/// `disconnected`, `reconnected` or `stopped`.
	pub kind: String,
	#[serde(default)]
	pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStop {
	/// Machine-readable reason (see [`LastStop::reason`]); `interrupted`
	/// when Pyroshine stopped before the session ended.
	pub reason: String,
	pub message: String,
	#[serde(default)]
	pub detail: Option<String>,
	#[serde(default)]
	pub hint: Option<String>,
	pub unexpected: bool,
}

/// Host-side performance over every statistics window of a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformanceSummary {
	/// Statistics windows aggregated (about one per second of streaming).
	pub windows: u32,
	pub streaming_ms: u64,
	pub frames: u64,
	pub fps_avg: f64,
	/// The slowest window.
	pub fps_min: f64,
	pub encoded_bitrate_avg_bps: f64,
	pub wire_bitrate_avg_bps: f64,
	pub stale_frames_dropped: u64,
	pub failed_packets: u64,
	pub discarded_packets: u64,
	/// Pipeline stages: mean of the windows' averages (weighted by frames)
	/// and the worst window's 95th percentile.
	pub stages: Vec<StageSummary>,
	#[serde(default)]
	pub gpu_busy_avg_percent: Option<f64>,
	#[serde(default)]
	pub encoder_busy_avg_percent: Option<f64>,
	#[serde(default)]
	pub gpu_temperature_max_c: Option<f64>,
	/// Findings by how many windows reported them, most frequent first.
	pub findings: Vec<FindingCount>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageSummary {
	pub id: String,
	pub label: String,
	pub avg_us: f64,
	pub p95_max_us: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingCount {
	pub id: String,
	pub severity: String,
	pub title: String,
	pub windows: u32,
}

/// A page of the session history, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryPage {
	/// Whether history is recorded (`[history] enabled` and storage works).
	pub available: bool,
	/// Why history is unavailable or the last write failed.
	#[serde(default)]
	pub error: Option<String>,
	pub retention_days: u32,
	pub max_sessions: u32,
	pub total: u64,
	pub entries: Vec<HistoryEntry>,
}

/// Release notifications (see the web interface's Updates page).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
	pub current_version: String,
	/// Check automatically once a day.
	pub automatic: bool,
	/// `stable` or `prerelease`.
	pub channel: String,
	pub checking: bool,
	#[serde(default)]
	pub last_checked_ms: Option<u64>,
	#[serde(default)]
	pub last_error: Option<String>,
	/// The newest release on the channel, when a check succeeded.
	#[serde(default)]
	pub latest: Option<ReleaseInfo>,
	/// `latest` is newer than the running server.
	pub update_available: bool,
	/// The operator dismissed the notice for `latest`.
	pub dismissed: bool,
	pub install: InstallGuidance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
	pub version: String,
	pub prerelease: bool,
	#[serde(default)]
	pub published_at: Option<String>,
	/// The release page on the project's repository.
	pub url: String,
}

/// How this installation is updated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallGuidance {
	/// `arch`, `debian`, `rpm`, `nix`, `portable` or `unknown`.
	pub kind: String,
	pub label: String,
	/// Commands or steps, in order.
	pub steps: Vec<String>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn session_fixture_preserves_launch_identity_and_round_trips() {
		let fixture: serde_json::Value =
			serde_json::from_str(include_str!("../../web/src/api/session.fixture.json")).unwrap();
		let snapshot: SessionSnapshot = serde_json::from_value(fixture.clone()).unwrap();
		let session = snapshot.session.as_ref().unwrap();
		assert_eq!(
			session.application,
			ApplicationSummary {
				id: 42,
				title: "Steam".into()
			}
		);
		assert_eq!(session.foreground_application.as_ref().unwrap().title, "Grim Dawn");
		assert_eq!(serde_json::to_value(snapshot).unwrap(), fixture);

		// Older daemons omit the additive field. New consumers still accept them.
		let mut legacy = fixture;
		legacy["session"]
			.as_object_mut()
			.unwrap()
			.remove("foreground_application");
		let snapshot: SessionSnapshot = serde_json::from_value(legacy).unwrap();
		assert_eq!(snapshot.session.unwrap().foreground_application, None);
	}

	/// Audio details from a daemon without audio quality still parse; the
	/// additive fields take their defaults.
	#[test]
	fn audio_details_from_an_older_daemon_parse() {
		let audio: AudioDetails = serde_json::from_value(serde_json::json!({
			"channels": 2,
			"channel_mask": 3,
			"high_quality": false,
			"opus_bitrate_bps": 96000,
			"packet_duration_ms": 5,
			"encrypted": true,
		}))
		.unwrap();
		assert_eq!(audio.quality, None);
		assert!(!audio.quality_requested);
		assert_eq!(
			(audio.opus_streams, audio.opus_coupled_streams, audio.sample_rate_hz),
			(0, 0, 0)
		);
	}

	#[test]
	fn phases_use_stable_snake_case_names() {
		for (phase, name) in [
			(SessionPhase::Idle, "idle"),
			(SessionPhase::Starting, "starting"),
			(SessionPhase::Streaming, "streaming"),
			(SessionPhase::ClientDisconnected, "client_disconnected"),
			(SessionPhase::Reconnecting, "reconnecting"),
			(SessionPhase::Stopping, "stopping"),
			(SessionPhase::Error, "error"),
		] {
			assert_eq!(serde_json::to_value(phase).unwrap(), name);
		}
		assert!(!SessionPhase::Idle.can_end());
		assert!(!SessionPhase::Stopping.can_end());
		assert!(SessionPhase::ClientDisconnected.can_end());
	}

	#[test]
	fn field_kinds_are_tagged() {
		let kind = FieldKind::Integer {
			min: 0.0,
			max: 10.0,
			unit: Some("s".into()),
			optional: false,
		};
		let value = serde_json::to_value(&kind).unwrap();
		assert_eq!(value["type"], "integer");
		assert_eq!(serde_json::from_value::<FieldKind>(value).unwrap(), kind);
	}
}
