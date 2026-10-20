// Mirrors moonshine-management/src/dto.rs. Keep the two in sync.

export type SessionPhase =
  | "idle"
  | "starting"
  | "streaming"
  | "client_disconnected"
  | "reconnecting"
  | "stopping"
  | "error";

export interface SessionSnapshot {
  phase: SessionPhase;
  session: SessionDetails | null;
  last_stop: LastStop | null;
}

export interface SessionDetails {
  epoch: number;
  application: { id: number; title: string };
  foreground_application?: { title: string } | null;
  client_address: string;
  started_at_ms: number;
  requested: {
    width: number;
    height: number;
    refresh_rate: number;
    hdr: boolean;
    audio_channels: number;
    audio_channel_mask: number;
  };
  video: VideoDetails | null;
  audio: AudioDetails | null;
}

export interface VideoDetails {
  codec: string;
  codec_label: string;
  chroma: string;
  bit_depth: number;
  dynamic_range: string;
  transfer: string;
  primaries: string;
  matrix: string;
  range: string;
  width: number;
  height: number;
  fps: number;
  bitrate_bps: number;
  packet_size: number;
  encrypted: boolean;
  minimum_fec_packets: number;
  max_reference_frames: number;
  pyrowave_dialect: string | null;
  /** Encoder backend and GPU; absent from older daemons. */
  encoder?: EncoderDetails | null;
}

export interface EncoderDetails {
  /** `vulkan_video`, `vaapi`, `nvenc` or `pyrowave`. */
  backend: string;
  backend_label: string;
  gpu: string;
}

export interface AudioDetails {
  channels: number;
  channel_mask: number;
  /** GameStream high-quality surround layout (one mono Opus stream per channel). */
  high_quality: boolean;
  /** Encoded Opus bitrate, after the packet-size limit. */
  opus_bitrate_bps: number;
  packet_duration_ms: number;
  encrypted: boolean;
  /** The remaining fields are absent from older daemons. */
  quality?: "standard" | "high" | "maximum" | null;
  quality_requested?: boolean;
  opus_streams?: number;
  opus_coupled_streams?: number;
  sample_rate_hz?: number;
}

export interface LastStop {
  epoch: number;
  reason: string;
  message: string;
  unexpected: boolean;
  completed: boolean;
  at_ms: number;
  /** What ended the session, when known; absent from older daemons. */
  detail?: string | null;
  /** What to do about it. */
  hint?: string | null;
}

export interface ServerInfo {
  api_version: number;
  version: string;
  name: string;
  pid: number;
  started_at_ms: number;
  config_path: string;
  capabilities: {
    codecs: { id: string; label: string }[];
    hdr_advertised: boolean;
    dma_buf: boolean;
    gpu: string;
  };
  health: HealthReport | null;
  listeners: {
    address: string;
    http_port: number;
    https_port: number;
    rtsp_port: number;
    video_port: number;
    audio_port: number;
    control_port: number;
  };
  pairing_enabled: boolean;
}

export interface HealthCheck {
  name: string;
  outcome: "passed" | "warning" | "failed";
  /** What the check found. */
  message: string;
  duration_ms: number;
  /** The remaining fields are absent from older daemons. */
  id?: string | null;
  /** What to change when the check did not pass. */
  hint?: string | null;
  /** A failure keeps Pyroshine from starting. */
  fatal?: boolean;
}

export interface HealthReport {
  all_fatal_passed: boolean;
  checks: HealthCheck[];
  checked_at_ms?: number | null;
  /** From "Run checks" rather than startup. */
  rechecked?: boolean;
}

export interface PendingPairing {
  client_id: string;
  request: string;
  requester: string;
  fingerprint: string | null;
  received_at_ms: number;
  approval_expires_in_ms: number;
  approved: boolean;
}

export interface PairingSnapshot {
  enabled: boolean;
  requests: PendingPairing[];
}

export type PairingOutcome = "paired" | "rejected" | "cancelled" | "expired" | "replaced" | "failed" | "cleared";

export interface PairingResolution {
  client_id: string;
  request: string;
  outcome: PairingOutcome;
}

export interface PairedClient {
  fingerprint: string;
  client_ids: string[];
  label: string | null;
  paired_at_ms: number | null;
  last_seen_ms: number | null;
  last_address: string | null;
}

export interface ClientsSnapshot {
  clients: PairedClient[];
  legacy_client_ids: string[];
}

export interface StageStats {
  id: string;
  label: string;
  avg_us: number;
  p50_us: number;
  p95_us: number;
  max_us: number;
}

export interface StreamStats {
  epoch: number;
  at_ms: number;
  window_ms: number;
  frames: number;
  fps: number;
  key_frames: number;
  encoded_bitrate_bps: number;
  wire_bitrate_bps: number;
  transport_overhead_percent: number | null;
  packets: number;
  failed_packets: number;
  discarded_packets: number;
  stale_frames_dropped: number;
  samples_dropped: number;
  stages: StageStats[];
  /** Frame pacing of the captured content; absent from older daemons. */
  capture?: CaptureStats | null;
  /** The daemon's reading of the window, warnings first; absent from older daemons. */
  findings?: Finding[];
  /** Capture GPU activity; absent when the driver exposes no sensors (and from older daemons). */
  gpu?: GpuStats | null;
}

/** Capture GPU readings over one window. Each value is null when the driver does not report it. */
export interface GpuStats {
  driver: string;
  /** `sysfs` (kernel interfaces) or `nvml` (NVIDIA). */
  source: string;
  busy_percent: number | null;
  encoder_busy_percent: number | null;
  core_clock_mhz: number | null;
  memory_clock_mhz: number | null;
  temperature_c: number | null;
  hotspot_c: number | null;
  power_w: number | null;
  power_limit_w: number | null;
  vram_used_bytes: number | null;
  vram_total_bytes: number | null;
  processes: GpuProcessStats[];
}

export interface GpuProcessStats {
  role: "application" | "pyroshine";
  label: string;
  /** Busiest first; a shared engine can exceed 100 %. */
  engines: GpuEngineStats[];
  vram_bytes: number | null;
}

export interface GpuEngineStats {
  engine: string;
  class: "graphics" | "compute" | "video_encode" | "video_decode" | "video" | "copy" | "other";
  busy_percent: number;
}

export interface Finding {
  id: string;
  severity: "info" | "warning";
  title: string;
  detail: string;
  hint: string | null;
}

export interface CaptureStats {
  /** `vrr`: captured as the application presents (client VRR); `fixed`: refresh clock. */
  pacing: "vrr" | "fixed" | "mixed";
  /** New frames per second of content time; 0 when the content was static. */
  source_fps: number;
  interval_p50_us: number;
  interval_p95_us: number;
  interval_p99_us: number;
  interval_max_us: number;
  interval_stddev_us: number;
  /** Intervals differing from the previous one by more than 2 ms, in percent. */
  uneven_percent: number;
  content_age_p50_us: number;
  content_age_p95_us: number;
  /** How frames reached the encoder; absent from older daemons. */
  path?: CapturePathStats | null;
}

export interface CapturePathStats {
  current: "direct" | "direct_late" | "composited" | "mixed";
  direct_frames: number;
  direct_late_frames: number;
  composited_frames: number;
  composition_reasons: { id: string; label: string; hint: string | null; frames: number }[];
}

export interface ServerEvent {
  level: "info" | "warning" | "error";
  code: string;
  message: string;
  at_ms: number;
  /** What to do about it; absent from older daemons. */
  hint?: string | null;
}

export interface Overview {
  connected: boolean;
  server: ServerInfo | null;
  session: SessionSnapshot | null;
  pairing: PairingSnapshot | null;
  clients: ClientsSnapshot | null;
  stats: StreamStats | null;
  /** The web interface itself: its address and the version its files were built for. */
  web?: { url: string | null; assets_version?: string | null } | null;
}

// Configuration editor.

export type ConfigValues = Record<string, unknown>;

export interface ConfigDocument {
  path: string;
  revision: string;
  writable: boolean;
  read_only_reason: string | null;
  restart_required: boolean;
  values: ConfigValues;
  defaults: ConfigValues;
}

export interface ConfigIssue {
  path: string | null;
  message: string;
}

export interface ValidationReport {
  valid: boolean;
  issues: ConfigIssue[];
  changed_paths: string[];
  restart_required: boolean;
}

export interface SaveOutcome {
  revision: string;
  changed_paths: string[];
  restart_required: boolean;
}

export interface ChoiceOption {
  value: string;
  label: string;
  description: string;
}

export type FieldKind =
  | { type: "text"; optional: boolean; placeholder: string | null; suggestions: string[] }
  | { type: "path"; optional: boolean; expands: boolean; directory: boolean }
  | { type: "bool" }
  | { type: "integer"; min: number; max: number; unit: string | null; optional: boolean }
  | { type: "number"; min: number; max: number; step: number; unit: string | null; optional: boolean }
  | { type: "port" }
  | { type: "choice"; options: ChoiceOption[]; unset_label: string | null }
  | { type: "command"; placeholders: string[] }
  | { type: "command_list" }
  | { type: "path_list"; expands: boolean }
  | { type: "text_list"; placeholder: string | null }
  | { type: "applications"; item: FieldSpec[] }
  | { type: "scanners"; variants: ScannerVariant[] };

export interface FieldSpec {
  path: string;
  section: string;
  label: string;
  help: string;
  kind: FieldKind;
  advanced: boolean;
  caution: string | null;
  required: boolean;
}

export interface ScannerVariant {
  id: string;
  label: string;
  description: string;
  fields: FieldSpec[];
}

export interface ConfigSchema {
  sections: { id: string; title: string; description: string }[];
  fields: FieldSpec[];
}

/** API error: `{"error": {"kind", "message"}}` from the server, or a client-side failure. */
export interface UiError {
  kind:
    | "unavailable"
    | "unauthenticated"
    | "invalid_credentials"
    | "rate_limited"
    | "not_configured"
    | "csrf"
    | "cross_origin"
    | "access_denied"
    | "invalid"
    | "not_found"
    | "conflict"
    | "read_only"
    | "failed"
    | string;
  message: string;
}

// Web sign-in (`/api/v1/auth`).

export interface AuthState {
  /** An administrator password exists. */
  configured: boolean;
  /** A setup code was issued and is still valid. */
  setup_pending: boolean;
  /** The credentials file is damaged; only a reset code helps. */
  locked: string | null;
  min_password_length: number;
  authenticated: boolean;
  csrf_token: string | null;
  session: { id: string; expires_at_ms: number } | null;
  api_version: number;
}

/** A signed-in browser (`/api/v1/auth/sessions`). */
export interface WebSession {
  id: string;
  created_at_ms: number;
  last_used_at_ms: number;
  expires_at_ms: number;
  address: string;
  agent: string;
  current: boolean;
}

// Session history (`/api/v1/history`). Every value is measured on the host.

export interface HistoryEntry {
  id: number;
  epoch: number;
  application: string;
  foreground_application?: string | null;
  client_address?: string | null;
  client_label?: string | null;
  started_at_ms: number;
  ended_at_ms?: number | null;
  requested?: { width: number; height: number; refresh_rate: number; hdr: boolean } | null;
  video?: {
    codec: string;
    codec_label: string;
    chroma: string;
    bit_depth: number;
    hdr: boolean;
    width: number;
    height: number;
    fps: number;
    bitrate_bps: number;
    encoder?: string | null;
    gpu?: string | null;
  } | null;
  audio_channels?: number | null;
  disconnects: number;
  reconnects: number;
  events: { at_ms: number; kind: "disconnected" | "reconnected" | "stopped" | string; detail?: string | null }[];
  stop?: { reason: string; message: string; detail?: string | null; hint?: string | null; unexpected: boolean } | null;
  performance?: PerformanceSummary | null;
}

export interface PerformanceSummary {
  windows: number;
  streaming_ms: number;
  frames: number;
  fps_avg: number;
  fps_min: number;
  encoded_bitrate_avg_bps: number;
  wire_bitrate_avg_bps: number;
  stale_frames_dropped: number;
  failed_packets: number;
  discarded_packets: number;
  stages: { id: string; label: string; avg_us: number; p95_max_us: number }[];
  gpu_busy_avg_percent?: number | null;
  encoder_busy_avg_percent?: number | null;
  gpu_temperature_max_c?: number | null;
  findings: { id: string; severity: string; title: string; windows: number }[];
}

export interface HistoryPage {
  available: boolean;
  error?: string | null;
  retention_days: number;
  max_sessions: number;
  total: number;
  entries: HistoryEntry[];
}

// Release notifications (`/api/v1/updates`).

export interface UpdateStatus {
  current_version: string;
  automatic: boolean;
  channel: "stable" | "prerelease";
  checking: boolean;
  last_checked_ms?: number | null;
  last_error?: string | null;
  latest?: { version: string; prerelease: boolean; published_at?: string | null; url: string } | null;
  update_available: boolean;
  dismissed: boolean;
  install: { kind: string; label: string; steps: string[] };
}

/// `GetLibrary` / `daemon://library`: the application catalog.
export interface LibrarySnapshot {
  generation: number;
  scanning: boolean;
  auto_refresh: boolean;
  poll_interval_secs: number;
  last_refresh_ms: number | null;
  notice: string | null;
  sources: LibrarySource[];
  applications: LibraryApplication[];
}

export type SourceStatus = "pending" | "ok" | "partial" | "failed";

export interface LibrarySource {
  scanner: number;
  kind: string;
  label: string;
  root: string;
  status: SourceStatus;
  message: string | null;
  hint: string | null;
  applications: number;
  stale_applications: number;
  last_scan_ms: number | null;
  duration_ms: number | null;
  watched: boolean;
}

export type ApplicationState = "available" | "stale" | "hidden" | "shadowed" | "duplicate";

export interface LibraryApplication {
  key: string;
  id: number;
  title: string;
  source: string;
  source_label: string;
  manual: boolean;
  scanner: number | null;
  native_id: string | null;
  state: ApplicationState;
  offered: boolean;
  state_detail: string | null;
  can_hide: boolean;
  artwork: string | null;
  command: string[];
}
