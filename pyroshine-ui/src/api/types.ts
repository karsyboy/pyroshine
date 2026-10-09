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
  health: {
    all_fatal_passed: boolean;
    checks: { name: string; outcome: "passed" | "warning" | "failed"; message: string; duration_ms: number }[];
  } | null;
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
}

export interface ServerEvent {
  level: "info" | "warning" | "error";
  code: string;
  message: string;
  at_ms: number;
}

export interface AttachError {
  kind: string;
  message: string;
}

export interface Overview {
  connected: boolean;
  /** Set when Pyroshine runs but this app cannot attach to it. */
  attach_error?: AttachError | null;
  server: ServerInfo | null;
  session: SessionSnapshot | null;
  pairing: PairingSnapshot | null;
  clients: ClientsSnapshot | null;
  stats: StreamStats | null;
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

export interface UiError {
  kind: "unavailable" | "access_denied" | "invalid" | "not_found" | "conflict" | "read_only" | "failed" | string;
  message: string;
}
