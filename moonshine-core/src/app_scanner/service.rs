//! The library service: keeps the application catalog current.
//!
//! One worker thread owns everything that changes the catalog: it runs the
//! scanners, follows their metadata (`app_scanner::watch`), reconciles results
//! ([`super::catalog`]) and publishes each new [`CatalogSnapshot`] through a
//! `watch` channel. Readers (`/applist`, `/launch`, the management interface)
//! only clone the current `Arc`; they never wait for a scan, and the worker
//! never touches session state, so a refresh cannot block or disturb a
//! stream. The thread runs at a lower CPU and idle I/O priority, scans one
//! source at a time, and only the sources whose metadata changed.
//!
//! Change detection, per source:
//!
//! - an inotify event on a relevant name marks the source changed; it is
//!   scanned once the burst has been quiet for [`QUIET`] (at most
//!   [`MAX_DELAY`] after the first event) and no sooner than
//!   [`MIN_SCAN_INTERVAL`] after its previous scan;
//! - every `poll_interval_secs`, a stat-only fingerprint of its metadata is
//!   compared, which catches anything inotify missed;
//! - a unit that failed to parse (a launcher rewriting a file) is retried
//!   with backoff ([`RETRY_BACKOFF`]);
//! - `RescanLibrary` scans every source immediately.
//!
//! A unit that cannot be read keeps the entries of its last successful scan
//! (also across restarts, from the retained cache) for up to
//! [`STALE_RETENTION`]; a temporary failure never empties the catalog.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::artwork::ArtworkCache;
use super::catalog::{CatalogSnapshot, SourceInput, UnitInput, reconcile};
use super::icons::IconIndex;
use super::identity::IdentityStore;
use super::watch::{Wake, WatchTarget, Watcher, fingerprint, wait_readable};
use super::{ApplicationScannerConfig, Discovered, ScannerKind, UnitError};
use crate::session::application::ApplicationConfig;

/// Quiet period that ends a burst of metadata changes.
pub const QUIET: Duration = Duration::from_secs(2);
/// Longest a change waits while a burst continues.
pub const MAX_DELAY: Duration = Duration::from_secs(10);
/// Shortest time between change-triggered scans of one source (a download
/// in progress rewrites its Steam manifest repeatedly).
pub const MIN_SCAN_INTERVAL: Duration = Duration::from_secs(15);
/// Retries of a unit that could not be parsed.
pub const RETRY_BACKOFF: [Duration; 5] = [
	Duration::from_secs(2),
	Duration::from_secs(5),
	Duration::from_secs(15),
	Duration::from_secs(30),
	Duration::from_secs(60),
];
/// How long entries of an unreadable unit are still offered.
pub const STALE_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Concurrent box art renderings (decoding is CPU-heavy).
const ARTWORK_RENDERS: usize = 2;

/// Safe domain of `[library].poll_interval_secs`.
pub const POLL_INTERVAL_SECS: std::ops::RangeInclusive<u64> = 30..=86_400;

/// `[library]`: how the application catalog follows launcher changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
	/// Update the catalog when installed games change, without a restart.
	pub auto_refresh: bool,

	/// Seconds between checks for changes that file notifications cannot
	/// report (network file systems, watch limits, libraries appearing).
	pub poll_interval_secs: u64,
}

impl Default for LibraryConfig {
	fn default() -> Self {
		Self {
			auto_refresh: true,
			poll_interval_secs: 300,
		}
	}
}

/// How a source's last scan went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceHealth {
	/// Not scanned yet.
	Pending,
	Ok,
	/// Some units failed; their previous entries are offered.
	Partial,
	/// The source could not be read at all; its previous entries are offered.
	Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceStatus {
	/// Position among the configured `[[application_scanner]]` entries.
	pub scanner: usize,
	pub kind: ScannerKind,
	pub root: String,
	pub health: SourceHealth,
	/// What failed, if anything.
	pub message: Option<String>,
	/// What the operator can do about it.
	pub hint: Option<String>,
	/// Entries the source currently provides (including retained ones).
	pub entries: usize,
	/// Entries offered from an earlier scan because their unit is unreadable.
	pub stale_entries: usize,
	pub last_scan_ms: Option<u64>,
	pub duration_ms: Option<u64>,
	/// Changes are reported by file notifications (otherwise only by the
	/// periodic check).
	pub watched: bool,
	/// Scans since startup.
	pub scans: u64,
}

/// Everything the management interface reports about the library.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryView {
	pub catalog: Arc<CatalogSnapshot>,
	pub sources: Vec<SourceStatus>,
	pub scanning: bool,
	pub last_refresh_ms: Option<u64>,
	pub auto_refresh: bool,
	pub poll_interval_secs: u64,
	/// A problem not tied to one source (identity file not writable).
	pub notice: Option<String>,
}

/// Why a library request failed.
#[derive(Debug, PartialEq, Eq)]
pub enum LibraryError {
	NotFound(String),
	Invalid(String),
	Unavailable(String),
	Failed(String),
}

enum Request {
	Rescan,
	SetHidden {
		key: String,
		hidden: bool,
		reply: tokio::sync::oneshot::Sender<Result<(), LibraryError>>,
	},
	Stop,
}

struct Control {
	requests: crossbeam_channel::Sender<Request>,
	wake: Arc<Wake>,
	stop: Arc<AtomicBool>,
}

impl Drop for Control {
	fn drop(&mut self) {
		// The worker finishes the source it is scanning, then exits. Nothing
		// waits for it: every file it writes is replaced atomically.
		self.stop.store(true, Ordering::Release);
		let _ = self.requests.send(Request::Stop);
		self.wake.wake();
	}
}

/// Change-detection timings (shortened by tests).
#[derive(Clone, Debug)]
pub(crate) struct Timing {
	pub quiet: Duration,
	pub max_delay: Duration,
	pub min_interval: Duration,
	pub retry: Vec<Duration>,
}

impl Default for Timing {
	fn default() -> Self {
		Self {
			quiet: QUIET,
			max_delay: MAX_DELAY,
			min_interval: MIN_SCAN_INTERVAL,
			retry: RETRY_BACKOFF.to_vec(),
		}
	}
}

/// Handle to the catalog. Cheap to clone; reads never block on scanning.
#[derive(Clone)]
pub struct Library {
	view: watch::Receiver<Arc<LibraryView>>,
	control: Option<Arc<Control>>,
	artwork: Arc<ArtworkCache>,
	renders: Arc<tokio::sync::Semaphore>,
}

fn unix_millis() -> u64 {
	SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn initial_view(
	manual: &[ApplicationConfig],
	scanners: &[ApplicationScannerConfig],
	options: &LibraryConfig,
) -> LibraryView {
	LibraryView {
		catalog: Arc::new(CatalogSnapshot::from_applications(manual)),
		sources: scanners
			.iter()
			.enumerate()
			.map(|(scanner, config)| SourceStatus {
				scanner,
				kind: config.kind(),
				root: config.root(),
				health: SourceHealth::Pending,
				message: None,
				hint: None,
				entries: 0,
				stale_entries: 0,
				last_scan_ms: None,
				duration_ms: None,
				watched: false,
				scans: 0,
			})
			.collect(),
		scanning: !scanners.is_empty(),
		last_refresh_ms: None,
		auto_refresh: options.auto_refresh,
		poll_interval_secs: options.poll_interval_secs,
		notice: None,
	}
}

/// Paths the library persists to.
#[derive(Clone, Debug, Default)]
pub struct LibraryPaths {
	/// Recorded IDs and visibility choices (`library.toml` beside `state.toml`).
	pub identity: Option<PathBuf>,
	/// Last successful scan results (regenerable; in the cache directory).
	pub retained: Option<PathBuf>,
	/// Rendered box art.
	pub artwork: Option<PathBuf>,
}

impl LibraryPaths {
	pub fn standard() -> Self {
		Self {
			identity: super::identity::default_path(),
			retained: dirs::cache_dir().map(|cache| cache.join("moonshine").join("library-cache.json")),
			artwork: ArtworkCache::default_directory(),
		}
	}
}

impl Library {
	/// Start the worker. The first catalog with discovered applications is
	/// published when the initial scan finishes ([`Self::ready`]); until then
	/// the catalog holds the manual entries.
	pub fn start(
		manual: Vec<ApplicationConfig>,
		scanners: Vec<ApplicationScannerConfig>,
		options: LibraryConfig,
		paths: LibraryPaths,
	) -> Self {
		Self::start_with(manual, scanners, options, paths, Timing::default())
	}

	pub(crate) fn start_with(
		manual: Vec<ApplicationConfig>,
		scanners: Vec<ApplicationScannerConfig>,
		options: LibraryConfig,
		paths: LibraryPaths,
		timing: Timing,
	) -> Self {
		let (publish, view) = watch::channel(Arc::new(initial_view(&manual, &scanners, &options)));
		let artwork = Arc::new(ArtworkCache::new(paths.artwork.clone()));
		let renders = Arc::new(tokio::sync::Semaphore::new(ARTWORK_RENDERS));
		let wake = match Wake::new() {
			Ok(wake) => Arc::new(wake),
			Err(error) => {
				tracing::error!(
					"Cannot start the application library worker: {error}; the catalog stays as configured"
				);
				return Self {
					view,
					control: None,
					artwork,
					renders,
				};
			},
		};
		let (requests, receiver) = crossbeam_channel::unbounded();
		let stop = Arc::new(AtomicBool::new(false));
		let worker = Worker::new(
			manual,
			scanners,
			options,
			paths,
			publish,
			receiver,
			wake.clone(),
			stop.clone(),
			timing,
		);
		let spawned = std::thread::Builder::new().name("library".into()).spawn(move || {
			lower_priority();
			worker.run();
		});
		if let Err(error) = spawned {
			tracing::error!("Cannot start the application library worker: {error}; the catalog stays as configured");
			return Self {
				view,
				control: None,
				artwork,
				renders,
			};
		}
		Self {
			view,
			control: Some(Arc::new(Control { requests, wake, stop })),
			artwork,
			renders,
		}
	}

	/// A catalog of `applications` that never changes (no worker).
	pub fn fixed(applications: &[ApplicationConfig]) -> Self {
		let mut view = initial_view(applications, &[], &LibraryConfig::default());
		view.scanning = false;
		let (_, receiver) = watch::channel(Arc::new(view));
		Self {
			view: receiver,
			control: None,
			artwork: Arc::new(ArtworkCache::new(None)),
			renders: Arc::new(tokio::sync::Semaphore::new(ARTWORK_RENDERS)),
		}
	}

	/// A library whose views a test publishes directly.
	#[cfg(test)]
	pub(crate) fn for_test(applications: &[ApplicationConfig]) -> (Self, watch::Sender<Arc<LibraryView>>) {
		let mut view = initial_view(applications, &[], &LibraryConfig::default());
		view.scanning = false;
		let (sender, receiver) = watch::channel(Arc::new(view));
		(
			Self {
				view: receiver,
				control: None,
				artwork: Arc::new(ArtworkCache::new(None)),
				renders: Arc::new(tokio::sync::Semaphore::new(ARTWORK_RENDERS)),
			},
			sender,
		)
	}

	/// Wait until the initial scan has been published, or `timeout`.
	/// Returns whether it was.
	pub async fn ready(&self, timeout: Duration) -> bool {
		let mut view = self.view.clone();
		tokio::time::timeout(
			timeout,
			view.wait_for(|view| !view.scanning || view.last_refresh_ms.is_some()),
		)
		.await
		.is_ok_and(|result| result.is_ok())
	}

	/// The current catalog.
	pub fn catalog(&self) -> Arc<CatalogSnapshot> {
		self.view.borrow().catalog.clone()
	}

	pub fn view(&self) -> Arc<LibraryView> {
		self.view.borrow().clone()
	}

	pub fn subscribe(&self) -> watch::Receiver<Arc<LibraryView>> {
		self.view.clone()
	}

	fn control(&self) -> Result<&Control, LibraryError> {
		self.control.as_deref().ok_or_else(|| {
			LibraryError::Unavailable("The application library is not running; restart Pyroshine.".into())
		})
	}

	/// Scan every source now (in the background).
	pub fn rescan(&self) -> Result<(), LibraryError> {
		let control = self.control()?;
		control
			.requests
			.send(Request::Rescan)
			.map_err(|_| LibraryError::Unavailable("The application library stopped.".into()))?;
		control.wake.wake();
		Ok(())
	}

	/// Show or hide a discovered application. The choice is saved and
	/// survives restarts and upgrades.
	pub async fn set_hidden(&self, key: &str, hidden: bool) -> Result<(), LibraryError> {
		let control = self.control()?;
		let (reply, answer) = tokio::sync::oneshot::channel();
		control
			.requests
			.send(Request::SetHidden {
				key: key.to_string(),
				hidden,
				reply,
			})
			.map_err(|_| LibraryError::Unavailable("The application library stopped.".into()))?;
		control.wake.wake();
		answer
			.await
			.map_err(|_| LibraryError::Unavailable("The application library stopped.".into()))?
	}

	/// The Moonlight box art tile for the first usable candidate. Decoding
	/// runs on a blocking thread, at most [`ARTWORK_RENDERS`] at a time.
	pub(crate) async fn render_artwork(&self, candidates: Vec<PathBuf>) -> Result<Vec<u8>, String> {
		let _permit = self.renders.acquire().await.map_err(|error| error.to_string())?;
		let artwork = self.artwork.clone();
		tokio::task::spawn_blocking(move || artwork.render(&candidates).map(|rendered| rendered.into_bytes()))
			.await
			.map_err(|error| error.to_string())?
	}
}

/// Scanning yields to everything else on the host: nice 10 and the idle
/// I/O class for this thread only (Linux priorities are per thread).
fn lower_priority() {
	// SAFETY: plain syscalls on the calling thread's ID; failures only leave
	// the default priority.
	unsafe {
		let tid = libc::gettid();
		libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 10);
		const IOPRIO_WHO_PROCESS: libc::c_long = 1;
		const IOPRIO_CLASS_IDLE: libc::c_long = 3;
		libc::syscall(
			libc::SYS_ioprio_set,
			IOPRIO_WHO_PROCESS,
			tid as libc::c_long,
			IOPRIO_CLASS_IDLE << 13,
		);
	}
}

/// One unit's entries as the catalog uses them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct UnitState {
	key: String,
	entries: Vec<Discovered>,
	/// Since when the entries are retained from an earlier scan.
	#[serde(default)]
	stale_since: Option<u64>,
	#[serde(skip)]
	error: Option<UnitError>,
}

struct Source {
	config: ApplicationScannerConfig,
	digest: String,
	units: Vec<UnitState>,
	status: SourceStatus,
	targets: Vec<WatchTarget>,
	fingerprint: Option<u64>,
	/// First and latest change event since the last scan.
	changed: Option<(Instant, Instant)>,
	forced: bool,
	last_scan: Option<Instant>,
	/// Next retry of a unit that did not parse, and how many were made.
	retry: Option<(Instant, usize)>,
	/// The last scan could not list the source's units at all.
	failure: Option<UnitError>,
}

impl Source {
	/// When this source should be scanned next, if it should.
	fn due(&self, now: Instant, timing: &Timing) -> Option<Instant> {
		if self.forced {
			return Some(now);
		}
		let mut due = None;
		if let Some((first, latest)) = self.changed {
			let mut at = (latest + timing.quiet).min(first + timing.max_delay);
			if let Some(last) = self.last_scan {
				at = at.max(last + timing.min_interval);
			}
			due = Some(at);
		}
		if let Some((at, _)) = self.retry {
			due = Some(due.map_or(at, |due: Instant| due.min(at)));
		}
		due
	}
}

/// Last successful results, kept across restarts in the cache directory.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RetainedFile {
	version: u32,
	sources: Vec<RetainedSource>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RetainedSource {
	scanner: usize,
	digest: String,
	units: Vec<UnitState>,
}

struct Worker {
	manual: Vec<ApplicationConfig>,
	options: LibraryConfig,
	sources: Vec<Source>,
	identity: IdentityStore,
	retained_path: Option<PathBuf>,
	retained: HashMap<usize, RetainedSource>,
	retained_dirty: bool,
	icons: IconIndex,
	watcher: Option<Watcher>,
	wake: Arc<Wake>,
	requests: crossbeam_channel::Receiver<Request>,
	stop: Arc<AtomicBool>,
	timing: Timing,
	publish: watch::Sender<Arc<LibraryView>>,
	last_poll: Instant,
	notice: Option<String>,
	last_refresh_ms: Option<u64>,
}

impl Worker {
	#[allow(clippy::too_many_arguments)]
	fn new(
		manual: Vec<ApplicationConfig>,
		scanners: Vec<ApplicationScannerConfig>,
		options: LibraryConfig,
		paths: LibraryPaths,
		publish: watch::Sender<Arc<LibraryView>>,
		requests: crossbeam_channel::Receiver<Request>,
		wake: Arc<Wake>,
		stop: Arc<AtomicBool>,
		timing: Timing,
	) -> Self {
		let identity = match paths.identity {
			Some(path) => IdentityStore::load(path),
			None => IdentityStore::in_memory(),
		};
		let retained = paths
			.retained
			.as_ref()
			.and_then(|path| std::fs::read(path).ok())
			.and_then(|bytes| serde_json::from_slice::<RetainedFile>(&bytes).ok())
			.filter(|file| file.version == 1)
			.map(|file| {
				file.sources
					.into_iter()
					.map(|source| (source.scanner, source))
					.collect()
			})
			.unwrap_or_default();
		let view = publish.borrow().clone();
		let sources = scanners
			.into_iter()
			.zip(view.sources.iter().cloned())
			.map(|(config, status)| Source {
				digest: config.digest(),
				config,
				units: Vec::new(),
				status,
				targets: Vec::new(),
				fingerprint: None,
				changed: None,
				forced: true,
				last_scan: None,
				retry: None,
				failure: None,
			})
			.collect();
		let watcher = if options.auto_refresh { Watcher::new() } else { None };
		Self {
			manual,
			options,
			sources,
			identity,
			retained_path: paths.retained,
			retained,
			retained_dirty: false,
			icons: IconIndex::new(),
			watcher,
			wake,
			requests,
			stop,
			timing,
			publish,
			last_poll: Instant::now(),
			notice: None,
			last_refresh_ms: None,
		}
	}

	fn run(mut self) {
		let started = Instant::now();
		self.scan_due(Instant::now());
		tracing::info!(
			applications = self.publish.borrow().catalog.offered().count(),
			elapsed_ms = started.elapsed().as_millis() as u64,
			"Application library ready"
		);
		loop {
			let now = Instant::now();
			let poll_at = self
				.options
				.auto_refresh
				.then(|| self.last_poll + Duration::from_secs(self.options.poll_interval_secs));
			let next = self
				.sources
				.iter()
				.filter_map(|source| source.due(now, &self.timing))
				.chain(poll_at)
				.min();
			let timeout = next.map_or(Duration::from_secs(3600), |next| next.saturating_duration_since(now));
			let mut fds = vec![self.wake.raw_fd()];
			if let Some(watcher) = &self.watcher {
				fds.push(watcher.raw_fd());
			}
			let ready = wait_readable(&fds, timeout);
			if ready[0] {
				self.wake.drain();
			}
			loop {
				match self.requests.try_recv() {
					Ok(Request::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => return,
					Ok(Request::Rescan) => {
						tracing::info!("Rescanning the application library at the operator's request");
						self.icons.clear();
						for source in &mut self.sources {
							source.forced = true;
						}
					},
					Ok(Request::SetHidden { key, hidden, reply }) => {
						let _ = reply.send(self.set_hidden(&key, hidden));
					},
					Err(crossbeam_channel::TryRecvError::Empty) => break,
				}
			}
			if ready.get(1).copied().unwrap_or(false) {
				self.file_events(Instant::now());
			}
			if poll_at.is_some_and(|poll_at| Instant::now() >= poll_at) {
				self.poll();
			}
			self.scan_due(Instant::now());
		}
	}

	fn file_events(&mut self, now: Instant) {
		let Some(watcher) = self.watcher.as_mut() else {
			return;
		};
		let changes = watcher.read();
		let mut changed = changes.sources;
		if changes.overflow {
			tracing::debug!("File notifications overflowed; checking every library");
			changed = (0..self.sources.len()).collect();
		}
		for index in changed {
			if let Some(source) = self.sources.get_mut(index) {
				let first = source.changed.map_or(now, |(first, _)| first);
				source.changed = Some((first, now));
			}
		}
		for index in changes.rearm {
			if let Some(source) = self.sources.get_mut(index) {
				source.targets = source.config.watch_targets();
				watcher.arm(index, &source.targets);
			}
		}
	}

	/// The periodic check: scan sources whose metadata fingerprint changed.
	fn poll(&mut self) {
		self.last_poll = Instant::now();
		for (index, source) in self.sources.iter_mut().enumerate() {
			let targets = source.config.watch_targets();
			let current = fingerprint(&targets);
			if source.fingerprint != Some(current) {
				tracing::debug!(scanner = index, "Library metadata changed since the last check");
				source.forced = true;
			}
			if targets != source.targets {
				source.targets = targets;
				if let Some(watcher) = self.watcher.as_mut() {
					watcher.arm(index, &source.targets);
				}
			}
		}
	}

	fn scan_due(&mut self, now: Instant) {
		let due: Vec<usize> = self
			.sources
			.iter()
			.enumerate()
			.filter(|(_, source)| source.due(now, &self.timing).is_some_and(|due| due <= now))
			.map(|(index, _)| index)
			.collect();
		if due.is_empty() && self.last_refresh_ms.is_some() {
			return;
		}
		// The first refresh of a library without scanners only publishes the
		// manual catalog; claiming a scan would contradict the initial view,
		// which already said nothing is being scanned.
		self.publish_view(!due.is_empty());
		for index in due {
			// A stop request ends the work between sources.
			if self.stop.load(Ordering::Acquire) {
				return;
			}
			self.scan_source(index);
		}
		self.rebuild();
	}

	fn scan_source(&mut self, index: usize) {
		let started = Instant::now();
		let now_ms = unix_millis();
		let result = self.sources[index].config.scan(&mut self.icons);
		let duration = started.elapsed();
		tracing::debug!(
			scanner = index,
			scan_ms = duration.as_millis() as u64,
			"Library source scanned"
		);
		let retained = self.retained.remove(&index);
		let source = &mut self.sources[index];
		let previous: Vec<UnitState> = if source.last_scan.is_some() {
			std::mem::take(&mut source.units)
		} else {
			// First scan since start: the results retained from the previous
			// run, if they were produced under the same configuration.
			retained
				.filter(|retained| retained.digest == source.digest)
				.map(|retained| retained.units)
				.unwrap_or_default()
		};
		let retention = STALE_RETENTION.as_millis() as u64;
		let keep = |previous: Option<&UnitState>, error: UnitError| -> UnitState {
			match previous {
				Some(previous) if !previous.entries.is_empty() => {
					let since = previous.stale_since.unwrap_or(now_ms);
					if now_ms.saturating_sub(since) > retention {
						tracing::info!(unit = previous.key, "Dropping entries unavailable for over a week");
						UnitState {
							key: previous.key.clone(),
							entries: Vec::new(),
							stale_since: None,
							error: Some(error),
						}
					} else {
						UnitState {
							key: previous.key.clone(),
							entries: previous.entries.clone(),
							stale_since: Some(since),
							error: Some(error),
						}
					}
				},
				_ => UnitState {
					key: previous.map_or_else(String::new, |previous| previous.key.clone()),
					entries: Vec::new(),
					stale_since: None,
					error: Some(error),
				},
			}
		};
		let mut retry = false;
		source.failure = result.failure.clone();
		let units: Vec<UnitState> = match result.failure {
			Some(error) => {
				retry |= matches!(error, UnitError::Corrupt(_));
				tracing::warn!(scanner = index, "{}", error.message());
				previous.iter().map(|unit| keep(Some(unit), error.clone())).collect()
			},
			None => result
				.units
				.into_iter()
				.map(|unit| {
					let earlier = previous.iter().find(|previous| previous.key == unit.key);
					match unit.result {
						Ok(mut entries) => {
							let mut error = None;
							if !unit.retain.is_empty() {
								retry = true;
								error = Some(UnitError::Corrupt(format!(
									"{} item(s) could not be read: {}",
									unit.retain.len(),
									unit.retain.join(", ")
								)));
								if let Some(earlier) = earlier {
									entries.extend(
										earlier
											.entries
											.iter()
											.filter(|entry| unit.retain.contains(&entry.native_id))
											.cloned(),
									);
								}
							}
							UnitState {
								key: unit.key,
								entries,
								stale_since: None,
								error,
							}
						},
						Err(error) => {
							retry |= matches!(error, UnitError::Corrupt(_));
							let mut state = keep(earlier, error);
							state.key = unit.key;
							state
						},
					}
				})
				.collect(),
		};

		let content_changed = units.len() != previous.len()
			|| units.iter().zip(&previous).any(|(unit, previous)| {
				unit.key != previous.key || unit.entries != previous.entries || unit.stale_since != previous.stale_since
			});
		self.retained_dirty |= content_changed || source.last_scan.is_none();
		source.units = units;
		source.last_scan = Some(Instant::now());
		source.changed = None;
		source.forced = false;
		source.retry = if retry {
			let attempt = source.retry.map_or(0, |(_, attempt)| attempt + 1);
			self.timing
				.retry
				.get(attempt)
				.map(|delay| (Instant::now() + *delay, attempt))
		} else {
			None
		};
		source.targets = source.config.watch_targets();
		source.fingerprint = Some(fingerprint(&source.targets));
		if let Some(watcher) = self.watcher.as_mut() {
			watcher.arm(index, &source.targets);
		}
		let watched = self.watcher.as_ref().is_some_and(|watcher| watcher.complete(index));
		source.status = status(source, now_ms, duration, watched);
	}

	fn set_hidden(&mut self, key: &str, hidden: bool) -> Result<(), LibraryError> {
		if key.starts_with("manual:") {
			return Err(LibraryError::Invalid(
				"Manual applications are shown as configured; remove them in Settings instead.".into(),
			));
		}
		let catalog = self.publish.borrow().catalog.clone();
		let Some(entry) = catalog.find_key(key) else {
			return Err(LibraryError::NotFound(
				"This application is no longer in the library.".into(),
			));
		};
		if self.identity.record(key).is_none() {
			return Err(LibraryError::Invalid(format!(
				"{} is offered through another entry and cannot be hidden on its own.",
				entry.application.title
			)));
		}
		if self.identity.set_hidden(key, hidden) {
			tracing::info!(key, hidden, "Application visibility changed");
			self.identity
				.save()
				.map_err(|error| LibraryError::Failed(format!("The choice could not be saved: {error}")))?;
			self.rebuild();
		}
		Ok(())
	}

	/// Reconcile and publish the catalog, and persist what changed.
	fn rebuild(&mut self) {
		let now_ms = unix_millis();
		let mut titles: Vec<String> = self
			.manual
			.iter()
			.filter(|application| application.boxart.is_none())
			.map(|application| application.title.to_ascii_lowercase())
			.collect();
		titles.extend(
			self.sources
				.iter()
				.flat_map(|source| &source.units)
				.flat_map(|unit| &unit.entries)
				.filter(|entry| entry.application.boxart.is_none())
				.map(|entry| entry.application.title.to_ascii_lowercase()),
		);
		let started = Instant::now();
		self.icons.prepare(titles.iter().map(String::as_str));
		let icons_done = Instant::now();

		let inputs: Vec<SourceInput> = self
			.sources
			.iter()
			.enumerate()
			.map(|(scanner, source)| SourceInput {
				scanner,
				kind: source.config.kind(),
				units: source
					.units
					.iter()
					.map(|unit| UnitInput {
						key: &unit.key,
						entries: &unit.entries,
						stale_since: unit.stale_since,
					})
					.collect(),
			})
			.collect();
		let icons = &self.icons;
		let entries = reconcile(
			&self.manual,
			&inputs,
			&mut self.identity,
			&|title| icons.get(&title.to_ascii_lowercase()),
			now_ms,
		);
		let reconciled = Instant::now();
		self.notice = self.identity.save().err();
		if let Some(notice) = &self.notice {
			tracing::warn!("{notice}");
		}
		self.save_retained();
		let persisted = Instant::now();
		tracing::debug!(
			icons_ms = icons_done.duration_since(started).as_millis() as u64,
			reconcile_ms = reconciled.duration_since(icons_done).as_millis() as u64,
			persist_ms = persisted.duration_since(reconciled).as_millis() as u64,
			"Library catalog rebuilt"
		);

		let current = self.publish.borrow().catalog.clone();
		if current.entries() != entries.as_slice() {
			let before: std::collections::HashSet<i32> = current.offered().map(|entry| entry.id).collect();
			let snapshot = CatalogSnapshot::new(current.generation + 1, entries);
			let after: std::collections::HashSet<i32> = snapshot.offered().map(|entry| entry.id).collect();
			tracing::info!(
				generation = snapshot.generation,
				offered = after.len(),
				added = after.difference(&before).count(),
				removed = before.difference(&after).count(),
				"Application library updated"
			);
			self.publish.send_modify(|view| {
				Arc::make_mut(view).catalog = Arc::new(snapshot);
			});
		}
		self.last_refresh_ms = Some(now_ms);
		self.publish_view(false);
	}

	fn save_retained(&mut self) {
		if !self.retained_dirty {
			return;
		}
		let Some(path) = &self.retained_path else {
			return;
		};
		let file = RetainedFile {
			version: 1,
			sources: self
				.sources
				.iter()
				.enumerate()
				.filter(|(_, source)| source.last_scan.is_some())
				.map(|(scanner, source)| RetainedSource {
					scanner,
					digest: source.digest.clone(),
					units: source
						.units
						.iter()
						.filter(|unit| !unit.entries.is_empty())
						.cloned()
						.collect(),
				})
				.collect(),
		};
		match serde_json::to_vec(&file)
			.map_err(|error| error.to_string())
			.and_then(|bytes| crate::durable::replace(path, &bytes).map_err(|error| error.to_string()))
		{
			Ok(()) => self.retained_dirty = false,
			Err(error) => tracing::debug!("Cannot save the retained library results: {error}"),
		}
	}

	fn publish_view(&mut self, scanning: bool) {
		let sources: Vec<SourceStatus> = self.sources.iter().map(|source| source.status.clone()).collect();
		let notice = self.notice.clone();
		let last_refresh_ms = self.last_refresh_ms;
		self.publish.send_if_modified(|view| {
			let changed = view.scanning != scanning
				|| view.sources != sources
				|| view.notice != notice
				|| view.last_refresh_ms != last_refresh_ms;
			if changed {
				let view = Arc::make_mut(view);
				view.scanning = scanning;
				view.sources = sources;
				view.notice = notice;
				view.last_refresh_ms = last_refresh_ms;
			}
			changed
		});
	}
}

/// Summarize a source after a scan.
fn status(source: &Source, now_ms: u64, duration: Duration, watched: bool) -> SourceStatus {
	let errors: Vec<(&UnitState, &UnitError)> = source
		.units
		.iter()
		.filter_map(|unit| unit.error.as_ref().map(|error| (unit, error)))
		.collect();
	let entries = source.units.iter().map(|unit| unit.entries.len()).sum();
	let stale_entries = source
		.units
		.iter()
		.filter(|unit| unit.stale_since.is_some())
		.map(|unit| unit.entries.len())
		.sum();
	// A missing desktop directory that never had entries is informational.
	let relevant: Vec<&(&UnitState, &UnitError)> = errors
		.iter()
		.filter(|(unit, error)| {
			!(source.config.kind() == ScannerKind::Desktop
				&& unit.entries.is_empty()
				&& matches!(error, UnitError::Unavailable(_)))
		})
		.collect();
	let all_failed = !source.units.is_empty() && relevant.len() == source.units.len();
	if let Some(failure) = &source.failure {
		return SourceStatus {
			scanner: source.status.scanner,
			kind: source.config.kind(),
			root: source.config.root(),
			health: SourceHealth::Failed,
			message: Some(failure.message().to_string()),
			hint: Some(match failure {
				UnitError::Corrupt(_) => {
					"The launcher may be updating its files; Pyroshine retries shortly and keeps offering the last known games.".to_string()
				},
				UnitError::Unavailable(_) => format!(
					"Check that {} exists and is readable (is the launcher installed for this user?), then rescan.",
					source.config.root()
				),
			}),
			entries,
			stale_entries,
			last_scan_ms: Some(now_ms),
			duration_ms: Some(duration.as_millis() as u64),
			watched,
			scans: source.status.scans + 1,
		};
	}
	let health = if relevant.is_empty() {
		SourceHealth::Ok
	} else if all_failed {
		SourceHealth::Failed
	} else {
		SourceHealth::Partial
	};
	let message = (!relevant.is_empty()).then(|| {
		relevant
			.iter()
			.map(|(_, error)| error.message().to_string())
			.collect::<Vec<_>>()
			.join("; ")
	});
	let hint = relevant.first().map(|(_, error)| match error {
		UnitError::Corrupt(_) if stale_entries > 0 || entries > 0 => {
			"The launcher may be updating its files; Pyroshine retries shortly and keeps offering the last known games.".to_string()
		},
		UnitError::Corrupt(_) => "The launcher may be updating its files; Pyroshine retries shortly.".to_string(),
		UnitError::Unavailable(_) if stale_entries > 0 => {
			"Games from this location are offered from the last successful scan until it is reachable again (for up to 7 days). Mount the drive or fix its permissions, then rescan.".to_string()
		},
		UnitError::Unavailable(_) => {
			"Check that the path in this scanner's configuration exists and is readable, then rescan.".to_string()
		},
	});
	SourceStatus {
		scanner: source.status.scanner,
		kind: source.config.kind(),
		root: source.config.root(),
		health,
		message,
		hint,
		entries,
		stale_entries,
		last_scan_ms: Some(now_ms),
		duration_ms: Some(duration.as_millis() as u64),
		watched,
		scans: source.status.scans + 1,
	}
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
