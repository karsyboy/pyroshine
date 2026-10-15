//! Application discovery and the managed application catalog.
//!
//! Scanners (`steam`, `lutris`, `heroic`, `desktop`) read launcher metadata
//! and report what they found per *unit*: a Steam library folder, a Heroic
//! library file, a desktop-entry directory, the Lutris database. A unit that
//! cannot be read reports an error instead of an empty list, so the catalog
//! keeps its last known-good entries rather than dropping a library that is
//! briefly unavailable or half-written.
//!
//! [`service::Library`] owns the catalog: it runs scanners on a background
//! thread, watches their metadata for changes (`watch`), reconciles results
//! with the manual `[[application]]` entries and recorded identities
//! ([`catalog`], `identity`) and publishes immutable snapshots that the
//! GameStream endpoints and the management interface read.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use desktop::DesktopApplicationScannerConfig;
use heroic::HeroicApplicationScannerConfig;
use lutris::LutrisApplicationScannerConfig;
use steam::SteamApplicationScannerConfig;

pub(crate) mod artwork;
pub mod catalog;
pub mod desktop;
pub mod heroic;
pub(crate) mod icons;
pub(crate) mod identity;
pub mod lutris;
pub mod service;
pub mod steam;
pub(crate) mod watch;

use crate::session::application::ApplicationConfig;

pub use self::artwork::ArtworkOrigin;
pub use self::catalog::{CatalogEntry, CatalogSnapshot, EntryState, Origin};
pub use self::identity::legacy_id;
pub use self::service::{Library, LibraryConfig, LibraryView, SourceHealth, SourceStatus};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationScannerConfig {
	/// Scans a 'libraryfolders.vdf' file from a Steam library directory.
	Steam(SteamApplicationScannerConfig),

	/// Scans directories containing freedesktop .desktop launchers.
	Desktop(DesktopApplicationScannerConfig),

	/// Scans the Lutris game database.
	Lutris(LutrisApplicationScannerConfig),

	/// Scans the Heroic Games Launcher library caches.
	Heroic(HeroicApplicationScannerConfig),
}

/// Launcher a scanner reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScannerKind {
	Steam,
	Desktop,
	Lutris,
	Heroic,
}

impl ScannerKind {
	/// Prefix of the identities of this launcher's entries (`steam:620`).
	pub fn id(self) -> &'static str {
		match self {
			Self::Steam => "steam",
			Self::Desktop => "desktop",
			Self::Lutris => "lutris",
			Self::Heroic => "heroic",
		}
	}

	pub fn label(self) -> &'static str {
		match self {
			Self::Steam => "Steam",
			Self::Desktop => "Desktop entries",
			Self::Lutris => "Lutris",
			Self::Heroic => "Heroic",
		}
	}

	/// Short name used to tell apart equal titles from different launchers.
	pub fn short_label(self) -> &'static str {
		match self {
			Self::Desktop => "Desktop",
			other => other.label(),
		}
	}
}

impl ApplicationScannerConfig {
	pub fn kind(&self) -> ScannerKind {
		match self {
			Self::Steam(_) => ScannerKind::Steam,
			Self::Desktop(_) => ScannerKind::Desktop,
			Self::Lutris(_) => ScannerKind::Lutris,
			Self::Heroic(_) => ScannerKind::Heroic,
		}
	}

	/// The location this scanner reads, for display.
	pub fn root(&self) -> String {
		match self {
			Self::Steam(config) => config.library.display().to_string(),
			Self::Lutris(config) => config.pga_db.display().to_string(),
			Self::Heroic(config) => config.config_dir.display().to_string(),
			Self::Desktop(config) => config
				.directories
				.iter()
				.map(|directory| directory.display().to_string())
				.collect::<Vec<_>>()
				.join(", "),
		}
	}

	/// Run the scanner. Blocking: call it from the library worker only.
	pub(crate) fn scan(&self, icons: &mut icons::IconIndex) -> SourceScan {
		match self {
			Self::Steam(config) => steam::scan(config),
			Self::Desktop(config) => desktop::scan(config, icons),
			Self::Lutris(config) => lutris::scan(config),
			Self::Heroic(config) => heroic::scan(config),
		}
	}

	/// Files and directories whose changes can change this scanner's result.
	pub(crate) fn watch_targets(&self) -> Vec<watch::WatchTarget> {
		match self {
			Self::Steam(config) => steam::watch_targets(config),
			Self::Desktop(config) => desktop::watch_targets(config),
			Self::Lutris(config) => lutris::watch_targets(config),
			Self::Heroic(config) => heroic::watch_targets(config),
		}
	}

	/// Digest of this scanner's configuration. Results retained across a
	/// restart are reused only under the configuration that produced them,
	/// so an edited command template never resurrects stale commands.
	pub(crate) fn digest(&self) -> String {
		use sha2::{Digest, Sha256};
		let text = serde_json::to_string(self).unwrap_or_default();
		hex::encode(&Sha256::digest(text.as_bytes())[..16])
	}
}

/// One application found by a scanner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Discovered {
	/// Launcher-native identity, unique per launcher: Steam app ID, Lutris
	/// slug, Heroic `runner/app_name`, desktop file ID.
	pub native_id: String,
	pub application: ApplicationConfig,
	/// Further images to try, in order, when `application.boxart` is missing
	/// or cannot be decoded.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub artwork_fallbacks: Vec<PathBuf>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub artwork_origin: Option<ArtworkOrigin>,
	/// Other identities of the same application (`lutris:id:42`), which
	/// launcher shortcuts may refer to.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub aliases: Vec<String>,
	/// Identity of the application a shortcut starts through its launcher
	/// (a desktop entry running `steam://rungameid/620` is `steam:620`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub launches: Option<String>,
}

impl Discovered {
	pub(crate) fn new(native_id: impl Into<String>, application: ApplicationConfig) -> Self {
		Self {
			native_id: native_id.into(),
			application,
			artwork_fallbacks: Vec::new(),
			artwork_origin: None,
			aliases: Vec::new(),
			launches: None,
		}
	}

	/// Set the box art from ordered candidates that exist on disk.
	pub(crate) fn with_artwork(mut self, mut candidates: Vec<PathBuf>, origin: ArtworkOrigin) -> Self {
		if candidates.is_empty() {
			return self;
		}
		self.application.boxart = Some(candidates.remove(0));
		self.artwork_fallbacks = candidates;
		self.artwork_origin = Some(origin);
		self
	}
}

/// Why a unit could not be read. Either way its previous entries are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnitError {
	/// The location is missing or not accessible (unmounted drive,
	/// permissions, I/O error). Retried by the periodic check.
	Unavailable(String),
	/// The metadata exists but cannot be parsed, typically because the
	/// launcher is rewriting it. Retried shortly.
	Corrupt(String),
}

impl UnitError {
	pub(crate) fn message(&self) -> &str {
		match self {
			Self::Unavailable(message) | Self::Corrupt(message) => message,
		}
	}

	/// Classify an I/O error: a missing file is unavailable; data that
	/// reads but does not parse is corrupt.
	pub(crate) fn io(context: impl std::fmt::Display, error: &std::io::Error) -> Self {
		Self::Unavailable(format!("{context}: {error}"))
	}
}

/// Result of one unit.
#[derive(Debug)]
pub(crate) struct Unit {
	/// Stable name of the unit within its scanner (a path).
	pub key: String,
	pub result: Result<Vec<Discovered>, UnitError>,
	/// Native IDs whose metadata could not be read this time; their previous
	/// entries are kept (a single half-written Steam manifest must not drop
	/// that game).
	pub retain: Vec<String>,
}

impl Unit {
	pub(crate) fn ok(key: impl Into<String>, entries: Vec<Discovered>) -> Self {
		Self {
			key: key.into(),
			result: Ok(entries),
			retain: Vec::new(),
		}
	}

	pub(crate) fn failed(key: impl Into<String>, error: UnitError) -> Self {
		Self {
			key: key.into(),
			result: Err(error),
			retain: Vec::new(),
		}
	}
}

/// Result of one scanner run.
#[derive(Debug, Default)]
pub(crate) struct SourceScan {
	/// Every unit the launcher currently lists. Units missing here were
	/// removed (for example a Steam library folder deleted in Steam), and
	/// their entries go with them.
	pub units: Vec<Unit>,
	/// The scanner could not even list its units (Steam root unreadable,
	/// `libraryfolders.vdf` half-written): every unit is kept as it was.
	pub failure: Option<UnitError>,
}

impl SourceScan {
	pub(crate) fn failed(error: UnitError) -> Self {
		Self {
			units: Vec::new(),
			failure: Some(error),
		}
	}

	/// Every application found, in unit order (tests).
	#[cfg(test)]
	pub(crate) fn applications(&self) -> Vec<ApplicationConfig> {
		self.units
			.iter()
			.filter_map(|unit| unit.result.as_ref().ok())
			.flatten()
			.map(|discovered| discovered.application.clone())
			.collect()
	}
}

/// Expand `~` and environment variables in a configured path.
pub(crate) fn expand_path(path: &std::path::Path) -> Option<PathBuf> {
	let path = path.to_string_lossy();
	let path = shellexpand::full(&path).ok()?;
	Some(PathBuf::from(path.as_ref()))
}

/// Identity a launcher command starts, when it is a shortcut to an
/// application another scanner owns: `steam://rungameid/<id>`,
/// `lutris:rungameid/<id>`, `lutris:rungame/<slug>`,
/// `heroic://launch/<runner>/<app>` or `heroic://launch?appName=..&runner=..`.
pub(crate) fn launcher_target(command: &[String]) -> Option<String> {
	for argument in command {
		let argument = argument.trim();
		if let Some(rest) = argument.split("steam://rungameid/").nth(1) {
			let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
			if !id.is_empty() {
				return Some(format!("steam:{id}"));
			}
		}
		if let Some(rest) = argument.strip_prefix("lutris:rungameid/") {
			let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
			if !id.is_empty() {
				return Some(format!("lutris:id:{id}"));
			}
		}
		if let Some(rest) = argument.strip_prefix("lutris:rungame/") {
			let slug = rest.trim_end_matches('/');
			if !slug.is_empty() {
				return Some(format!("lutris:{slug}"));
			}
		}
		if let Some(rest) = argument.strip_prefix("heroic://launch") {
			if let Some(query) = rest.strip_prefix('?') {
				let params: std::collections::HashMap<String, String> =
					url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
				if let (Some(app), Some(runner)) = (params.get("appName"), params.get("runner")) {
					return Some(format!("heroic:{runner}/{app}"));
				}
			} else if let Some(path) = rest.strip_prefix('/') {
				let mut parts = path.splitn(2, '/');
				if let (Some(runner), Some(app)) = (parts.next(), parts.next())
					&& !runner.is_empty()
					&& !app.is_empty()
				{
					return Some(format!("heroic:{runner}/{}", app.trim_end_matches('/')));
				}
			}
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn recognizes_launcher_shortcuts() {
		let target = |command: &[&str]| launcher_target(&command.iter().map(|s| s.to_string()).collect::<Vec<_>>());
		assert_eq!(
			target(&["/usr/bin/steam", "steam://rungameid/620"]),
			Some("steam:620".into())
		);
		assert_eq!(
			target(&["env", "steam", "-silent", "steam://rungameid/1091500/"]),
			Some("steam:1091500".into())
		);
		assert_eq!(target(&["lutris", "lutris:rungameid/42"]), Some("lutris:id:42".into()));
		assert_eq!(target(&["lutris", "lutris:rungame/quake"]), Some("lutris:quake".into()));
		assert_eq!(
			target(&["xdg-open", "heroic://launch/legendary/Fortnite"]),
			Some("heroic:legendary/Fortnite".into())
		);
		assert_eq!(
			target(&["heroic", "heroic://launch?appName=abc&runner=gog"]),
			Some("heroic:gog/abc".into())
		);
		assert_eq!(target(&["/usr/bin/firefox", "%u"]), None);
		assert_eq!(target(&["steam", "steam://open/bigpicture"]), None);
	}
}
