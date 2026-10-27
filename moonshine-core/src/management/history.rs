//! Persistent session history.
//!
//! Every session is recorded with what was negotiated, its disconnects and
//! resumes, how it ended and a summary of the host's statistics windows. Only
//! host-side measurements and sanitized identities are kept: no keys,
//! credentials, PINs or packet contents. The client's address (and the
//! operator's name for it) is recorded unless `[history]
//! record_client_address` is off.
//!
//! Recording never touches a media path and never waits for storage:
//!
//! - The [`Recorder`] follows the management session snapshots and statistics
//!   windows in memory. It produces a row write when a session opens, when
//!   its client disconnects or resumes, and when it ends.
//! - Writes go to one writer thread over a bounded channel with `try_send`;
//!   when the thread is behind (a slow disk), the write is dropped and
//!   counted, the stream is unaffected.
//! - The writer owns the SQLite database (`history.sqlite3` beside
//!   `state.toml`). It applies retention after every write (age and count),
//!   migrates by `user_version`, and replaces a database that fails its
//!   integrity check (the damaged file is kept beside it). When storage fails
//!   (read-only, full disk), the error is reported to the web interface and
//!   recording continues to be attempted; history never stops a session.
//!
//! A session that was still open when Pyroshine stopped is closed at the
//! next start with the reason `interrupted`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

use moonshine_management::ManagementError;
use moonshine_management::dto::{
	FindingCount, HistoryEntry, HistoryEvent, HistoryMode, HistoryPage, HistoryStop, HistoryVideo, PerformanceSummary,
	SessionDetails, SessionPhase, SessionSnapshot, StageSummary, StreamStats,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::telemetry::unix_millis;

/// `[history]`: the session history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HistoryConfig {
	/// Record finished sessions.
	pub enabled: bool,
	/// Sessions older than this are removed.
	pub retention_days: u32,
	/// At most this many sessions are kept; the oldest are removed first.
	pub max_sessions: u32,
	/// Record the client's address (and the name you gave that client).
	pub record_client_address: bool,
}

impl Default for HistoryConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			retention_days: 90,
			max_sessions: 1000,
			record_client_address: true,
		}
	}
}

pub const RETENTION_DAYS: std::ops::RangeInclusive<u32> = 1..=3650;
pub const MAX_SESSIONS: std::ops::RangeInclusive<u32> = 10..=100_000;

impl HistoryConfig {
	pub(crate) fn validate(&self) -> Result<(), String> {
		if !RETENTION_DAYS.contains(&self.retention_days) {
			return Err(format!(
				"history.retention_days must be between {} and {}, not {}",
				RETENTION_DAYS.start(),
				RETENTION_DAYS.end(),
				self.retention_days
			));
		}
		if !MAX_SESSIONS.contains(&self.max_sessions) {
			return Err(format!(
				"history.max_sessions must be between {} and {}, not {}",
				MAX_SESSIONS.start(),
				MAX_SESSIONS.end(),
				self.max_sessions
			));
		}
		Ok(())
	}
}

/// Where the database lives: beside the pairing state.
pub(crate) fn history_path(state_path: &Path) -> PathBuf {
	crate::durable::directory(state_path).join("history.sqlite3")
}

/// Writes waiting for the writer thread before new ones are dropped.
const QUEUE: usize = 64;
/// Events kept per session.
const MAX_EVENTS: usize = 32;
/// Findings kept per session summary.
const MAX_FINDINGS: usize = 8;
/// Longest a page query may take before the web interface reports it.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA_VERSION: i32 = 1;

/// A row write produced by the [`Recorder`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Write {
	/// Identifies the session's row across writes.
	pub key: String,
	pub entry: HistoryEntry,
	/// The session ended with this write.
	pub finished: bool,
}

#[derive(Default)]
struct StageTotal {
	label: String,
	weighted_us: f64,
	frames: u64,
	p95_max_us: f64,
}

/// Running totals of a session's statistics windows.
#[derive(Default)]
struct Accumulator {
	windows: u32,
	streaming_ms: u64,
	frames: u64,
	fps_min: Option<f64>,
	encoded_bits: f64,
	wire_bits: f64,
	stale: u64,
	failed: u64,
	discarded: u64,
	stages: BTreeMap<String, StageTotal>,
	gpu_busy: (f64, u32),
	encoder_busy: (f64, u32),
	temperature_max: Option<f64>,
	findings: BTreeMap<String, FindingCount>,
}

impl Accumulator {
	fn add(&mut self, stats: &StreamStats) {
		self.windows += 1;
		self.streaming_ms += stats.window_ms;
		self.frames += stats.frames;
		self.fps_min = Some(self.fps_min.map_or(stats.fps, |min| min.min(stats.fps)));
		let seconds = stats.window_ms as f64 / 1000.0;
		self.encoded_bits += stats.encoded_bitrate_bps * seconds;
		self.wire_bits += stats.wire_bitrate_bps * seconds;
		self.stale += stats.stale_frames_dropped;
		self.failed += stats.failed_packets;
		self.discarded += stats.discarded_packets;
		for stage in &stats.stages {
			let total = self.stages.entry(stage.id.clone()).or_default();
			total.label.clone_from(&stage.label);
			total.weighted_us += stage.avg_us * stats.frames as f64;
			total.frames += stats.frames;
			total.p95_max_us = total.p95_max_us.max(stage.p95_us);
		}
		if let Some(gpu) = &stats.gpu {
			if let Some(busy) = gpu.busy_percent {
				self.gpu_busy.0 += busy;
				self.gpu_busy.1 += 1;
			}
			if let Some(busy) = gpu.encoder_busy_percent {
				self.encoder_busy.0 += busy;
				self.encoder_busy.1 += 1;
			}
			if let Some(temperature) = gpu.temperature_c {
				self.temperature_max = Some(self.temperature_max.map_or(temperature, |max| max.max(temperature)));
			}
		}
		for finding in &stats.findings {
			let count = self.findings.entry(finding.id.clone()).or_insert_with(|| FindingCount {
				id: finding.id.clone(),
				severity: finding.severity.clone(),
				title: finding.title.clone(),
				windows: 0,
			});
			count.windows += 1;
		}
	}

	fn summary(&self) -> Option<PerformanceSummary> {
		if self.windows == 0 || self.streaming_ms == 0 {
			return None;
		}
		let seconds = self.streaming_ms as f64 / 1000.0;
		let average = |(sum, count): (f64, u32)| (count > 0).then(|| sum / f64::from(count));
		let mut findings: Vec<_> = self.findings.values().cloned().collect();
		findings.sort_by(|a, b| b.windows.cmp(&a.windows).then_with(|| a.id.cmp(&b.id)));
		findings.truncate(MAX_FINDINGS);
		Some(PerformanceSummary {
			windows: self.windows,
			streaming_ms: self.streaming_ms,
			frames: self.frames,
			fps_avg: self.frames as f64 / seconds,
			fps_min: self.fps_min.unwrap_or_default(),
			encoded_bitrate_avg_bps: self.encoded_bits / seconds,
			wire_bitrate_avg_bps: self.wire_bits / seconds,
			stale_frames_dropped: self.stale,
			failed_packets: self.failed,
			discarded_packets: self.discarded,
			stages: self
				.stages
				.iter()
				.filter(|(_, total)| total.frames > 0)
				.map(|(id, total)| StageSummary {
					id: id.clone(),
					label: total.label.clone(),
					avg_us: total.weighted_us / total.frames as f64,
					p95_max_us: total.p95_max_us,
				})
				.collect(),
			gpu_busy_avg_percent: average(self.gpu_busy),
			encoder_busy_avg_percent: average(self.encoder_busy),
			gpu_temperature_max_c: self.temperature_max,
			findings,
		})
	}
}

struct Open {
	key: String,
	entry: HistoryEntry,
	phase: SessionPhase,
	stats: Accumulator,
}

/// Follows session snapshots and statistics; see the module documentation.
pub(crate) struct Recorder {
	open: Option<Open>,
	record_address: bool,
}

fn push_event(entry: &mut HistoryEntry, at_ms: u64, kind: &str, detail: Option<String>) {
	if entry.events.len() < MAX_EVENTS {
		entry.events.push(HistoryEvent {
			at_ms,
			kind: kind.into(),
			detail,
		});
	}
}

impl Recorder {
	pub(crate) fn new(record_address: bool) -> Self {
		Self {
			open: None,
			record_address,
		}
	}

	fn update(&mut self, details: &SessionDetails, label: Option<String>) {
		let open = self.open.as_mut().expect("open session");
		let entry = &mut open.entry;
		entry.application.clone_from(&details.application.title);
		entry.foreground_application = details
			.foreground_application
			.as_ref()
			.map(|foreground| foreground.title.clone())
			.filter(|title| *title != details.application.title)
			.or(entry.foreground_application.take());
		if self.record_address {
			entry.client_address = Some(details.client_address.clone());
			if label.is_some() {
				entry.client_label = label;
			}
		}
		entry.requested = Some(HistoryMode {
			width: details.requested.width,
			height: details.requested.height,
			refresh_rate: details.requested.refresh_rate,
			hdr: details.requested.hdr,
		});
		if let Some(video) = &details.video {
			entry.video = Some(HistoryVideo {
				codec: video.codec.clone(),
				codec_label: video.codec_label.clone(),
				chroma: video.chroma.clone(),
				bit_depth: video.bit_depth,
				hdr: video.dynamic_range != "SDR",
				width: video.width,
				height: video.height,
				fps: video.fps,
				bitrate_bps: video.bitrate_bps,
				encoder: video.encoder.as_ref().map(|encoder| encoder.backend_label.clone()),
				gpu: video.encoder.as_ref().map(|encoder| encoder.gpu.clone()),
			});
		}
		if let Some(audio) = &details.audio {
			entry.audio_channels = Some(audio.channels);
		}
	}

	fn finish(&mut self, snapshot: &SessionSnapshot, now_ms: u64) -> Option<Write> {
		let mut open = self.open.take()?;
		let stop = snapshot
			.last_stop
			.as_ref()
			.filter(|stop| stop.epoch == open.entry.epoch);
		let ended = stop.map_or(now_ms, |stop| stop.at_ms);
		open.entry.ended_at_ms = Some(ended.max(open.entry.started_at_ms));
		open.entry.stop = Some(match stop {
			Some(stop) => HistoryStop {
				reason: stop.reason.clone(),
				message: stop.message.clone(),
				detail: stop.detail.clone(),
				hint: stop.hint.clone(),
				unexpected: stop.unexpected,
			},
			None => HistoryStop {
				reason: "unknown".into(),
				message: "The session ended without a reported reason.".into(),
				detail: None,
				hint: None,
				unexpected: false,
			},
		});
		let detail = open.entry.stop.as_ref().map(|stop| stop.message.clone());
		push_event(&mut open.entry, ended, "stopped", detail);
		open.entry.performance = open.stats.summary();
		Some(Write {
			key: open.key,
			entry: open.entry,
			finished: true,
		})
	}

	/// Follow a session snapshot. `label` names the client at an address.
	pub(crate) fn observe(
		&mut self,
		snapshot: &SessionSnapshot,
		now_ms: u64,
		label: impl Fn(&str) -> Option<String>,
	) -> Vec<Write> {
		let mut writes = Vec::new();
		match &snapshot.session {
			Some(details) => {
				if self.open.as_ref().is_some_and(|open| open.entry.epoch != details.epoch) {
					writes.extend(self.finish(snapshot, now_ms));
				}
				let opened = self.open.is_none();
				if opened {
					self.open = Some(Open {
						key: format!("{}-{}", details.started_at_ms, details.epoch),
						entry: HistoryEntry {
							id: 0,
							epoch: details.epoch,
							application: details.application.title.clone(),
							foreground_application: None,
							client_address: None,
							client_label: None,
							started_at_ms: details.started_at_ms,
							ended_at_ms: None,
							requested: None,
							video: None,
							audio_channels: None,
							disconnects: 0,
							reconnects: 0,
							events: Vec::new(),
							stop: None,
							performance: None,
						},
						phase: snapshot.phase,
						stats: Accumulator::default(),
					});
				}
				let label = self.record_address.then(|| label(&details.client_address)).flatten();
				self.update(details, label);
				let open = self.open.as_mut().expect("open session");
				let previous = std::mem::replace(&mut open.phase, snapshot.phase);
				let mut changed = opened;
				match (previous, snapshot.phase) {
					(SessionPhase::Streaming, SessionPhase::ClientDisconnected) => {
						open.entry.disconnects += 1;
						push_event(&mut open.entry, now_ms, "disconnected", None);
						changed = true;
					},
					(SessionPhase::ClientDisconnected | SessionPhase::Reconnecting, SessionPhase::Streaming) => {
						open.entry.reconnects += 1;
						push_event(&mut open.entry, now_ms, "reconnected", None);
						changed = true;
					},
					// The negotiated mode arrives with the first stream.
					(SessionPhase::Starting, SessionPhase::Streaming) => changed = true,
					_ => {},
				}
				if changed {
					writes.push(Write {
						key: open.key.clone(),
						entry: open.entry.clone(),
						finished: false,
					});
				}
			},
			// Teardown in progress: the stop arrives with the next snapshot.
			None if snapshot.phase == SessionPhase::Stopping => {},
			None => writes.extend(self.finish(snapshot, now_ms)),
		}
		writes
	}

	pub(crate) fn observe_stats(&mut self, stats: &StreamStats) {
		if let Some(open) = self.open.as_mut().filter(|open| open.entry.epoch == stats.epoch) {
			open.stats.add(stats);
		}
	}
}

/// The SQLite database. Used only by the writer thread (and tests).
struct Store {
	connection: Connection,
	retention: Duration,
	max_sessions: u32,
}

fn corrupt(error: &rusqlite::Error) -> bool {
	matches!(
		error.sqlite_error_code(),
		Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
	)
}

impl Store {
	/// Open (creating or migrating) the database at `path`. A database that
	/// fails its integrity check is moved aside and replaced.
	fn open(path: &Path, config: &HistoryConfig, now: SystemTime) -> Result<Self, String> {
		match Self::open_checked(path, config) {
			Ok(store) => Ok(store),
			Err(error) if error.1 => {
				let aside = path.with_extension(format!(
					"sqlite3.corrupt-{}",
					now.duration_since(SystemTime::UNIX_EPOCH)
						.map_or(0, |elapsed| elapsed.as_secs())
				));
				tracing::warn!(
					path = %path.display(),
					aside = %aside.display(),
					"The session history is damaged ({}); starting a new one and keeping the old file",
					error.0
				);
				std::fs::rename(path, &aside)
					.map_err(|error| format!("Cannot move the damaged history aside: {error}"))?;
				for suffix in ["-wal", "-shm"] {
					let mut side = path.as_os_str().to_owned();
					side.push(suffix);
					let _ = std::fs::remove_file(PathBuf::from(side));
				}
				Self::open_checked(path, config).map_err(|error| error.0)
			},
			Err(error) => Err(error.0),
		}
	}

	/// The error and whether it means the file is damaged.
	fn open_checked(path: &Path, config: &HistoryConfig) -> Result<Self, (String, bool)> {
		let fail = |error: rusqlite::Error| {
			let damaged = corrupt(&error);
			(error.to_string(), damaged)
		};
		crate::durable::create_directories(crate::durable::directory(path))
			.map_err(|error| (format!("Cannot create {}: {error}", path.display()), false))?;
		let connection = Connection::open(path).map_err(fail)?;
		// Private like the rest of the state directory.
		use std::os::unix::fs::PermissionsExt;
		let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
		connection.busy_timeout(Duration::from_secs(2)).map_err(fail)?;
		let check: String = connection
			.query_row("PRAGMA quick_check", [], |row| row.get(0))
			.map_err(fail)?;
		if check != "ok" {
			return Err((format!("integrity check failed: {check}"), true));
		}
		let version: i32 = connection
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.map_err(fail)?;
		if version > SCHEMA_VERSION {
			return Err((
				format!("it was written by a newer Pyroshine (schema {version}); update Pyroshine"),
				false,
			));
		}
		if version < 1 {
			connection
				.execute_batch(
					"BEGIN;
					 CREATE TABLE IF NOT EXISTS sessions (
						id INTEGER PRIMARY KEY AUTOINCREMENT,
						session_key TEXT NOT NULL UNIQUE,
						started_at_ms INTEGER NOT NULL,
						ended_at_ms INTEGER,
						record TEXT NOT NULL
					 );
					 CREATE INDEX IF NOT EXISTS sessions_started ON sessions(started_at_ms);
					 PRAGMA user_version = 1;
					 COMMIT;",
				)
				.map_err(fail)?;
		}
		let _ = connection.pragma_update(None, "journal_mode", "WAL");
		let _ = connection.pragma_update(None, "synchronous", "NORMAL");
		Ok(Self {
			connection,
			retention: Duration::from_secs(u64::from(config.retention_days) * 86_400),
			max_sessions: config.max_sessions,
		})
	}

	/// Close sessions left open by a previous run.
	fn close_interrupted(&self) -> rusqlite::Result<usize> {
		let mut statement = self
			.connection
			.prepare("SELECT session_key, record FROM sessions WHERE ended_at_ms IS NULL")?;
		let open: Vec<(String, String)> = statement
			.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
			.collect::<rusqlite::Result<_>>()?;
		for (key, record) in &open {
			let Ok(mut entry) = serde_json::from_str::<HistoryEntry>(record) else {
				continue;
			};
			let ended = entry
				.events
				.last()
				.map_or(entry.started_at_ms, |event| event.at_ms)
				.max(entry.started_at_ms);
			entry.ended_at_ms = Some(ended);
			entry.stop = Some(HistoryStop {
				reason: "interrupted".into(),
				message: "Pyroshine stopped before the session ended; its end time is the last recorded event.".into(),
				detail: None,
				hint: None,
				unexpected: true,
			});
			self.write(&Write {
				key: key.clone(),
				entry,
				finished: true,
			})?;
		}
		Ok(open.len())
	}

	fn write(&self, write: &Write) -> rusqlite::Result<()> {
		let record = serde_json::to_string(&write.entry).expect("history entries serialize");
		self.connection.execute(
			"INSERT INTO sessions (session_key, started_at_ms, ended_at_ms, record) VALUES (?1, ?2, ?3, ?4)
			 ON CONFLICT(session_key) DO UPDATE SET ended_at_ms = excluded.ended_at_ms, record = excluded.record",
			params![
				write.key,
				write.entry.started_at_ms as i64,
				write.entry.ended_at_ms.map(|at| at as i64),
				record
			],
		)?;
		Ok(())
	}

	fn retain(&self, now: SystemTime) -> rusqlite::Result<()> {
		let cutoff = unix_millis(now).saturating_sub(self.retention.as_millis() as u64) as i64;
		self.connection
			.execute("DELETE FROM sessions WHERE started_at_ms < ?1", params![cutoff])?;
		self.connection.execute(
			"DELETE FROM sessions WHERE id NOT IN (SELECT id FROM sessions ORDER BY started_at_ms DESC, id DESC LIMIT ?1)",
			params![i64::from(self.max_sessions)],
		)?;
		Ok(())
	}

	fn entry(&self, id: i64) -> rusqlite::Result<Option<HistoryEntry>> {
		self.connection
			.query_row("SELECT id, record FROM sessions WHERE id = ?1", params![id], |row| {
				Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
			})
			.optional()
			.map(|row| row.and_then(|(id, record)| decode(id, &record)))
	}

	fn page(&self, limit: u32, before: Option<i64>) -> rusqlite::Result<(u64, Vec<HistoryEntry>)> {
		let total: i64 = self
			.connection
			.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
		let mut statement = self.connection.prepare(
			"SELECT id, record FROM sessions WHERE (?1 IS NULL OR id < ?1) ORDER BY started_at_ms DESC, id DESC LIMIT ?2",
		)?;
		let entries = statement
			.query_map(params![before, i64::from(limit)], |row| {
				Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
			})?
			.filter_map(|row| row.ok().and_then(|(id, record)| decode(id, &record)))
			.collect();
		Ok((total.max(0) as u64, entries))
	}

	fn clear(&self) -> rusqlite::Result<()> {
		self.connection.execute("DELETE FROM sessions", [])?;
		Ok(())
	}
}

/// A stored record; unreadable ones are skipped rather than failing a page.
fn decode(id: i64, record: &str) -> Option<HistoryEntry> {
	serde_json::from_str::<HistoryEntry>(record)
		.ok()
		.map(|entry| HistoryEntry { id, ..entry })
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

enum Command {
	Write(Box<Write>),
	Page {
		limit: u32,
		before: Option<i64>,
		reply: Reply<(u64, Vec<HistoryEntry>)>,
	},
	Entry {
		id: i64,
		reply: Reply<Option<HistoryEntry>>,
	},
	Clear {
		reply: Reply<()>,
	},
}

struct Shared {
	/// Why history is unavailable, or the last storage failure.
	error: Mutex<Option<String>>,
	/// Whether the database opened.
	available: std::sync::atomic::AtomicBool,
	dropped: AtomicU64,
}

/// Handle to the history: recording and queries.
pub(crate) struct History {
	config: HistoryConfig,
	recorder: Mutex<Recorder>,
	commands: Option<mpsc::SyncSender<Command>>,
	shared: Arc<Shared>,
}

impl History {
	/// Start the writer thread on `path`. `on_saved` receives every finished
	/// session after it is stored.
	pub(crate) fn start(
		path: PathBuf,
		config: &HistoryConfig,
		on_saved: impl Fn(&HistoryEntry) + Send + 'static,
	) -> Self {
		let shared = Arc::new(Shared {
			error: Mutex::new(None),
			available: false.into(),
			dropped: AtomicU64::new(0),
		});
		if !config.enabled {
			*shared.error.lock().unwrap() = Some("Session history is turned off ([history] enabled = false).".into());
			return Self {
				config: config.clone(),
				recorder: Mutex::new(Recorder::new(config.record_client_address)),
				commands: None,
				shared,
			};
		}
		let (sender, receiver) = mpsc::sync_channel(QUEUE);
		let thread_shared = shared.clone();
		let thread_config = config.clone();
		let spawned = std::thread::Builder::new()
			.name("session-history".into())
			.spawn(move || writer(path, thread_config, receiver, thread_shared, on_saved));
		let commands = match spawned {
			Ok(_) => Some(sender),
			Err(error) => {
				*shared.error.lock().unwrap() = Some(format!("The history writer could not start: {error}"));
				None
			},
		};
		Self {
			config: config.clone(),
			recorder: Mutex::new(Recorder::new(config.record_client_address)),
			commands,
			shared,
		}
	}

	fn submit(&self, writes: Vec<Write>) {
		let Some(commands) = &self.commands else { return };
		for write in writes {
			if commands.try_send(Command::Write(Box::new(write))).is_err() {
				let dropped = self.shared.dropped.fetch_add(1, Ordering::Relaxed) + 1;
				tracing::warn!(
					dropped,
					"The session history writer is behind; a history update was dropped"
				);
			}
		}
	}

	pub(crate) fn observe(&self, snapshot: &SessionSnapshot, label: impl Fn(&str) -> Option<String>) {
		if self.commands.is_none() {
			return;
		}
		let writes = self
			.recorder
			.lock()
			.unwrap()
			.observe(snapshot, unix_millis(SystemTime::now()), label);
		self.submit(writes);
	}

	pub(crate) fn observe_stats(&self, stats: &StreamStats) {
		if self.commands.is_some() {
			self.recorder.lock().unwrap().observe_stats(stats);
		}
	}

	async fn ask<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, ManagementError> {
		let commands = self
			.commands
			.as_ref()
			.ok_or_else(|| ManagementError::Failed(self.error().unwrap_or_default()))?;
		let (reply, response) = oneshot::channel();
		commands
			.try_send(command(reply))
			.map_err(|_| ManagementError::Failed("The session history is busy; try again.".into()))?;
		match tokio::time::timeout(QUERY_TIMEOUT, response).await {
			Ok(Ok(result)) => result.map_err(ManagementError::Failed),
			Ok(Err(_)) => Err(ManagementError::Failed(
				"The session history stopped; see the Pyroshine log.".into(),
			)),
			Err(_) => Err(ManagementError::Failed(
				"The session history did not answer in time.".into(),
			)),
		}
	}

	fn error(&self) -> Option<String> {
		self.shared.error.lock().unwrap().clone()
	}

	pub(crate) async fn page(&self, limit: u32, before: Option<i64>) -> HistoryPage {
		let limit = limit.clamp(1, 200);
		let base = HistoryPage {
			available: self.shared.available.load(Ordering::Acquire),
			error: self.error(),
			retention_days: self.config.retention_days,
			max_sessions: self.config.max_sessions,
			total: 0,
			entries: Vec::new(),
		};
		if !base.available {
			return base;
		}
		match self.ask(|reply| Command::Page { limit, before, reply }).await {
			Ok((total, entries)) => HistoryPage { total, entries, ..base },
			Err(error) => HistoryPage {
				error: Some(error.message()),
				..base
			},
		}
	}

	pub(crate) async fn entry(&self, id: i64) -> Result<HistoryEntry, ManagementError> {
		self.ask(|reply| Command::Entry { id, reply })
			.await?
			.ok_or_else(|| ManagementError::NotFound("This session is no longer in the history.".into()))
	}

	pub(crate) async fn clear(&self) -> Result<(), ManagementError> {
		self.ask(|reply| Command::Clear { reply }).await
	}
}

fn unavailable<T>() -> Result<T, String> {
	Err("Session history is unavailable; see the Pyroshine log.".to_string())
}

fn writer(
	path: PathBuf,
	config: HistoryConfig,
	commands: mpsc::Receiver<Command>,
	shared: Arc<Shared>,
	on_saved: impl Fn(&HistoryEntry),
) {
	let set_error = |error: Option<String>| *shared.error.lock().unwrap() = error;
	let mut store = match Store::open(&path, &config, SystemTime::now()) {
		Ok(store) => store,
		Err(error) => {
			tracing::warn!(path = %path.display(), "Session history unavailable: {error}");
			set_error(Some(format!("Session history unavailable: {error}")));
			// Keep answering, so callers get the error instead of a timeout.
			for command in commands {
				match command {
					Command::Write(_) => {},
					Command::Page { reply, .. } => drop(reply.send(unavailable())),
					Command::Entry { reply, .. } => drop(reply.send(unavailable())),
					Command::Clear { reply } => drop(reply.send(unavailable())),
				}
			}
			return;
		},
	};
	shared.available.store(true, Ordering::Release);
	match store.close_interrupted() {
		Ok(0) => {},
		Ok(count) => tracing::info!(count, "Closed sessions that Pyroshine stopped during"),
		Err(error) => tracing::warn!("Cannot close interrupted history sessions: {error}"),
	}
	let _ = store.retain(SystemTime::now());
	let mut failing = false;
	for command in commands {
		match command {
			Command::Write(write) => {
				let result = store.write(&write).and_then(|()| store.retain(SystemTime::now()));
				match result {
					Ok(()) => {
						if failing {
							tracing::info!("Session history writes work again");
							failing = false;
							set_error(None);
						}
						if write.finished {
							// Report the stored row (with its identifier).
							let stored = store
								.connection
								.query_row(
									"SELECT id FROM sessions WHERE session_key = ?1",
									params![write.key],
									|row| row.get::<_, i64>(0),
								)
								.ok();
							if let Some(id) = stored {
								on_saved(&HistoryEntry {
									id,
									..write.entry.clone()
								});
							}
						}
					},
					Err(error) => {
						if !failing {
							tracing::warn!(path = %path.display(), "Cannot write the session history: {error}");
						}
						failing = true;
						set_error(Some(format!("The last session could not be saved: {error}")));
						if corrupt(&error) {
							match Store::open(&path, &config, SystemTime::now()) {
								Ok(replacement) => {
									store = replacement;
									let _ = store.write(&write);
								},
								Err(error) => tracing::warn!("Cannot recreate the session history: {error}"),
							}
						}
					},
				}
			},
			Command::Page { limit, before, reply } => {
				let _ = reply.send(store.page(limit, before).map_err(|error| error.to_string()));
			},
			Command::Entry { id, reply } => {
				let _ = reply.send(store.entry(id).map_err(|error| error.to_string()));
			},
			Command::Clear { reply } => {
				let _ = reply.send(store.clear().map_err(|error| error.to_string()));
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use moonshine_management::dto::{
		ApplicationSummary, EncoderDetails, Finding, GpuStats, LastStop, RequestedMode, StageStats, VideoDetails,
	};

	use super::*;

	fn details(epoch: u64, video: bool) -> SessionDetails {
		SessionDetails {
			epoch,
			application: ApplicationSummary {
				id: 7,
				title: "Steam".into(),
			},
			foreground_application: None,
			steam_game: None,
			capture: None,
			client_address: "192.168.1.50".into(),
			started_at_ms: 1_000,
			requested: RequestedMode {
				width: 1920,
				height: 1080,
				refresh_rate: 60,
				hdr: false,
				audio_channels: 2,
				audio_channel_mask: 3,
			},
			video: video.then(|| VideoDetails {
				codec: "hevc".into(),
				codec_label: "HEVC".into(),
				chroma: "4:2:0".into(),
				bit_depth: 10,
				dynamic_range: "HDR10".into(),
				transfer: "PQ".into(),
				primaries: "BT.2020".into(),
				matrix: "BT.2020 NCL".into(),
				range: "limited".into(),
				width: 1920,
				height: 1080,
				fps: 60,
				bitrate_bps: 20_000_000,
				packet_size: 1024,
				encrypted: true,
				minimum_fec_packets: 2,
				max_reference_frames: 1,
				pyrowave_dialect: None,
				encoder: Some(EncoderDetails {
					backend: "vulkan_video".into(),
					backend_label: "Vulkan Video".into(),
					gpu: "Test GPU".into(),
				}),
			}),
			audio: None,
		}
	}

	fn snapshot(phase: SessionPhase, session: Option<SessionDetails>, stop: Option<LastStop>) -> SessionSnapshot {
		SessionSnapshot {
			phase,
			session,
			last_stop: stop,
		}
	}

	fn stop(epoch: u64) -> LastStop {
		LastStop {
			epoch,
			reason: "user_stopped".into(),
			message: "The client ended the session.".into(),
			unexpected: false,
			completed: true,
			at_ms: 90_000,
			detail: None,
			hint: None,
		}
	}

	fn stats(epoch: u64, fps: f64) -> StreamStats {
		StreamStats {
			epoch,
			at_ms: 0,
			window_ms: 1000,
			frames: fps as u64,
			fps,
			key_frames: 0,
			encoded_bitrate_bps: 10_000_000.0,
			wire_bitrate_bps: 12_000_000.0,
			transport_overhead_percent: None,
			packets: 100,
			failed_packets: 1,
			discarded_packets: 0,
			stale_frames_dropped: 2,
			samples_dropped: 0,
			stages: vec![StageStats {
				id: "encode".into(),
				label: "Encode".into(),
				avg_us: if fps > 50.0 { 2000.0 } else { 4000.0 },
				p50_us: 0.0,
				p95_us: if fps > 50.0 { 3000.0 } else { 9000.0 },
				max_us: 0.0,
			}],
			capture: None,
			findings: vec![Finding {
				id: "encoder_slow".into(),
				severity: "warning".into(),
				title: "Encoder is slow".into(),
				detail: String::new(),
				hint: None,
			}],
			gpu: Some(GpuStats {
				driver: "test".into(),
				source: "test".into(),
				busy_percent: Some(50.0),
				encoder_busy_percent: None,
				core_clock_mhz: None,
				memory_clock_mhz: None,
				temperature_c: Some(fps),
				hotspot_c: None,
				power_w: None,
				power_limit_w: None,
				vram_used_bytes: None,
				vram_total_bytes: None,
				processes: Vec::new(),
			}),
		}
	}

	fn no_label(_: &str) -> Option<String> {
		Some("Laptop".into())
	}

	#[test]
	fn a_session_is_recorded_with_reconnects_stop_and_performance() {
		let mut recorder = Recorder::new(true);
		let writes = recorder.observe(
			&snapshot(SessionPhase::Starting, Some(details(3, false)), None),
			1_000,
			no_label,
		);
		assert_eq!(writes.len(), 1, "opening writes the row");
		assert!(!writes[0].finished);
		let writes = recorder.observe(
			&snapshot(SessionPhase::Streaming, Some(details(3, true)), None),
			2_000,
			no_label,
		);
		assert_eq!(writes[0].entry.video.as_ref().unwrap().codec, "hevc");
		assert!(writes[0].entry.video.as_ref().unwrap().hdr);
		recorder.observe_stats(&stats(3, 60.0));
		recorder.observe_stats(&stats(3, 30.0));
		recorder.observe_stats(&stats(4, 1.0)); // another epoch: ignored
		recorder.observe(
			&snapshot(SessionPhase::ClientDisconnected, Some(details(3, true)), None),
			10_000,
			no_label,
		);
		recorder.observe(
			&snapshot(SessionPhase::Reconnecting, Some(details(3, true)), None),
			20_000,
			no_label,
		);
		recorder.observe(
			&snapshot(SessionPhase::Streaming, Some(details(3, true)), None),
			21_000,
			no_label,
		);
		assert!(
			recorder
				.observe(&snapshot(SessionPhase::Stopping, None, None), 89_000, no_label)
				.is_empty()
		);
		let writes = recorder.observe(&snapshot(SessionPhase::Idle, None, Some(stop(3))), 90_500, no_label);
		assert_eq!(writes.len(), 1);
		let entry = &writes[0].entry;
		assert!(writes[0].finished);
		assert_eq!((entry.disconnects, entry.reconnects), (1, 1));
		assert_eq!(entry.ended_at_ms, Some(90_000));
		assert_eq!(entry.stop.as_ref().unwrap().reason, "user_stopped");
		assert_eq!(entry.client_label.as_deref(), Some("Laptop"));
		let kinds: Vec<_> = entry.events.iter().map(|event| event.kind.as_str()).collect();
		assert_eq!(kinds, ["disconnected", "reconnected", "stopped"]);
		let performance = entry.performance.as_ref().unwrap();
		assert_eq!(performance.windows, 2);
		assert!((performance.fps_avg - 45.0).abs() < 1e-9);
		assert_eq!(performance.fps_min, 30.0);
		assert_eq!(performance.stale_frames_dropped, 4);
		let encode = &performance.stages[0];
		assert!((encode.avg_us - (2000.0 * 60.0 + 4000.0 * 30.0) / 90.0).abs() < 1e-6);
		assert_eq!(encode.p95_max_us, 9000.0);
		assert_eq!(performance.gpu_busy_avg_percent, Some(50.0));
		assert_eq!(performance.gpu_temperature_max_c, Some(60.0));
		assert_eq!(performance.findings[0].windows, 2);
	}

	#[test]
	fn client_addresses_are_left_out_when_disabled() {
		let mut recorder = Recorder::new(false);
		recorder.observe(
			&snapshot(SessionPhase::Streaming, Some(details(1, true)), None),
			0,
			no_label,
		);
		let writes = recorder.observe(&snapshot(SessionPhase::Idle, None, None), 5_000, no_label);
		let entry = &writes[0].entry;
		assert_eq!(
			(entry.client_address.as_ref(), entry.client_label.as_ref()),
			(None, None)
		);
		assert_eq!(entry.stop.as_ref().unwrap().reason, "unknown");
		assert!(entry.performance.is_none(), "no statistics are invented");
	}

	#[test]
	fn a_new_epoch_closes_the_previous_session() {
		let mut recorder = Recorder::new(true);
		recorder.observe(
			&snapshot(SessionPhase::Streaming, Some(details(1, true)), None),
			0,
			no_label,
		);
		let writes = recorder.observe(
			&snapshot(SessionPhase::Starting, Some(details(2, false)), Some(stop(1))),
			0,
			no_label,
		);
		assert_eq!(writes.len(), 2);
		assert!(writes[0].finished && writes[0].entry.epoch == 1);
		assert!(!writes[1].finished && writes[1].entry.epoch == 2);
	}

	fn entry(key: u64, started: u64) -> Write {
		let mut recorder = Recorder::new(true);
		let mut session = details(key, true);
		session.started_at_ms = started;
		recorder.observe(
			&snapshot(SessionPhase::Streaming, Some(session), None),
			started,
			no_label,
		);
		let mut writes = recorder.observe(
			&snapshot(
				SessionPhase::Idle,
				None,
				Some(LastStop {
					at_ms: started + 1000,
					..stop(key)
				}),
			),
			started + 1000,
			no_label,
		);
		writes.remove(0)
	}

	#[test]
	fn the_store_keeps_pages_retention_and_interrupted_sessions() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("history.sqlite3");
		let config = HistoryConfig {
			max_sessions: 10,
			retention_days: 1,
			..Default::default()
		};
		let now = SystemTime::now();
		let now_ms = unix_millis(now);
		let store = Store::open(&path, &config, now).unwrap();
		for index in 0..12u64 {
			store.write(&entry(index, now_ms - 10_000 + index)).unwrap();
		}
		// Too old for the retention period.
		store.write(&entry(99, now_ms - 3 * 86_400_000)).unwrap();
		store.retain(now).unwrap();
		let (total, page) = store.page(5, None).unwrap();
		assert_eq!(total, 10);
		assert_eq!(page.len(), 5);
		assert_eq!(page[0].epoch, 11, "newest first");
		let (_, next) = store.page(5, Some(page[4].id)).unwrap();
		assert_eq!(next[0].epoch, 6);
		assert_eq!(store.entry(page[0].id).unwrap().unwrap().epoch, 11);

		// A session still open when the server stopped.
		let mut open = entry(50, now_ms);
		open.entry.ended_at_ms = None;
		open.entry.stop = None;
		open.finished = false;
		store.write(&open).unwrap();
		drop(store);
		let reopened = Store::open(&path, &config, now).unwrap();
		assert_eq!(reopened.close_interrupted().unwrap(), 1);
		let (_, page) = reopened.page(1, None).unwrap();
		assert_eq!(page[0].stop.as_ref().unwrap().reason, "interrupted");
		assert!(page[0].ended_at_ms.is_some());
		use std::os::unix::fs::PermissionsExt;
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
	}

	#[test]
	fn a_damaged_database_is_replaced_and_kept_aside() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("history.sqlite3");
		std::fs::write(&path, b"this is not an sqlite database at all, just text....").unwrap();
		let store = Store::open(&path, &HistoryConfig::default(), SystemTime::now()).unwrap();
		store.write(&entry(1, unix_millis(SystemTime::now()))).unwrap();
		let aside: Vec<_> = std::fs::read_dir(directory.path())
			.unwrap()
			.filter_map(Result::ok)
			.filter(|entry| entry.file_name().to_string_lossy().contains("corrupt"))
			.collect();
		assert_eq!(aside.len(), 1);
	}

	#[tokio::test]
	async fn storage_failures_are_reported_without_blocking() {
		let directory = tempfile::tempdir().unwrap();
		let blocked = directory.path().join("file");
		std::fs::write(&blocked, "a file where a directory is needed").unwrap();
		let history = History::start(blocked.join("history.sqlite3"), &HistoryConfig::default(), |_| {});
		// Recording does not wait for the failed store.
		history.observe(
			&snapshot(SessionPhase::Streaming, Some(details(1, true)), None),
			no_label,
		);
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		while history.error().is_none() {
			assert!(std::time::Instant::now() < deadline);
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		let page = history.page(10, None).await;
		assert!(!page.available);
		assert!(page.error.unwrap().contains("unavailable"));
		assert!(history.entry(1).await.is_err());
	}

	#[tokio::test]
	async fn finished_sessions_are_stored_and_announced() {
		let directory = tempfile::tempdir().unwrap();
		let (sender, mut saved) = tokio::sync::mpsc::unbounded_channel();
		let history = History::start(
			directory.path().join("history.sqlite3"),
			&HistoryConfig::default(),
			move |entry| {
				let _ = sender.send(entry.clone());
			},
		);
		// Recent enough for the retention period.
		let now = unix_millis(SystemTime::now());
		let mut session = details(1, true);
		session.started_at_ms = now - 60_000;
		history.observe(&snapshot(SessionPhase::Streaming, Some(session), None), no_label);
		history.observe_stats(&stats(1, 60.0));
		let ended = LastStop { at_ms: now, ..stop(1) };
		history.observe(&snapshot(SessionPhase::Idle, None, Some(ended)), no_label);
		let entry = tokio::time::timeout(Duration::from_secs(5), saved.recv())
			.await
			.unwrap()
			.unwrap();
		assert!(entry.id > 0);
		let page = history.page(10, None).await;
		assert!(page.available && page.error.is_none());
		assert_eq!(page.entries[0].id, entry.id);
		assert_eq!(history.entry(entry.id).await.unwrap().performance.unwrap().windows, 1);
		history.clear().await.unwrap();
		assert_eq!(history.page(10, None).await.total, 0);
		let disabled = History::start(
			directory.path().join("other.sqlite3"),
			&HistoryConfig {
				enabled: false,
				..Default::default()
			},
			|_| {},
		);
		assert!(!disabled.page(1, None).await.available);
		assert!(!directory.path().join("other.sqlite3").exists());
	}
}
