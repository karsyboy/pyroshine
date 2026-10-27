//! Icon theme lookups for desktop entries and title fallbacks.
//!
//! Icon themes hold tens of thousands of files. Instead of keeping an index
//! of all of them, a lookup walks the icon roots once for every name not
//! resolved yet and keeps only the answers, so repeated catalog refreshes do
//! not walk the themes again. Misses are remembered for [`MISS_TTL`] (an
//! icon installed after its desktop entry is found on a later refresh), and
//! a manual rescan clears everything.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use walkdir::WalkDir;

pub(crate) const SUPPORTED_IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "ico"];

/// Icon theme contexts that never hold application icons.
const SKIPPED_CONTEXTS: [&str; 15] = [
	"actions",
	"animations",
	"categories",
	"cursors",
	"devices",
	"emblems",
	"emotes",
	"intl",
	"mimes",
	"mimetypes",
	"panel",
	"places",
	"preferences",
	"status",
	"stock",
];

/// How long a name that matched no icon is not searched for again.
const MISS_TTL: Duration = Duration::from_secs(3600);

/// Lookup key: lowercase file stem and, when the name had one, lowercase extension.
type Name = (String, Option<String>);

#[derive(Default)]
pub(crate) struct IconIndex {
	resolved: HashMap<Name, (Option<PathBuf>, Instant)>,
	/// Directory walks performed (tests and resource reporting).
	walks: u64,
	/// Overrides the XDG icon roots (tests).
	roots: Option<Vec<PathBuf>>,
}

fn name(icon: &str) -> Name {
	let path = Path::new(icon);
	let stem = path
		.file_stem()
		.and_then(|stem| stem.to_str())
		.unwrap_or(icon)
		.to_ascii_lowercase();
	let extension = path
		.extension()
		.and_then(|extension| extension.to_str())
		.filter(|extension| is_supported_extension(extension))
		.map(str::to_ascii_lowercase);
	match extension {
		Some(extension) => (stem, Some(extension)),
		// "org.example.App" has no image extension: the whole name is the stem.
		None => (icon.to_ascii_lowercase(), None),
	}
}

impl IconIndex {
	pub(crate) fn new() -> Self {
		Self::default()
	}

	#[cfg(test)]
	pub(crate) fn with_roots(roots: Vec<PathBuf>) -> Self {
		Self {
			roots: Some(roots),
			..Self::default()
		}
	}

	#[cfg(test)]
	pub(crate) fn walks(&self) -> u64 {
		self.walks
	}

	/// Forget every answer (manual rescan).
	pub(crate) fn clear(&mut self) {
		self.resolved.clear();
	}

	/// Resolve every name in `icons` that has no current answer, with a
	/// single walk of the icon roots.
	pub(crate) fn prepare<'a>(&mut self, icons: impl IntoIterator<Item = &'a str>) {
		let now = Instant::now();
		let wanted: HashSet<Name> = icons
			.into_iter()
			.filter(|icon| !icon.is_empty())
			.map(name)
			.filter(|name| match self.resolved.get(name) {
				Some((Some(_), _)) => false,
				Some((None, at)) => now.duration_since(*at) >= MISS_TTL,
				None => true,
			})
			.collect();
		if wanted.is_empty() {
			return;
		}
		let stems: HashSet<&str> = wanted.iter().map(|(stem, _)| stem.as_str()).collect();
		let mut best: HashMap<Name, (i32, PathBuf)> = HashMap::new();
		self.walks += 1;
		for root in self.roots.clone().unwrap_or_else(icon_search_roots) {
			if !root.exists() {
				continue;
			}
			// Themes alias many directories through symlinks; following them
			// visits the same files many times over. Symlinked files (icon
			// aliases) are kept, and contexts that never hold application
			// icons are skipped.
			for entry in WalkDir::new(root)
				.follow_links(false)
				.into_iter()
				.filter_entry(|entry| {
					!(entry.file_type().is_dir()
						&& entry
							.file_name()
							.to_str()
							.is_some_and(|name| SKIPPED_CONTEXTS.contains(&name)))
				})
				.filter_map(|entry| entry.ok())
				.filter(|entry| entry.file_type().is_file() || entry.file_type().is_symlink())
			{
				let path = entry.path();
				let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
					continue;
				};
				if !is_supported_extension(extension) {
					continue;
				}
				let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
					continue;
				};
				let stem = stem.to_ascii_lowercase();
				if !stems.contains(stem.as_str()) {
					continue;
				}
				let score = score_icon_path(path);
				let extension = extension.to_ascii_lowercase();
				for key in [(stem.clone(), None), (stem.clone(), Some(extension))] {
					if !wanted.contains(&key) {
						continue;
					}
					if best.get(&key).is_none_or(|(current, _)| score > *current) {
						best.insert(key, (score, path.to_path_buf()));
					}
				}
			}
		}
		for key in wanted {
			let found = best.remove(&key).map(|(_, path)| path);
			self.resolved.insert(key, (found, now));
		}
	}

	/// The answer for `icon`; [`Self::prepare`] it first.
	pub(crate) fn get(&self, icon: &str) -> Option<PathBuf> {
		self.resolved.get(&name(icon)).and_then(|(path, _)| path.clone())
	}

	/// Resolve a desktop entry's `Icon` value: an absolute image path, a
	/// path relative to the desktop file, or a theme icon name.
	pub(crate) fn resolve(&self, icon: &str, desktop_file: &Path) -> Option<PathBuf> {
		let icon_path = Path::new(icon);
		if icon_path.is_absolute() {
			return is_supported_image(icon_path).then(|| icon_path.to_path_buf());
		}
		if icon.contains('/') {
			let path = desktop_file.parent()?.join(icon);
			return is_supported_image(&path).then_some(path);
		}
		self.get(icon)
	}
}

fn icon_search_roots() -> Vec<PathBuf> {
	let mut roots = Vec::new();

	if let Some(data_home) = xdg_data_home() {
		roots.push(data_home.join("icons"));
		roots.push(data_home.join("pixmaps"));
	}

	for data_dir in xdg_data_dirs() {
		roots.push(data_dir.join("icons"));
		roots.push(data_dir.join("pixmaps"));
	}

	roots
}

pub(crate) fn xdg_data_home() -> Option<PathBuf> {
	if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
		return Some(PathBuf::from(data_home));
	}

	let home = env::var_os("HOME")?;
	Some(PathBuf::from(home).join(".local/share"))
}

fn xdg_data_dirs() -> Vec<PathBuf> {
	match env::var_os("XDG_DATA_DIRS") {
		Some(value) if !value.is_empty() => env::split_paths(&value).collect(),
		_ => vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")],
	}
}

fn is_supported_extension(extension: &str) -> bool {
	SUPPORTED_IMAGE_EXTENSIONS
		.iter()
		.any(|supported| supported.eq_ignore_ascii_case(extension))
}

pub(crate) fn is_supported_image(path: &Path) -> bool {
	path.is_file()
		&& path
			.extension()
			.and_then(|extension| extension.to_str())
			.is_some_and(is_supported_extension)
}

fn score_icon_path(path: &Path) -> i32 {
	let mut score = 0;
	let path_string = path.to_string_lossy();

	if path_string.contains("/hicolor/") {
		score += 50;
	}

	if path_string.contains("/apps/") {
		score += 25;
	}

	if path_string.contains("/scalable/") {
		score -= 5;
	}

	for component in path.components() {
		let component = component.as_os_str().to_string_lossy();
		let Some((width, height)) = component.split_once('x') else {
			continue;
		};

		let Ok(width) = width.parse::<i32>() else {
			continue;
		};
		let Ok(height) = height.parse::<i32>() else {
			continue;
		};

		score += std::cmp::min(width, height);
	}

	score
}

#[cfg(test)]
mod tests {
	use super::*;

	fn touch(path: &Path) {
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, "png").unwrap();
	}

	#[test]
	fn prefers_large_hicolor_app_icons_and_walks_once_per_batch() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("icons");
		touch(&root.join("hicolor/32x32/apps/game.png"));
		let large = root.join("hicolor/256x256/apps/game.png");
		touch(&large);
		// Not application icons, and symlinked directories are not followed.
		touch(&root.join("hicolor/512x512/mimetypes/game.png"));
		std::os::unix::fs::symlink(root.join("hicolor/256x256"), root.join("hicolor/1024x1024")).unwrap();
		let other = root.join("Adwaita/64x64/apps/other.svg.png");
		touch(&other);

		let mut index = IconIndex::with_roots(vec![root]);
		index.prepare(["game", "missing", "Game"]);
		assert_eq!(index.walks(), 1);
		assert_eq!(index.get("game"), Some(large.clone()));
		assert_eq!(index.get("GAME"), Some(large));
		assert_eq!(index.get("missing"), None);

		// Known answers, including misses, do not walk again.
		index.prepare(["game", "missing"]);
		assert_eq!(index.walks(), 1);
		index.clear();
		index.prepare(["game"]);
		assert_eq!(index.walks(), 2);
	}

	#[test]
	fn names_with_an_image_extension_match_that_extension_only() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("pixmaps");
		let jpg = root.join("logo.jpg");
		touch(&jpg);
		touch(&root.join("hicolor/512x512/apps/logo.png"));
		let mut index = IconIndex::with_roots(vec![root]);
		index.prepare(["logo.jpg", "org.example.Game"]);
		assert_eq!(index.get("logo.jpg"), Some(jpg));
		assert_eq!(index.get("org.example.Game"), None);
	}
}
