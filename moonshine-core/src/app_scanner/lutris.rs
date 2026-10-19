use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::watch::{Filter, WatchTarget};
use super::{ArtworkOrigin, Discovered, SourceScan, Unit, UnitError, expand_path};
use crate::session::application::ApplicationConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LutrisApplicationScannerConfig {
	/// Path to the Lutris database (ie. `~/.local/share/lutris/pga.db`).
	#[serde(default = "default_pga_db")]
	pub pga_db: PathBuf,

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

	/// Overrides of `[frame_limit]` for each discovered application.
	#[serde(flatten)]
	pub frame_limit: crate::session::frame_limit::FrameLimitOverride,
}

fn default_pga_db() -> PathBuf {
	dirs::data_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("lutris")
		.join("pga.db")
}

fn is_valid_slug(slug: &str) -> bool {
	slug.chars()
		.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Scan the Lutris database as one unit.
///
/// A missing database means Lutris is not installed (no games). A database
/// that cannot be opened or queried, typically while Lutris writes to it, is
/// an error of the unit, so its games are kept until a retry succeeds.
pub(crate) fn scan(config: &LutrisApplicationScannerConfig) -> SourceScan {
	let Some(db_path) = expand_path(&config.pga_db) else {
		return SourceScan::failed(UnitError::Unavailable(format!(
			"Cannot expand the Lutris database path {}",
			config.pga_db.display()
		)));
	};
	let key = db_path.display().to_string();

	match std::fs::metadata(&db_path) {
		Ok(_) => {},
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			tracing::debug!(
				"Lutris database not found at {:?}, no Lutris games will be scanned.",
				db_path
			);
			return SourceScan {
				units: vec![Unit::ok(key, Vec::new())],
				failure: None,
			};
		},
		Err(error) => {
			return SourceScan {
				units: vec![Unit::failed(key, UnitError::io(db_path.display(), &error))],
				failure: None,
			};
		},
	}

	let games = match read_games(&db_path) {
		Ok(games) => games,
		Err(error) => {
			tracing::warn!("Failed to read the Lutris database at {:?}: {error}", db_path);
			return SourceScan {
				units: vec![Unit::failed(
					key,
					UnitError::Corrupt(format!("Cannot read the Lutris database: {error}")),
				)],
				failure: None,
			};
		},
	};

	let art = ArtLocations::new(&db_path);
	let mut applications = Vec::new();
	let mut slugs = HashSet::new();
	let mut skipped = 0;

	for (id, name, slug) in games {
		if !is_valid_slug(&slug) {
			tracing::debug!("Skipping game '{}' with invalid slug '{}'.", name, slug);
			skipped += 1;
			continue;
		}

		let application = ApplicationConfig {
			title: name,
			pre_command: config.pre_command.clone(),
			post_command: config.post_command.clone(),
			command: config.command.iter().map(|cmd| cmd.replace("{slug}", &slug)).collect(),
			output_scale: None,
			boxart: None,
			stdout: config.stdout.clone(),
			stderr: config.stderr.clone(),
			launch_timeout_secs: config.launch_timeout_secs,
			frame_limit: config.frame_limit.clone(),
		};
		// The same game can be installed twice (different runners).
		let native_id = if slugs.insert(slug.clone()) {
			slug.clone()
		} else {
			format!("{slug}#{id}")
		};
		let mut discovered =
			Discovered::new(native_id, application).with_artwork(art.find(&slug), ArtworkOrigin::Launcher);
		discovered.aliases.push(format!("lutris:id:{id}"));
		applications.push(discovered);
	}

	tracing::debug!("Scanned {} Lutris games ({} skipped).", applications.len(), skipped);

	SourceScan {
		units: vec![Unit::ok(key, applications)],
		failure: None,
	}
}

fn read_games(db_path: &Path) -> rusqlite::Result<Vec<(i64, String, String)>> {
	// Read-only: never take a write lock or create files beside Lutris's database.
	let conn = Connection::open_with_flags(
		db_path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)?;
	conn.busy_timeout(std::time::Duration::from_secs(5))?;
	let mut stmt = conn.prepare("SELECT id, name, slug FROM games WHERE installed = 1 ORDER BY name, id")?;
	let games = stmt
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
		.collect::<rusqlite::Result<Vec<_>>>()?;
	Ok(games)
}

/// The database's directory, filtered to the database and its journals.
pub(crate) fn watch_targets(config: &LutrisApplicationScannerConfig) -> Vec<WatchTarget> {
	let Some(db_path) = expand_path(&config.pga_db) else {
		return Vec::new();
	};
	let mut targets = vec![WatchTarget::directory(
		crate::durable::directory(&db_path).to_path_buf(),
		Filter::Prefix("pga.db"),
	)];
	targets.extend(
		ArtLocations::new(&db_path)
			.directories
			.into_iter()
			.map(WatchTarget::probe),
	);
	targets
}

/// Where Lutris keeps game art: cover art (portrait), then banners and icons.
/// Lutris has moved these between its data and cache directories across
/// releases, so both are checked, next to the configured database first.
struct ArtLocations {
	directories: Vec<PathBuf>,
	icons: Option<PathBuf>,
}

impl ArtLocations {
	fn new(db_path: &Path) -> Self {
		let mut roots = vec![crate::durable::directory(db_path).to_path_buf()];
		for root in [
			dirs::data_dir().map(|dir| dir.join("lutris")),
			dirs::cache_dir().map(|dir| dir.join("lutris")),
		]
		.into_iter()
		.flatten()
		{
			if !roots.contains(&root) {
				roots.push(root);
			}
		}
		let mut directories: Vec<PathBuf> = roots.iter().map(|root| root.join("coverart")).collect();
		directories.extend(roots.iter().map(|root| root.join("banners")));
		Self {
			directories,
			icons: super::icons::xdg_data_home().map(|home| home.join("icons/hicolor/128x128/apps")),
		}
	}

	fn find(&self, slug: &str) -> Vec<PathBuf> {
		let mut candidates: Vec<PathBuf> = self
			.directories
			.iter()
			.flat_map(|directory| ["jpg", "png"].map(|extension| directory.join(format!("{slug}.{extension}"))))
			.filter(|path| path.is_file())
			.collect();
		if let Some(icon) = self
			.icons
			.as_ref()
			.map(|icons| icons.join(format!("lutris_{slug}.png")))
			.filter(|path| path.is_file())
		{
			candidates.push(icon);
		}
		candidates
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn database(path: &Path, games: &[(i64, &str, &str, bool)]) {
		let conn = Connection::open(path).unwrap();
		conn.execute_batch(
			"CREATE TABLE IF NOT EXISTS games (id INTEGER PRIMARY KEY, name TEXT, slug TEXT, directory TEXT, installed INTEGER)",
		)
		.unwrap();
		conn.execute("DELETE FROM games", []).unwrap();
		for (id, name, slug, installed) in games {
			conn.execute(
				"INSERT INTO games (id, name, slug, installed) VALUES (?1, ?2, ?3, ?4)",
				rusqlite::params![id, name, slug, *installed as i64],
			)
			.unwrap();
		}
	}

	fn config(pga_db: PathBuf) -> LutrisApplicationScannerConfig {
		LutrisApplicationScannerConfig {
			pga_db,
			command: vec!["lutris".into(), "lutris:rungame/{slug}".into()],
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 2,
			frame_limit: Default::default(),
		}
	}

	#[test]
	fn scans_installed_games_with_cover_art_next_to_the_database() {
		let directory = tempfile::tempdir().unwrap();
		let db = directory.path().join("pga.db");
		database(
			&db,
			&[
				(1, "Quake", "quake", true),
				(2, "Doom", "doom", false),
				(3, "Bad", "../x", true),
			],
		);
		std::fs::create_dir_all(directory.path().join("coverart")).unwrap();
		let cover = directory.path().join("coverart/quake.jpg");
		std::fs::write(&cover, "jpg").unwrap();

		let result = scan(&config(db));
		let entries = result.units[0].result.as_ref().unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].native_id, "quake");
		assert_eq!(entries[0].aliases, vec!["lutris:id:1".to_string()]);
		assert_eq!(entries[0].application.command, vec!["lutris", "lutris:rungame/quake"]);
		assert_eq!(entries[0].application.boxart, Some(cover));
	}

	#[test]
	fn a_missing_database_is_empty_and_a_corrupt_one_is_an_error() {
		let directory = tempfile::tempdir().unwrap();
		let result = scan(&config(directory.path().join("pga.db")));
		assert_eq!(result.units[0].result.as_ref().unwrap().len(), 0);

		let corrupt = directory.path().join("corrupt.db");
		std::fs::write(&corrupt, "this is not a database, just a partial write").unwrap();
		let result = scan(&config(corrupt));
		assert!(matches!(result.units[0].result, Err(UnitError::Corrupt(_))));
	}
}
