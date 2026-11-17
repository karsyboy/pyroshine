//! Faugus Launcher library (`games.json`).
//!
//! Ported from upstream Moonshine's Faugus scanner (moonshine#264, by István
//! Urbán) onto the fork's unit/catalog model.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::watch::{Filter, WatchTarget};
use super::{ArtworkOrigin, Discovered, SourceScan, Unit, UnitError, expand_path};
use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaugusApplicationScannerConfig {
	/// Path to the Faugus Launcher data directory, the one holding `games.json`
	/// (ie. `~/.local/share/faugus-launcher`).
	#[serde(default = "default_data_dir")]
	pub data_dir: PathBuf,

	/// The command to run.
	pub command: Vec<String>,

	/// Also scan games that are hidden in the Faugus library.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub include_hidden: bool,

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

	/// Extra environment variables for each discovered application. They
	/// override the variables Pyroshine sets, such as `DISPLAY`.
	#[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
	pub env: std::collections::BTreeMap<String, String>,
}

/// The library, inside the data directory.
const GAMES_JSON: &str = "games.json";

/// Art directories inside the data directory, each holding a `<gameid>.png` per game.
///
/// Current releases keep portrait covers in `covers/` and wide banners in
/// `banners/`. Older releases kept the portrait art in `banners/` (moved to
/// `covers/` on upgrade), so looking there second serves both layouts.
const COVERS_DIR: &str = "covers";
const BANNERS_DIR: &str = "banners";
const ICONS_DIR: &str = "icons";

/// Faugus' runner for games it hands over to Steam.
const STEAM_RUNNER: &str = "Steam";

const FLATPAK_APP_ID: &str = "io.github.Faugus.faugus-launcher";

/// Where Faugus may keep its library, current layout first.
///
/// Current releases store `games.json` in the data directory and move it there on
/// first start from the configuration directory, where older releases kept it.
fn candidate_data_dirs() -> Vec<PathBuf> {
	let flatpak = dirs::home_dir().map(|home| home.join(".var/app").join(FLATPAK_APP_ID));
	[
		dirs::data_dir(),
		flatpak.as_ref().map(|flatpak| flatpak.join("data")),
		dirs::config_dir(),
		flatpak.as_ref().map(|flatpak| flatpak.join("config")),
	]
	.into_iter()
	.flatten()
	.map(|directory| directory.join("faugus-launcher"))
	.collect()
}

/// The first candidate that holds a library, or else the first candidate.
///
/// A native install wins over a Flatpak one, and either wins over a library left
/// behind by an older release.
fn pick_data_dir(candidates: Vec<PathBuf>) -> PathBuf {
	candidates
		.iter()
		.find(|directory| directory.join(GAMES_JSON).is_file())
		.or(candidates.first())
		.cloned()
		.unwrap_or_else(|| PathBuf::from("faugus-launcher"))
}

fn default_data_dir() -> PathBuf {
	pick_data_dir(candidate_data_dirs())
}

/// An entry of `games.json`, reduced to the fields Pyroshine uses.
#[derive(Debug)]
struct FaugusGame {
	game_id: String,
	title: String,

	/// The game's executable.
	path: Option<String>,

	runner: Option<String>,
	hidden: bool,

	/// Path of the portrait cover art.
	cover: Option<String>,

	/// Path of the game's icon.
	icon: Option<String>,
}

impl FaugusGame {
	/// Read an entry leniently.
	///
	/// Faugus is written in Python and has renamed fields across releases, so a
	/// field holding something unexpected counts as absent instead of dropping the
	/// game. Only an entry without an id or a title is skipped.
	fn from_json(entry: &Value) -> Option<Self> {
		let text = |key: &str| {
			entry
				.get(key)
				.and_then(Value::as_str)
				.filter(|value| !value.trim().is_empty())
				.map(str::to_string)
		};

		Some(Self {
			game_id: text("gameid")?,
			title: text("title")?,
			path: text("path"),
			runner: text("runner"),
			hidden: entry.get("hidden").and_then(Value::as_bool).unwrap_or(false),
			// Older releases named the cover field `banner`.
			cover: text("cover").or_else(|| text("banner")),
			icon: text("icon"),
		})
	}
}

/// Scan `games.json` as one unit.
///
/// A missing library means Faugus is not installed (no games). A library that
/// cannot be read or parsed, typically while Faugus rewrites it, is an error of
/// the unit, so its games are kept until a retry succeeds.
pub(crate) fn scan(config: &FaugusApplicationScannerConfig) -> SourceScan {
	let Some(data_dir) = expand_path(&config.data_dir) else {
		return SourceScan::failed(UnitError::Unavailable(format!(
			"Cannot expand the Faugus data directory {}",
			config.data_dir.display()
		)));
	};
	let games_json = data_dir.join(GAMES_JSON);
	let key = games_json.display().to_string();

	let contents = match std::fs::read_to_string(&games_json) {
		Ok(contents) => contents,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			tracing::debug!(
				"Faugus library not found at {:?}, no Faugus games will be scanned.",
				games_json
			);
			return SourceScan {
				units: vec![Unit::ok(key, Vec::new())],
				failure: None,
			};
		},
		Err(error) => {
			return SourceScan {
				units: vec![Unit::failed(key, UnitError::io(games_json.display(), &error))],
				failure: None,
			};
		},
	};
	let entries: Vec<Value> = match serde_json::from_str(&contents) {
		Ok(entries) => entries,
		Err(error) => {
			tracing::warn!("Failed to parse the Faugus library at {:?}: {error}", games_json);
			return SourceScan {
				units: vec![Unit::failed(
					key,
					UnitError::Corrupt(format!("Cannot parse the Faugus library: {error}")),
				)],
				failure: None,
			};
		},
	};

	let mut applications = Vec::new();
	let mut ids = HashSet::new();
	let (mut invalid, mut hidden, mut steam) = (0, 0, 0);

	for (index, entry) in entries.iter().enumerate() {
		let Some(game) = FaugusGame::from_json(entry) else {
			invalid += 1;
			continue;
		};

		if game.hidden && !config.include_hidden {
			hidden += 1;
			continue;
		}

		// Faugus hands these to Steam and exits five seconds later, which would end
		// the session while the game runs under a Steam outside the session. The
		// Steam scanner launches them properly.
		if game.runner.as_deref() == Some(STEAM_RUNNER) {
			steam += 1;
			continue;
		}

		let application = ApplicationConfig {
			title: game.title.clone(),
			pre_command: config.pre_command.clone(),
			post_command: config.post_command.clone(),
			command: config
				.command
				.iter()
				.map(|arg| arg.replace("{game_id}", &game.game_id))
				.collect(),
			working_directory: game_directory(&game),
			stdout: config.stdout.clone(),
			stderr: config.stderr.clone(),
			launch_timeout_secs: config.launch_timeout_secs,
			frame_limit: config.frame_limit.clone(),
			env: config.env.clone(),
			..Default::default()
		};
		// Faugus generates unique ids, but a hand-edited library may repeat one.
		let native_id = if ids.insert(game.game_id.clone()) {
			game.game_id.clone()
		} else {
			format!("{}#{index}", game.game_id)
		};
		applications.push(
			Discovered::new(native_id, application)
				.with_artwork(find_boxart(&data_dir, &game), ArtworkOrigin::Launcher),
		);
	}

	tracing::debug!(
		"Scanned {} Faugus games ({} hidden, {} Steam, {} invalid entries skipped).",
		applications.len(),
		hidden,
		steam,
		invalid
	);

	SourceScan {
		units: vec![Unit::ok(key, applications)],
		failure: None,
	}
}

/// The library file and the art directories.
pub(crate) fn watch_targets(config: &FaugusApplicationScannerConfig) -> Vec<WatchTarget> {
	let Some(data_dir) = expand_path(&config.data_dir) else {
		return Vec::new();
	};
	let mut targets = vec![WatchTarget::directory(data_dir.clone(), Filter::Names(&[GAMES_JSON]))];
	targets.extend([COVERS_DIR, BANNERS_DIR, ICONS_DIR].map(|directory| WatchTarget::probe(data_dir.join(directory))));
	targets
}

/// Identity of the Faugus game a launcher command starts
/// (`faugus-launcher --game <id>`, also through `flatpak run`).
pub(crate) fn shortcut_target(command: &[String]) -> Option<String> {
	// The executable, `--command=/app/bin/faugus-launcher` or the Flatpak app ID.
	let launcher = command
		.iter()
		.position(|argument| argument.ends_with("faugus-launcher"))?;
	let mut arguments = command[launcher + 1..].iter();
	arguments.find(|argument| *argument == "--game")?;
	let id = arguments.next()?.trim();
	(!id.is_empty()).then(|| format!("faugus:{id}"))
}

/// Expand `~` and environment variables, as Faugus does, keeping the variables if one is unset.
fn expand(path: &str) -> PathBuf {
	match shellexpand::full(path) {
		Ok(expanded) => PathBuf::from(expanded.as_ref()),
		Err(_) => PathBuf::from(shellexpand::tilde(path).as_ref()),
	}
}

/// The directory a game runs in when started from Faugus' window: the one holding its executable.
///
/// `faugus-launcher --game` keeps the caller's directory instead, which for the
/// units Pyroshine starts is the home directory, and games that load their data
/// through relative paths quit right after starting there.
fn game_directory(game: &FaugusGame) -> Option<PathBuf> {
	let executable = expand(game.path.as_deref()?);
	let directory = executable.parent()?;
	(directory.is_absolute() && directory.is_dir()).then(|| directory.to_path_buf())
}

/// The game's art in order of preference: the portrait cover over a banner or the icon.
///
/// The paths `games.json` records are tried before the art directories, where a
/// game's art is named after its id.
fn find_boxart(data_dir: &Path, game: &FaugusGame) -> Vec<PathBuf> {
	let recorded = |path: &Option<String>| path.as_deref().map(expand).filter(|path| path.is_file());
	let stored = |directory: &str| {
		// The id becomes a file name, so it must not lead out of the directory.
		(!game.game_id.contains('/') && game.game_id != "..")
			.then(|| data_dir.join(directory).join(format!("{}.png", game.game_id)))
			.filter(|path| path.is_file())
	};

	let mut candidates = Vec::new();
	for candidate in [
		recorded(&game.cover),
		stored(COVERS_DIR),
		stored(BANNERS_DIR),
		recorded(&game.icon),
		stored(ICONS_DIR),
	]
	.into_iter()
	.flatten()
	{
		if !candidates.contains(&candidate) {
			candidates.push(candidate);
		}
	}
	candidates
}

#[cfg(test)]
mod tests {
	use std::fs;

	use tempfile::tempdir;

	use super::*;
	use crate::app_scanner::ApplicationScannerConfig;

	fn scanner_config(data_dir: PathBuf) -> FaugusApplicationScannerConfig {
		FaugusApplicationScannerConfig {
			data_dir,
			command: vec![
				"/usr/bin/faugus-launcher".to_string(),
				"--game".to_string(),
				"{game_id}".to_string(),
			],
			include_hidden: false,
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
			frame_limit: Default::default(),
			env: Default::default(),
		}
	}

	fn write_library(data_dir: &Path, games: Value) {
		fs::create_dir_all(data_dir).unwrap();
		fs::write(data_dir.join(GAMES_JSON), games.to_string()).unwrap();
	}

	/// Create a game's executable in its own directory, returning the directory.
	fn install_game(root: &Path, name: &str) -> PathBuf {
		let directory = root.join(name);
		fs::create_dir_all(&directory).unwrap();
		fs::write(directory.join("game.exe"), b"MZ").unwrap();
		directory
	}

	fn write_art(data_dir: &Path, directory: &str, game_id: &str) -> PathBuf {
		let path = data_dir.join(directory).join(format!("{game_id}.png"));
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, b"png").unwrap();
		path
	}

	fn entries(config: &FaugusApplicationScannerConfig) -> Vec<Discovered> {
		let result = scan(config);
		assert!(result.failure.is_none());
		result.units[0].result.clone().unwrap()
	}

	fn titles(entries: &[Discovered]) -> Vec<&str> {
		entries.iter().map(|entry| entry.application.title.as_str()).collect()
	}

	#[test]
	fn starts_games_in_the_directory_of_their_executable() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		let game_dir = install_game(tempdir.path(), "Kane and Lynch Dead Men");
		write_library(
			&data_dir,
			serde_json::json!([{
				"gameid": "kane-lynch-dead-men",
				"title": "Kane & Lynch: Dead Men",
				"path": game_dir.join("game.exe"),
				"runner": "Proton-CachyOS Latest",
			}]),
		);

		let found = entries(&scanner_config(data_dir));

		assert_eq!(titles(&found), ["Kane & Lynch: Dead Men"]);
		assert_eq!(found[0].native_id, "kane-lynch-dead-men");
		assert_eq!(
			found[0].application.command,
			["/usr/bin/faugus-launcher", "--game", "kane-lynch-dead-men"]
		);
		assert_eq!(found[0].application.working_directory, Some(game_dir));
	}

	#[test]
	fn has_no_working_directory_without_a_game_directory() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		write_library(
			&data_dir,
			serde_json::json!([
				{ "gameid": "no-path", "title": "No Path" },
				{ "gameid": "gone", "title": "Gone", "path": tempdir.path().join("missing/game.exe") },
				{ "gameid": "relative", "title": "Relative", "path": "game.exe" },
			]),
		);

		let found = entries(&scanner_config(data_dir));

		assert_eq!(titles(&found), ["No Path", "Gone", "Relative"]);
		for entry in &found {
			assert_eq!(entry.application.working_directory, None, "{}", entry.application.title);
		}
	}

	#[test]
	fn skips_hidden_games_unless_asked_and_always_steam_games() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		write_library(
			&data_dir,
			serde_json::json!([
				{ "gameid": "shown", "title": "Shown" },
				{ "gameid": "hidden", "title": "Hidden", "hidden": true },
				{ "gameid": "portal-2", "title": "Portal 2", "runner": "Steam" },
			]),
		);

		let mut config = scanner_config(data_dir);
		assert_eq!(titles(&entries(&config)), ["Shown"]);

		config.include_hidden = true;
		assert_eq!(titles(&entries(&config)), ["Shown", "Hidden"]);
	}

	#[test]
	fn reads_entries_leniently_and_keeps_ids_unique() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		write_library(
			&data_dir,
			serde_json::json!([
				// Unexpected types count as absent rather than dropping the game.
				{ "gameid": "odd", "title": "Odd Types", "hidden": "yes", "path": 7, "cover": false },
				{ "title": "No Id" },
				{ "gameid": "no-title" },
				{ "gameid": "blank", "title": "   " },
				"not a game",
				{ "gameid": "odd", "title": "Odd Again" },
			]),
		);

		let found = entries(&scanner_config(data_dir));

		assert_eq!(titles(&found), ["Odd Types", "Odd Again"]);
		assert_eq!(found[0].native_id, "odd");
		assert_eq!(found[1].native_id, "odd#5");
		assert_eq!(found[0].application.boxart, None);
	}

	#[test]
	fn prefers_the_recorded_cover() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		let recorded = tempdir.path().join("elsewhere/cover.png");
		fs::create_dir_all(recorded.parent().unwrap()).unwrap();
		fs::write(&recorded, b"png").unwrap();
		let stored = write_art(&data_dir, COVERS_DIR, "hades");
		write_library(
			&data_dir,
			serde_json::json!([{ "gameid": "hades", "title": "Hades", "cover": recorded }]),
		);

		let found = entries(&scanner_config(data_dir));
		assert_eq!(found[0].application.boxart, Some(recorded));
		assert_eq!(found[0].artwork_fallbacks, [stored]);
		assert_eq!(found[0].artwork_origin, Some(ArtworkOrigin::Launcher));
	}

	#[test]
	fn falls_back_from_covers_to_banners_to_icons() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		write_library(
			&data_dir,
			serde_json::json!([
				// Recorded paths that no longer exist are passed over.
				{ "gameid": "all", "title": "All", "cover": tempdir.path().join("gone.png") },
				{ "gameid": "banner", "title": "Banner" },
				{ "gameid": "icon", "title": "Icon" },
				{ "gameid": "none", "title": "None" },
			]),
		);
		let cover = write_art(&data_dir, COVERS_DIR, "all");
		write_art(&data_dir, BANNERS_DIR, "all");
		write_art(&data_dir, ICONS_DIR, "all");
		let banner = write_art(&data_dir, BANNERS_DIR, "banner");
		write_art(&data_dir, ICONS_DIR, "banner");
		let icon = write_art(&data_dir, ICONS_DIR, "icon");

		let found = entries(&scanner_config(data_dir));
		let boxart: Vec<_> = found.iter().map(|entry| entry.application.boxart.clone()).collect();

		assert_eq!(boxart, [Some(cover), Some(banner), Some(icon), None]);
		assert_eq!(found[0].artwork_fallbacks.len(), 2);
	}

	#[test]
	fn reads_the_cover_field_of_older_releases() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		let banner = write_art(&data_dir, BANNERS_DIR, "hades");
		write_library(
			&data_dir,
			serde_json::json!([{ "gameid": "hades", "title": "Hades", "banner": banner }]),
		);

		let found = entries(&scanner_config(data_dir));
		assert_eq!(found[0].application.boxart, Some(banner));
	}

	#[test]
	fn keeps_art_lookups_inside_the_data_directory() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		write_art(tempdir.path(), "outside", "secret");
		write_library(
			&data_dir,
			serde_json::json!([{ "gameid": "../../outside/secret", "title": "Escape" }]),
		);

		let found = entries(&scanner_config(data_dir));
		assert_eq!(found[0].application.boxart, None);
	}

	#[test]
	fn a_missing_library_is_empty_and_a_malformed_one_is_an_error() {
		let tempdir = tempdir().unwrap();
		assert!(entries(&scanner_config(tempdir.path().join("missing"))).is_empty());

		let data_dir = tempdir.path().join("faugus-launcher");
		fs::create_dir_all(&data_dir).unwrap();
		fs::write(data_dir.join(GAMES_JSON), "{ not json").unwrap();
		let result = scan(&scanner_config(data_dir));
		assert!(matches!(result.units[0].result, Err(UnitError::Corrupt(_))));
	}

	#[test]
	fn watches_the_library_and_its_art() {
		let tempdir = tempdir().unwrap();
		let data_dir = tempdir.path().join("faugus-launcher");
		let paths: Vec<PathBuf> = watch_targets(&scanner_config(data_dir.clone()))
			.into_iter()
			.map(|target| target.path)
			.collect();
		assert_eq!(
			paths,
			[
				data_dir.clone(),
				data_dir.join(COVERS_DIR),
				data_dir.join(BANNERS_DIR),
				data_dir.join(ICONS_DIR)
			]
		);
	}

	#[test]
	fn picks_the_first_data_directory_holding_a_library() {
		let tempdir = tempdir().unwrap();
		let candidates: Vec<PathBuf> = ["native", "flatpak", "legacy"]
			.iter()
			.map(|name| tempdir.path().join(name))
			.collect();

		assert_eq!(pick_data_dir(candidates.clone()), candidates[0]);

		write_library(&candidates[2], serde_json::json!([]));
		assert_eq!(pick_data_dir(candidates.clone()), candidates[2]);

		write_library(&candidates[1], serde_json::json!([]));
		assert_eq!(pick_data_dir(candidates.clone()), candidates[1]);
	}

	#[test]
	fn recognizes_faugus_shortcuts() {
		let command = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
		assert_eq!(
			shortcut_target(&command(&["/usr/bin/faugus-launcher", "--game", "hades"])),
			Some("faugus:hades".into())
		);
		assert_eq!(
			shortcut_target(&command(&[
				"flatpak",
				"run",
				"--command=/app/bin/faugus-launcher",
				"io.github.Faugus.faugus-launcher",
				"--game",
				"hades"
			])),
			Some("faugus:hades".into())
		);
		assert_eq!(shortcut_target(&command(&["faugus-launcher"])), None);
		assert_eq!(shortcut_target(&command(&["game", "--game", "hades"])), None);
	}

	#[test]
	fn parses_a_minimal_scanner_entry() {
		let config: ApplicationScannerConfig = toml::from_str(
			r#"
type = "faugus"
command = ["faugus-launcher", "--game", "{game_id}"]
"#,
		)
		.unwrap();

		let ApplicationScannerConfig::Faugus(config) = config else {
			panic!("expected a Faugus scanner, got {config:?}");
		};
		assert!(!config.include_hidden);
		assert_eq!(config.launch_timeout_secs, 2);
	}
}
