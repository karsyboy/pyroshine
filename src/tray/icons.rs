//! Tray icons: the Pyroshine logo with a status badge, rendered once at
//! startup in the sizes panels ask for, and the menu's Material icons.

use std::sync::LazyLock;

use image::imageops::FilterType;
use image::{Rgba, RgbaImage};

/// Badge drawn in the lower right corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Badge {
	/// Idle: the plain logo.
	None,
	/// A client is streaming: green play button.
	Streaming,
	/// The application runs without a client: amber pause.
	Retained,
	/// Launching or reconnecting: blue dots.
	Busy,
	/// Teardown in progress: grey stop square.
	Stopping,
	/// Error: red exclamation mark.
	Error,
	/// A pairing request waits for the operator: indigo keypad.
	Pairing,
	/// The daemon is not running: the logo greyed out.
	Unavailable,
}

const SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

static BASE: LazyLock<Vec<RgbaImage>> = LazyLock::new(|| {
	let logo = image::load_from_memory_with_format(include_bytes!("tray.png"), image::ImageFormat::Png)
		.expect("embedded tray icon decodes")
		.into_rgba8();
	SIZES
		.iter()
		.map(|size| image::imageops::resize(&logo, *size, *size, FilterType::Lanczos3))
		.collect()
});

/// ARGB32 pixmaps for `badge` in every size.
pub fn pixmaps(badge: Badge) -> Vec<ksni::Icon> {
	BASE.iter()
		.map(|base| {
			let image = render(base, badge);
			let mut data = image.into_raw();
			for pixel in data.as_chunks_mut::<4>().0 {
				pixel.rotate_right(1); // RGBA to ARGB
			}
			ksni::Icon {
				width: base.width() as i32,
				height: base.height() as i32,
				data,
			}
		})
		.collect()
}

fn render(base: &RgbaImage, badge: Badge) -> RgbaImage {
	let mut image = base.clone();
	if badge == Badge::Unavailable {
		for pixel in image.pixels_mut() {
			let [r, g, b, a] = pixel.0;
			let grey = (0.3 * r as f32 + 0.59 * g as f32 + 0.11 * b as f32) as u8;
			*pixel = Rgba([grey, grey, grey, (a as f32 * 0.55) as u8]);
		}
		return image;
	}
	let (color, glyph): ([u8; 3], fn(f32, f32) -> bool) = match badge {
		Badge::None | Badge::Unavailable => return image,
		Badge::Streaming => ([0x2e, 0x7d, 0x32], play),
		Badge::Retained => ([0xef, 0x8f, 0x00], pause),
		Badge::Busy => ([0x15, 0x65, 0xc0], dots),
		Badge::Stopping => ([0x61, 0x61, 0x61], stop),
		Badge::Error => ([0xc6, 0x28, 0x28], exclamation),
		Badge::Pairing => ([0x39, 0x49, 0xab], keypad),
	};
	let size = image.width() as f32;
	// Badge circle in the lower right, 58% of the icon, with a light rim so it
	// stays readable on dark and light panels.
	let radius = size * 0.29;
	let (cx, cy) = (size - radius, size - radius);
	let samples = 4;
	for y in 0..image.height() {
		for x in 0..image.width() {
			let (mut rim, mut fill, mut mark) = (0.0, 0.0, 0.0);
			for sy in 0..samples {
				for sx in 0..samples {
					let px = x as f32 + (sx as f32 + 0.5) / samples as f32;
					let py = y as f32 + (sy as f32 + 0.5) / samples as f32;
					let (dx, dy) = ((px - cx) / radius, (py - cy) / radius);
					let distance = (dx * dx + dy * dy).sqrt();
					if distance <= 1.0 {
						rim += 1.0;
						if distance <= 0.84 {
							fill += 1.0;
							if glyph(dx, dy) {
								mark += 1.0;
							}
						}
					}
				}
			}
			let total = (samples * samples) as f32;
			let (rim, fill, mark) = (rim / total, fill / total, mark / total);
			if rim == 0.0 {
				continue;
			}
			let pixel = image.get_pixel_mut(x, y);
			blend(pixel, [255, 255, 255], rim);
			blend(pixel, color, fill);
			blend(pixel, [255, 255, 255], mark);
		}
	}
	image
}

/// Composite `color` with `coverage` over `pixel` (straight alpha).
fn blend(pixel: &mut Rgba<u8>, color: [u8; 3], coverage: f32) {
	if coverage <= 0.0 {
		return;
	}
	let destination_alpha = pixel.0[3] as f32 / 255.0;
	let alpha = coverage + destination_alpha * (1.0 - coverage);
	for (channel, over) in pixel.0.iter_mut().zip(color) {
		let under = *channel as f32 * destination_alpha * (1.0 - coverage);
		*channel = ((over as f32 * coverage + under) / alpha).round() as u8;
	}
	pixel.0[3] = (alpha * 255.0).round() as u8;
}

// Glyphs in badge coordinates: the unit circle, y pointing down.

fn play(x: f32, y: f32) -> bool {
	// Triangle with vertices (-0.28, -0.42), (-0.28, 0.42), (0.45, 0).
	let x = x + 0.04;
	x >= -0.28 && y.abs() <= 0.42 * (0.45 - x) / 0.73
}

fn pause(x: f32, y: f32) -> bool {
	y.abs() <= 0.4 && ((-0.32..=-0.09).contains(&x) || (0.09..=0.32).contains(&x))
}

fn stop(x: f32, y: f32) -> bool {
	x.abs() <= 0.32 && y.abs() <= 0.32
}

fn dots(x: f32, y: f32) -> bool {
	[-0.4f32, 0.0, 0.4]
		.iter()
		.any(|cx| (x - cx).powi(2) + y.powi(2) <= 0.14f32.powi(2))
}

fn exclamation(x: f32, y: f32) -> bool {
	(x.abs() <= 0.1 && (-0.48..=0.14).contains(&y)) || x.powi(2) + (y - 0.36).powi(2) <= 0.11f32.powi(2)
}

fn keypad(x: f32, y: f32) -> bool {
	[(-0.24f32, -0.24f32), (0.24, -0.24), (-0.24, 0.24), (0.24, 0.24)]
		.iter()
		.any(|(cx, cy)| (x - cx).powi(2) + (y - cy).powi(2) <= 0.15f32.powi(2))
}

/// Menu entries' icons: the Material icons the web interface uses for the
/// same pages (`menu-icons/`, made by `web/scripts/generate-icons.py`). Theme
/// icon names would mix styles from whatever icon theme is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuIcon {
	Dashboard,
	Pair,
	Pairing,
	EndSession,
	History,
	Updates,
	Hide,
}

impl MenuIcon {
	const ALL: [MenuIcon; 7] = [
		MenuIcon::Dashboard,
		MenuIcon::Pair,
		MenuIcon::Pairing,
		MenuIcon::EndSession,
		MenuIcon::History,
		MenuIcon::Updates,
		MenuIcon::Hide,
	];

	/// White alpha mask.
	fn mask(self) -> &'static [u8] {
		match self {
			MenuIcon::Dashboard => include_bytes!("menu-icons/dashboard.png"),
			MenuIcon::Pair => include_bytes!("menu-icons/pair.png"),
			MenuIcon::Pairing => include_bytes!("menu-icons/pairing.png"),
			MenuIcon::EndSession => include_bytes!("menu-icons/end-session.png"),
			MenuIcon::History => include_bytes!("menu-icons/history.png"),
			MenuIcon::Updates => include_bytes!("menu-icons/updates.png"),
			MenuIcon::Hide => include_bytes!("menu-icons/hide.png"),
		}
	}
}

/// Whether the desktop draws menus light or dark (the desktop portal's
/// `color-scheme`); menu icons take the matching text color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Style {
	#[default]
	Light,
	Dark,
}

impl Style {
	fn color(self) -> [u8; 3] {
		match self {
			// Breeze's text colors, close to other desktops' too.
			Style::Light => [0x23, 0x26, 0x29],
			Style::Dark => [0xfc, 0xfc, 0xfc],
		}
	}
}

/// PNG menu icons for both styles.
pub fn menu_icons() -> std::collections::HashMap<(MenuIcon, Style), Vec<u8>> {
	let mut icons = std::collections::HashMap::new();
	for icon in MenuIcon::ALL {
		let mask = image::load_from_memory_with_format(icon.mask(), image::ImageFormat::Png)
			.expect("embedded menu icon decodes")
			.into_rgba8();
		for style in [Style::Light, Style::Dark] {
			let [red, green, blue] = style.color();
			let mut tinted = mask.clone();
			for pixel in tinted.pixels_mut() {
				*pixel = Rgba([red, green, blue, pixel[3]]);
			}
			let mut png = Vec::new();
			tinted
				.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
				.expect("an in-memory PNG encodes");
			icons.insert((icon, style), png);
		}
	}
	icons
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_badge_renders_every_size() {
		for badge in [
			Badge::None,
			Badge::Streaming,
			Badge::Retained,
			Badge::Busy,
			Badge::Stopping,
			Badge::Error,
			Badge::Pairing,
			Badge::Unavailable,
		] {
			let icons = pixmaps(badge);
			assert_eq!(icons.len(), SIZES.len());
			for (icon, size) in icons.iter().zip(SIZES) {
				assert_eq!(icon.data.len(), (size * size * 4) as usize);
			}
		}
		// The streaming badge is green where the plain logo is not.
		let plain = render(&BASE[5], Badge::None);
		let streaming = render(&BASE[5], Badge::Streaming);
		let corner = streaming.get_pixel(52, 46).0;
		assert_ne!(plain.get_pixel(52, 46).0, corner);
		assert!(corner[1] > corner[0] || corner == [255, 255, 255, 255], "{corner:?}");
	}

	#[test]
	fn menu_icons_take_the_style_color() {
		let icons = menu_icons();
		assert_eq!(icons.len(), MenuIcon::ALL.len() * 2);
		for style in [Style::Light, Style::Dark] {
			let png = &icons[&(MenuIcon::History, style)];
			let image = image::load_from_memory(png).unwrap().into_rgba8();
			let opaque = image.pixels().find(|pixel| pixel[3] == 255).expect("an opaque pixel");
			assert_eq!(opaque.0[..3], style.color());
			assert!(image.pixels().any(|pixel| pixel[3] == 0), "transparent background");
		}
	}
}
