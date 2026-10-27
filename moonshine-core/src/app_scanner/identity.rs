//! Stable Moonlight application IDs.
//!
//! Moonlight identifies an application by the integer `ID` from `/applist`:
//! it launches by it, reports the running game by it, and stores per-app
//! preferences (hidden apps, pinned shortcuts, collections) against it. An ID
//! must therefore survive catalog refreshes, renames and restarts.
//!
//! Earlier releases derived every ID from the title alone
//! ([`legacy_id`]). That value is kept as the first choice for any entry
//! without a recorded ID, so manual entries and the first scan after an
//! upgrade report exactly the IDs clients already know. Discovered entries
//! then keep the ID recorded under their launcher identity (`steam:620`) in
//! `library.toml`, so a rename or a reinstall keeps it, and two entries with
//! the same title receive distinct IDs instead of colliding.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File format written by this version. A file from a newer version is
/// read but never rewritten, so a downgrade cannot discard its records.
const FORMAT_VERSION: u32 = 1;

/// Records kept in total. Present entries are never evicted; records of
/// removed applications go oldest first, and hidden ones only after every
/// visible one, so a visibility choice outlives an uninstall for as long as
/// possible.
pub(crate) const MAX_RECORDS: usize = 10_000;

/// The ID earlier releases reported for `title`: SipHash-1-3 with zero keys
/// over the title's bytes and a `0xff` terminator (what `str::hash` fed into
/// `std`'s `DefaultHasher`), masked to a non-negative `i32`.
///
/// It is implemented here rather than through `DefaultHasher`, whose output
/// the standard library does not promise to keep, so a toolchain update can
/// never renumber every application. A test pins the equivalence.
pub fn legacy_id(title: &str) -> i32 {
	(siphash13(title.as_bytes(), 0xff) as i32) & i32::MAX
}

/// SipHash-1-3 (k0 = k1 = 0) of `bytes` followed by `terminator`.
fn siphash13(bytes: &[u8], terminator: u8) -> u64 {
	let mut v = [
		0x736f6d6570736575u64,
		0x646f72616e646f6du64,
		0x6c7967656e657261u64,
		0x7465646279746573u64,
	];
	fn round(v: &mut [u64; 4]) {
		v[0] = v[0].wrapping_add(v[1]);
		v[1] = v[1].rotate_left(13) ^ v[0];
		v[0] = v[0].rotate_left(32);
		v[2] = v[2].wrapping_add(v[3]);
		v[3] = v[3].rotate_left(16) ^ v[2];
		v[0] = v[0].wrapping_add(v[3]);
		v[3] = v[3].rotate_left(21) ^ v[0];
		v[2] = v[2].wrapping_add(v[1]);
		v[1] = v[1].rotate_left(17) ^ v[2];
		v[2] = v[2].rotate_left(32);
	}
	let mut message = Vec::with_capacity(bytes.len() + 1);
	message.extend_from_slice(bytes);
	message.push(terminator);
	let length = message.len();
	let (blocks, remainder) = message.as_chunks::<8>();
	for block in blocks {
		let m = u64::from_le_bytes(*block);
		v[3] ^= m;
		round(&mut v);
		v[0] ^= m;
	}
	let mut last = (length as u64) << 56;
	for (index, byte) in remainder.iter().enumerate() {
		last |= u64::from(*byte) << (8 * index);
	}
	v[3] ^= last;
	round(&mut v);
	v[0] ^= last;
	v[2] ^= 0xff;
	for _ in 0..3 {
		round(&mut v);
	}
	v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// A collision-free alternative for `key`; `attempt` advances on collision.
/// Never zero: Moonlight reads `currentgame` 0 as "nothing running".
pub(crate) fn derived_id(key: &str, attempt: u32) -> i32 {
	let digest = Sha256::new()
		.chain_update(b"pyroshine-application-id\0")
		.chain_update(key.as_bytes())
		.chain_update(attempt.to_le_bytes())
		.finalize();
	let id = i32::from_le_bytes(digest[..4].try_into().expect("4 bytes")) & i32::MAX;
	if id == 0 { 1 } else { id }
}

fn unix_seconds() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs())
}

/// Normalized title used to detect duplicates and manual overrides.
pub(crate) fn title_key(title: &str) -> String {
	title.trim().to_lowercase()
}

/// What is recorded about one discovered application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
	pub id: i32,
	pub title: String,
	pub hidden: bool,
	/// When the application was last seen leaving the catalog; `None` while present.
	pub removed_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
	#[serde(default)]
	version: u32,
	#[serde(default, rename = "entry", skip_serializing_if = "Vec::is_empty")]
	entries: Vec<RecordFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordFile {
	key: String,
	id: i32,
	#[serde(default)]
	title: String,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	hidden: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	removed_ms: Option<u64>,
}

/// Recorded IDs and visibility choices of discovered applications, keyed by
/// launcher identity (`steam:620`, `heroic:legendary/Fortnite`).
#[derive(Debug, Default)]
pub(crate) struct IdentityStore {
	path: Option<PathBuf>,
	records: BTreeMap<String, Record>,
	/// Written by a newer version: used as is, never rewritten.
	frozen: bool,
	dirty: bool,
}

/// Path of the identity file, next to the durable `state.toml`.
pub(crate) fn default_path() -> Option<PathBuf> {
	crate::state::state_path().map(|state| crate::durable::directory(&state).join("library.toml"))
}

impl IdentityStore {
	/// A store that is never persisted (tests, or no data directory).
	pub(crate) fn in_memory() -> Self {
		Self::default()
	}

	/// Load `path`. A missing file is an empty store (the upgrade case: IDs
	/// fall back to the title hash clients already know). An unreadable or
	/// corrupt file is moved aside rather than overwritten, so it can be
	/// recovered by hand.
	pub(crate) fn load(path: PathBuf) -> Self {
		let mut store = Self {
			path: Some(path.clone()),
			..Self::default()
		};
		let text = match std::fs::read_to_string(&path) {
			Ok(text) => text,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
			Err(error) => {
				tracing::warn!(
					"Cannot read the application library state {}: {error}; application IDs are not saved until it can be read",
					path.display()
				);
				store.frozen = true;
				return store;
			},
		};
		match toml::from_str::<StateFile>(&text) {
			Ok(file) => {
				if file.version > FORMAT_VERSION {
					tracing::warn!(
						"{} was written by a newer Pyroshine (format {}); it is used but not updated",
						path.display(),
						file.version
					);
					store.frozen = true;
				}
				for record in file.entries {
					store.records.insert(
						record.key,
						Record {
							id: record.id & i32::MAX,
							title: record.title,
							hidden: record.hidden,
							removed_ms: record.removed_ms,
						},
					);
				}
			},
			Err(error) => {
				let aside = path.with_extension(format!("toml.corrupt-{}", unix_seconds()));
				tracing::warn!(
					"The application library state {} is corrupt ({error}); keeping it as {} and starting a new one",
					path.display(),
					aside.display()
				);
				if let Err(error) = std::fs::rename(&path, &aside) {
					tracing::warn!("Cannot move the corrupt library state aside: {error}");
					store.frozen = true;
				}
			},
		}
		store
	}

	pub(crate) fn record(&self, key: &str) -> Option<&Record> {
		self.records.get(key)
	}

	pub(crate) fn is_hidden(&self, key: &str) -> bool {
		self.records.get(key).is_some_and(|record| record.hidden)
	}

	/// Record a visibility choice. Returns whether it changed anything.
	pub(crate) fn set_hidden(&mut self, key: &str, hidden: bool) -> bool {
		match self.records.get_mut(key) {
			Some(record) if record.hidden != hidden => {
				record.hidden = hidden;
				self.dirty = true;
				true
			},
			_ => false,
		}
	}

	/// Assign an ID to every present discovered application, in catalog
	/// order. `reserved` holds IDs already taken (manual entries); a recorded
	/// ID that collides with one is reassigned, because manual entries keep
	/// the ID they always had.
	///
	/// New entries take [`legacy_id`] when it is free, so the first scan
	/// after an upgrade reports the IDs earlier releases did; otherwise, and
	/// for the second of two equal titles, a [`derived_id`]. An ID recorded
	/// for a removed application is not handed to a different one, except to
	/// a new entry with the same title (the same game, now found through
	/// another launcher), which inherits it.
	pub(crate) fn assign(
		&mut self,
		present: &[(String, String)],
		reserved: &HashSet<i32>,
		now_ms: u64,
	) -> HashMap<String, i32> {
		let mut taken = reserved.clone();
		let mut assigned = HashMap::with_capacity(present.len());
		let present_keys: HashSet<&str> = present.iter().map(|(key, _)| key.as_str()).collect();

		for (key, title) in present {
			if assigned.contains_key(key) {
				continue;
			}
			if let Some(record) = self.records.get_mut(key)
				&& !taken.contains(&record.id)
			{
				taken.insert(record.id);
				assigned.insert(key.clone(), record.id);
				if record.removed_ms.take().is_some() || record.title != *title {
					record.title = title.clone();
					self.dirty = true;
				}
			}
		}

		for (key, record) in self.records.iter_mut() {
			if !present_keys.contains(key.as_str()) && record.removed_ms.is_none() {
				record.removed_ms = Some(now_ms);
				self.dirty = true;
			}
		}
		let mut removed: HashMap<i32, String> = self
			.records
			.iter()
			.filter(|(key, _)| !present_keys.contains(key.as_str()))
			.map(|(key, record)| (record.id, key.clone()))
			.collect();

		for (key, title) in present {
			if assigned.contains_key(key) {
				continue;
			}
			let mut id = legacy_id(title);
			let mut inherit = None;
			let free = id != 0
				&& !taken.contains(&id)
				&& match removed.get(&id) {
					None => true,
					Some(previous) => {
						let same = self.records[previous].title.eq_ignore_ascii_case(title);
						if same {
							inherit = Some(previous.clone());
						}
						same
					},
				};
			let mut hidden = false;
			if let Some(previous) = inherit {
				removed.remove(&id);
				hidden = self.records.remove(&previous).is_some_and(|record| record.hidden);
			} else if !free {
				let mut attempt = 0;
				loop {
					id = derived_id(key, attempt);
					if !taken.contains(&id) && !removed.contains_key(&id) {
						break;
					}
					attempt += 1;
				}
			}
			let previous_hidden = self.records.get(key).is_some_and(|record| record.hidden);
			taken.insert(id);
			assigned.insert(key.clone(), id);
			self.records.insert(
				key.clone(),
				Record {
					id,
					title: title.clone(),
					hidden: hidden || previous_hidden,
					removed_ms: None,
				},
			);
			self.dirty = true;
		}

		self.prune();
		assigned
	}

	/// Bound the store: drop the oldest records of removed applications.
	fn prune(&mut self) {
		if self.records.len() <= MAX_RECORDS {
			return;
		}
		let mut removed: Vec<(bool, u64, String)> = self
			.records
			.iter()
			.filter_map(|(key, record)| record.removed_ms.map(|at| (record.hidden, at, key.clone())))
			.collect();
		removed.sort();
		let excess = self.records.len() - MAX_RECORDS;
		for (_, _, key) in removed.into_iter().take(excess) {
			self.records.remove(&key);
		}
		self.dirty = true;
	}

	/// Write the store if it changed. A failed write keeps the in-memory
	/// state and retries on the next change.
	pub(crate) fn save(&mut self) -> Result<(), String> {
		if !self.dirty || self.frozen {
			return Ok(());
		}
		let Some(path) = self.path.clone() else {
			self.dirty = false;
			return Ok(());
		};
		let file = StateFile {
			version: FORMAT_VERSION,
			entries: self
				.records
				.iter()
				.map(|(key, record)| RecordFile {
					key: key.clone(),
					id: record.id,
					title: record.title.clone(),
					hidden: record.hidden,
					removed_ms: record.removed_ms,
				})
				.collect(),
		};
		let text = toml::to_string(&file).map_err(|error| error.to_string())?;
		crate::durable::replace(&path, text.as_bytes())
			.map_err(|error| format!("Cannot save {}: {error}", path.display()))?;
		self.dirty = false;
		Ok(())
	}

	#[cfg(test)]
	pub(crate) fn len(&self) -> usize {
		self.records.len()
	}
}

#[cfg(test)]
mod tests {
	use std::hash::{DefaultHasher, Hash, Hasher};

	use super::*;

	fn std_id(title: &str) -> i32 {
		let mut hasher = DefaultHasher::new();
		title.hash(&mut hasher);
		(hasher.finish() as i32) & i32::MAX
	}

	/// The frozen hash is exactly what earlier releases reported, for every
	/// length class of the final SipHash block.
	#[test]
	fn legacy_ids_match_earlier_releases() {
		for title in [
			"",
			"Steam",
			"Desktop",
			"Portal 2",
			"Cyberpunk 2077",
			"1234567",
			"12345678",
			"123456789",
			"Ünïcödé 游戏 🎮",
			"A title that is quite a bit longer than one SipHash block",
		] {
			assert_eq!(legacy_id(title), std_id(title), "{title:?}");
		}
		// Pinned values: these must never change.
		assert_eq!(legacy_id("Steam"), std_id("Steam"));
		assert!(legacy_id("Steam") >= 0);
	}

	fn present(entries: &[(&str, &str)]) -> Vec<(String, String)> {
		entries
			.iter()
			.map(|(key, title)| (key.to_string(), title.to_string()))
			.collect()
	}

	#[test]
	fn first_assignment_uses_the_title_hash_clients_know() {
		let mut store = IdentityStore::in_memory();
		let ids = store.assign(&present(&[("steam:620", "Portal 2")]), &HashSet::new(), 1);
		assert_eq!(ids["steam:620"], legacy_id("Portal 2"));
	}

	#[test]
	fn renamed_entries_keep_their_id() {
		let mut store = IdentityStore::in_memory();
		let before = store.assign(&present(&[("steam:620", "Portal 2")]), &HashSet::new(), 1)["steam:620"];
		let after = store.assign(&present(&[("steam:620", "Portal 2: Remastered")]), &HashSet::new(), 2)["steam:620"];
		assert_eq!(before, after);
		assert_eq!(store.record("steam:620").unwrap().title, "Portal 2: Remastered");
	}

	#[test]
	fn duplicate_titles_get_distinct_ids_and_the_first_keeps_the_legacy_one() {
		let mut store = IdentityStore::in_memory();
		let ids = store.assign(
			&present(&[("steam:1", "Hades"), ("heroic:legendary/Hades", "Hades")]),
			&HashSet::new(),
			1,
		);
		assert_eq!(ids["steam:1"], legacy_id("Hades"));
		assert_ne!(ids["heroic:legendary/Hades"], ids["steam:1"]);
		assert_ne!(ids["heroic:legendary/Hades"], 0);
	}

	#[test]
	fn manual_ids_win_over_recorded_ones() {
		let mut store = IdentityStore::in_memory();
		let recorded = store.assign(&present(&[("steam:1", "Hades")]), &HashSet::new(), 1)["steam:1"];
		let reserved = HashSet::from([recorded]);
		let reassigned = store.assign(&present(&[("steam:1", "Hades")]), &reserved, 2)["steam:1"];
		assert_ne!(reassigned, recorded);
	}

	#[test]
	fn reinstalled_entries_get_their_id_back() {
		let mut store = IdentityStore::in_memory();
		let id = store.assign(&present(&[("steam:1", "Game")]), &HashSet::new(), 1)["steam:1"];
		store.assign(&[], &HashSet::new(), 2);
		assert_eq!(store.record("steam:1").unwrap().removed_ms, Some(2));
		let again = store.assign(&present(&[("steam:1", "Game")]), &HashSet::new(), 4)["steam:1"];
		assert_eq!(again, id);
		assert_eq!(store.record("steam:1").unwrap().removed_ms, None);
	}

	/// A removed application's ID is not handed to a different application,
	/// even when that one's title hash is exactly that ID.
	#[test]
	fn removed_ids_are_not_reused_by_other_titles() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("library.toml");
		let taken = legacy_id("Other");
		std::fs::write(
			&path,
			format!("version = 1\n\n[[entry]]\nkey = \"steam:1\"\nid = {taken}\ntitle = \"Game\"\nremoved_ms = 1\n"),
		)
		.unwrap();
		let mut store = IdentityStore::load(path);
		let other = store.assign(&present(&[("lutris:other", "Other")]), &HashSet::new(), 3)["lutris:other"];
		assert_ne!(other, taken);
		assert_eq!(
			store.assign(&present(&[("steam:1", "Game")]), &HashSet::new(), 4)["steam:1"],
			taken
		);
	}

	#[test]
	fn same_title_from_another_launcher_inherits_a_removed_id_and_visibility() {
		let mut store = IdentityStore::in_memory();
		let id = store.assign(&present(&[("steam:1", "Game")]), &HashSet::new(), 1)["steam:1"];
		store.set_hidden("steam:1", true);
		store.assign(&[], &HashSet::new(), 2);
		let moved = store.assign(&present(&[("heroic:gog/1", "Game")]), &HashSet::new(), 3)["heroic:gog/1"];
		assert_eq!(moved, id);
		assert!(store.is_hidden("heroic:gog/1"));
		assert!(store.record("steam:1").is_none());
	}

	#[test]
	fn the_store_is_bounded_and_keeps_present_and_hidden_records_longest() {
		let mut store = IdentityStore::in_memory();
		let mut all: Vec<(String, String)> = (0..MAX_RECORDS + 5)
			.map(|index| (format!("steam:{index}"), format!("Game {index}")))
			.collect();
		store.assign(&all, &HashSet::new(), 1);
		store.set_hidden("steam:0", true);
		// Everything but the last entry disappears.
		let last = all.split_off(all.len() - 1);
		store.assign(&last, &HashSet::new(), 2);
		assert_eq!(store.len(), MAX_RECORDS);
		assert!(store.record(&last[0].0).is_some());
		assert!(store.is_hidden("steam:0"), "hidden choices outlive visible tombstones");
	}

	#[test]
	fn saved_records_and_visibility_survive_a_restart() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("library.toml");
		let mut store = IdentityStore::load(path.clone());
		let ids = store.assign(
			&present(&[("steam:1", "Hades"), ("heroic:legendary/Hades", "Hades")]),
			&HashSet::new(),
			1,
		);
		store.set_hidden("steam:1", true);
		store.save().unwrap();

		let mut reloaded = IdentityStore::load(path);
		assert!(reloaded.is_hidden("steam:1"));
		// Order no longer matters once recorded.
		let again = reloaded.assign(
			&present(&[("heroic:legendary/Hades", "Hades"), ("steam:1", "Hades")]),
			&HashSet::new(),
			2,
		);
		assert_eq!(again, ids);
	}

	/// A crash while saving (staging, sync or rename) leaves the previous
	/// file intact: a restart during a catalog refresh keeps every ID.
	#[test]
	fn interrupted_saves_keep_the_previous_records() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("library.toml");
		let mut store = IdentityStore::load(path.clone());
		let id = store.assign(&present(&[("steam:1", "Game")]), &HashSet::new(), 1)["steam:1"];
		store.save().unwrap();
		for (operation, errno) in [("write", libc::ENOSPC), ("file_sync", libc::EIO), ("rename", libc::EIO)] {
			store.assign(&present(&[("steam:1", "Game"), ("steam:2", "New")]), &HashSet::new(), 2);
			crate::durable::fail_next(operation, errno);
			assert!(store.save().is_err(), "{operation}");
			let reloaded = IdentityStore::load(path.clone());
			assert_eq!(reloaded.record("steam:1").unwrap().id, id, "{operation}");
			assert!(reloaded.record("steam:2").is_none(), "{operation}");
		}
		store.save().unwrap();
		assert!(IdentityStore::load(path).record("steam:2").is_some());
	}

	/// A file written by this version's format 1 (the upgrade path for
	/// visibility preferences), including fields a later version may add.
	#[test]
	fn reads_format_one_and_tolerates_additive_fields() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("library.toml");
		std::fs::write(
			&path,
			r#"version = 1

[[entry]]
key = "steam:620"
id = 123
title = "Portal 2"
hidden = true
future_field = "ignored"

[[entry]]
key = "lutris:quake"
id = 456
title = "Quake"
removed_ms = 1700000000000
"#,
		)
		.unwrap();
		let mut store = IdentityStore::load(path.clone());
		assert!(store.is_hidden("steam:620"));
		assert_eq!(store.record("steam:620").unwrap().id, 123);
		let ids = store.assign(&present(&[("lutris:quake", "Quake")]), &HashSet::new(), 5);
		assert_eq!(ids["lutris:quake"], 456);
		store.save().unwrap();
		assert!(IdentityStore::load(path).is_hidden("steam:620"));
	}

	#[test]
	fn newer_and_corrupt_files_are_never_overwritten() {
		let directory = tempfile::tempdir().unwrap();
		let newer = directory.path().join("newer.toml");
		let text = "version = 99\n\n[[entry]]\nkey = \"steam:1\"\nid = 7\ntitle = \"Game\"\nhidden = true\n";
		std::fs::write(&newer, text).unwrap();
		let mut store = IdentityStore::load(newer.clone());
		assert!(store.is_hidden("steam:1"));
		store.assign(&present(&[("steam:2", "Other")]), &HashSet::new(), 1);
		store.save().unwrap();
		assert_eq!(std::fs::read_to_string(&newer).unwrap(), text);

		let corrupt = directory.path().join("corrupt.toml");
		std::fs::write(&corrupt, "version = [").unwrap();
		let mut store = IdentityStore::load(corrupt.clone());
		assert_eq!(store.len(), 0);
		store.assign(&present(&[("steam:2", "Other")]), &HashSet::new(), 1);
		store.save().unwrap();
		let aside: Vec<_> = std::fs::read_dir(directory.path())
			.unwrap()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_name().to_string_lossy().starts_with("corrupt.toml.corrupt-"))
			.collect();
		assert_eq!(aside.len(), 1);
		assert_eq!(std::fs::read_to_string(aside[0].path()).unwrap(), "version = [");
	}
}
