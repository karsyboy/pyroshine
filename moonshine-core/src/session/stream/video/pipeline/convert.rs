//! Conventional-codec encoder input processing.
//!
//! Converts the captured RGB DMA-BUF into the encoder's YCbCr input image in a
//! single compute dispatch plus one buffer-to-image copy. The dispatch can also
//! composite the compositor cursor and a Steam notification while sampling the
//! source, which lets a fullscreen direct export keep its zero-render path
//! while those layers stay part of the encoded picture (see
//! `compositor/cursor.rs`). This is the only RGB-to-YCbCr path of the
//! conventional codecs.
//!
//! Three extents are kept apart:
//!
//! * the *frame* extent is the negotiated stream size (what the client shows);
//! * the *conversion* extent is the frame rounded up to the encoder's block
//!   alignment (see [`conversion_extent`]) and is what the copy writes into
//!   the encoder input image, whose own (coded) extent is at least as large.
//!   Pixels past the frame replicate its nearest edge pixel, so alignment
//!   padding is deterministic and cheap to encode; codec cropping metadata
//!   written by the encoder hides it;
//! * the *buffer* extent rounds the conversion extent up to whole 4x2 blocks
//!   so every 32-bit output word belongs to exactly one shader invocation
//!   (see [`Layout`]). Those extra samples are never copied.
//!
//! The work is scheduled for latency:
//!
//! * each invocation writes whole 32-bit words for a 4x2 pixel block, so no
//!   atomics and no buffer clear are needed;
//! * the submission prefers the device's dedicated compute family, which on
//!   AMD runs concurrently with a game's graphics queue instead of waiting
//!   behind its frames (a GPU-bound game otherwise delays every conversion by
//!   roughly a frame and caps the stream frame rate);
//! * conversion GPU time is measured with timestamps when supported.
//!
//! Every converter verifies its own output on the GPU against [`reference`]
//! when it is created (see [`InputConverter::new`]), so a driver that
//! miscompiles the shader fails the stream setup with a precise error instead
//! of streaming corrupted colors.

use std::sync::Arc;

use ash::vk;
use pixelforge::{Codec, ColorSpace, VideoContext};

use super::dmabuf::CachedImport;
use crate::session::compositor::frame::{MAX_OVERLAYS, OverlayFormat, OverlayImage};

/// Precompiled from `shaders/convert.comp` by `scripts/build-shaders.sh`.
const CONVERT_SPIRV: &[u8] = include_bytes!("shaders/convert.spv");

/// Pixels per shader invocation (one 4x2 block).
const BLOCK_WIDTH: u32 = 4;
const BLOCK_HEIGHT: u32 = 2;
/// Invocations per workgroup in each dimension (`local_size_x/y`).
const GROUP_SIZE: u32 = 8;

/// Views kept for recently seen source images (swapchains rotate 2-4 images).
const MAX_CACHED_VIEWS: usize = 8;

/// Queue preference for the conversion submission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionQueueMode {
	/// Dedicated compute family when the device exposes one, else graphics.
	#[default]
	Auto,
	/// The graphics-capable family Pixelforge uses for its own work.
	Graphics,
	/// Require the dedicated compute family; falls back to graphics if absent.
	Compute,
}

/// Mirrors the shader's push-constant block (std430 offsets).
#[repr(C)]
#[derive(Clone, Copy)]
struct PushConstants {
	width: u32,
	height: u32,
	source_max_x: u32,
	source_max_y: u32,
	output_format: u32,
	color_space: u32,
	full_range: u32,
	sdr_white_nits: f32,
	luma_pitch: u32,
	chroma_offset: u32,
	chroma_pitch: u32,
	blocks_x: u32,
	blocks_y: u32,
	_padding: [u32; 3],
	layer_rect: [[i32; 4]; MAX_OVERLAYS],
	layer_opacity: [f32; MAX_OVERLAYS],
	layer_opaque: [u32; MAX_OVERLAYS],
}

// Every implementation supports at least 128 bytes of push constants.
const _: () = assert!(std::mem::size_of::<PushConstants>() == 112);

/// Where a late-composition layer's texels come from.
pub(crate) enum LayerInput<'a> {
	/// CPU texels, uploaded once per content generation.
	Pixels(&'a OverlayImage),
	/// An imported client DMA-BUF, read in place like the game source.
	Imported {
		import: &'a Arc<CachedImport>,
		format: vk::Format,
		first_use: bool,
		/// Ignore the alpha channel (X formats).
		opaque: bool,
	},
}

/// One layer composited over the source, at `(x, y)` in output pixels.
pub(crate) struct Layer<'a> {
	pub input: LayerInput<'a>,
	pub x: i32,
	pub y: i32,
	pub width: u32,
	pub height: u32,
	pub opacity: f32,
}

/// Layers bottom to top (notification, cursor).
pub(crate) type Layers<'a> = [Option<Layer<'a>>; MAX_OVERLAYS];

/// Where the converted planes are copied, for each encoder backend.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Target {
	/// A multi-planar image addressed by plane aspects, and the layout it is
	/// in before the conversion and must be left in afterwards.
	Image { image: vk::Image, layout: vk::ImageLayout },
	/// One single-plane image per output plane, imported from memory another
	/// API owns (the DMA-BUF layers of a VA-API surface). Their content is
	/// replaced: each is acquired from `QUEUE_FAMILY_EXTERNAL` without
	/// preserving it and released back in `GENERAL` after the copy.
	ExternalPlanes { images: [vk::Image; 2], count: usize },
	/// A buffer with exactly this converter's [`Layout`] that another API reads
	/// in place (an NVENC input buffer shared with CUDA). It receives a
	/// buffer copy.
	Buffer { buffer: vk::Buffer },
}

impl Target {
	/// A Pixelforge encoder input slot. Pixelforge clears each slot at
	/// creation and leaves it in `VIDEO_ENCODE_SRC_KHR`, and every conversion
	/// returns it there.
	pub(crate) fn encoder_input(image: vk::Image) -> Self {
		Self::Image {
			image,
			layout: vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
		}
	}
}

/// Output sample layout of a conversion.
///
/// The 2-plane layouts match Vulkan Video's input formats; the packed 4:4:4
/// layouts are what VA-API (and NVENC for 8-bit) encode 4:4:4 from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ConvertFormat {
	/// 8-bit luma plane, then interleaved CbCr at half width and height (NV12).
	Nv12,
	/// NV12 with 16-bit samples holding 10 bits in the high bits (P010).
	P010,
	/// 8-bit luma plane, then interleaved CbCr at full resolution
	/// (`G8_B8R8_2PLANE_444_UNORM`).
	Yuv444,
	/// [`Self::Yuv444`] with P010-style 16-bit samples.
	Yuv444P10,
	/// One little-endian 32-bit word per pixel: Cr, Cb, Y and 0xFF from the
	/// lowest byte. VA-API `XYUV`, DRM `XYUV8888` and NVENC `AYUV` share it.
	Xyuv,
	/// One little-endian 32-bit word per pixel: Cb in bits 0-9, Y in 10-19,
	/// Cr in 20-29 and alpha 3 in 30-31 (VA-API and DRM `Y410`).
	Y410,
}

impl ConvertFormat {
	/// Bits per sample.
	pub(crate) fn bit_depth(self) -> u32 {
		match self {
			Self::Nv12 | Self::Yuv444 | Self::Xyuv => 8,
			Self::P010 | Self::Yuv444P10 | Self::Y410 => 10,
		}
	}

	/// Whether chroma has half the luma width and height.
	pub(crate) fn subsampled(self) -> bool {
		matches!(self, Self::Nv12 | Self::P010)
	}
}

/// Per-format sample sizes: the 2-plane (Y + interleaved CbCr) layouts, or a
/// single plane of packed 4:4:4 words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FormatInfo {
	/// Shader format code.
	code: u32,
	/// Bytes per luma sample, or per pixel of a packed format.
	luma_bytes: u32,
	/// Bytes per CbCr pair; 0 for packed formats (no chroma plane).
	chroma_bytes: u32,
	/// 4:2:0 (chroma at half width and height) rather than 4:4:4.
	subsampled: bool,
}

impl FormatInfo {
	fn packed(&self) -> bool {
		self.chroma_bytes == 0
	}
}

fn format_info(format: ConvertFormat) -> FormatInfo {
	let info = |code, luma_bytes, chroma_bytes, subsampled| FormatInfo {
		code,
		luma_bytes,
		chroma_bytes,
		subsampled,
	};
	match format {
		ConvertFormat::Nv12 => info(0, 1, 2, true),
		ConvertFormat::Yuv444 => info(2, 1, 2, false),
		ConvertFormat::P010 => info(3, 2, 4, true),
		ConvertFormat::Yuv444P10 => info(4, 2, 4, false),
		ConvertFormat::Xyuv => info(5, 4, 0, false),
		ConvertFormat::Y410 => info(6, 4, 0, false),
	}
}

fn color_space_code(color_space: ColorSpace) -> u32 {
	match color_space {
		ColorSpace::Bt709 => 0,
		ColorSpace::Bt2020 => 1,
		ColorSpace::SrgbToBt2020Pq => 2,
		ColorSpace::Bt709LinearToBt2020Pq => 3,
	}
}

/// Extent the converter writes into a Pixelforge encoder input image for a
/// `width` x `height` frame: rounded up to the codec's block size (macroblock,
/// CTB or superblock).
///
/// Pixelforge sizes its input images to the coded extent, which it aligns to
/// a multiple of this block size (and of the device's picture access
/// granularity) and never below the device minimum, so this extent always
/// fits the image. The blocks past the frame are cropped by the codec headers
/// Pixelforge writes; filling them with replicated edge pixels keeps them
/// cheap to encode. Any further granularity padding keeps the zeros Pixelforge
/// cleared the image with.
pub(crate) fn conversion_extent(codec: Codec, width: u32, height: u32) -> Result<(u32, u32), String> {
	let unit = pixelforge::codec_block_size(codec);
	match (
		width.checked_next_multiple_of(unit),
		height.checked_next_multiple_of(unit),
	) {
		(Some(w), Some(h)) if width != 0 && height != 0 => Ok((w, h)),
		_ => Err(format!("{width}x{height} has no {codec:?} conversion extent")),
	}
}

/// Plane layout of the conversion buffer and of its copy into the encoder
/// input.
///
/// Both planes start on a 32-bit word and every row is a whole number of
/// words, so each word lies in one row and belongs to exactly one 4x2 block:
/// luma rows hold `4 * blocks_x` samples, chroma rows hold the CbCr pairs of
/// the same blocks (`2 * blocks_x` for 4:2:0, `4 * blocks_x` for 4:4:4).
/// Packed 4:4:4 formats have one plane of one word per pixel. The
/// copy regions address the conversion extent through `bufferRowLength`, so
/// the samples past it are never copied. Word-aligned offsets also satisfy
/// `VkBufferImageCopy::bufferOffset` (a multiple of each plane's element
/// size, at most four bytes) on any queue family.
///
/// [`Self::aligned`] pads rows to a byte alignment for a [`Target::Buffer`]
/// read in place: there every plane shares the luma row pitch in bytes
/// (4:2:0) and the chroma plane starts right after the last luma row, which is
/// how NVENC addresses a registered pitched buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
	pub format: ConvertFormat,
	info: FormatInfo,
	/// Conversion extent copied into the target image.
	pub extent: (u32, u32),
	/// Invocation grid: 4x2 pixel blocks covering `extent`.
	pub blocks: (u32, u32),
	/// Luma row pitch in 32-bit words.
	pub luma_pitch: u32,
	/// Start of the chroma plane in 32-bit words.
	pub chroma_offset: u32,
	/// Chroma row pitch in 32-bit words.
	pub chroma_pitch: u32,
	/// Buffer size in bytes.
	pub size: u64,
}

impl Layout {
	#[cfg(test)]
	pub(crate) fn new(format: ConvertFormat, extent: (u32, u32)) -> Result<Self, String> {
		Self::aligned(format, extent, 4)
	}

	/// A layout whose row pitches are multiples of `row_alignment` bytes (a
	/// multiple of four).
	pub(crate) fn aligned(
		format: ConvertFormat,
		(width, height): (u32, u32),
		row_alignment: u32,
	) -> Result<Self, String> {
		let info = format_info(format);
		if row_alignment == 0 || !row_alignment.is_multiple_of(4) {
			return Err(format!("row alignment {row_alignment} is not a multiple of four bytes"));
		}
		if width == 0 || height == 0 {
			return Err(format!("empty conversion extent {width}x{height}"));
		}
		if info.subsampled && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
			return Err(format!(
				"4:2:0 conversion extent {width}x{height} must be even (chroma covers 2x2 pixels)"
			));
		}
		let blocks_x = u64::from(width.div_ceil(BLOCK_WIDTH));
		let blocks_y = u64::from(height.div_ceil(BLOCK_HEIGHT));
		let alignment_words = u64::from(row_alignment / 4);
		// Words per row before alignment, then the chroma rows' share of it:
		// equal for 4:2:0 (half as many pairs of twice the size), twice the
		// luma width for 2-plane 4:4:4, none for packed formats.
		let luma_words = blocks_x * u64::from(info.luma_bytes);
		let luma_pitch = luma_words.next_multiple_of(alignment_words);
		let (chroma_pitch, chroma_rows) = if info.packed() {
			(0, 0)
		} else if info.subsampled {
			(
				luma_pitch * 2 * u64::from(info.chroma_bytes) / 4 / u64::from(info.luma_bytes),
				blocks_y,
			)
		} else {
			(
				luma_pitch * u64::from(info.chroma_bytes) / u64::from(info.luma_bytes),
				blocks_y * 2,
			)
		};
		let chroma_offset = luma_pitch * blocks_y * 2;
		let words = chroma_offset + chroma_pitch * chroma_rows;
		// The shader indexes words with 32-bit integers.
		let word = |value: u64| {
			u32::try_from(value).map_err(|_| format!("{format:?} {width}x{height} exceeds 32-bit buffer addressing"))
		};
		word(words)?;
		Ok(Self {
			format,
			info,
			extent: (width, height),
			blocks: (word(blocks_x)?, word(blocks_y)?),
			luma_pitch: word(luma_pitch)?,
			chroma_offset: word(chroma_offset)?,
			chroma_pitch: word(chroma_pitch)?,
			size: words * 4,
		})
	}

	/// Chroma plane extent of the conversion.
	pub(crate) fn chroma_extent(&self) -> (u32, u32) {
		if self.info.subsampled {
			(self.extent.0 / 2, self.extent.1 / 2)
		} else {
			self.extent
		}
	}

	/// Number of output planes (2, or 1 for packed formats).
	pub(crate) fn plane_count(&self) -> usize {
		if self.info.packed() { 1 } else { 2 }
	}

	/// Row pitch of the luma (or packed) plane in bytes.
	pub(crate) fn row_pitch_bytes(&self) -> u32 {
		self.luma_pitch * 4
	}

	/// Rows of the luma plane in the buffer: the conversion height rounded up
	/// to the 4x2 block grid. The chroma plane follows them.
	pub(crate) fn luma_rows(&self) -> u32 {
		self.blocks.1 * BLOCK_HEIGHT
	}

	/// Plane regions for a multi-planar target ([`Target::Image`]); only the
	/// first [`Self::plane_count`] are used.
	pub(crate) fn copy_regions(&self) -> [vk::BufferImageCopy; 2] {
		self.plane_regions([vk::ImageAspectFlags::PLANE_0, vk::ImageAspectFlags::PLANE_1])
	}

	/// The same regions for per-plane images ([`Target::ExternalPlanes`]).
	pub(crate) fn plane_image_regions(&self) -> [vk::BufferImageCopy; 2] {
		self.plane_regions([vk::ImageAspectFlags::COLOR; 2])
	}

	fn plane_regions(&self, aspects: [vk::ImageAspectFlags; 2]) -> [vk::BufferImageCopy; 2] {
		let region = |offset, row_length, aspect, (width, height)| vk::BufferImageCopy {
			buffer_offset: offset,
			buffer_row_length: row_length,
			buffer_image_height: 0,
			image_subresource: vk::ImageSubresourceLayers {
				aspect_mask: aspect,
				mip_level: 0,
				base_array_layer: 0,
				layer_count: 1,
			},
			image_offset: vk::Offset3D::default(),
			image_extent: vk::Extent3D {
				width,
				height,
				depth: 1,
			},
		};
		let chroma = if self.info.packed() {
			// Unused: packed formats have one plane.
			region(0, 0, aspects[1], (0, 0))
		} else {
			region(
				u64::from(self.chroma_offset) * 4,
				self.chroma_pitch * 4 / self.info.chroma_bytes,
				aspects[1],
				self.chroma_extent(),
			)
		};
		[
			region(0, self.luma_pitch * 4 / self.info.luma_bytes, aspects[0], self.extent),
			chroma,
		]
	}

	/// Decode the conversion extent of a buffer with this layout into code
	/// values (10-bit samples shifted down from their P010 position).
	pub(crate) fn read_planes(&self, bytes: &[u8]) -> Result<reference::Planes, String> {
		if (bytes.len() as u64) < self.size {
			return Err(format!("buffer holds {} of {} bytes", bytes.len(), self.size));
		}
		if self.info.packed() {
			return self.read_packed(bytes);
		}
		let ten_bit = self.info.luma_bytes == 2;
		let sample = |offset: usize| -> Result<u16, String> {
			if !ten_bit {
				return Ok(u16::from(bytes[offset]));
			}
			let raw = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
			if raw & 0x3f != 0 {
				return Err(format!("10-bit sample 0x{raw:04x} at byte {offset} has low bits set"));
			}
			Ok(raw >> 6)
		};
		let (width, height) = self.extent;
		let (chroma_width, chroma_height) = self.chroma_extent();
		let luma_bytes = self.info.luma_bytes as usize;
		let mut planes = reference::Planes::new(self.extent, (chroma_width, chroma_height));
		for y in 0..height as usize {
			for x in 0..width as usize {
				planes.luma[y * width as usize + x] = sample(y * self.luma_pitch as usize * 4 + x * luma_bytes)?;
			}
		}
		let component = luma_bytes;
		for y in 0..chroma_height as usize {
			let row = (self.chroma_offset as usize + y * self.chroma_pitch as usize) * 4;
			for x in 0..chroma_width as usize {
				let pair = row + x * 2 * component;
				planes.chroma[y * chroma_width as usize + x] = [sample(pair)?, sample(pair + component)?];
			}
		}
		Ok(planes)
	}

	/// [`Self::read_planes`] for the packed 4:4:4 formats.
	fn read_packed(&self, bytes: &[u8]) -> Result<reference::Planes, String> {
		let (width, height) = self.extent;
		let mut planes = reference::Planes::new(self.extent, self.extent);
		for y in 0..height as usize {
			for x in 0..width as usize {
				let offset = y * self.luma_pitch as usize * 4 + x * 4;
				let word = u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]);
				let index = y * width as usize + x;
				let (luma, cb, cr) = match self.format {
					ConvertFormat::Xyuv => {
						if word >> 24 != 0xff {
							return Err(format!(
								"XYUV word 0x{word:08x} at byte {offset} has alpha other than 0xff"
							));
						}
						((word >> 16) & 0xff, (word >> 8) & 0xff, word & 0xff)
					},
					_ => {
						if word >> 30 != 3 {
							return Err(format!(
								"Y410 word 0x{word:08x} at byte {offset} has alpha other than 3"
							));
						}
						((word >> 10) & 0x3ff, word & 0x3ff, (word >> 20) & 0x3ff)
					},
				};
				planes.luma[index] = luma as u16;
				planes.chroma[index] = [cb as u16, cr as u16];
			}
		}
		Ok(planes)
	}
}

/// Independent CPU definition of the conversion, used by the creation-time
/// self-check and the tests.
///
/// The equations follow the standards rather than the shader: luma weights
/// Kr/Kb of Rec. ITU-R BT.709 / BT.2020 (non-constant luminance), Cb and Cr as
/// scaled B-Y and R-Y differences, SMPTE ST 2084 (PQ), the IEC 61966-2-1 sRGB
/// decoding function, the Rec. ITU-R BT.2087 primaries conversion, and the
/// Rec. ITU-R BT.2100 / ITU-T H.273 quantization with round-to-nearest. 4:2:0
/// chroma is the mean of the 2x2 co-sited pixels; pixels past the frame
/// replicate its nearest edge pixel. Arithmetic is in f64, so the GPU (f32)
/// may differ by one code where a value lies on a rounding boundary.
pub(crate) mod reference {
	use super::ConvertFormat;
	use pixelforge::ColorSpace;

	/// Color conversion parameters of one frame.
	#[derive(Clone, Copy, Debug)]
	pub(crate) struct Color {
		pub space: ColorSpace,
		pub full_range: bool,
		/// Nits of the SDR/scRGB reference white in the PQ conversions.
		pub sdr_white_nits: f32,
	}

	/// Code values of both planes over an extent. 10-bit samples are stored
	/// as 0..=1023.
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub(crate) struct Planes {
		pub extent: (u32, u32),
		pub chroma_extent: (u32, u32),
		pub luma: Vec<u16>,
		/// (Cb, Cr) per chroma sample.
		pub chroma: Vec<[u16; 2]>,
	}

	impl Planes {
		pub(crate) fn new(extent: (u32, u32), chroma_extent: (u32, u32)) -> Self {
			Self {
				extent,
				chroma_extent,
				luma: vec![0; extent.0 as usize * extent.1 as usize],
				chroma: vec![[0; 2]; chroma_extent.0 as usize * chroma_extent.1 as usize],
			}
		}
	}

	fn pq(linear: f64) -> f64 {
		const M1: f64 = 2610.0 / 16384.0;
		const M2: f64 = 2523.0 / 4096.0 * 128.0;
		const C1: f64 = 3424.0 / 4096.0;
		const C2: f64 = 2413.0 / 4096.0 * 32.0;
		const C3: f64 = 2392.0 / 4096.0 * 32.0;
		let lm1 = linear.max(0.0).powf(M1);
		((C1 + C2 * lm1) / (1.0 + C3 * lm1)).powf(M2)
	}

	fn srgb_to_linear(value: f64) -> f64 {
		let value = value.clamp(0.0, 1.0);
		if value <= 0.04045 {
			value / 12.92
		} else {
			((value + 0.055) / 1.055).powf(2.4)
		}
	}

	fn bt709_to_bt2020([r, g, b]: [f64; 3]) -> [f64; 3] {
		[
			0.6274 * r + 0.3293 * g + 0.0433 * b,
			0.0691 * r + 0.9195 * g + 0.0114 * b,
			0.0164 * r + 0.0880 * g + 0.8956 * b,
		]
	}

	/// Normalized (Y, Cb + 1/2, Cr + 1/2), each clamped to [0, 1], of an RGB
	/// sample in the source's signal encoding.
	pub(crate) fn ycbcr(rgb: [f32; 3], color: Color) -> [f64; 3] {
		let rgb = rgb.map(f64::from);
		let scale = f64::from(color.sdr_white_nits) / 10_000.0;
		let signal = match color.space {
			ColorSpace::Bt709 | ColorSpace::Bt2020 => rgb,
			ColorSpace::SrgbToBt2020Pq => bt709_to_bt2020(rgb.map(srgb_to_linear)).map(|c| pq(c * scale)),
			ColorSpace::Bt709LinearToBt2020Pq => bt709_to_bt2020(rgb).map(|c| pq(c * scale)),
		};
		let (kr, kb) = match color.space {
			ColorSpace::Bt709 => (0.2126, 0.0722),
			_ => (0.2627, 0.0593),
		};
		let [r, g, b] = signal;
		let y = kr * r + (1.0 - kr - kb) * g + kb * b;
		let cb = (b - y) / (2.0 * (1.0 - kb));
		let cr = (r - y) / (2.0 * (1.0 - kr));
		[
			y.clamp(0.0, 1.0),
			(cb + 0.5).clamp(0.0, 1.0),
			(cr + 0.5).clamp(0.0, 1.0),
		]
	}

	fn round_clamped(value: f64, bits: u32) -> u16 {
		(value + 0.5).floor().clamp(0.0, f64::from((1u32 << bits) - 1)) as u16
	}

	/// Luma code of a normalized Y.
	pub(crate) fn luma_code(y: f64, bits: u32, full_range: bool) -> u16 {
		let value = if full_range {
			y * f64::from((1u32 << bits) - 1)
		} else {
			(219.0 * y + 16.0) * f64::from(1u32 << (bits - 8))
		};
		round_clamped(value, bits)
	}

	/// Chroma code of a normalized Cb or Cr (offset by 1/2).
	pub(crate) fn chroma_code(c: f64, bits: u32, full_range: bool) -> u16 {
		let e = c - 0.5;
		let value = if full_range {
			e * f64::from((1u32 << bits) - 1) + f64::from(1u32 << (bits - 1))
		} else {
			(224.0 * e + 128.0) * f64::from(1u32 << (bits - 8))
		};
		round_clamped(value, bits)
	}

	/// Expected planes of `format` over `extent` for a `frame`-sized source
	/// whose (composited) RGB is `rgb(x, y)`. Rows for which `row` is false
	/// are left zero (and skipped by [`compare`]); chroma rows follow their
	/// first luma row.
	pub(crate) fn convert(
		format: ConvertFormat,
		color: Color,
		frame: (u32, u32),
		extent: (u32, u32),
		rgb: &dyn Fn(u32, u32) -> [f32; 3],
		row: &dyn Fn(u32) -> bool,
	) -> Planes {
		let (bits, subsampled) = (format.bit_depth(), format.subsampled());
		let chroma_extent = if subsampled {
			(extent.0.div_ceil(2), extent.1.div_ceil(2))
		} else {
			extent
		};
		let mut planes = Planes::new(extent, chroma_extent);
		let at = |x: u32, y: u32| ycbcr(rgb(x.min(frame.0 - 1), y.min(frame.1 - 1)), color);
		let width = extent.0 as usize;
		for y in 0..extent.1 {
			let include = if subsampled { row(y & !1) } else { row(y) };
			if !include {
				continue;
			}
			for x in 0..extent.0 {
				let [luma, cb, cr] = at(x, y);
				planes.luma[y as usize * width + x as usize] = luma_code(luma, bits, color.full_range);
				if !subsampled {
					planes.chroma[y as usize * width + x as usize] = [
						chroma_code(cb, bits, color.full_range),
						chroma_code(cr, bits, color.full_range),
					];
				} else if y % 2 == 0 && x % 2 == 0 {
					let mut sum = [0.0; 2];
					for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
						let [_, cb, cr] = at(x + dx, y + dy);
						sum[0] += cb;
						sum[1] += cr;
					}
					let index = (y / 2) as usize * chroma_extent.0 as usize + (x / 2) as usize;
					planes.chroma[index] = sum.map(|s| chroma_code(s / 4.0, bits, color.full_range));
				}
			}
		}
		planes
	}

	/// Outcome of a plane comparison within the tolerance.
	#[derive(Debug, Default)]
	pub(crate) struct Agreement {
		/// Samples (luma or chroma components) that differ at all.
		pub differing: u64,
		pub compared: u64,
		pub max_difference: u16,
	}

	/// Compare `actual` with `expected` over the rows `row` selects. Fails
	/// with a description that locates the differences by plane and by sample
	/// position within the shader's 4x2 blocks, which identifies layout or
	/// per-block defects (e.g. a lost second chroma sample of every block).
	pub(crate) fn compare(
		expected: &Planes,
		actual: &Planes,
		tolerance: u16,
		row: &dyn Fn(u32) -> bool,
	) -> Result<Agreement, String> {
		if expected.extent != actual.extent || expected.chroma_extent != actual.chroma_extent {
			return Err(format!(
				"plane extents differ: expected {:?}/{:?}, got {:?}/{:?}",
				expected.extent, expected.chroma_extent, actual.extent, actual.chroma_extent
			));
		}
		let mut agreement = Agreement::default();
		// Out-of-tolerance samples by plane (0 luma, 1 Cb, 2 Cr) and by column
		// within a block (4 luma columns; 2 or 4 chroma columns).
		let mut failures = [[0u64; 4]; 3];
		let mut first: Option<String> = None;
		let subsampled = expected.chroma_extent != expected.extent;
		let chroma_per_block = if subsampled { 2 } else { 4 };
		let mut check = |plane: usize, x: u32, y: u32, want: u16, got: u16| {
			agreement.compared += 1;
			let difference = want.abs_diff(got);
			if difference == 0 {
				return;
			}
			agreement.differing += 1;
			agreement.max_difference = agreement.max_difference.max(difference);
			if difference > tolerance {
				let per_block = if plane == 0 { 4 } else { chroma_per_block };
				failures[plane][(x % per_block) as usize] += 1;
				first.get_or_insert_with(|| {
					format!("{} ({x}, {y}): expected {want}, got {got}", ["Y", "Cb", "Cr"][plane])
				});
			}
		};
		let (width, height) = expected.extent;
		for y in (0..height).filter(|&y| row(y)) {
			for x in 0..width {
				let index = (y * width + x) as usize;
				check(0, x, y, expected.luma[index], actual.luma[index]);
			}
		}
		let (chroma_width, chroma_height) = expected.chroma_extent;
		for y in 0..chroma_height {
			if !row(if subsampled { y * 2 } else { y }) {
				continue;
			}
			for x in 0..chroma_width {
				let index = (y * chroma_width + x) as usize;
				for component in 0..2 {
					check(
						1 + component,
						x,
						y,
						expected.chroma[index][component],
						actual.chroma[index][component],
					);
				}
			}
		}
		let total: u64 = failures.iter().flatten().sum();
		if total == 0 {
			return Ok(agreement);
		}
		let by_column = |plane: usize, columns: usize| {
			failures[plane][..columns]
				.iter()
				.map(u64::to_string)
				.collect::<Vec<_>>()
				.join("/")
		};
		Err(format!(
			"{total} samples differ by more than {tolerance} (max {}); first at {}; \
			 by column within a 4x2 block: Y {}, Cb {}, Cr {}",
			agreement.max_difference,
			first.unwrap_or_default(),
			by_column(0, 4),
			by_column(1, chroma_per_block as usize),
			by_column(2, chroma_per_block as usize),
		))
	}
}

fn color_range() -> vk::ImageSubresourceRange {
	vk::ImageSubresourceRange {
		aspect_mask: vk::ImageAspectFlags::COLOR,
		base_mip_level: 0,
		level_count: 1,
		base_array_layer: 0,
		layer_count: 1,
	}
}

fn find_memory_type(context: &VideoContext, type_bits: u32, required: vk::MemoryPropertyFlags) -> Result<u32, String> {
	// SAFETY: plain physical-device property query.
	let properties = unsafe {
		context
			.instance()
			.get_physical_device_memory_properties(context.physical_device())
	};
	(0..properties.memory_type_count)
		.find(|&index| {
			type_bits & (1 << index) != 0
				&& properties.memory_types[index as usize]
					.property_flags
					.contains(required)
		})
		.ok_or_else(|| format!("no memory type with {required:?}"))
}

/// GPU-resident copy of one layer's CPU texels.
struct OverlayTexture {
	image: vk::Image,
	memory: vk::DeviceMemory,
	views: [vk::ImageView; 2],
	staging: vk::Buffer,
	staging_memory: vk::DeviceMemory,
	staging_ptr: *mut u8,
	width: u32,
	height: u32,
	/// Content generation currently in `image`; `None` until first upload.
	generation: Option<u64>,
	/// The image is still UNDEFINED and needs its first layout transition.
	needs_init: bool,
}

impl OverlayTexture {
	const EMPTY: Self = Self {
		image: vk::Image::null(),
		memory: vk::DeviceMemory::null(),
		views: [vk::ImageView::null(); 2],
		staging: vk::Buffer::null(),
		staging_memory: vk::DeviceMemory::null(),
		staging_ptr: std::ptr::null_mut(),
		width: 0,
		height: 0,
		generation: None,
		needs_init: true,
	};
}

/// Five-second conversion summary for `log_stats`.
#[derive(Default)]
pub(crate) struct ConvertWindow {
	started: Option<std::time::Instant>,
	frames: u64,
	gpu_ns: u64,
	gpu_samples: u64,
	late_layer_frames: u64,
}

impl ConvertWindow {
	pub(crate) fn record(&mut self, gpu_ns: Option<u64>, layered: bool) {
		self.frames += 1;
		if let Some(ns) = gpu_ns {
			self.gpu_ns += ns;
			self.gpu_samples += 1;
		}
		self.late_layer_frames += u64::from(layered);
	}

	pub(crate) fn maybe_log(&mut self, async_compute: bool) {
		let started = *self.started.get_or_insert_with(std::time::Instant::now);
		if started.elapsed() < std::time::Duration::from_secs(5) {
			return;
		}
		tracing::info!(
			async_compute,
			converted_frames = self.frames,
			convert_gpu_us = (self.gpu_samples != 0).then(|| self.gpu_ns as f64 / self.gpu_samples as f64 / 1e3),
			late_layer_frames = self.late_layer_frames,
			"Video conversion summary"
		);
		*self = Self {
			started: Some(std::time::Instant::now()),
			..Self::default()
		};
	}
}

/// Result of one conversion; the source is no longer read by the GPU.
pub(crate) struct ConvertOutcome {
	/// Measured GPU execution time of the conversion and copy, if supported.
	pub gpu_ns: Option<u64>,
}

/// What last wrote the output buffer before a copy is recorded.
#[derive(Clone, Copy)]
enum BufferWriter {
	/// The dispatch earlier in the same command buffer.
	Dispatch,
	/// A conversion whose submission already completed (static recovery).
	Completed,
}

/// The sampled source of one conversion.
struct SourceBinding {
	image: vk::Image,
	view: vk::ImageView,
	extent: (u32, u32),
	/// First use of an imported (EXCLUSIVE, external) image: acquire it.
	first_use: bool,
}

pub(crate) struct InputConverter {
	context: VideoContext,
	queue: vk::Queue,
	queue_family: u32,
	/// Whether conversion runs on a dedicated compute family.
	pub(crate) async_compute: bool,
	/// Frame (logical) extent.
	width: u32,
	height: u32,
	layout: Layout,
	color_space: ColorSpace,
	full_range: bool,
	sdr_white_nits: f32,
	set_layout: vk::DescriptorSetLayout,
	pipeline_layout: vk::PipelineLayout,
	pipeline: vk::Pipeline,
	sampler: vk::Sampler,
	descriptor_pool: vk::DescriptorPool,
	descriptor_set: vk::DescriptorSet,
	output_buffer: vk::Buffer,
	output_memory: vk::DeviceMemory,
	/// Host mapping of `output_memory` for self-check converters, else null.
	readback: *const u8,
	/// Whether `output_buffer` holds a completed conversion (false from the
	/// start of a conversion until it succeeds).
	output_valid: bool,
	/// Upload textures for CPU-texel layers, one per layer slot.
	layer_textures: [OverlayTexture; MAX_OVERLAYS],
	/// Views of recently used source images. The `Arc` pins each image for as
	/// long as its view exists; entries the importer no longer holds are
	/// dropped first.
	views: Vec<(Arc<CachedImport>, vk::ImageView)>,
	command_pool: vk::CommandPool,
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	query_pool: vk::QueryPool,
	timestamp_period_ns: f64,
}

impl InputConverter {
	/// Create a converter for `frame`-sized captures that writes `extent`
	/// (see [`conversion_extent`]) into the encoder input image.
	///
	/// Before returning, a small converter with the same queue and output
	/// format converts a fixed pattern with partial blocks and edge padding
	/// and its output is compared with [`reference`]; a mismatch is returned
	/// as an error that locates the defect, since every frame would be
	/// corrupted the same way.
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn new(
		context: VideoContext,
		mode: ConversionQueueMode,
		frame: (u32, u32),
		extent: (u32, u32),
		output_format: ConvertFormat,
		row_alignment: u32,
		color_space: ColorSpace,
		full_range: bool,
	) -> Result<Self, ConverterError> {
		let converter = Self::create(
			context.clone(),
			mode,
			frame,
			extent,
			output_format,
			row_alignment,
			color_space,
			full_range,
			false,
		)
		.map_err(ConverterError::Create)?;
		self_check(&context, mode, output_format)?;
		Ok(converter)
	}

	#[allow(clippy::too_many_arguments)]
	fn create(
		context: VideoContext,
		mode: ConversionQueueMode,
		(width, height): (u32, u32),
		extent: (u32, u32),
		output_format: ConvertFormat,
		row_alignment: u32,
		color_space: ColorSpace,
		full_range: bool,
		readback: bool,
	) -> Result<Self, String> {
		if width == 0 || height == 0 || extent.0 < width || extent.1 < height {
			return Err(format!(
				"conversion extent {}x{} does not cover the {width}x{height} frame",
				extent.0, extent.1
			));
		}
		let layout = Layout::aligned(output_format, extent, row_alignment)?;
		let limits = context.device_properties().limits;
		if layout.size > u64::from(limits.max_storage_buffer_range) {
			return Err(format!(
				"{output_format:?} {}x{} needs a {} byte storage buffer; this GPU addresses at most {}",
				extent.0, extent.1, layout.size, limits.max_storage_buffer_range
			));
		}
		let groups = (
			layout.blocks.0.div_ceil(GROUP_SIZE),
			layout.blocks.1.div_ceil(GROUP_SIZE),
		);
		if groups.0 > limits.max_compute_work_group_count[0] || groups.1 > limits.max_compute_work_group_count[1] {
			return Err(format!(
				"{}x{} needs {}x{} workgroups; this GPU dispatches at most {}x{}",
				extent.0,
				extent.1,
				groups.0,
				groups.1,
				limits.max_compute_work_group_count[0],
				limits.max_compute_work_group_count[1]
			));
		}
		// SAFETY: plain physical-device property query.
		let families = unsafe {
			context
				.instance()
				.get_physical_device_queue_family_properties(context.physical_device())
		};
		let dedicated = context.transfer_queue_family();
		let dedicated_compute = dedicated != context.compute_queue_family()
			&& families.get(dedicated as usize).is_some_and(|family| {
				family.queue_flags.contains(vk::QueueFlags::COMPUTE)
					&& !family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
			});
		let use_dedicated = dedicated_compute && mode != ConversionQueueMode::Graphics;
		if mode == ConversionQueueMode::Compute && !dedicated_compute && !readback {
			tracing::warn!("No dedicated compute queue family is available; converting on the graphics family");
		}
		// The encoder input image is shared CONCURRENTLY by Pixelforge between
		// its encode, transfer and compute families, so either family may write it.
		let (queue_family, queue) = if use_dedicated {
			(dedicated, context.transfer_queue())
		} else {
			(context.compute_queue_family(), context.compute_queue())
		};
		let timestamps = families
			.get(queue_family as usize)
			.is_some_and(|family| family.timestamp_valid_bits != 0);

		let mut converter = Self {
			queue,
			queue_family,
			async_compute: use_dedicated,
			width,
			height,
			layout,
			color_space,
			full_range,
			sdr_white_nits: 203.0,
			set_layout: vk::DescriptorSetLayout::null(),
			pipeline_layout: vk::PipelineLayout::null(),
			pipeline: vk::Pipeline::null(),
			sampler: vk::Sampler::null(),
			descriptor_pool: vk::DescriptorPool::null(),
			descriptor_set: vk::DescriptorSet::null(),
			output_buffer: vk::Buffer::null(),
			output_memory: vk::DeviceMemory::null(),
			readback: std::ptr::null(),
			output_valid: false,
			layer_textures: [OverlayTexture::EMPTY; MAX_OVERLAYS],
			views: Vec::new(),
			command_pool: vk::CommandPool::null(),
			command_buffer: vk::CommandBuffer::null(),
			fence: vk::Fence::null(),
			query_pool: vk::QueryPool::null(),
			timestamp_period_ns: f64::from(limits.timestamp_period),
			context,
		};
		// Partially created objects are released by Drop on error.
		converter.create_resources(timestamps, readback)?;
		// 1x1 placeholders keep the layer bindings valid without layers.
		for slot in 0..MAX_OVERLAYS {
			converter.ensure_overlay_capacity(slot, 1, 1)?;
		}
		Ok(converter)
	}

	fn create_resources(&mut self, timestamps: bool, readback: bool) -> Result<(), String> {
		let device = self.context.device().clone();
		let err = |what: &str| {
			let what = what.to_string();
			move |e: vk::Result| format!("{what}: {e}")
		};
		let bindings = [
			vk::DescriptorSetLayoutBinding::default()
				.binding(0)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(1)
				.descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(2)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
			vk::DescriptorSetLayoutBinding::default()
				.binding(3)
				.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1)
				.stage_flags(vk::ShaderStageFlags::COMPUTE),
		];
		let push_range = vk::PushConstantRange::default()
			.stage_flags(vk::ShaderStageFlags::COMPUTE)
			.size(std::mem::size_of::<PushConstants>() as u32);
		let pool_sizes = [
			vk::DescriptorPoolSize::default()
				.ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
				.descriptor_count(1 + MAX_OVERLAYS as u32),
			vk::DescriptorPoolSize::default()
				.ty(vk::DescriptorType::STORAGE_BUFFER)
				.descriptor_count(1),
		];
		let code = ash::util::read_spv(&mut std::io::Cursor::new(CONVERT_SPIRV))
			.map_err(|e| format!("invalid converter SPIR-V: {e}"))?;

		// SAFETY: every handle below is created on this converter's device and
		// stored immediately so Drop destroys it on any later error.
		unsafe {
			self.set_layout = device
				.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)
				.map_err(err("descriptor set layout"))?;
			let set_layouts = [self.set_layout];
			self.pipeline_layout = device
				.create_pipeline_layout(
					&vk::PipelineLayoutCreateInfo::default()
						.set_layouts(&set_layouts)
						.push_constant_ranges(std::slice::from_ref(&push_range)),
					None,
				)
				.map_err(err("pipeline layout"))?;
			let module = device
				.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
				.map_err(err("shader module"))?;
			let stage = vk::PipelineShaderStageCreateInfo::default()
				.stage(vk::ShaderStageFlags::COMPUTE)
				.module(module)
				.name(c"main");
			let pipeline = device.create_compute_pipelines(
				vk::PipelineCache::null(),
				&[vk::ComputePipelineCreateInfo::default()
					.stage(stage)
					.layout(self.pipeline_layout)],
				None,
			);
			device.destroy_shader_module(module, None);
			self.pipeline = pipeline.map_err(|(_, e)| format!("compute pipeline: {e}"))?[0];
			self.sampler = device
				.create_sampler(
					&vk::SamplerCreateInfo::default()
						.mag_filter(vk::Filter::NEAREST)
						.min_filter(vk::Filter::NEAREST)
						.address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
						.unnormalized_coordinates(false),
					None,
				)
				.map_err(err("sampler"))?;
			self.descriptor_pool = device
				.create_descriptor_pool(
					&vk::DescriptorPoolCreateInfo::default()
						.max_sets(1)
						.pool_sizes(&pool_sizes),
					None,
				)
				.map_err(err("descriptor pool"))?;
			self.descriptor_set = device
				.allocate_descriptor_sets(
					&vk::DescriptorSetAllocateInfo::default()
						.descriptor_pool(self.descriptor_pool)
						.set_layouts(&set_layouts),
				)
				.map_err(err("descriptor set"))?[0];

			self.output_buffer = device
				.create_buffer(
					&vk::BufferCreateInfo::default()
						.size(self.layout.size)
						.usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC)
						.sharing_mode(vk::SharingMode::EXCLUSIVE),
					None,
				)
				.map_err(err("output buffer"))?;
			let requirements = device.get_buffer_memory_requirements(self.output_buffer);
			let memory_type = if readback {
				// Host-cached system memory: the CPU reads every sample back, which
				// is very slow from write-combined device memory.
				let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
				find_memory_type(
					&self.context,
					requirements.memory_type_bits,
					host | vk::MemoryPropertyFlags::HOST_CACHED,
				)
				.or_else(|_| find_memory_type(&self.context, requirements.memory_type_bits, host))?
			} else {
				find_memory_type(
					&self.context,
					requirements.memory_type_bits,
					vk::MemoryPropertyFlags::DEVICE_LOCAL,
				)?
			};
			self.output_memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("output memory"))?;
			device
				.bind_buffer_memory(self.output_buffer, self.output_memory, 0)
				.map_err(err("bind output memory"))?;
			if readback {
				self.readback = device
					.map_memory(self.output_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
					.map_err(err("map output memory"))?
					.cast();
			}
			let buffer_info = vk::DescriptorBufferInfo::default()
				.buffer(self.output_buffer)
				.offset(0)
				.range(vk::WHOLE_SIZE);
			device.update_descriptor_sets(
				&[vk::WriteDescriptorSet::default()
					.dst_set(self.descriptor_set)
					.dst_binding(1)
					.descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
					.buffer_info(std::slice::from_ref(&buffer_info))],
				&[],
			);

			self.command_pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default()
						.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
						.queue_family_index(self.queue_family),
					None,
				)
				.map_err(err("command pool"))?;
			self.command_buffer = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(self.command_pool)
						.level(vk::CommandBufferLevel::PRIMARY)
						.command_buffer_count(1),
				)
				.map_err(err("command buffer"))?[0];
			self.fence = device
				.create_fence(&vk::FenceCreateInfo::default(), None)
				.map_err(err("fence"))?;
			if timestamps {
				self.query_pool = device
					.create_query_pool(
						&vk::QueryPoolCreateInfo::default()
							.query_type(vk::QueryType::TIMESTAMP)
							.query_count(2),
						None,
					)
					.map_err(err("timestamp query pool"))?;
			}
		}
		Ok(())
	}

	/// The conversion buffer's plane layout.
	pub(crate) fn layout(&self) -> &Layout {
		&self.layout
	}

	pub(crate) fn set_color(&mut self, color_space: ColorSpace, full_range: bool, sdr_white_nits: f32) {
		self.color_space = color_space;
		self.full_range = full_range;
		self.sdr_white_nits = sdr_white_nits;
	}

	/// (Re)allocate a slot's upload texture and staging buffer for `width` x `height`.
	fn ensure_overlay_capacity(&mut self, slot: usize, width: u32, height: u32) -> Result<(), String> {
		let texture = &self.layer_textures[slot];
		if texture.width >= width && texture.height >= height && texture.image != vk::Image::null() {
			return Ok(());
		}
		let width = width.max(texture.width).max(1);
		let height = height.max(texture.height).max(1);
		self.destroy_overlay(slot);
		let device = self.context.device().clone();
		let err = |what: &'static str| move |e: vk::Result| format!("overlay {what}: {e}");
		// SAFETY: objects are created on this device, recorded in `self.layer_textures`
		// immediately, and only destroyed after the GPU finished with them
		// (every conversion waits for its fence before returning).
		unsafe {
			self.layer_textures[slot].image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(vk::Format::R8G8B8A8_UNORM)
						.flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
						.extent(vk::Extent3D {
							width,
							height,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
						.sharing_mode(vk::SharingMode::EXCLUSIVE)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.map_err(err("image"))?;
			let requirements = device.get_image_memory_requirements(self.layer_textures[slot].image);
			let memory_type = find_memory_type(
				&self.context,
				requirements.memory_type_bits,
				vk::MemoryPropertyFlags::DEVICE_LOCAL,
			)?;
			self.layer_textures[slot].memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("memory"))?;
			device
				.bind_image_memory(self.layer_textures[slot].image, self.layer_textures[slot].memory, 0)
				.map_err(err("bind memory"))?;
			// RGBA (xcursor) and BGRA (wl_shm ARGB8888) views of the same texels.
			for (view_index, format) in [vk::Format::R8G8B8A8_UNORM, vk::Format::B8G8R8A8_UNORM]
				.into_iter()
				.enumerate()
			{
				self.layer_textures[slot].views[view_index] = device
					.create_image_view(
						&vk::ImageViewCreateInfo::default()
							.image(self.layer_textures[slot].image)
							.view_type(vk::ImageViewType::TYPE_2D)
							.format(format)
							.subresource_range(color_range()),
						None,
					)
					.map_err(err("view"))?;
			}
			let staging_size = u64::from(width) * u64::from(height) * 4;
			self.layer_textures[slot].staging = device
				.create_buffer(
					&vk::BufferCreateInfo::default()
						.size(staging_size)
						.usage(vk::BufferUsageFlags::TRANSFER_SRC)
						.sharing_mode(vk::SharingMode::EXCLUSIVE),
					None,
				)
				.map_err(err("staging buffer"))?;
			let requirements = device.get_buffer_memory_requirements(self.layer_textures[slot].staging);
			let memory_type = find_memory_type(
				&self.context,
				requirements.memory_type_bits,
				vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
			)?;
			self.layer_textures[slot].staging_memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(memory_type),
					None,
				)
				.map_err(err("staging memory"))?;
			device
				.bind_buffer_memory(
					self.layer_textures[slot].staging,
					self.layer_textures[slot].staging_memory,
					0,
				)
				.map_err(err("bind staging memory"))?;
			self.layer_textures[slot].staging_ptr = device
				.map_memory(
					self.layer_textures[slot].staging_memory,
					0,
					vk::WHOLE_SIZE,
					vk::MemoryMapFlags::empty(),
				)
				.map_err(err("map staging memory"))?
				.cast();
			// Transparent until the first real upload.
			std::ptr::write_bytes(self.layer_textures[slot].staging_ptr, 0, staging_size as usize);
		}
		self.layer_textures[slot].width = width;
		self.layer_textures[slot].height = height;
		self.layer_textures[slot].generation = None;
		self.layer_textures[slot].needs_init = true;
		Ok(())
	}

	fn destroy_overlay(&mut self, slot: usize) {
		let device = self.context.device();
		// SAFETY: callers only reach this after the last submission completed.
		unsafe {
			for view in &mut self.layer_textures[slot].views {
				if *view != vk::ImageView::null() {
					device.destroy_image_view(*view, None);
					*view = vk::ImageView::null();
				}
			}
			if self.layer_textures[slot].image != vk::Image::null() {
				device.destroy_image(self.layer_textures[slot].image, None);
				self.layer_textures[slot].image = vk::Image::null();
			}
			if self.layer_textures[slot].memory != vk::DeviceMemory::null() {
				device.free_memory(self.layer_textures[slot].memory, None);
				self.layer_textures[slot].memory = vk::DeviceMemory::null();
			}
			if self.layer_textures[slot].staging != vk::Buffer::null() {
				device.destroy_buffer(self.layer_textures[slot].staging, None);
				self.layer_textures[slot].staging = vk::Buffer::null();
			}
			if self.layer_textures[slot].staging_memory != vk::DeviceMemory::null() {
				device.free_memory(self.layer_textures[slot].staging_memory, None);
				self.layer_textures[slot].staging_memory = vk::DeviceMemory::null();
				self.layer_textures[slot].staging_ptr = std::ptr::null_mut();
			}
		}
	}

	/// View of `source`, created on first use and cached while the importer
	/// keeps the image alive.
	fn source_view(&mut self, source: &Arc<CachedImport>, format: vk::Format) -> Result<vk::ImageView, String> {
		if let Some((_, view)) = self.views.iter().find(|(import, _)| Arc::ptr_eq(import, source)) {
			return Ok(*view);
		}
		let device = self.context.device().clone();
		// Every previous submission has completed (each conversion waits), so
		// views of images the importer evicted can be destroyed now.
		self.views.retain(|(import, view)| {
			let keep = Arc::strong_count(import) > 1;
			if !keep {
				// SAFETY: no pending GPU work references the view.
				unsafe { device.destroy_image_view(*view, None) };
			}
			keep
		});
		if self.views.len() >= MAX_CACHED_VIEWS {
			let (_, view) = self.views.remove(0);
			// SAFETY: as above.
			unsafe { device.destroy_image_view(view, None) };
		}
		// SAFETY: the image is pinned by `source` for the lifetime of the view.
		let view = unsafe {
			device.create_image_view(
				&vk::ImageViewCreateInfo::default()
					.image(source.image())
					.view_type(vk::ImageViewType::TYPE_2D)
					.format(format)
					.subresource_range(color_range()),
				None,
			)
		}
		.map_err(|e| format!("source image view: {e}"))?;
		self.views.push((Arc::clone(source), view));
		Ok(view)
	}

	/// Stage `image` in `slot` when its content changed. Returns the view to
	/// sample and whether the staging buffer holds new content to upload.
	fn prepare_pixels(&mut self, slot: usize, image: &OverlayImage) -> Result<(vk::ImageView, bool), String> {
		self.ensure_overlay_capacity(slot, image.width, image.height)?;
		let texture = &mut self.layer_textures[slot];
		let upload = texture.generation != Some(image.generation);
		if upload {
			let row = image.width as usize * 4;
			let stride = texture.width as usize * 4;
			// SAFETY: the staging buffer holds texture.width x texture.height
			// texels, at least the image's extent; no GPU work reads it now.
			unsafe {
				for y in 0..image.height as usize {
					std::ptr::copy_nonoverlapping(
						image.pixels[y * row..].as_ptr(),
						texture.staging_ptr.add(y * stride),
						row,
					);
				}
			}
			texture.generation = Some(image.generation);
		}
		let view = match image.format {
			OverlayFormat::Rgba8 => texture.views[0],
			OverlayFormat::Bgra8 => texture.views[1],
		};
		Ok((view, upload))
	}

	/// Convert `source` into `target`, the encoder's input image, compositing
	/// `layers` bottom to top, and wait for completion. On success no GPU work
	/// reads the source or any imported layer any more, and `target` is back
	/// in its layout.
	pub(crate) fn convert(
		&mut self,
		source: &Arc<CachedImport>,
		source_format: vk::Format,
		first_use: bool,
		layers: &Layers<'_>,
		target: Target,
	) -> Result<ConvertOutcome, vk::Result> {
		self.output_valid = false;
		let view = self
			.source_view(source, source_format)
			.map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
		let binding = SourceBinding {
			image: source.image(),
			view,
			extent: source.extent(),
			first_use,
		};
		let outcome = self.dispatch(&binding, layers, Some(target))?;
		self.output_valid = true;
		Ok(outcome)
	}

	/// Record, submit and wait for one conversion; copy the result into
	/// `target` when given, else make it host-visible (self-check).
	fn dispatch(
		&mut self,
		source: &SourceBinding,
		layers: &Layers<'_>,
		target: Option<Target>,
	) -> Result<ConvertOutcome, vk::Result> {
		let layout = self.layout;
		let mut push = PushConstants {
			width: self.width,
			height: self.height,
			// A source smaller than the frame (a capture that raced an extent
			// change) is edge-extended rather than read out of bounds.
			source_max_x: source.extent.0.min(self.width).max(1) - 1,
			source_max_y: source.extent.1.min(self.height).max(1) - 1,
			output_format: layout.info.code,
			color_space: color_space_code(self.color_space),
			full_range: u32::from(self.full_range),
			sdr_white_nits: self.sdr_white_nits,
			luma_pitch: layout.luma_pitch,
			chroma_offset: layout.chroma_offset,
			chroma_pitch: layout.chroma_pitch,
			blocks_x: layout.blocks.0,
			blocks_y: layout.blocks.1,
			_padding: [0; 3],
			layer_rect: [[0; 4]; MAX_OVERLAYS],
			layer_opacity: [0.0; MAX_OVERLAYS],
			layer_opaque: [0; MAX_OVERLAYS],
		};
		// Per slot: the view to bind, whether staged texels must be uploaded,
		// and an imported image with its first-use flag (needs acquire/release).
		let mut layer_views = [vk::ImageView::null(); MAX_OVERLAYS];
		let mut uploads = [false; MAX_OVERLAYS];
		let mut imported: [Option<(vk::Image, bool)>; MAX_OVERLAYS] = [None; MAX_OVERLAYS];
		for (slot, layer) in layers.iter().enumerate() {
			let Some(layer) = layer else {
				// Placeholder: never sampled while the rectangle width is zero.
				layer_views[slot] = self.layer_textures[slot].views[0];
				continue;
			};
			let (Ok(width), Ok(height)) = (i32::try_from(layer.width), i32::try_from(layer.height)) else {
				return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
			};
			push.layer_rect[slot] = [layer.x, layer.y, width, height];
			push.layer_opacity[slot] = layer.opacity;
			match &layer.input {
				LayerInput::Pixels(image) => {
					let (view, upload) = self
						.prepare_pixels(slot, image)
						.map_err(|_| vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
					layer_views[slot] = view;
					uploads[slot] = upload;
				},
				LayerInput::Imported {
					import,
					format,
					first_use,
					opaque,
				} => {
					layer_views[slot] = self
						.source_view(import, *format)
						.map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
					imported[slot] = Some((import.image(), *first_use));
					push.layer_opaque[slot] = u32::from(*opaque);
				},
			}
		}
		let device = self.context.device().clone();
		let cmd = self.command_buffer;
		let source_image = source.image;

		// SAFETY: all handles belong to this device; the command buffer is not
		// pending (the previous submission was waited for); descriptor updates
		// happen while no submission uses the set.
		unsafe {
			let image_info = |view, layout| {
				vk::DescriptorImageInfo::default()
					.sampler(self.sampler)
					.image_view(view)
					.image_layout(layout)
			};
			// Imported layers are sampled in GENERAL (see the barrier below).
			let layer_layout = |slot: usize| {
				if imported[slot].is_some() {
					vk::ImageLayout::GENERAL
				} else {
					vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
				}
			};
			let image_infos = [
				image_info(source.view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
				image_info(layer_views[0], layer_layout(0)),
				image_info(layer_views[1], layer_layout(1)),
			];
			let writes = [(0u32, 0usize), (2, 1), (3, 2)].map(|(binding, info)| {
				vk::WriteDescriptorSet::default()
					.dst_set(self.descriptor_set)
					.dst_binding(binding)
					.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
					.image_info(std::slice::from_ref(&image_infos[info]))
			});
			device.update_descriptor_sets(&writes, &[]);

			device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
			device.begin_command_buffer(
				cmd,
				&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
			)?;
			if self.query_pool != vk::QueryPool::null() {
				device.cmd_reset_query_pool(cmd, self.query_pool, 0, 2);
				device.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, 0);
			}

			// The first use of an imported (EXCLUSIVE) DMA-BUF acquires it from
			// the external owner; later uses start from the GENERAL layout left
			// below. Imported layers are client DMA-BUFs and follow the same
			// protocol.
			let acquire = |image: vk::Image, first_use: bool| {
				vk::ImageMemoryBarrier::default()
					.old_layout(if first_use {
						vk::ImageLayout::UNDEFINED
					} else {
						vk::ImageLayout::GENERAL
					})
					.new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
					.src_queue_family_index(if first_use {
						vk::QUEUE_FAMILY_EXTERNAL
					} else {
						vk::QUEUE_FAMILY_IGNORED
					})
					.dst_queue_family_index(if first_use {
						self.queue_family
					} else {
						vk::QUEUE_FAMILY_IGNORED
					})
					.image(image)
					.subresource_range(color_range())
					.src_access_mask(if first_use {
						vk::AccessFlags::empty()
					} else {
						vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
					})
					.dst_access_mask(vk::AccessFlags::SHADER_READ)
			};
			// Layers are other clients' buffers (e.g. XWayland's). Any layout
			// transition after the first acquire can make the driver treat the
			// buffer as written, and implicit sync then waits for the owner's
			// own pending reads, which queue behind a GPU-bound game. Sample
			// them in GENERAL with a memory dependency only.
			let layer_barrier = |image: vk::Image, layer_first_use: bool| {
				let barrier = acquire(image, layer_first_use).new_layout(vk::ImageLayout::GENERAL);
				if layer_first_use {
					barrier
				} else {
					barrier.old_layout(vk::ImageLayout::GENERAL)
				}
			};
			let mut acquires = [acquire(source_image, source.first_use); 1 + MAX_OVERLAYS];
			let mut acquire_count = 1;
			for (image, layer_first_use) in imported.iter().flatten() {
				acquires[acquire_count] = layer_barrier(*image, *layer_first_use);
				acquire_count += 1;
			}
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::COMPUTE_SHADER,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&acquires[..acquire_count],
			);
			for (slot, &upload) in uploads.iter().enumerate() {
				let texture = &self.layer_textures[slot];
				let texture_barrier = |old, new, src, dst| {
					vk::ImageMemoryBarrier::default()
						.old_layout(old)
						.new_layout(new)
						.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.image(texture.image)
						.subresource_range(color_range())
						.src_access_mask(src)
						.dst_access_mask(dst)
				};
				if upload {
					let old = if texture.needs_init {
						vk::ImageLayout::UNDEFINED
					} else {
						vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
					};
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							old,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::AccessFlags::SHADER_READ,
							vk::AccessFlags::TRANSFER_WRITE,
						)],
					);
					let region = vk::BufferImageCopy {
						buffer_offset: 0,
						buffer_row_length: texture.width,
						buffer_image_height: 0,
						image_subresource: vk::ImageSubresourceLayers {
							aspect_mask: vk::ImageAspectFlags::COLOR,
							mip_level: 0,
							base_array_layer: 0,
							layer_count: 1,
						},
						image_offset: vk::Offset3D::default(),
						image_extent: vk::Extent3D {
							width: texture.width,
							height: texture.height,
							depth: 1,
						},
					};
					device.cmd_copy_buffer_to_image(
						cmd,
						texture.staging,
						texture.image,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						&[region],
					);
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
							vk::AccessFlags::TRANSFER_WRITE,
							vk::AccessFlags::SHADER_READ,
						)],
					);
				} else if texture.needs_init {
					// Placeholder contents are irrelevant; only the layout matters.
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TOP_OF_PIPE,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[texture_barrier(
							vk::ImageLayout::UNDEFINED,
							vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
							vk::AccessFlags::empty(),
							vk::AccessFlags::SHADER_READ,
						)],
					);
				}
				self.layer_textures[slot].needs_init = false;
			}

			device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
			device.cmd_bind_descriptor_sets(
				cmd,
				vk::PipelineBindPoint::COMPUTE,
				self.pipeline_layout,
				0,
				&[self.descriptor_set],
				&[],
			);
			device.cmd_push_constants(
				cmd,
				self.pipeline_layout,
				vk::ShaderStageFlags::COMPUTE,
				0,
				std::slice::from_raw_parts(
					(&push as *const PushConstants).cast::<u8>(),
					std::mem::size_of::<PushConstants>(),
				),
			);
			// The previous conversion's copy (TRANSFER_READ of this buffer)
			// completed before its fence; no barrier is needed before these
			// writes.
			device.cmd_dispatch(
				cmd,
				layout.blocks.0.div_ceil(GROUP_SIZE),
				layout.blocks.1.div_ceil(GROUP_SIZE),
				1,
			);

			match target {
				Some(target) => self.record_copy(cmd, BufferWriter::Dispatch, target),
				None => {
					let to_host = [vk::BufferMemoryBarrier::default()
						.src_access_mask(vk::AccessFlags::SHADER_WRITE)
						.dst_access_mask(vk::AccessFlags::HOST_READ)
						.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.buffer(self.output_buffer)
						.size(vk::WHOLE_SIZE)];
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::PipelineStageFlags::HOST,
						vk::DependencyFlags::empty(),
						&[],
						&to_host,
						&[],
					);
				},
			}
			// Sampled images return to GENERAL for their next use; imported
			// layers already are in GENERAL.
			let release = [vk::ImageMemoryBarrier::default()
				.old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
				.new_layout(vk::ImageLayout::GENERAL)
				.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
				.image(source_image)
				.subresource_range(color_range())
				.src_access_mask(vk::AccessFlags::SHADER_READ)
				.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)];
			device.cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::COMPUTE_SHADER,
				vk::PipelineStageFlags::BOTTOM_OF_PIPE,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				&release,
			);
			if self.query_pool != vk::QueryPool::null() {
				device.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.query_pool, 1);
			}
			device.end_command_buffer(cmd)?;
			// Encode submission needs the converted input; Pixelforge's encode
			// API takes no wait semaphore, so completion is established here.
			self.submit_and_wait(cmd)?;
		}

		let gpu_ns = (self.query_pool != vk::QueryPool::null())
			.then(|| {
				let mut stamps = [0u64; 2];
				// SAFETY: both queries were written by the completed submission.
				unsafe { device.get_query_pool_results(self.query_pool, 0, &mut stamps, vk::QueryResultFlags::TYPE_64) }
					.ok()
					.map(|()| (stamps[1].saturating_sub(stamps[0]) as f64 * self.timestamp_period_ns) as u64)
			})
			.flatten();
		Ok(ConvertOutcome { gpu_ns })
	}

	/// Record the copy of the output buffer into `target` after `writer`
	/// (the dispatch in this command buffer, or a completed earlier one)
	/// wrote the buffer, leaving `target` ready for its encoder.
	///
	/// The encode that reads the target is submitted after the host observed
	/// this submission's fence, so no barrier orders the copy against it; the
	/// target's previous reader (an encode) completed before its encoder handed
	/// the target out again.
	///
	/// # Safety
	///
	/// `cmd` is recording on this converter's queue family, and every target
	/// handle belongs to this converter's device.
	unsafe fn record_copy(&self, cmd: vk::CommandBuffer, writer: BufferWriter, target: Target) {
		let (src_stage, src_access) = match writer {
			BufferWriter::Dispatch => (vk::PipelineStageFlags::COMPUTE_SHADER, vk::AccessFlags::SHADER_WRITE),
			BufferWriter::Completed => (vk::PipelineStageFlags::ALL_COMMANDS, vk::AccessFlags::MEMORY_WRITE),
		};
		let device = self.context.device();
		let buffer_barrier = [vk::BufferMemoryBarrier::default()
			.src_access_mask(src_access)
			.dst_access_mask(vk::AccessFlags::TRANSFER_READ)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.buffer(self.output_buffer)
			.size(vk::WHOLE_SIZE)];
		let image_barrier = |image, old, new, src, dst, (src_family, dst_family)| {
			vk::ImageMemoryBarrier::default()
				.src_access_mask(src)
				.dst_access_mask(dst)
				.old_layout(old)
				.new_layout(new)
				.src_queue_family_index(src_family)
				.dst_queue_family_index(dst_family)
				.image(image)
				.subresource_range(color_range())
		};
		let ignored = (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED);
		// SAFETY: guaranteed by the caller.
		unsafe {
			match target {
				Target::Image { image, layout } => {
					device.cmd_pipeline_barrier(
						cmd,
						src_stage,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&buffer_barrier,
						&[image_barrier(
							image,
							layout,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
							vk::AccessFlags::TRANSFER_WRITE,
							ignored,
						)],
					);
					let regions = self.layout.copy_regions();
					device.cmd_copy_buffer_to_image(
						cmd,
						self.output_buffer,
						image,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						&regions[..self.layout.plane_count()],
					);
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::BOTTOM_OF_PIPE,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&[image_barrier(
							image,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							layout,
							vk::AccessFlags::TRANSFER_WRITE,
							vk::AccessFlags::empty(),
							ignored,
						)],
					);
				},
				Target::ExternalPlanes { images, count } => {
					let count = count.min(self.layout.plane_count());
					// Acquire without preserving: the copy replaces every texel
					// the encoder reads (the surface measures the conversion extent).
					let acquire: Vec<_> = images[..count]
						.iter()
						.map(|&image| {
							image_barrier(
								image,
								vk::ImageLayout::UNDEFINED,
								vk::ImageLayout::TRANSFER_DST_OPTIMAL,
								vk::AccessFlags::empty(),
								vk::AccessFlags::TRANSFER_WRITE,
								(vk::QUEUE_FAMILY_EXTERNAL, self.queue_family),
							)
						})
						.collect();
					device.cmd_pipeline_barrier(
						cmd,
						src_stage | vk::PipelineStageFlags::TOP_OF_PIPE,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&buffer_barrier,
						&acquire,
					);
					let regions = self.layout.plane_image_regions();
					for (image, region) in images[..count].iter().zip(regions.iter()) {
						device.cmd_copy_buffer_to_image(
							cmd,
							self.output_buffer,
							*image,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							std::slice::from_ref(region),
						);
					}
					let release: Vec<_> = images[..count]
						.iter()
						.map(|&image| {
							image_barrier(
								image,
								vk::ImageLayout::TRANSFER_DST_OPTIMAL,
								vk::ImageLayout::GENERAL,
								vk::AccessFlags::TRANSFER_WRITE,
								vk::AccessFlags::empty(),
								(self.queue_family, vk::QUEUE_FAMILY_EXTERNAL),
							)
						})
						.collect();
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::BOTTOM_OF_PIPE,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&release,
					);
				},
				Target::Buffer { buffer } => {
					device.cmd_pipeline_barrier(
						cmd,
						src_stage,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&buffer_barrier,
						&[],
					);
					device.cmd_copy_buffer(
						cmd,
						self.output_buffer,
						buffer,
						&[vk::BufferCopy::default().size(self.layout.size)],
					);
					// Make the copy available before the fence: the reader is
					// another API that observes the host's fence wait.
					let available = [vk::BufferMemoryBarrier::default()
						.src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
						.dst_access_mask(vk::AccessFlags::MEMORY_READ)
						.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
						.buffer(buffer)
						.size(vk::WHOLE_SIZE)];
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::BOTTOM_OF_PIPE,
						vk::DependencyFlags::empty(),
						&[],
						&available,
						&[],
					);
				},
			}
		}
	}

	/// Submit `cmd` to this converter's queue and wait for it.
	///
	/// # Safety
	///
	/// `cmd` was recorded from this converter's pool and is executable.
	unsafe fn submit_and_wait(&self, cmd: vk::CommandBuffer) -> Result<(), vk::Result> {
		let device = self.context.device();
		let command_buffers = [cmd];
		// SAFETY: guaranteed by the caller; the fence is unsignaled (it is reset
		// after every wait).
		unsafe {
			device.queue_submit(
				self.queue,
				&[vk::SubmitInfo::default().command_buffers(&command_buffers)],
				self.fence,
			)?;
			let waited = device.wait_for_fences(&[self.fence], true, u64::MAX);
			device.reset_fences(&[self.fence])?;
			waited
		}
	}

	/// Copy the last completed conversion into `target` (an encoder input
	/// slot) without converting again. `Ok(false)` if there is none.
	///
	/// Static-scene recovery needs this: the encoder rotates its input slots,
	/// so the current slot never holds the latest frame (review 2026-10-05
	/// BUG-002).
	pub(crate) fn copy_last_output(&self, target: Target) -> Result<bool, vk::Result> {
		if !self.output_valid {
			return Ok(false);
		}
		let device = self.context.device();
		let cmd = self.command_buffer;
		// SAFETY: the command buffer is not pending (every submission is waited
		// for); the buffer's writes completed under the conversion's fence.
		unsafe {
			device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
			device.begin_command_buffer(
				cmd,
				&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
			)?;
			self.record_copy(cmd, BufferWriter::Completed, target);
			device.end_command_buffer(cmd)?;
			if let Err(e) = self.submit_and_wait(cmd) {
				// A failed wait leaves the submission state unknown; idle the
				// queue before the command buffer is reused.
				let _ = device.queue_wait_idle(self.queue);
				return Err(e);
			}
		}
		Ok(true)
	}

	/// Host view of the last conversion of a self-check converter.
	fn readback(&self) -> &[u8] {
		assert!(!self.readback.is_null(), "not a readback converter");
		// SAFETY: the mapping covers the whole allocation (at least `size`
		// bytes) for the converter's lifetime; the last dispatch made its
		// writes host-visible and completed.
		unsafe { std::slice::from_raw_parts(self.readback, self.layout.size as usize) }
	}
}

/// Frame and conversion extents of the self-check: one partial block in each
/// direction, with edge padding in both directions for 4:2:0 and none for
/// 4:4:4.
const SELF_CHECK_FRAME: (u32, u32) = (9, 5);

/// Self-check source pixel (RGBA8): saturated and mid colors, greys and a
/// diagonal ramp, so neighbouring chroma samples differ and none is neutral by
/// accident.
fn self_check_pixel(x: u32, y: u32) -> [u8; 4] {
	const PALETTE: [[u8; 3]; 8] = [
		[255, 0, 0],
		[0, 255, 0],
		[0, 0, 255],
		[255, 255, 0],
		[0, 255, 255],
		[255, 0, 255],
		[128, 128, 128],
		[200, 120, 40],
	];
	let [r, g, b] = PALETTE[((x + 3 * y) % 8) as usize];
	let ramp = ((x * 17 + y * 29) % 64) as u8;
	[
		r.saturating_sub(ramp),
		g.saturating_sub(ramp / 2),
		b.saturating_add(ramp / 4),
		255,
	]
}

/// Why a converter could not be provided.
#[derive(Debug)]
pub(crate) enum ConverterError {
	/// Creating the Vulkan objects failed.
	Create(String),
	/// The converter runs, but its output on this GPU differs from the CPU
	/// reference: every frame would carry wrong colors.
	SelfCheck(SelfCheckFailure),
}

impl std::fmt::Display for ConverterError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Create(error) => f.write_str(error),
			Self::SelfCheck(failure) => failure.fmt(f),
		}
	}
}

impl From<SelfCheckFailure> for ConverterError {
	fn from(failure: SelfCheckFailure) -> Self {
		Self::SelfCheck(failure)
	}
}

/// A self-check that ran and produced wrong output (or could not run).
#[derive(Debug)]
pub(crate) struct SelfCheckFailure {
	pub format: ConvertFormat,
	/// Where the output differs (plane and block column), or why the check
	/// could not run.
	pub detail: String,
}

impl std::fmt::Display for SelfCheckFailure {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{:?} converter self-check failed on this GPU: {}",
			self.format, self.detail
		)
	}
}

/// Operator name of a converter output format and its stream profile.
pub(crate) fn format_label(format: ConvertFormat) -> &'static str {
	match format {
		ConvertFormat::Nv12 => "4:2:0 8-bit (NV12)",
		ConvertFormat::P010 => "4:2:0 10-bit (P010)",
		ConvertFormat::Yuv444 => "4:4:4 8-bit",
		ConvertFormat::Yuv444P10 => "4:4:4 10-bit",
		ConvertFormat::Xyuv => "4:4:4 8-bit (packed XYUV)",
		ConvertFormat::Y410 => "4:4:4 10-bit (packed Y410)",
	}
}

/// Convert the self-check pattern on `mode`'s queue and compare with
/// [`reference`] (BT.709, limited range). Stream setup runs it for every
/// converter; the health check runs it for every format a verified profile
/// needs.
pub(crate) fn self_check(
	context: &VideoContext,
	mode: ConversionQueueMode,
	format: ConvertFormat,
) -> Result<(), SelfCheckFailure> {
	self_check_inner(context, mode, format).map_err(|detail| SelfCheckFailure { format, detail })
}

fn self_check_inner(context: &VideoContext, mode: ConversionQueueMode, format: ConvertFormat) -> Result<(), String> {
	let frame = SELF_CHECK_FRAME;
	let subsampled = format.subsampled();
	let extent = if subsampled {
		(frame.0.next_multiple_of(2), frame.1.next_multiple_of(2))
	} else {
		frame
	};
	let mut converter = InputConverter::create(
		context.clone(),
		mode,
		frame,
		extent,
		format,
		4,
		ColorSpace::Bt709,
		false,
		true,
	)?;
	let pixels: Vec<u8> = (0..frame.1)
		.flat_map(|y| (0..frame.0).flat_map(move |x| self_check_pixel(x, y)))
		.collect();
	let source = SelfCheckSource::new(&converter, frame, &pixels)?;
	converter
		.dispatch(
			&SourceBinding {
				image: source.image,
				view: source.view,
				extent: frame,
				first_use: false,
			},
			&[None, None],
			None,
		)
		.map_err(|e| format!("conversion failed: {e}"))?;
	let actual = converter.layout().read_planes(converter.readback())?;
	let rgb = |x: u32, y: u32| {
		let [r, g, b, _] = self_check_pixel(x, y);
		[r, g, b].map(|c| f32::from(c) / 255.0)
	};
	let expected = reference::convert(
		format,
		reference::Color {
			space: ColorSpace::Bt709,
			full_range: false,
			sdr_white_nits: 203.0,
		},
		frame,
		extent,
		&rgb,
		&|_| true,
	);
	reference::compare(&expected, &actual, 1, &|_| true).map(|_| ())
}

/// The self-check's source image (RGBA8, GENERAL layout) and view, freed on
/// drop. Uploaded through the converter's own queue and command buffer.
struct SelfCheckSource {
	context: VideoContext,
	image: vk::Image,
	memory: vk::DeviceMemory,
	view: vk::ImageView,
}

impl SelfCheckSource {
	fn new(converter: &InputConverter, (width, height): (u32, u32), pixels: &[u8]) -> Result<Self, String> {
		let context = &converter.context;
		let device = context.device();
		let err = |what: &'static str| move |e: vk::Result| format!("self-check {what}: {e}");
		let mut source = Self {
			context: context.clone(),
			image: vk::Image::null(),
			memory: vk::DeviceMemory::null(),
			view: vk::ImageView::null(),
		};
		// SAFETY: test objects on this device, stored immediately so Drop
		// frees them; the upload is waited for before the staging buffer goes.
		unsafe {
			source.image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(vk::Format::R8G8B8A8_UNORM)
						.extent(vk::Extent3D {
							width,
							height,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
						.sharing_mode(vk::SharingMode::EXCLUSIVE)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.map_err(err("image"))?;
			let requirements = device.get_image_memory_requirements(source.image);
			source.memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(requirements.size)
						.memory_type_index(find_memory_type(
							context,
							requirements.memory_type_bits,
							vk::MemoryPropertyFlags::DEVICE_LOCAL,
						)?),
					None,
				)
				.map_err(err("image memory"))?;
			device
				.bind_image_memory(source.image, source.memory, 0)
				.map_err(err("bind image memory"))?;
			source.view = device
				.create_image_view(
					&vk::ImageViewCreateInfo::default()
						.image(source.image)
						.view_type(vk::ImageViewType::TYPE_2D)
						.format(vk::Format::R8G8B8A8_UNORM)
						.subresource_range(color_range()),
					None,
				)
				.map_err(err("view"))?;

			let staging = device
				.create_buffer(
					&vk::BufferCreateInfo::default()
						.size(pixels.len() as u64)
						.usage(vk::BufferUsageFlags::TRANSFER_SRC)
						.sharing_mode(vk::SharingMode::EXCLUSIVE),
					None,
				)
				.map_err(err("staging buffer"))?;
			let upload = (|| {
				let requirements = device.get_buffer_memory_requirements(staging);
				let memory = device
					.allocate_memory(
						&vk::MemoryAllocateInfo::default()
							.allocation_size(requirements.size)
							.memory_type_index(find_memory_type(
								context,
								requirements.memory_type_bits,
								vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
							)?),
						None,
					)
					.map_err(err("staging memory"))?;
				let result = (|| {
					device
						.bind_buffer_memory(staging, memory, 0)
						.map_err(err("bind staging memory"))?;
					let mapped = device
						.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
						.map_err(err("map staging memory"))?;
					std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped.cast::<u8>(), pixels.len());
					let cmd = converter.command_buffer;
					device
						.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
						.map_err(err("reset"))?;
					device
						.begin_command_buffer(
							cmd,
							&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
						)
						.map_err(err("begin"))?;
					let barrier = |old, new, src, dst| {
						[vk::ImageMemoryBarrier::default()
							.old_layout(old)
							.new_layout(new)
							.src_access_mask(src)
							.dst_access_mask(dst)
							.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
							.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
							.image(source.image)
							.subresource_range(color_range())]
					};
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TOP_OF_PIPE,
						vk::PipelineStageFlags::TRANSFER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&barrier(
							vk::ImageLayout::UNDEFINED,
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::AccessFlags::empty(),
							vk::AccessFlags::TRANSFER_WRITE,
						),
					);
					device.cmd_copy_buffer_to_image(
						cmd,
						staging,
						source.image,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						&[vk::BufferImageCopy {
							image_subresource: vk::ImageSubresourceLayers {
								aspect_mask: vk::ImageAspectFlags::COLOR,
								mip_level: 0,
								base_array_layer: 0,
								layer_count: 1,
							},
							image_extent: vk::Extent3D {
								width,
								height,
								depth: 1,
							},
							..Default::default()
						}],
					);
					device.cmd_pipeline_barrier(
						cmd,
						vk::PipelineStageFlags::TRANSFER,
						vk::PipelineStageFlags::COMPUTE_SHADER,
						vk::DependencyFlags::empty(),
						&[],
						&[],
						&barrier(
							vk::ImageLayout::TRANSFER_DST_OPTIMAL,
							vk::ImageLayout::GENERAL,
							vk::AccessFlags::TRANSFER_WRITE,
							vk::AccessFlags::SHADER_READ,
						),
					);
					device.end_command_buffer(cmd).map_err(err("end"))?;
					if let Err(e) = converter.submit_and_wait(cmd) {
						let _ = device.queue_wait_idle(converter.queue);
						return Err(format!("self-check upload: {e}"));
					}
					Ok(())
				})();
				device.free_memory(memory, None);
				result
			})();
			device.destroy_buffer(staging, None);
			upload?;
		}
		Ok(source)
	}
}

impl Drop for SelfCheckSource {
	fn drop(&mut self) {
		let device = self.context.device();
		// SAFETY: every submission that used these objects was waited for (or
		// its queue idled) before the converter returned.
		unsafe {
			if self.view != vk::ImageView::null() {
				device.destroy_image_view(self.view, None);
			}
			if self.image != vk::Image::null() {
				device.destroy_image(self.image, None);
			}
			if self.memory != vk::DeviceMemory::null() {
				device.free_memory(self.memory, None);
			}
		}
	}
}

impl Drop for InputConverter {
	fn drop(&mut self) {
		let device = self.context.device().clone();
		// SAFETY: every conversion waits for its fence, so no submission that
		// references these objects can still be pending. A failed wait leaves
		// the queue in an unknown state; idle the queue before destroying.
		unsafe {
			let _ = device.queue_wait_idle(self.queue);
			for (_, view) in self.views.drain(..) {
				device.destroy_image_view(view, None);
			}
			for slot in 0..MAX_OVERLAYS {
				self.destroy_overlay(slot);
			}
			if self.query_pool != vk::QueryPool::null() {
				device.destroy_query_pool(self.query_pool, None);
			}
			if self.fence != vk::Fence::null() {
				device.destroy_fence(self.fence, None);
			}
			if self.command_pool != vk::CommandPool::null() {
				device.destroy_command_pool(self.command_pool, None);
			}
			if self.output_buffer != vk::Buffer::null() {
				device.destroy_buffer(self.output_buffer, None);
			}
			if self.output_memory != vk::DeviceMemory::null() {
				device.free_memory(self.output_memory, None);
			}
			if self.descriptor_pool != vk::DescriptorPool::null() {
				device.destroy_descriptor_pool(self.descriptor_pool, None);
			}
			if self.sampler != vk::Sampler::null() {
				device.destroy_sampler(self.sampler, None);
			}
			if self.pipeline != vk::Pipeline::null() {
				device.destroy_pipeline(self.pipeline, None);
			}
			if self.pipeline_layout != vk::PipelineLayout::null() {
				device.destroy_pipeline_layout(self.pipeline_layout, None);
			}
			if self.set_layout != vk::DescriptorSetLayout::null() {
				device.destroy_descriptor_set_layout(self.set_layout, None);
			}
		}
	}
}

#[cfg(test)]
mod tests;
