//! Bottles Library scanner (from Moonshine's Bottles scanner).
//!
//! Only entries added to Bottles' Library ("Add to Library" on a program) are
//! offered, the same list Bottles shows on its Library page. Entries that
//! Bottles launches through Steam are left to the Steam scanner, and UMU
//! entries only appear once Bottles reports them ready.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::watch::{Filter, WatchTarget};
use super::{ArtworkOrigin, Discovered, SourceScan, Unit, UnitError, expand_path};
use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BottlesApplicationScannerConfig {
	/// Path to the Bottles data directory (ie. `~/.local/share/bottles`).
	///
	/// Bottles stores its data in `~/.local/share/bottles` when installed
	/// natively and in `~/.var/app/com.usebottles.bottles/data/bottles` as a
	/// Flatpak.
	#[serde(default = "default_data_dir")]
	pub data_dir: PathBuf,

	/// The command to run for entries stored in a bottle.
	///
	/// `{bottle}` is replaced with the bottle name, `{program_id}` with the
	/// entry's program id, and `{name}` with the entry's display name.
	pub command: Vec<String>,

	/// The command to run for entries backed by Bottles' UMU integration.
	///
	/// `{umu_game}` is replaced with the entry's UMU game id, `{name}` with the
	/// entry's display name. Optional: without it, UMU entries are skipped.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub umu_command: Vec<String>,

	/// Commands to run before launching each scanned application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after each scanned application's session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// systemd StandardOutput value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<String>,

	/// systemd StandardError value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,

	/// Seconds to wait for each scanned application to reach an active state after launch.
	#[serde(default = "crate::session::application::default_launch_timeout")]
	pub launch_timeout_secs: u64,

	/// Overrides of `[frame_limit]` for each discovered application.
	#[serde(flatten)]
	pub frame_limit: crate::session::frame_limit::FrameLimitOverride,
}

/// Name of the file Bottles stores its Library in, inside the data directory.
const LIBRARY_YML: &str = "library.yml";

/// Bottles keeps bottle prefixes in a `bottles/` subdirectory of its data directory.
const BOTTLES_DIR: &str = "bottles";

/// Grid art Bottles downloaded for a bottle lives in its `grids/` directory.
const GRIDS_DIR: &str = "grids";

/// Bottles stores UMU games and their covers in these data directory subdirectories.
const UMU_DIR: &str = "umu";
const UMU_GAMES_DIR: &str = "games";
const UMU_COVERS_DIR: &str = "covers";
const UMU_GAME_CONFIG: &str = "game.yml";

fn native_data_dir() -> PathBuf {
	dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("bottles")
}

fn flatpak_data_dir() -> Option<PathBuf> {
	Some(
		dirs::home_dir()?
			.join(".var/app/com.usebottles.bottles/data")
			.join("bottles"),
	)
}

/// Pick the data directory that actually holds a `library.yml`.
///
/// Leftover files from an uninstalled native Bottles can leave an otherwise
/// empty native directory behind after the user moved to the Flatpak, so
/// preferring the native directory purely on its existence would scan nothing.
/// When neither candidate holds a library, fall back to whichever directory
/// exists, and to the native directory when neither does.
fn choose_data_dir(native: &Path, flatpak: Option<&Path>) -> PathBuf {
	let mut candidates = vec![native.to_path_buf()];
	candidates.extend(flatpak.map(Path::to_path_buf));

	for candidate in &candidates {
		if candidate.join(LIBRARY_YML).exists() {
			return candidate.clone();
		}
	}

	candidates
		.into_iter()
		.find(|candidate| candidate.exists())
		.unwrap_or_else(|| native.to_path_buf())
}

fn default_data_dir() -> PathBuf {
	let native = native_data_dir();
	let flatpak = flatpak_data_dir();
	choose_data_dir(&native, flatpak.as_deref())
}

/// A single entry of Bottles' `library.yml`, keyed by a uuid Bottles generates.
///
/// Bottles stores far more per entry; only the fields Pyroshine needs are
/// declared. Every field is optional so one malformed entry fails only itself.
#[derive(Debug, Deserialize)]
struct BottlesLibraryEntry {
	name: Option<String>,
	/// The program's id inside its bottle.
	id: Option<String>,

	#[serde(default)]
	bottle: Option<BottlesEntryBottle>,

	#[serde(default)]
	icon: Option<String>,

	#[serde(default)]
	thumbnail: Option<String>,

	/// Entries Bottles launches through Steam carry this marker; Bottles
	/// cannot launch them itself.
	#[serde(default)]
	steam: bool,

	/// `umu` for entries backed by Bottles' UMU integration. Older entries
	/// have no `source` at all and are regular bottle entries.
	#[serde(default)]
	source: Option<String>,

	#[serde(default)]
	source_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BottlesEntryBottle {
	#[serde(default)]
	name: Option<String>,

	/// Bottle directory name inside `bottles/`, or an absolute path for
	/// bottles created with a custom path.
	#[serde(default)]
	path: Option<String>,
}

/// The subset of a UMU game's config Pyroshine reads: its readiness state
/// (`draft`, `installing`, `ready`, `failed` or `stopped`).
#[derive(Debug, Deserialize)]
struct UmuGameConfig {
	#[serde(default)]
	state: Option<String>,
}

/// Scan the Bottles Library as one unit.
///
/// A missing library means Bottles is not installed, or its Library is
/// empty (no games). A library that cannot be read or parsed, typically while
/// Bottles rewrites it, is an error of the unit, so its games are kept until a
/// retry succeeds.
pub(crate) fn scan(config: &BottlesApplicationScannerConfig) -> SourceScan {
	let Some(data_dir) = expand_path(&config.data_dir) else {
		return SourceScan::failed(UnitError::Unavailable(format!(
			"Cannot expand the Bottles data directory path {}",
			config.data_dir.display()
		)));
	};
	let library_path = data_dir.join(LIBRARY_YML);
	let key = library_path.display().to_string();

	let contents = match std::fs::read_to_string(&library_path) {
		Ok(contents) => contents,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			tracing::debug!(
				"Bottles library not found at {:?}, no Bottles games will be scanned.",
				library_path
			);
			return SourceScan {
				units: vec![Unit::ok(key, Vec::new())],
				failure: None,
			};
		},
		Err(error) => {
			return SourceScan {
				units: vec![Unit::failed(key, UnitError::io(library_path.display(), &error))],
				failure: None,
			};
		},
	};

	// Bottles writes an empty document when its Library is empty, so the parse
	// target is optional: that reads as no entries, not as a failure.
	let library: Option<HashMap<String, yaml_serde::Value>> = match yaml_serde::from_str(&contents) {
		Ok(library) => library,
		Err(error) => {
			tracing::warn!("Failed to parse the Bottles library at {:?}: {error}", library_path);
			return SourceScan {
				units: vec![Unit::failed(
					key,
					UnitError::Corrupt(format!("Cannot parse the Bottles library: {error}")),
				)],
				failure: None,
			};
		},
	};

	// Deserialize entry by entry: one entry with an unexpected type has to cost
	// only that entry rather than the whole library.
	let mut entries: Vec<(String, BottlesLibraryEntry)> = Vec::new();
	for (uuid, value) in library.unwrap_or_default() {
		match yaml_serde::from_value::<BottlesLibraryEntry>(value) {
			Ok(entry) => entries.push((uuid, entry)),
			Err(e) => tracing::debug!("Skipping malformed Bottles library entry {uuid}: {e}"),
		}
	}

	// Map order is arbitrary, so sort for a stable list and stable identities.
	entries.sort_by_cached_key(|(uuid, entry)| (EntrySortKey::new(entry), uuid.clone()));

	let mut applications = Vec::new();
	let mut native_ids = HashSet::new();
	let mut skipped = 0;

	for (uuid, entry) in entries {
		match discover(config, &data_dir, &entry) {
			Some(mut discovered) => {
				// A program listed twice in the Library keeps one identity per entry.
				if !native_ids.insert(discovered.native_id.clone()) {
					discovered.native_id = format!("{}#{uuid}", discovered.native_id);
				}
				applications.push(discovered);
			},
			None => skipped += 1,
		}
	}

	tracing::debug!("Scanned {} Bottles games ({} skipped).", applications.len(), skipped);

	SourceScan {
		units: vec![Unit::ok(key, applications)],
		failure: None,
	}
}

/// The application one Library entry offers, or `None` when it cannot be
/// launched from here.
fn discover(
	config: &BottlesApplicationScannerConfig,
	data_dir: &Path,
	entry: &BottlesLibraryEntry,
) -> Option<Discovered> {
	let name = entry.name.as_deref().map(str::trim).filter(|name| !name.is_empty())?;

	// Bottles drops id-less entries when it loads the library; do the same.
	let id = entry.id.as_deref().filter(|id| !id.is_empty())?;

	if entry.steam {
		return None;
	}

	if entry.source.as_deref() == Some("umu") {
		if config.umu_command.is_empty() {
			tracing::debug!("Skipping UMU entry '{name}': no umu_command configured.");
			return None;
		}

		let Some(source_id) = entry.source_id.as_deref().filter(|id| is_safe_path_component(id)) else {
			tracing::debug!("Skipping UMU entry '{name}' without a usable source id.");
			return None;
		};

		// Readiness lives in Bottles' internal UMU store, not library.yml.
		// Bottles' own UI refuses to launch anything not `ready`; expose
		// only those, and skip when the state file is absent or unreadable.
		let state_path = umu_game_config(data_dir, source_id);
		if read_umu_state(&state_path).as_deref() != Some("ready") {
			tracing::debug!("Skipping UMU entry '{name}' that is not ready ({:?}).", state_path);
			return None;
		}

		let command = substitute(&config.umu_command, &[("{umu_game}", source_id), ("{name}", name)]);
		let application = build_application(config, name, command);
		return Some(
			Discovered::new(format!("umu:{source_id}"), application)
				.with_artwork(artwork(data_dir, entry, None), ArtworkOrigin::Launcher),
		);
	}

	let Some(bottle_name) = entry
		.bottle
		.as_ref()
		.and_then(|bottle| bottle.name.as_deref().filter(|n| !n.is_empty()))
	else {
		tracing::debug!("Skipping entry '{name}' without a bottle.");
		return None;
	};

	let bottle_dir = bottle_dir(data_dir, entry.bottle.as_ref())?;

	// Bottles removes entries for deleted bottles only lazily, from its UI;
	// drop them here so they never become dead launch buttons.
	if !bottle_dir.exists() {
		tracing::debug!(
			"Skipping entry '{name}' whose bottle {:?} no longer exists.",
			bottle_dir
		);
		return None;
	}

	let command = substitute(
		&config.command,
		&[("{bottle}", bottle_name), ("{program_id}", id), ("{name}", name)],
	);
	let application = build_application(config, name, command);
	let mut discovered = Discovered::new(id, application)
		.with_artwork(artwork(data_dir, entry, Some(&bottle_dir)), ArtworkOrigin::Launcher);
	// Bottles' desktop shortcuts name the bottle and program (see `launcher_target`).
	discovered.aliases.push(program_alias(bottle_name, name));
	Some(discovered)
}

/// Identity a Bottles desktop shortcut refers to: its bottle and program name.
pub(crate) fn program_alias(bottle: &str, program: &str) -> String {
	format!("bottles:program:{bottle}/{program}")
}

/// Orders entries by name, then bottle, then UMU source id, then id, so the
/// application list is stable even when names and bottles repeat.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct EntrySortKey {
	name: String,
	bottle: String,
	source_id: String,
	id: String,
}

impl EntrySortKey {
	fn new(entry: &BottlesLibraryEntry) -> Self {
		Self {
			name: entry.name.clone().unwrap_or_default(),
			bottle: entry
				.bottle
				.as_ref()
				.and_then(|bottle| bottle.name.clone())
				.unwrap_or_default(),
			source_id: entry.source_id.clone().unwrap_or_default(),
			id: entry.id.clone().unwrap_or_default(),
		}
	}
}

/// Resolve a bottle's prefix directory, handling bottles created with a
/// custom path, whose `path` is absolute.
///
/// A relative path has to stay a plain directory name: it comes from
/// `library.yml`, and joining `..` onto the data directory would point the scan
/// at an unrelated part of the filesystem.
fn bottle_dir(data_dir: &Path, bottle: Option<&BottlesEntryBottle>) -> Option<PathBuf> {
	let bottle = bottle?;
	let path = bottle.path.as_deref().or(bottle.name.as_deref())?;
	let path = Path::new(path);

	if path.is_absolute() {
		return Some(path.to_path_buf());
	}

	let name = path.to_str()?;
	if !is_safe_path_component(name) {
		tracing::debug!("Ignoring bottle path {name:?} that is not a plain directory name.");
		return None;
	}

	Some(data_dir.join(BOTTLES_DIR).join(name))
}

fn umu_game_config(data_dir: &Path, source_id: &str) -> PathBuf {
	data_dir
		.join(UMU_DIR)
		.join(UMU_GAMES_DIR)
		.join(source_id)
		.join(UMU_GAME_CONFIG)
}

/// Read a UMU game's readiness state from Bottles' internal store.
///
/// A missing or unparsable state file reads as `None`, which the caller
/// treats as not ready, so this fails safe on Bottles layout changes.
fn read_umu_state(path: &Path) -> Option<String> {
	let contents = std::fs::read_to_string(path).ok()?;
	let config: UmuGameConfig = yaml_serde::from_str(&contents).ok()?;
	config.state
}

/// Box art candidates Bottles has on disk, best first.
///
/// Thumbnails Bottles curated (`grid:` for bottles, `umu-grid:` for UMU
/// games) win; the icon extracted from the program's executable follows.
fn artwork(data_dir: &Path, entry: &BottlesLibraryEntry, bottle_dir: Option<&Path>) -> Vec<PathBuf> {
	let mut candidates = Vec::new();
	if let Some(thumbnail) = entry.thumbnail.as_deref() {
		if let Some(file) = thumbnail.strip_prefix("grid:")
			&& let Some(bottle_dir) = bottle_dir
			&& is_safe_path_component(file)
		{
			candidates.push(bottle_dir.join(GRIDS_DIR).join(file));
		} else if let Some(file) = thumbnail.strip_prefix("umu-grid:")
			&& is_safe_path_component(file)
		{
			candidates.push(data_dir.join(UMU_DIR).join(UMU_COVERS_DIR).join(file));
		}
	}

	if let Some(icon) = entry.icon.as_deref().map(str::trim).filter(|icon| !icon.is_empty()) {
		let icon = PathBuf::from(icon);
		// Bottles writes absolute paths. A relative one belongs to the data
		// directory, not to whatever directory Pyroshine happened to start in.
		candidates.push(if icon.is_absolute() { icon } else { data_dir.join(icon) });
	}

	candidates.retain(|path| path.is_file());
	candidates
}

fn substitute(template: &[String], replacements: &[(&str, &str)]) -> Vec<String> {
	template
		.iter()
		.map(|token| substitute_token(token, replacements))
		.collect()
}

/// Expand the `{token}` placeholders of one argv element in a single pass.
///
/// One pass matters: replacing token by token over the whole string would expand
/// text that an earlier replacement inserted, so a bottle named `{name}` would
/// pull the entry's name into the command.
fn substitute_token(token: &str, replacements: &[(&str, &str)]) -> String {
	let mut expanded = String::with_capacity(token.len());
	let mut rest = token;

	while let Some(start) = rest.find('{') {
		expanded.push_str(&rest[..start]);
		let placeholder = &rest[start..];

		let Some(end) = placeholder.find('}') else {
			expanded.push_str(placeholder);
			return expanded;
		};

		// Keep the braces: callers pass the token exactly as it appears in the
		// template, so `{bottle}` matches `{bottle}`.
		let token = &placeholder[..=end];
		match replacements.iter().find(|(name, _)| *name == token) {
			Some((_, value)) => expanded.push_str(value),
			None => expanded.push_str(token),
		}

		rest = &placeholder[end + 1..];
	}

	expanded.push_str(rest);
	expanded
}

fn build_application(config: &BottlesApplicationScannerConfig, title: &str, command: Vec<String>) -> ApplicationConfig {
	ApplicationConfig {
		title: title.to_string(),
		mode: crate::session::application::SessionMode::Game,
		pre_command: config.pre_command.clone(),
		post_command: config.post_command.clone(),
		command,
		output_scale: None,
		boxart: None,
		stdout: config.stdout.clone(),
		stderr: config.stderr.clone(),
		launch_timeout_secs: config.launch_timeout_secs,
		frame_limit: config.frame_limit.clone(),
	}
}

/// Guard against a value from a Bottles file escaping the directory it is
/// joined into (thumbnail filenames, UMU game ids, relative bottle paths).
fn is_safe_path_component(value: &str) -> bool {
	!value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\'])
}

/// The Library file, bottle presence and UMU readiness.
pub(crate) fn watch_targets(config: &BottlesApplicationScannerConfig) -> Vec<WatchTarget> {
	let Some(data_dir) = expand_path(&config.data_dir) else {
		return Vec::new();
	};
	vec![
		WatchTarget::directory(data_dir.clone(), Filter::Names(&[LIBRARY_YML])),
		WatchTarget::probe(data_dir.join(BOTTLES_DIR)),
		WatchTarget::probe(data_dir.join(UMU_DIR).join(UMU_GAMES_DIR)),
	]
}

#[cfg(test)]
mod tests {
	use std::fs;

	use tempfile::tempdir;

	use super::*;

	const LIBRARY_ENTRY: &str = r#"
89ebedb3-0be5-453b-a39c-623c1c8bbed0:
  bottle:
    name: Gaming
    path: Gaming
  icon: /icons/hades.png
  id: abc-123
  name: Hades
  thumbnail: grid:cover.jpg
"#;

	fn config(data_dir: PathBuf) -> BottlesApplicationScannerConfig {
		BottlesApplicationScannerConfig {
			data_dir,
			command: vec![
				"bottles-cli".into(),
				"run".into(),
				"-b".into(),
				"{bottle}".into(),
				"--program-id".into(),
				"{program_id}".into(),
			],
			umu_command: vec![
				"bottles-cli".into(),
				"umu".into(),
				"run".into(),
				"--game".into(),
				"{umu_game}".into(),
			],
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
			frame_limit: Default::default(),
		}
	}

	fn write_library(data_dir: &Path, contents: &str) {
		fs::create_dir_all(data_dir).unwrap();
		fs::write(data_dir.join(LIBRARY_YML), contents).unwrap();
	}

	fn make_bottle(data_dir: &Path, path: &str) -> PathBuf {
		let dir = data_dir.join(BOTTLES_DIR).join(path);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join("bottle.yml"), "Name: test\n").unwrap();
		dir
	}

	fn write_umu_game(data_dir: &Path, source_id: &str, state: &str) {
		let dir = data_dir.join(UMU_DIR).join(UMU_GAMES_DIR).join(source_id);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join(UMU_GAME_CONFIG), format!("state: {state}\n")).unwrap();
	}

	fn discovered(scan: &SourceScan) -> Vec<&Discovered> {
		scan.units
			.iter()
			.filter_map(|unit| unit.result.as_ref().ok())
			.flatten()
			.collect()
	}

	#[test]
	fn scans_a_bottle_entry_with_grid_boxart() {
		let tmp = tempdir().unwrap();
		write_library(tmp.path(), LIBRARY_ENTRY);
		let bottle = make_bottle(tmp.path(), "Gaming");
		fs::create_dir_all(bottle.join(GRIDS_DIR)).unwrap();
		let cover = bottle.join(GRIDS_DIR).join("cover.jpg");
		fs::write(&cover, b"jpeg").unwrap();

		let scan = scan(&config(tmp.path().to_path_buf()));
		let found = discovered(&scan);
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].native_id, "abc-123");
		assert_eq!(found[0].application.title, "Hades");
		assert_eq!(
			found[0].application.command,
			["bottles-cli", "run", "-b", "Gaming", "--program-id", "abc-123"]
		);
		assert_eq!(found[0].application.boxart.as_deref(), Some(cover.as_path()));
		assert_eq!(found[0].aliases, ["bottles:program:Gaming/Hades"]);
	}

	#[test]
	fn a_missing_library_is_an_empty_unit() {
		let tmp = tempdir().unwrap();
		let scan = scan(&config(tmp.path().join("bottles")));
		assert!(scan.failure.is_none());
		assert_eq!(scan.units.len(), 1);
		assert_eq!(scan.units[0].result.as_ref().unwrap().len(), 0);
	}

	#[test]
	fn empty_and_null_libraries_have_no_entries() {
		for contents in ["", "{}\n", "~\n"] {
			let tmp = tempdir().unwrap();
			write_library(tmp.path(), contents);
			let scan = scan(&config(tmp.path().to_path_buf()));
			assert_eq!(scan.units[0].result.as_ref().unwrap().len(), 0, "{contents:?}");
		}
	}

	#[test]
	fn an_unparsable_library_keeps_its_previous_entries() {
		let tmp = tempdir().unwrap();
		write_library(tmp.path(), "entries: [unterminated");
		let scan = scan(&config(tmp.path().to_path_buf()));
		assert!(matches!(scan.units[0].result, Err(UnitError::Corrupt(_))));
	}

	#[test]
	fn skips_entries_without_identity_steam_entries_and_missing_bottles() {
		let tmp = tempdir().unwrap();
		write_library(
			tmp.path(),
			r#"
a:
  bottle: {name: Gaming, path: Gaming}
  id: no-name
b:
  bottle: {name: Gaming, path: Gaming}
  name: No id
c:
  bottle: {name: Gaming, path: Gaming}
  id: steam-1
  name: Through Steam
  steam: true
d:
  bottle: {name: Gone, path: Gone}
  id: gone-1
  name: Deleted bottle
e:
  bottle: {name: Gaming, path: ../escape}
  id: escape-1
  name: Escapes
"#,
		);
		make_bottle(tmp.path(), "Gaming");
		let scan = scan(&config(tmp.path().to_path_buf()));
		assert!(discovered(&scan).is_empty());
	}

	#[test]
	fn exposes_only_ready_umu_entries_when_a_command_is_configured() {
		let tmp = tempdir().unwrap();
		write_library(
			tmp.path(),
			r#"
a:
  id: umu-a
  name: Ready Game
  source: umu
  source_id: ready-game
b:
  id: umu-b
  name: Installing Game
  source: umu
  source_id: installing-game
c:
  id: umu-c
  name: No State
  source: umu
  source_id: no-state
"#,
		);
		write_umu_game(tmp.path(), "ready-game", "ready");
		write_umu_game(tmp.path(), "installing-game", "installing");

		let scan = scan(&config(tmp.path().to_path_buf()));
		let found = discovered(&scan);
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].native_id, "umu:ready-game");
		assert_eq!(
			found[0].application.command,
			["bottles-cli", "umu", "run", "--game", "ready-game"]
		);

		let mut without = config(tmp.path().to_path_buf());
		without.umu_command.clear();
		assert!(discovered(&super::scan(&without)).is_empty());
	}

	#[test]
	fn falls_back_to_the_entry_icon_and_ignores_escaping_thumbnails() {
		let tmp = tempdir().unwrap();
		let icon = tmp.path().join("hades.png");
		fs::write(&icon, b"png").unwrap();
		write_library(
			tmp.path(),
			&format!(
				r#"
a:
  bottle: {{name: Gaming, path: Gaming}}
  icon: {}
  id: abc-123
  name: Hades
  thumbnail: grid:../../outside.jpg
"#,
				icon.display()
			),
		);
		make_bottle(tmp.path(), "Gaming");
		let scan = scan(&config(tmp.path().to_path_buf()));
		assert_eq!(discovered(&scan)[0].application.boxart.as_deref(), Some(icon.as_path()));
	}

	#[test]
	fn a_program_listed_twice_keeps_distinct_identities() {
		let tmp = tempdir().unwrap();
		write_library(
			tmp.path(),
			r#"
first:
  bottle: {name: Gaming, path: Gaming}
  id: abc-123
  name: Hades
second:
  bottle: {name: Gaming, path: Gaming}
  id: abc-123
  name: Hades
"#,
		);
		make_bottle(tmp.path(), "Gaming");
		let scan = scan(&config(tmp.path().to_path_buf()));
		let ids: Vec<&str> = discovered(&scan).iter().map(|found| found.native_id.as_str()).collect();
		assert_eq!(ids, ["abc-123", "abc-123#second"]);
	}

	#[test]
	fn prefers_the_directory_that_contains_the_library() {
		let tmp = tempdir().unwrap();
		let native = tmp.path().join("native");
		let flatpak = tmp.path().join("flatpak");
		fs::create_dir_all(&native).unwrap();
		write_library(&flatpak, LIBRARY_ENTRY);
		assert_eq!(choose_data_dir(&native, Some(&flatpak)), flatpak);
		write_library(&native, LIBRARY_ENTRY);
		assert_eq!(choose_data_dir(&native, Some(&flatpak)), native);
	}

	#[test]
	fn substitution_does_not_expand_the_values_it_inserts() {
		assert_eq!(
			substitute_token("{bottle}:{name}", &[("{bottle}", "{name}"), ("{name}", "Hades")]),
			"{name}:Hades"
		);
		assert_eq!(substitute_token("{unknown}{", &[]), "{unknown}{");
	}

	#[test]
	fn watches_the_library_bottles_and_umu_state() {
		let tmp = tempdir().unwrap();
		let paths: Vec<PathBuf> = watch_targets(&config(tmp.path().to_path_buf()))
			.into_iter()
			.map(|target| target.path)
			.collect();
		assert_eq!(
			paths,
			[
				tmp.path().to_path_buf(),
				tmp.path().join(BOTTLES_DIR),
				tmp.path().join(UMU_DIR).join(UMU_GAMES_DIR),
			]
		);
	}
}
