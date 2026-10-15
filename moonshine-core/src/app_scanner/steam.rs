use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use steamlocate::{Library, SteamDir};

use super::watch::{Filter, WatchTarget};
use super::{ArtworkOrigin, Discovered, SourceScan, Unit, UnitError, expand_path};
use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteamApplicationScannerConfig {
	/// Path to a Steam library (ie. `~/.local/share/Steam`).
	pub library: PathBuf,

	/// The command to run.
	pub command: Vec<String>,

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
}

/// Steam tools that are installed like games but cannot be played.
const TOOL_PREFIXES: [&str; 3] = ["Proton", "Steam Linux Runtime", "Steamworks Common Redistributables"];

/// Box art Steam caches per app, best first: portrait capsules, then the
/// landscape header art (letterboxed into Moonlight's portrait tile).
const LIBRARY_CACHE_ART: [(&str, bool); 4] = [
	("library_600x900.jpg", true),
	("library_capsule.jpg", true),
	("library_header.jpg", false),
	("header.jpg", false),
];

/// Scan every library folder Steam lists, one unit per folder.
pub(crate) fn scan(config: &SteamApplicationScannerConfig) -> SourceScan {
	let Some(root) = expand_path(&config.library) else {
		return SourceScan::failed(UnitError::Unavailable(format!(
			"Cannot expand the Steam library path {}",
			config.library.display()
		)));
	};
	let steam_dir = match SteamDir::from_dir(&root) {
		Ok(steam_dir) => steam_dir,
		Err(error) => {
			tracing::warn!("Failed to locate Steam directory at {}: {error}", root.display());
			return SourceScan::failed(UnitError::Unavailable(format!(
				"No Steam installation at {}: {error}",
				root.display()
			)));
		},
	};
	let library_paths = match steam_dir.library_paths() {
		Ok(paths) => paths,
		Err(error) => {
			// Usually Steam rewriting libraryfolders.vdf; retried shortly.
			tracing::warn!("Failed to list Steam libraries: {error}");
			return SourceScan::failed(UnitError::Corrupt(format!(
				"Cannot read Steam's library folder list: {error}"
			)));
		},
	};
	let art = ArtLocations::new(&root);

	let units = library_paths
		.iter()
		.map(|path| {
			let key = path.display().to_string();
			let library = match Library::from_dir(path) {
				Ok(library) => library,
				Err(error) => {
					tracing::warn!("Failed to read Steam library {}: {error}", path.display());
					return Unit::failed(
						key,
						UnitError::Unavailable(format!("Steam library {} is not accessible: {error}", path.display())),
					);
				},
			};
			let mut unit = Unit::ok(key, Vec::new());
			let mut entries = Vec::new();
			for &app_id in library.app_ids() {
				let app = match library.app(app_id) {
					Some(Ok(app)) => app,
					Some(Err(error)) => {
						tracing::warn!("Failed to read the manifest of Steam app {app_id}: {error}");
						unit.retain.push(app_id.to_string());
						continue;
					},
					None => continue,
				};
				// Skip apps without a name.
				let Some(title) = app.name.clone() else {
					tracing::debug!("Skipping app {} without a name.", app.app_id);
					continue;
				};
				if TOOL_PREFIXES.iter().any(|prefix| title.starts_with(prefix)) {
					continue;
				}
				let application = ApplicationConfig {
					title,
					pre_command: config.pre_command.clone(),
					post_command: config.post_command.clone(),
					command: config
						.command
						.iter()
						.map(|cmd| cmd.replace("{game_id}", &app.app_id.to_string()))
						.collect(),
					output_scale: None,
					boxart: None,
					stdout: config.stdout.clone(),
					stderr: config.stderr.clone(),
					launch_timeout_secs: config.launch_timeout_secs,
				};
				let (candidates, origin) = art.find(app.app_id);
				if candidates.is_empty() {
					tracing::debug!("No boxart found for Steam app '{}'.", application.title);
				}
				entries.push(Discovered::new(app.app_id.to_string(), application).with_artwork(candidates, origin));
			}
			entries.sort_by(|a, b| a.native_id.cmp(&b.native_id));
			unit.result = Ok(entries);
			unit
		})
		.collect();

	SourceScan { units, failure: None }
}

/// `libraryfolders.vdf` and every library's manifests, plus art locations
/// for the periodic check.
pub(crate) fn watch_targets(config: &SteamApplicationScannerConfig) -> Vec<WatchTarget> {
	let Some(root) = expand_path(&config.library) else {
		return Vec::new();
	};
	let mut targets = vec![WatchTarget::directory(
		root.join("steamapps"),
		Filter::Manifests(&["libraryfolders.vdf"], ".acf"),
	)];
	if let Ok(paths) = SteamDir::from_dir(&root).and_then(|steam_dir| steam_dir.library_paths()) {
		for path in paths {
			let steamapps = path.join("steamapps");
			if steamapps != root.join("steamapps") {
				targets.push(WatchTarget::directory(steamapps, Filter::Suffix(".acf")));
			}
		}
	}
	// Steam downloads art when the library first shows a game; the periodic
	// check notices a new per-app directory or custom grid image.
	targets.push(WatchTarget::probe(root.join("appcache/librarycache")));
	for grid in ArtLocations::new(&root).grids {
		targets.push(WatchTarget::probe(grid));
	}
	targets
}

/// Where Steam keeps box art.
struct ArtLocations {
	librarycache: PathBuf,
	/// `userdata/<account>/config/grid`: art the user set in Steam.
	grids: Vec<PathBuf>,
	/// `$XDG_DATA_HOME/icons/hicolor`, where Steam puts shortcut icons.
	icons: Option<PathBuf>,
}

impl ArtLocations {
	fn new(root: &Path) -> Self {
		let mut grids: Vec<PathBuf> = std::fs::read_dir(root.join("userdata"))
			.map(|entries| {
				entries
					.flatten()
					.map(|entry| entry.path().join("config/grid"))
					.filter(|grid| grid.is_dir())
					.collect()
			})
			.unwrap_or_default();
		grids.sort();
		Self {
			librarycache: root.join("appcache/librarycache"),
			grids,
			icons: super::icons::xdg_data_home().map(|home| home.join("icons/hicolor")),
		}
	}

	/// Existing art for `app_id`, best first.
	fn find(&self, app_id: u32) -> (Vec<PathBuf>, ArtworkOrigin) {
		let mut candidates = Vec::new();
		let mut origin = ArtworkOrigin::Steam;
		for grid in &self.grids {
			for extension in ["png", "jpg", "jpeg", "webp"] {
				let path = grid.join(format!("{app_id}p.{extension}"));
				if path.is_file() {
					if candidates.is_empty() {
						origin = ArtworkOrigin::SteamCustom;
					}
					candidates.push(path);
				}
			}
		}
		// Current clients keep art in `<app_id>/` or one hashed directory
		// below it; older ones as `<app_id>_<name>` next to the directories.
		let directory = self.librarycache.join(app_id.to_string());
		let mut directories = vec![directory.clone()];
		if let Ok(entries) = std::fs::read_dir(&directory) {
			let mut nested: Vec<PathBuf> = entries
				.flatten()
				.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
				.map(|entry| entry.path())
				.collect();
			nested.sort();
			directories.extend(nested);
		}
		for (name, _portrait) in LIBRARY_CACHE_ART {
			if let Some(path) = directories
				.iter()
				.map(|directory| directory.join(name))
				.chain(std::iter::once(self.librarycache.join(format!("{app_id}_{name}"))))
				.find(|path| path.is_file())
			{
				candidates.push(path);
			}
		}
		// Last resort: the icon Steam installs for a game's desktop shortcut.
		if let Some(icons) = &self.icons
			&& let Some(icon) = STEAM_ICON_SIZES
				.iter()
				.map(|size| icons.join(format!("{size}/apps/steam_icon_{app_id}.png")))
				.find(|path| path.is_file())
		{
			candidates.push(icon);
		}
		(candidates, origin)
	}
}

/// Sizes Steam installs shortcut icons in, largest first.
const STEAM_ICON_SIZES: [&str; 6] = ["256x256", "128x128", "96x96", "64x64", "48x48", "32x32"];

#[cfg(test)]
pub(crate) mod tests {
	use std::fs;

	use super::*;

	fn manifest(app_id: u32, name: &str) -> String {
		format!(
			"\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"{name}\"\n}}\n"
		)
	}

	/// A Steam root with `libraries` as library folders (the first is the root).
	pub(crate) fn steam_root(root: &Path, libraries: &[&Path]) {
		fs::create_dir_all(root.join("steamapps")).unwrap();
		let mut vdf = String::from("\"libraryfolders\"\n{\n");
		for (index, library) in libraries.iter().enumerate() {
			fs::create_dir_all(library.join("steamapps")).unwrap();
			vdf += &format!(
				"\t\"{index}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n",
				library.display()
			);
		}
		vdf += "}\n";
		fs::write(root.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
	}

	pub(crate) fn install(library: &Path, app_id: u32, name: &str) {
		fs::write(
			library.join(format!("steamapps/appmanifest_{app_id}.acf")),
			manifest(app_id, name),
		)
		.unwrap();
	}

	pub(crate) fn config(root: &Path) -> SteamApplicationScannerConfig {
		SteamApplicationScannerConfig {
			library: root.to_path_buf(),
			command: vec!["steam".into(), "steam://rungameid/{game_id}".into()],
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
		}
	}

	#[test]
	fn scans_every_library_folder_and_skips_tools() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("Steam");
		let second = directory.path().join("Games");
		steam_root(&root, &[&root, &second]);
		install(&root, 620, "Portal 2");
		install(&root, 1493710, "Proton Experimental");
		install(&second, 400, "Portal");

		let result = scan(&config(&root));
		assert!(result.failure.is_none());
		assert_eq!(result.units.len(), 2);
		let titles: Vec<_> = result.applications().into_iter().map(|app| app.title).collect();
		assert_eq!(titles, vec!["Portal 2", "Portal"]);
		assert_eq!(
			result.applications()[0].command,
			vec!["steam".to_string(), "steam://rungameid/620".to_string()]
		);
	}

	/// An inaccessible library folder is an error of its unit only; a
	/// half-written manifest keeps that one game.
	#[test]
	fn unreadable_libraries_and_manifests_are_reported_not_dropped() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("Steam");
		let unmounted = directory.path().join("Unmounted");
		steam_root(&root, &[&root, &unmounted]);
		install(&root, 620, "Portal 2");
		fs::write(
			root.join("steamapps/appmanifest_400.acf"),
			"\"AppState\"\n{\n\t\"appid\"",
		)
		.unwrap();
		fs::remove_dir_all(&unmounted).unwrap();

		let result = scan(&config(&root));
		assert_eq!(result.units[0].retain, vec!["400".to_string()]);
		assert_eq!(result.units[0].result.as_ref().unwrap().len(), 1);
		assert!(matches!(result.units[1].result, Err(UnitError::Unavailable(_))));
	}

	#[test]
	fn a_half_written_library_list_fails_the_whole_source() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("Steam");
		steam_root(&root, &[&root]);
		fs::write(
			root.join("steamapps/libraryfolders.vdf"),
			"\"libraryfolders\"\n{\n\t\"0\"\n\t{",
		)
		.unwrap();
		let result = scan(&config(&root));
		assert!(result.failure.is_some(), "{result:?}");
	}

	#[test]
	fn prefers_custom_grid_art_then_portrait_cache_art() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("Steam");
		steam_root(&root, &[&root]);
		install(&root, 620, "Portal 2");
		install(&root, 400, "Portal");
		install(&root, 70, "Half-Life");
		let cache = root.join("appcache/librarycache");
		fs::create_dir_all(cache.join("620/0123abcd")).unwrap();
		fs::write(cache.join("620/0123abcd/library_600x900.jpg"), "jpg").unwrap();
		fs::write(cache.join("620/header.jpg"), "jpg").unwrap();
		fs::create_dir_all(cache.join("400")).unwrap();
		fs::write(cache.join("400/header.jpg"), "jpg").unwrap();
		let grid = root.join("userdata/1234/config/grid");
		fs::create_dir_all(&grid).unwrap();
		fs::write(grid.join("620p.png"), "png").unwrap();

		let result = scan(&config(&root));
		let entries = result.units[0].result.as_ref().unwrap();
		let entry = |id: &str| entries.iter().find(|entry| entry.native_id == id).unwrap();
		assert_eq!(entry("620").application.boxart, Some(grid.join("620p.png")));
		assert_eq!(entry("620").artwork_origin, Some(ArtworkOrigin::SteamCustom));
		assert_eq!(
			entry("620").artwork_fallbacks,
			vec![
				cache.join("620/0123abcd/library_600x900.jpg"),
				cache.join("620/header.jpg")
			]
		);
		assert_eq!(entry("400").application.boxart, Some(cache.join("400/header.jpg")));
		assert_eq!(entry("70").application.boxart, None);
		assert_eq!(entry("70").artwork_origin, None);
	}
}
