//! Change detection for launcher metadata.
//!
//! Each scanner names the directories whose entries can change its result,
//! with a filter for the names that matter ([`WatchTarget`]). They are
//! watched with inotify, non-recursively, so unrelated activity (Steam
//! writing downloads or shader caches, Heroic caching API responses) never
//! wakes the library. Watches are re-armed after every scan of their
//! source, which follows directories that were created, replaced or
//! unmounted; a queue overflow marks every source changed.
//!
//! inotify can miss changes (watch limits, network file systems, paths that
//! did not exist yet), so the library also compares a cheap stat-only
//! [`fingerprint`] of every target periodically and scans a source only when
//! it changed.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::hash::{Hash, Hasher};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

/// Watches per source; beyond it the source relies on the periodic check.
pub(crate) const MAX_WATCHES_PER_SOURCE: usize = 512;

/// Names within a watched directory that matter to a scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
	/// Files ending in this suffix.
	Suffix(&'static str),
	/// Files starting with this prefix (a database and its journals).
	Prefix(&'static str),
	/// These exact names.
	Names(&'static [&'static str]),
	/// These exact names, or names ending in the suffix.
	Manifests(&'static [&'static str], &'static str),
	/// Heroic's configuration directory: stores and caches being created.
	Store,
}

impl Filter {
	pub(crate) fn matches(self, name: &OsStr) -> bool {
		let name = name.to_string_lossy();
		match self {
			Self::Suffix(suffix) => name.ends_with(suffix),
			Self::Prefix(prefix) => name.starts_with(prefix),
			Self::Names(names) => names.contains(&name.as_ref()),
			Self::Manifests(names, suffix) => names.contains(&name.as_ref()) || name.ends_with(suffix),
			Self::Store => name.ends_with("_store") || name == "store_cache" || name == "sideload_apps",
		}
	}
}

/// A location whose changes can change a scanner's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchTarget {
	pub path: PathBuf,
	/// `None`: only the path's own metadata is compared periodically
	/// (art directories that change often and matter rarely).
	pub filter: Option<Filter>,
	/// Directories created inside it matter too (recursive desktop dirs).
	pub subdirectories: bool,
}

impl WatchTarget {
	pub(crate) fn directory(path: PathBuf, filter: Filter) -> Self {
		Self {
			path,
			filter: Some(filter),
			subdirectories: false,
		}
	}

	pub(crate) fn probe(path: PathBuf) -> Self {
		Self {
			path,
			filter: None,
			subdirectories: false,
		}
	}

	pub(crate) fn with_subdirectories(mut self) -> Self {
		self.subdirectories = true;
		self
	}
}

/// Stat-only digest of `targets`: each directory's matching entries (name,
/// size, modification time, inode) and each probe's own metadata. Missing
/// and unreadable paths hash their error kind, so a mount appearing changes
/// it too.
pub(crate) fn fingerprint(targets: &[WatchTarget]) -> u64 {
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	for target in targets {
		target.path.hash(&mut hasher);
		match std::fs::metadata(&target.path) {
			Ok(metadata) => {
				if target.filter.is_none() {
					(metadata.mtime(), metadata.mtime_nsec(), metadata.ino()).hash(&mut hasher);
				}
			},
			Err(error) => {
				(error.kind() as u32).hash(&mut hasher);
				continue;
			},
		}
		let Some(filter) = target.filter else {
			continue;
		};
		let mut entries: Vec<(Vec<u8>, i64, i64, u64, u64)> = std::fs::read_dir(&target.path)
			.map(|entries| {
				entries
					.flatten()
					.filter(|entry| {
						filter.matches(&entry.file_name())
							|| (target.subdirectories && entry.file_type().is_ok_and(|kind| kind.is_dir()))
					})
					.filter_map(|entry| {
						let metadata = entry.metadata().ok()?;
						Some((
							entry.file_name().as_bytes().to_vec(),
							metadata.mtime(),
							metadata.mtime_nsec(),
							metadata.len(),
							metadata.ino(),
						))
					})
					.collect()
			})
			.unwrap_or_default();
		entries.sort();
		entries.hash(&mut hasher);
	}
	hasher.finish()
}

/// What a batch of inotify events means for the sources.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
	/// Sources with a relevant change.
	pub sources: Vec<usize>,
	/// Sources whose watches must be re-armed (a watched directory was
	/// removed, replaced, unmounted, or a subdirectory appeared).
	pub rearm: Vec<usize>,
	/// The kernel dropped events: everything may have changed.
	pub overflow: bool,
}

struct Watch {
	path: PathBuf,
	/// Sources watching this directory, with their filters. inotify returns
	/// one descriptor per inode, so sources share it.
	subscribers: Vec<(usize, WatchTarget)>,
}

/// inotify instance for the library worker.
pub(crate) struct Watcher {
	fd: OwnedFd,
	watches: HashMap<i32, Watch>,
	/// Sources that hit [`MAX_WATCHES_PER_SOURCE`] or the system limit.
	limited: Vec<usize>,
}

const MASK: u32 = libc::IN_CREATE
	| libc::IN_DELETE
	| libc::IN_MOVED_FROM
	| libc::IN_MOVED_TO
	| libc::IN_CLOSE_WRITE
	| libc::IN_MODIFY
	| libc::IN_DELETE_SELF
	| libc::IN_MOVE_SELF
	| libc::IN_ONLYDIR;

impl Watcher {
	/// A non-blocking inotify instance, or `None` when the kernel refuses one
	/// (the periodic check then covers every source).
	pub(crate) fn new() -> Option<Self> {
		// SAFETY: inotify_init1 has no preconditions; the returned descriptor
		// is owned exclusively by the OwnedFd below.
		let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
		if fd < 0 {
			tracing::warn!(
				"Cannot watch game libraries for changes ({}); checking them periodically instead",
				std::io::Error::last_os_error()
			);
			return None;
		}
		Some(Self {
			// SAFETY: `fd` is a fresh, valid descriptor nothing else owns.
			fd: unsafe { OwnedFd::from_raw_fd(fd) },
			watches: HashMap::new(),
			limited: Vec::new(),
		})
	}

	pub(crate) fn raw_fd(&self) -> RawFd {
		self.fd.as_raw_fd()
	}

	/// Whether `source` is fully covered by watches.
	pub(crate) fn complete(&self, source: usize) -> bool {
		!self.limited.contains(&source)
	}

	#[cfg(test)]
	pub(crate) fn watch_count(&self) -> usize {
		self.watches.len()
	}

	/// Replace `source`'s watches with `targets`. Missing directories are
	/// skipped; the periodic check notices them appearing and the next scan
	/// re-arms.
	pub(crate) fn arm(&mut self, source: usize, targets: &[WatchTarget]) {
		self.limited.retain(|limited| *limited != source);
		let mut wanted: Vec<&WatchTarget> = targets.iter().filter(|target| target.filter.is_some()).collect();
		if wanted.len() > MAX_WATCHES_PER_SOURCE {
			wanted.truncate(MAX_WATCHES_PER_SOURCE);
			self.limited.push(source);
		}

		// Drop this source's subscriptions, then subscribe again.
		let mut released = Vec::new();
		for (wd, watch) in self.watches.iter_mut() {
			watch.subscribers.retain(|(subscriber, _)| *subscriber != source);
			if watch.subscribers.is_empty() {
				released.push(*wd);
			}
		}

		for target in wanted {
			let Ok(path) = CString::new(target.path.as_os_str().as_bytes()) else {
				continue;
			};
			// SAFETY: `path` is a valid NUL-terminated string and the
			// descriptor is a live inotify instance.
			let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), MASK) };
			if wd < 0 {
				let error = std::io::Error::last_os_error();
				match error.raw_os_error() {
					Some(libc::ENOENT) | Some(libc::ENOTDIR) => {},
					Some(libc::ENOSPC) => {
						tracing::warn!(
							"The inotify watch limit is reached; {} is checked periodically instead (raise fs.inotify.max_user_watches)",
							target.path.display()
						);
						if !self.limited.contains(&source) {
							self.limited.push(source);
						}
					},
					_ => tracing::debug!("Cannot watch {}: {error}", target.path.display()),
				}
				continue;
			}
			released.retain(|released| *released != wd);
			self.watches
				.entry(wd)
				.or_insert_with(|| Watch {
					path: target.path.clone(),
					subscribers: Vec::new(),
				})
				.subscribers
				.push((source, target.clone()));
		}

		for wd in released {
			self.watches.remove(&wd);
			// SAFETY: removing a watch descriptor of this live instance; an
			// already-gone descriptor only returns EINVAL.
			unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
		}
	}

	/// Drain pending events without blocking.
	pub(crate) fn read(&mut self) -> Changes {
		let mut changes = Changes::default();
		let mut buffer = vec![0u8; 64 * 1024];
		loop {
			// SAFETY: reading into a buffer we own, of the length passed.
			let read = unsafe { libc::read(self.fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
			if read <= 0 {
				break;
			}
			let mut offset = 0;
			let read = read as usize;
			while offset + std::mem::size_of::<libc::inotify_event>() <= read {
				// SAFETY: the kernel writes whole inotify_event records; the
				// header is read unaligned from within the filled range.
				let event: libc::inotify_event =
					unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast()) };
				let name_start = offset + std::mem::size_of::<libc::inotify_event>();
				let name_end = (name_start + event.len as usize).min(read);
				let name = &buffer[name_start..name_end];
				let name = &name[..name.iter().position(|byte| *byte == 0).unwrap_or(name.len())];
				self.event(event.wd, event.mask, OsStr::from_bytes(name), &mut changes);
				offset = name_end;
			}
		}
		changes.sources.sort_unstable();
		changes.sources.dedup();
		changes.rearm.sort_unstable();
		changes.rearm.dedup();
		changes
	}

	fn event(&mut self, wd: i32, mask: u32, name: &OsStr, changes: &mut Changes) {
		if mask & libc::IN_Q_OVERFLOW != 0 {
			changes.overflow = true;
			return;
		}
		let Some(watch) = self.watches.get(&wd) else {
			return;
		};
		let gone = mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT) != 0;
		for (source, target) in &watch.subscribers {
			let directory = mask & libc::IN_ISDIR != 0;
			let relevant = gone
				|| target.filter.is_some_and(|filter| filter.matches(name))
				|| (directory && target.subdirectories);
			if relevant {
				changes.sources.push(*source);
			}
			if gone || (directory && target.subdirectories && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0) {
				changes.rearm.push(*source);
			}
		}
		if mask & libc::IN_IGNORED != 0 {
			tracing::debug!("Stopped watching {}", watch.path.display());
			self.watches.remove(&wd);
		}
	}
}

/// Wait until any of `fds` is readable or `timeout` passes; which are readable.
pub(crate) fn wait_readable(fds: &[RawFd], timeout: std::time::Duration) -> Vec<bool> {
	let mut polls: Vec<libc::pollfd> = fds
		.iter()
		.map(|&fd| libc::pollfd {
			fd,
			events: libc::POLLIN,
			revents: 0,
		})
		.collect();
	let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
	// SAFETY: `polls` is a valid array of pollfd of the given length.
	let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, millis) };
	if ready <= 0 {
		return vec![false; fds.len()];
	}
	polls.iter().map(|poll| poll.revents & libc::POLLIN != 0).collect()
}

/// An eventfd that wakes the worker for requests.
pub(crate) struct Wake(OwnedFd);

impl Wake {
	pub(crate) fn new() -> std::io::Result<Self> {
		// SAFETY: eventfd has no preconditions; the descriptor is owned below.
		let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
		if fd < 0 {
			return Err(std::io::Error::last_os_error());
		}
		// SAFETY: `fd` is a fresh descriptor nothing else owns.
		Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
	}

	pub(crate) fn raw_fd(&self) -> RawFd {
		self.0.as_raw_fd()
	}

	pub(crate) fn wake(&self) {
		let one: u64 = 1;
		// SAFETY: writing 8 bytes from a valid u64 to our eventfd.
		unsafe { libc::write(self.0.as_raw_fd(), (&one as *const u64).cast(), 8) };
	}

	pub(crate) fn drain(&self) {
		let mut value: u64 = 0;
		// SAFETY: reading 8 bytes into a valid u64 from our non-blocking eventfd.
		unsafe { libc::read(self.0.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
	}
}

/// Collect events until a relevant one arrives or `timeout` passes (tests).
#[cfg(test)]
pub(crate) fn wait_for_changes(watcher: &mut Watcher, timeout: std::time::Duration) -> Changes {
	let deadline = std::time::Instant::now() + timeout;
	let mut all = Changes::default();
	while std::time::Instant::now() < deadline {
		wait_readable(&[watcher.raw_fd()], std::time::Duration::from_millis(50));
		let changes = watcher.read();
		all.sources.extend(changes.sources);
		all.rearm.extend(changes.rearm);
		all.overflow |= changes.overflow;
		if !all.sources.is_empty() {
			// Collect the rest of the burst.
			std::thread::sleep(std::time::Duration::from_millis(50));
			let changes = watcher.read();
			all.sources.extend(changes.sources);
			all.rearm.extend(changes.rearm);
			break;
		}
	}
	all.sources.sort_unstable();
	all.sources.dedup();
	all.rearm.sort_unstable();
	all.rearm.dedup();
	all
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	const WAIT: Duration = Duration::from_secs(2);

	#[test]
	fn filters_unrelated_names_and_shares_directories_between_sources() {
		let directory = tempfile::tempdir().unwrap();
		let steamapps = directory.path().join("steamapps");
		std::fs::create_dir_all(&steamapps).unwrap();
		let mut watcher = Watcher::new().unwrap();
		watcher.arm(0, &[WatchTarget::directory(steamapps.clone(), Filter::Suffix(".acf"))]);
		watcher.arm(
			1,
			&[WatchTarget::directory(
				steamapps.clone(),
				Filter::Names(&["libraryfolders.vdf"]),
			)],
		);
		assert_eq!(watcher.watch_count(), 1);

		std::fs::write(steamapps.join("downloading.tmp"), "x").unwrap();
		assert_eq!(
			wait_for_changes(&mut watcher, Duration::from_millis(300)).sources,
			Vec::<usize>::new()
		);

		std::fs::write(steamapps.join("appmanifest_620.acf"), "x").unwrap();
		assert_eq!(wait_for_changes(&mut watcher, WAIT).sources, vec![0]);
		std::fs::write(steamapps.join("libraryfolders.vdf"), "x").unwrap();
		assert_eq!(wait_for_changes(&mut watcher, WAIT).sources, vec![1]);

		// Dropping one subscriber keeps the shared descriptor for the other.
		watcher.arm(1, &[]);
		assert_eq!(watcher.watch_count(), 1);
		watcher.arm(0, &[]);
		assert_eq!(watcher.watch_count(), 0);
	}

	/// A replaced directory (removed and recreated, as on an unmount or a
	/// launcher rewriting its cache) is reported for re-arming, and watching
	/// resumes once re-armed.
	#[test]
	fn replaced_directories_are_reported_and_re_armed() {
		let directory = tempfile::tempdir().unwrap();
		let cache = directory.path().join("store_cache");
		std::fs::create_dir_all(&cache).unwrap();
		let targets = [WatchTarget::directory(cache.clone(), Filter::Suffix("_library.json"))];
		let mut watcher = Watcher::new().unwrap();
		watcher.arm(3, &targets);

		std::fs::remove_dir_all(&cache).unwrap();
		let changes = wait_for_changes(&mut watcher, WAIT);
		assert_eq!(changes.sources, vec![3]);
		assert_eq!(changes.rearm, vec![3]);

		std::fs::create_dir_all(&cache).unwrap();
		watcher.arm(3, &targets);
		std::fs::write(cache.join("gog_library.json"), "{}").unwrap();
		assert_eq!(wait_for_changes(&mut watcher, WAIT).sources, vec![3]);
	}

	#[test]
	fn new_subdirectories_of_recursive_targets_ask_for_re_arming() {
		let directory = tempfile::tempdir().unwrap();
		let applications = directory.path().join("applications");
		std::fs::create_dir_all(&applications).unwrap();
		let mut watcher = Watcher::new().unwrap();
		watcher.arm(
			0,
			&[WatchTarget::directory(applications.clone(), Filter::Suffix(".desktop")).with_subdirectories()],
		);
		std::fs::create_dir_all(applications.join("wine")).unwrap();
		let changes = wait_for_changes(&mut watcher, WAIT);
		assert_eq!((changes.sources, changes.rearm), (vec![0], vec![0]));
	}

	#[test]
	fn fingerprints_change_with_relevant_entries_only() {
		let directory = tempfile::tempdir().unwrap();
		let steamapps = directory.path().join("steamapps");
		std::fs::create_dir_all(&steamapps).unwrap();
		let targets = [
			WatchTarget::directory(steamapps.clone(), Filter::Suffix(".acf")),
			WatchTarget::probe(directory.path().join("missing")),
		];
		let before = fingerprint(&targets);
		std::fs::write(steamapps.join("unrelated.tmp"), "x").unwrap();
		assert_eq!(fingerprint(&targets), before);
		std::fs::write(steamapps.join("appmanifest_1.acf"), "x").unwrap();
		let installed = fingerprint(&targets);
		assert_ne!(installed, before);
		std::fs::create_dir_all(directory.path().join("missing")).unwrap();
		assert_ne!(fingerprint(&targets), installed, "a path appearing changes it");
	}
}
