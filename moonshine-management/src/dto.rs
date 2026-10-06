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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDetails {
	pub channels: u8,
	pub channel_mask: u32,
	pub high_quality: bool,
	pub opus_bitrate_bps: u32,
	pub packet_duration_ms: u32,
	pub encrypted: bool,
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
	/// Whether teardown completed (false when it exceeded its deadline).
	pub completed: bool,
	pub at_ms: u64,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
	pub name: String,
	/// `passed`, `warning` or `failed`.
	pub outcome: String,
	pub message: String,
	pub duration_ms: u64,
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

/// Notable backend event, sent by `ServerEvent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEvent {
	/// `info`, `warning` or `error`.
	pub level: String,
	pub code: String,
	pub message: String,
	pub at_ms: u64,
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn session_fixture_preserves_launch_identity_and_round_trips() {
		let fixture: serde_json::Value =
			serde_json::from_str(include_str!("../../pyroshine-ui/src/api/session.fixture.json")).unwrap();
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
