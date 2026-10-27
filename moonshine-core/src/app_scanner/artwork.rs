//! Box art for `/appasset`: where it came from, and a bounded cache of the
//! rendered images.
//!
//! Moonlight shows box art in a fixed 200x267 tile and stretches whatever it
//! receives, so every image is letterboxed into that ratio
//! ([`fit_to_boxart`]) and sent as PNG. Decoding and resizing costs tens of
//! milliseconds per image and Moonlight requests every tile when it shows
//! the app grid, so rendered images are cached on disk under a key that
//! changes with the source file (path, size, modification time, inode). The
//! cache is bounded ([`MAX_CACHE_BYTES`], [`MAX_CACHE_FILES`]); the oldest
//! renderings are evicted first. Sources that fail to decode are remembered
//! and skipped until they change, and the next candidate is tried, so a
//! corrupt image falls back to other art instead of failing every request.

use std::collections::VecDeque;
use std::io::Cursor;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use image::ImageFormat;
use image::imageops::FilterType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where an entry's box art came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkOrigin {
	/// Art the user assigned in Steam (`userdata/*/config/grid`).
	SteamCustom,
	/// Art Steam downloaded for its library view.
	Steam,
	/// Cover art or icons the launcher (Lutris, Heroic) saved.
	Launcher,
	/// The desktop entry's icon.
	DesktopIcon,
	/// The `boxart` of a manual `[[application]]` entry.
	Manual,
	/// A theme icon named like the application (fallback).
	IconTheme,
}

impl ArtworkOrigin {
	pub fn id(self) -> &'static str {
		match self {
			Self::SteamCustom => "steam_custom",
			Self::Steam => "steam",
			Self::Launcher => "launcher",
			Self::DesktopIcon => "desktop_icon",
			Self::Manual => "manual",
			Self::IconTheme => "icon_theme",
		}
	}
}

pub(crate) const BOXART_WIDTH: u32 = 600;
pub(crate) const BOXART_HEIGHT: u32 = 801;

/// Source images larger than this are not decoded.
pub(crate) const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
/// Rendered images kept on disk, in bytes and files.
pub(crate) const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_CACHE_FILES: usize = 2048;
/// Undecodable sources remembered (by fingerprint) so they are not retried.
const MAX_REMEMBERED_FAILURES: usize = 256;
/// Bumped when the rendering changes, so old renderings are not served.
const RENDER_VERSION: u32 = 1;

/// Fit the image into a 600x801 canvas (the 200:267 ratio Moonlight
/// displays), preserving its aspect ratio, centered on transparency.
pub(crate) fn fit_to_boxart(asset: image::DynamicImage) -> image::DynamicImage {
	let (w, h) = (asset.width(), asset.height());

	// Already the right aspect ratio (within a small tolerance), return as-is.
	let target_ratio = BOXART_WIDTH as f64 / BOXART_HEIGHT as f64;
	let image_ratio = w as f64 / h as f64;
	if (image_ratio - target_ratio).abs() < 0.01 {
		return asset;
	}

	// Scale the image to fit within the box art dimensions while preserving aspect ratio.
	let scale = f64::min(BOXART_WIDTH as f64 / w as f64, BOXART_HEIGHT as f64 / h as f64);
	let new_w = ((w as f64 * scale).round() as u32).clamp(1, BOXART_WIDTH);
	let new_h = ((h as f64 * scale).round() as u32).clamp(1, BOXART_HEIGHT);
	let resized = asset.resize_exact(new_w, new_h, FilterType::Lanczos3);

	// Center the resized image on a transparent canvas.
	let mut canvas = image::RgbaImage::new(BOXART_WIDTH, BOXART_HEIGHT);
	let offset_x = (BOXART_WIDTH - new_w) / 2;
	let offset_y = (BOXART_HEIGHT - new_h) / 2;
	image::imageops::overlay(&mut canvas, &resized.to_rgba8(), offset_x as i64, offset_y as i64);

	image::DynamicImage::ImageRgba8(canvas)
}

/// Rendered box art, cached on disk.
pub(crate) struct ArtworkCache {
	directory: Option<PathBuf>,
	state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
	/// Fingerprints of sources that did not decode, oldest first.
	failed: VecDeque<String>,
	/// Bytes and files in the cache directory, once counted.
	usage: Option<(u64, usize)>,
}

/// Outcome of a rendering, for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rendered {
	/// From the on-disk cache.
	Cached(Vec<u8>),
	/// Decoded and resized now.
	Fresh(Vec<u8>),
}

impl Rendered {
	pub(crate) fn into_bytes(self) -> Vec<u8> {
		match self {
			Self::Cached(bytes) | Self::Fresh(bytes) => bytes,
		}
	}
}

impl ArtworkCache {
	/// `$XDG_CACHE_HOME/moonshine/boxart`: regenerable, never backed up.
	pub(crate) fn default_directory() -> Option<PathBuf> {
		dirs::cache_dir().map(|cache| cache.join("moonshine").join("boxart"))
	}

	pub(crate) fn new(directory: Option<PathBuf>) -> Self {
		Self {
			directory,
			state: Mutex::default(),
		}
	}

	/// Render the first candidate that can be decoded. Blocking (file I/O
	/// and image decoding): run it on a blocking thread.
	pub(crate) fn render(&self, candidates: &[PathBuf]) -> Result<Rendered, String> {
		let mut reasons = Vec::new();
		for candidate in candidates {
			match self.render_one(candidate) {
				Ok(rendered) => return Ok(rendered),
				Err(reason) => reasons.push(reason),
			}
		}
		Err(if reasons.is_empty() {
			"No box art is available.".into()
		} else {
			reasons.join("; ")
		})
	}

	fn render_one(&self, configured: &Path) -> Result<Rendered, String> {
		let path = super::expand_path(configured).ok_or_else(|| format!("Cannot expand {}", configured.display()))?;
		let metadata = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
		if !metadata.is_file() {
			return Err(format!("{} is not a file", path.display()));
		}
		if metadata.len() > MAX_SOURCE_BYTES {
			return Err(format!("{} is larger than {MAX_SOURCE_BYTES} bytes", path.display()));
		}
		let fingerprint = fingerprint(&path, &metadata);
		if self.state.lock().unwrap().failed.contains(&fingerprint) {
			return Err(format!("{} cannot be decoded", path.display()));
		}

		let cached = self
			.directory
			.as_ref()
			.map(|directory| directory.join(format!("{fingerprint}.png")));
		if let Some(cached) = &cached
			&& let Ok(bytes) = std::fs::read(cached)
		{
			// Mark it recently used for eviction.
			let _ = std::fs::File::options()
				.write(true)
				.open(cached)
				.and_then(|file| file.set_modified(SystemTime::now()));
			return Ok(Rendered::Cached(bytes));
		}

		let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
		let png = match decode(&bytes) {
			Ok(png) => png,
			Err(error) => {
				let mut state = self.state.lock().unwrap();
				if state.failed.len() == MAX_REMEMBERED_FAILURES {
					state.failed.pop_front();
				}
				state.failed.push_back(fingerprint);
				tracing::warn!("Box art {} cannot be used: {error}", path.display());
				return Err(format!("{} cannot be decoded: {error}", path.display()));
			},
		};
		if let Some(cached) = cached {
			self.store(&cached, &png);
		}
		Ok(Rendered::Fresh(png))
	}

	/// Write a rendering and keep the cache within its bounds. Failures only
	/// cost a re-render later.
	fn store(&self, path: &Path, png: &[u8]) {
		let Some(directory) = self.directory.as_ref() else {
			return;
		};
		let written = std::fs::create_dir_all(directory)
			.and_then(|()| tempfile::NamedTempFile::new_in(directory))
			.and_then(|mut temporary| {
				use std::io::Write;
				temporary.write_all(png)?;
				temporary.persist(path).map_err(|error| error.error)?;
				Ok(())
			});
		if let Err(error) = written {
			tracing::debug!("Cannot cache box art in {}: {error}", directory.display());
			return;
		}
		let mut state = self.state.lock().unwrap();
		let (bytes, files) = state.usage.get_or_insert_with(|| usage(directory));
		*bytes += png.len() as u64;
		*files += 1;
		if *bytes > MAX_CACHE_BYTES || *files > MAX_CACHE_FILES {
			state.usage = Some(evict(directory, MAX_CACHE_BYTES * 3 / 4, MAX_CACHE_FILES * 3 / 4));
		}
	}
}

fn fingerprint(path: &Path, metadata: &std::fs::Metadata) -> String {
	use std::os::unix::ffi::OsStrExt;
	let digest = Sha256::new()
		.chain_update(RENDER_VERSION.to_le_bytes())
		.chain_update(path.as_os_str().as_bytes())
		.chain_update(metadata.len().to_le_bytes())
		.chain_update(metadata.mtime().to_le_bytes())
		.chain_update(metadata.mtime_nsec().to_le_bytes())
		.chain_update(metadata.ino().to_le_bytes())
		.finalize();
	hex::encode(&digest[..16])
}

/// Decode with bounded allocations and render the Moonlight tile.
fn decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
	let mut reader = image::ImageReader::new(Cursor::new(bytes))
		.with_guessed_format()
		.map_err(|error| error.to_string())?;
	let mut limits = image::Limits::default();
	limits.max_image_width = Some(16_384);
	limits.max_image_height = Some(16_384);
	limits.max_alloc = Some(512 * 1024 * 1024);
	reader.limits(limits);
	let image = reader.decode().map_err(|error| error.to_string())?;
	let mut png = Cursor::new(Vec::new());
	fit_to_boxart(image)
		.write_to(&mut png, ImageFormat::Png)
		.map_err(|error| error.to_string())?;
	Ok(png.into_inner())
}

fn cached_files(directory: &Path) -> Vec<(SystemTime, u64, PathBuf)> {
	std::fs::read_dir(directory)
		.map(|entries| {
			entries
				.flatten()
				.filter(|entry| entry.path().extension().is_some_and(|extension| extension == "png"))
				.filter_map(|entry| {
					let metadata = entry.metadata().ok()?;
					Some((metadata.modified().ok()?, metadata.len(), entry.path()))
				})
				.collect()
		})
		.unwrap_or_default()
}

fn usage(directory: &Path) -> (u64, usize) {
	let files = cached_files(directory);
	(files.iter().map(|(_, size, _)| size).sum(), files.len())
}

/// Delete the least recently used renderings until within the targets.
fn evict(directory: &Path, target_bytes: u64, target_files: usize) -> (u64, usize) {
	let mut files = cached_files(directory);
	files.sort();
	let mut bytes: u64 = files.iter().map(|(_, size, _)| size).sum();
	let mut count = files.len();
	for (_, size, path) in files {
		if bytes <= target_bytes && count <= target_files {
			break;
		}
		if std::fs::remove_file(&path).is_ok() {
			bytes = bytes.saturating_sub(size);
			count -= 1;
		}
	}
	(bytes, count)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn png(path: &Path, width: u32, height: u32) {
		image::RgbaImage::from_pixel(width, height, image::Rgba([200, 10, 10, 255]))
			.save(path)
			.unwrap();
	}

	#[test]
	fn renders_once_then_serves_the_cached_tile() {
		let directory = tempfile::tempdir().unwrap();
		let source = directory.path().join("cover.png");
		png(&source, 460, 215);
		let cache = ArtworkCache::new(Some(directory.path().join("cache")));

		let first = cache.render(std::slice::from_ref(&source)).unwrap();
		assert!(matches!(first, Rendered::Fresh(_)));
		let tile = image::load_from_memory(&first.into_bytes()).unwrap();
		assert_eq!((tile.width(), tile.height()), (BOXART_WIDTH, BOXART_HEIGHT));
		assert!(matches!(
			cache.render(std::slice::from_ref(&source)).unwrap(),
			Rendered::Cached(_)
		));

		// A changed source is rendered again.
		png(&source, 300, 400);
		let changed = std::fs::File::options().write(true).open(&source).unwrap();
		changed
			.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
			.unwrap();
		assert!(matches!(
			cache.render(std::slice::from_ref(&source)).unwrap(),
			Rendered::Fresh(_)
		));
	}

	/// A corrupt or missing image falls back to the next candidate, and a
	/// corrupt one is not decoded again until it changes.
	#[test]
	fn corrupt_and_missing_candidates_fall_back() {
		let directory = tempfile::tempdir().unwrap();
		let corrupt = directory.path().join("corrupt.jpg");
		std::fs::write(&corrupt, b"\xff\xd8\xff\xe0 truncated jpeg").unwrap();
		let missing = directory.path().join("missing.png");
		let icon = directory.path().join("icon.png");
		png(&icon, 64, 64);
		let cache = ArtworkCache::new(None);
		let candidates = [missing.clone(), corrupt.clone(), icon];
		assert!(matches!(cache.render(&candidates).unwrap(), Rendered::Fresh(_)));
		assert_eq!(cache.state.lock().unwrap().failed.len(), 1);
		let error = cache.render(&[missing, corrupt]).unwrap_err();
		assert!(error.contains("cannot be decoded"), "{error}");
		assert_eq!(cache.state.lock().unwrap().failed.len(), 1, "not decoded again");
		assert!(cache.render(&[]).is_err());
	}

	#[test]
	fn the_cache_evicts_the_least_recently_used_renderings() {
		let directory = tempfile::tempdir().unwrap();
		let cache_dir = directory.path().join("cache");
		std::fs::create_dir_all(&cache_dir).unwrap();
		let now = SystemTime::now();
		for index in 0..10u64 {
			let path = cache_dir.join(format!("{index:02}.png"));
			std::fs::write(&path, vec![0; 100]).unwrap();
			std::fs::File::options()
				.write(true)
				.open(&path)
				.unwrap()
				.set_modified(now - std::time::Duration::from_secs(100 - index))
				.unwrap();
		}
		assert_eq!(evict(&cache_dir, 450, 100), (400, 4));
		let mut left: Vec<_> = std::fs::read_dir(&cache_dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect();
		left.sort();
		assert_eq!(left, vec!["06.png", "07.png", "08.png", "09.png"]);
	}

	#[test]
	fn oversized_sources_are_not_decoded() {
		let directory = tempfile::tempdir().unwrap();
		let huge = directory.path().join("huge.png");
		std::fs::File::create(&huge)
			.unwrap()
			.set_len(MAX_SOURCE_BYTES + 1)
			.unwrap();
		let error = ArtworkCache::new(None).render(&[huge]).unwrap_err();
		assert!(error.contains("larger than"), "{error}");
	}
}
