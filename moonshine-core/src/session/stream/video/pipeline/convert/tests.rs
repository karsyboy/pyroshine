//! Converter contracts: plane layout, the CPU reference, defect detection and
//! (on a Vulkan Video GPU, `MOONSHINE_TEST_GPU=1`) the shader, copy regions,
//! late composition and encode/decode round trips against the reference.

use super::reference::{self, Color, Planes};
use super::*;
use crate::session::stream::video::pipeline::EncoderBackend;

/// The 4x2 block that wrote each 32-bit word of a layout, following the
/// shader's indexing. Returns per-word owners (`None`: never written) or the
/// first word written twice or out of bounds.
fn word_owners(layout: &Layout) -> Result<Vec<Option<(u32, u32)>>, String> {
	let words = (layout.size / 4) as usize;
	let mut owners = vec![None; words];
	let (blocks_x, blocks_y) = layout.blocks;
	for by in 0..blocks_y {
		for bx in 0..blocks_x {
			let (y0, lp, co, cp) = (by * 2, layout.luma_pitch, layout.chroma_offset, layout.chroma_pitch);
			let row_a = y0 * lp;
			let row_b = row_a + lp;
			let written: Vec<u32> = match layout.info.code {
				0 => vec![row_a + bx, row_b + bx, co + by * cp + bx],
				3 => {
					let c = co + by * cp + 2 * bx;
					vec![
						row_a + 2 * bx,
						row_a + 2 * bx + 1,
						row_b + 2 * bx,
						row_b + 2 * bx + 1,
						c,
						c + 1,
					]
				},
				2 => {
					let ca = co + y0 * cp + 2 * bx;
					vec![row_a + bx, row_b + bx, ca, ca + 1, ca + cp, ca + cp + 1]
				},
				4 => {
					let ca = co + y0 * cp + 4 * bx;
					let mut w = vec![row_a + 2 * bx, row_a + 2 * bx + 1, row_b + 2 * bx, row_b + 2 * bx + 1];
					w.extend((0..4).flat_map(|i| [ca + i, ca + cp + i]));
					w
				},
				code => return Err(format!("format code {code}")),
			};
			for word in written {
				let owner = owners
					.get_mut(word as usize)
					.ok_or_else(|| format!("block ({bx}, {by}) writes word {word} past {words}"))?;
				if let Some(previous) = owner.replace((bx, by)) {
					return Err(format!("word {word} written by {previous:?} and ({bx}, {by})"));
				}
			}
		}
	}
	Ok(owners)
}

/// Byte ranges the copy regions read, per `VkBufferImageCopy` addressing.
fn copied_bytes(layout: &Layout) -> Vec<std::ops::Range<u64>> {
	let mut ranges = Vec::new();
	for (region, texel) in layout
		.copy_regions()
		.iter()
		.zip([layout.info.luma_bytes, layout.info.chroma_bytes])
	{
		let pitch = u64::from(region.buffer_row_length) * u64::from(texel);
		for row in 0..u64::from(region.image_extent.height) {
			let start = region.buffer_offset + row * pitch;
			ranges.push(start..start + u64::from(region.image_extent.width) * u64::from(texel));
		}
	}
	ranges
}

const FORMATS: [ConvertFormat; 4] = [
	ConvertFormat::Nv12,
	ConvertFormat::P010,
	ConvertFormat::Yuv444,
	ConvertFormat::Yuv444P10,
];

/// The multi-planar Vulkan image format of a 2-plane conversion format.
fn vulkan_format(format: ConvertFormat) -> vk::Format {
	match format {
		ConvertFormat::Nv12 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
		ConvertFormat::P010 => vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
		ConvertFormat::Yuv444 => vk::Format::G8_B8R8_2PLANE_444_UNORM,
		ConvertFormat::Yuv444P10 => vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16,
		ConvertFormat::Xyuv | ConvertFormat::Y410 => vk::Format::UNDEFINED,
	}
}

fn subsampled(format: ConvertFormat) -> bool {
	format_info(format).subsampled
}

/// The smallest valid conversion extent covering a frame.
fn tight_extent(format: ConvertFormat, (w, h): (u32, u32)) -> (u32, u32) {
	if subsampled(format) {
		(w.next_multiple_of(2), h.next_multiple_of(2))
	} else {
		(w, h)
	}
}

#[test]
fn captured_dmabufs_are_acquired_from_a_foreign_producer_when_supported() {
	assert_eq!(dmabuf_producer_family(true), vk::QUEUE_FAMILY_FOREIGN_EXT);
	assert_eq!(dmabuf_producer_family(false), vk::QUEUE_FAMILY_EXTERNAL);
}

#[test]
fn external_buffers_are_handed_over_through_queue_family_ownership() {
	use ash::vk::Handle as _;
	let buffer = vk::Buffer::from_raw(0x1234);
	let [acquire, release] = external_buffer_handoff(buffer, 2);
	// Acquired from the external reader for the copy, released back after it.
	assert_eq!(
		(acquire.src_queue_family_index, acquire.dst_queue_family_index),
		(vk::QUEUE_FAMILY_EXTERNAL, 2)
	);
	assert_eq!(acquire.dst_access_mask, vk::AccessFlags::TRANSFER_WRITE);
	assert_eq!(
		(release.src_queue_family_index, release.dst_queue_family_index),
		(2, vk::QUEUE_FAMILY_EXTERNAL)
	);
	assert_eq!(release.src_access_mask, vk::AccessFlags::TRANSFER_WRITE);
	for barrier in [acquire, release] {
		assert_eq!(
			(barrier.buffer, barrier.offset, barrier.size),
			(buffer, 0, vk::WHOLE_SIZE)
		);
	}
}

#[test]
fn layout_matches_hand_computed_planes() {
	// 1080p NV12 is the tightly packed classic layout.
	let l = Layout::new(ConvertFormat::Nv12, (1920, 1080)).unwrap();
	assert_eq!((l.luma_pitch, l.chroma_offset, l.chroma_pitch), (480, 480 * 1080, 480));
	assert_eq!(l.size, 1920 * 1080 * 3 / 2);
	let [y, c] = l.copy_regions();
	assert_eq!(
		(y.buffer_offset, y.buffer_row_length, y.image_extent.width),
		(0, 1920, 1920)
	);
	assert_eq!(
		(
			c.buffer_offset,
			c.buffer_row_length,
			c.image_extent.width,
			c.image_extent.height
		),
		(1920 * 1080, 960, 960, 540)
	);

	// 1366 is not a multiple of four: rows are padded to 1368 bytes.
	let l = Layout::new(ConvertFormat::Nv12, (1366, 768)).unwrap();
	assert_eq!((l.luma_pitch, l.chroma_offset, l.chroma_pitch), (342, 342 * 768, 342));
	let [y, c] = l.copy_regions();
	assert_eq!((y.buffer_row_length, y.image_extent.width), (1368, 1366));
	assert_eq!((c.buffer_offset, c.buffer_row_length), (1368 * 768, 684));
	assert_eq!((c.image_extent.width, c.image_extent.height), (683, 384));
	assert_eq!(l.size, 1368 * 768 + 1368 * 384);

	// P010: 16-bit luma, 32-bit CbCr texels.
	let l = Layout::new(ConvertFormat::P010, (1366, 768)).unwrap();
	let [y, c] = l.copy_regions();
	assert_eq!((l.luma_pitch, y.buffer_row_length), (684, 1368));
	assert_eq!(
		(c.buffer_offset, c.buffer_row_length, c.image_extent.width),
		(2736 * 768, 684, 683)
	);

	// 4:4:4 at an odd extent: rows padded to whole blocks (1368 x 768).
	let l = Layout::new(ConvertFormat::Yuv444, (1365, 767)).unwrap();
	let [y, c] = l.copy_regions();
	assert_eq!((l.luma_pitch, l.chroma_offset, l.chroma_pitch), (342, 342 * 768, 684));
	assert_eq!(
		(y.image_extent.width, y.image_extent.height, y.buffer_row_length),
		(1365, 767, 1368)
	);
	assert_eq!(
		(c.image_extent.width, c.image_extent.height, c.buffer_row_length),
		(1365, 767, 1368)
	);

	// One pixel still occupies a whole 4x2 block.
	let l = Layout::new(ConvertFormat::Yuv444P10, (1, 1)).unwrap();
	assert_eq!(
		(l.blocks, l.luma_pitch, l.chroma_offset, l.chroma_pitch, l.size),
		((1, 1), 2, 4, 4, 48)
	);
}

#[test]
fn layout_rejects_unrepresentable_extents() {
	assert!(Layout::new(ConvertFormat::Nv12, (0, 2)).is_err());
	assert!(
		Layout::new(ConvertFormat::Nv12, (1365, 768)).is_err(),
		"odd 4:2:0 width"
	);
	assert!(
		Layout::new(ConvertFormat::P010, (1366, 767)).is_err(),
		"odd 4:2:0 height"
	);
	assert!(Layout::new(ConvertFormat::Yuv444, (1365, 767)).is_ok());
	// Word indices must fit the shader's 32-bit arithmetic.
	assert!(Layout::new(ConvertFormat::Yuv444P10, (1 << 20, 1 << 20)).is_err());
	assert!(Layout::new(ConvertFormat::Nv12, (u32::MAX - 1, u32::MAX - 1)).is_err());
	assert!(Layout::new(ConvertFormat::Yuv444P10, (16_384, 16_384)).is_ok());
}

/// Every word a copy region reads is written by exactly one block, no block
/// writes outside the buffer, and the planes do not overlap, across extents
/// around block, chroma and row-alignment boundaries.
#[test]
fn every_copied_byte_has_exactly_one_writer() {
	let mut extents: Vec<(u32, u32)> = vec![(1, 1), (2, 2), (3, 5), (5, 7), (4, 2), (6, 4), (8, 2)];
	for w in [1366, 1365, 1920, 1921, 3440, 2880, 7680, 31, 32, 33, 63, 64, 65] {
		for h in [1, 2, 3, 767, 768, 1080, 1081, 4320] {
			extents.push((w, h));
		}
	}
	let mut seed = 0x1234_5678_u64;
	for _ in 0..64 {
		seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
		extents.push((1 + (seed >> 33) as u32 % 700, 1 + (seed >> 13) as u32 % 300));
	}
	for format in FORMATS {
		for &frame in &extents {
			let extent = tight_extent(format, frame);
			let layout = Layout::new(format, extent).unwrap();
			let owners = word_owners(&layout).unwrap_or_else(|e| panic!("{format:?} {extent:?}: {e}"));
			let ranges = copied_bytes(&layout);
			for range in &ranges {
				assert!(
					range.end <= layout.size,
					"{format:?} {extent:?}: copy reads past the buffer"
				);
				for word in range.start / 4..range.end.div_ceil(4) {
					assert!(
						owners[word as usize].is_some(),
						"{format:?} {extent:?}: copied word {word} is never written"
					);
				}
			}
			let [y, c] = layout.copy_regions();
			let luma_end = u64::from(y.buffer_row_length) * u64::from(layout.info.luma_bytes) * u64::from(extent.1);
			assert!(luma_end <= c.buffer_offset, "{format:?} {extent:?}: planes overlap");
			assert_eq!(c.buffer_offset % 4, 0);
			assert!(y.buffer_row_length >= y.image_extent.width && c.buffer_row_length >= c.image_extent.width);
		}
	}
}

#[test]
fn conversion_extent_rounds_to_codec_blocks() {
	assert_eq!(conversion_extent(Codec::H264, 1920, 1080), Ok((1920, 1088)));
	assert_eq!(conversion_extent(Codec::H264, 1366, 768), Ok((1376, 768)));
	assert_eq!(conversion_extent(Codec::H265, 1920, 1080), Ok((1920, 1088)));
	assert_eq!(conversion_extent(Codec::H265, 3440, 1440), Ok((3456, 1440)));
	assert_eq!(conversion_extent(Codec::AV1, 1365, 767), Ok((1408, 768)));
	assert_eq!(conversion_extent(Codec::AV1, 1, 1), Ok((64, 64)));
	assert!(conversion_extent(Codec::H264, 0, 1080).is_err());
	assert!(conversion_extent(Codec::AV1, u32::MAX, 2).is_err());
	// Always a valid 4:2:0 extent.
	for codec in [Codec::H264, Codec::H265, Codec::AV1] {
		let (w, h) = conversion_extent(codec, 1365, 767).unwrap();
		assert!(Layout::new(ConvertFormat::Nv12, (w, h)).is_ok());
	}
}

const BT709_LIMITED: Color = Color {
	space: ColorSpace::Bt709,
	full_range: false,
	sdr_white_nits: 203.0,
};

#[test]
fn reference_matches_published_code_values() {
	let code = |rgb: [f32; 3], color: Color, bits| {
		let [y, cb, cr] = reference::ycbcr(rgb, color);
		[
			reference::luma_code(y, bits, color.full_range),
			reference::chroma_code(cb, bits, color.full_range),
			reference::chroma_code(cr, bits, color.full_range),
		]
	};
	// Rec. BT.709 limited range: black, white and the 100% color bars.
	assert_eq!(code([0.0; 3], BT709_LIMITED, 8), [16, 128, 128]);
	assert_eq!(code([1.0; 3], BT709_LIMITED, 8), [235, 128, 128]);
	assert_eq!(code([1.0, 0.0, 0.0], BT709_LIMITED, 8), [63, 102, 240]);
	assert_eq!(code([0.0, 1.0, 0.0], BT709_LIMITED, 8), [173, 42, 26]);
	assert_eq!(code([0.0, 0.0, 1.0], BT709_LIMITED, 8), [32, 240, 118]);
	assert_eq!(code([1.0; 3], BT709_LIMITED, 10), [940, 512, 512]);
	assert_eq!(code([0.0; 3], BT709_LIMITED, 10), [64, 512, 512]);
	// Full range keeps neutral chroma at 2^(bits-1).
	let full = Color {
		full_range: true,
		..BT709_LIMITED
	};
	assert_eq!(code([0.5; 3], full, 8), [128, 128, 128]);
	assert_eq!(code([1.0; 3], full, 10), [1023, 512, 512]);
	// BT.2100 HDR reference white (203 cd/m2) is about 58% PQ.
	let pq = Color {
		space: ColorSpace::SrgbToBt2020Pq,
		..BT709_LIMITED
	};
	let [y, cb, cr] = code([1.0; 3], pq, 10);
	assert!((570..=580).contains(&y), "PQ reference white {y}");
	assert_eq!((cb, cr), (512, 512));
}

/// The comparison must flag the exact corruption the NVIDIA report showed:
/// intact luma, and the second chroma sample of every 4x2 block wrong.
#[test]
fn comparison_detects_lost_second_chroma_sample() {
	let frame = (64, 16);
	let rgb = |x: u32, y: u32| [(x % 7) as f32 / 7.0, (y % 5) as f32 / 5.0, ((x + y) % 3) as f32 / 3.0];
	for format in [ConvertFormat::Nv12, ConvertFormat::P010] {
		let expected = reference::convert(format, BT709_LIMITED, frame, frame, &rgb, &|_| true);
		assert!(reference::compare(&expected, &expected, 0, &|_| true).is_ok());
		let mut corrupted = expected.clone();
		for (index, sample) in corrupted.chroma.iter_mut().enumerate() {
			if index % 2 == 1 {
				*sample = [64, 64];
			}
		}
		let error = reference::compare(&expected, &corrupted, 1, &|_| true).unwrap_err();
		assert!(error.contains("Y 0/0/0/0"), "{error}");
		assert!(error.contains("Cb 0/"), "{error}");
		// A chroma plane shifted by one row (wrong plane offset) also fails.
		let mut shifted = expected.clone();
		shifted.chroma.rotate_left(expected.chroma_extent.0 as usize);
		assert!(reference::compare(&expected, &shifted, 1, &|_| true).is_err());
	}
}

#[test]
fn reference_replicates_edges_into_padding() {
	let rgb = |x: u32, y: u32| [x as f32 / 4.0, y as f32 / 4.0, 0.5];
	let planes = reference::convert(ConvertFormat::Yuv444, BT709_LIMITED, (3, 2), (8, 4), &rgb, &|_| true);
	for y in 0..4 {
		for x in 0..8 {
			let edge = (x.min(2) + 8 * y.min(1)) as usize;
			assert_eq!(planes.luma[(y * 8 + x) as usize], planes.luma[edge]);
			assert_eq!(planes.chroma[(y * 8 + x) as usize], planes.chroma[edge]);
		}
	}
}

#[test]
fn self_check_pattern_has_no_neutral_or_repeated_chroma() {
	let (w, h) = SELF_CHECK_FRAME;
	let rgb = |x: u32, y: u32| {
		let [r, g, b, _] = self_check_pixel(x, y);
		[r, g, b].map(|c| f32::from(c) / 255.0)
	};
	for format in FORMATS {
		let extent = tight_extent(format, (w, h));
		let planes = reference::convert(format, BT709_LIMITED, (w, h), extent, &rgb, &|_| true);
		let neutral = if format_info(format).luma_bytes == 2 { 512 } else { 128 };
		let mut chroma = planes.chroma.clone();
		assert!(chroma.iter().all(|c| *c != [neutral, neutral]), "{format:?}");
		// Neighbouring chroma samples differ, so a lost or duplicated sample is
		// visible.
		for pair in chroma.windows(2) {
			assert_ne!(pair[0], pair[1], "{format:?}");
		}
		chroma.dedup();
	}
	assert_eq!(std::mem::size_of::<PushConstants>(), 112);
}

#[test]
fn embedded_shader_is_spirv() {
	let words = ash::util::read_spv(&mut std::io::Cursor::new(CONVERT_SPIRV)).unwrap();
	assert_eq!(words[0], 0x0723_0203);
}

/// Every layout, reference and layer check on a real GPU: the dimension
/// matrix (including 1x1, odd and 8K extents) for all formats, input formats,
/// color conversions and ranges, edge padding, late composition at the frame
/// edges and the copy into planar images.
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core converter_matches_reference -- --ignored --nocapture`
#[test]
#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
fn converter_matches_reference_on_gpu() {
	gpu::require();
	gpu::reference_matrix();
}

/// End to end through the encoders: convert, encode, decode with `ffmpeg`
/// and compare with the reference for every codec, chroma format and bit
/// depth the GPU encodes, at aligned and unaligned extents.
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core encode_decode_matches -- --ignored --nocapture`
#[test]
#[ignore = "needs a Vulkan Video GPU and ffmpeg: MOONSHINE_TEST_GPU=1"]
fn encode_decode_matches_reference_on_gpu() {
	gpu::require();
	gpu::encode_decode_matrix(false);
}

/// The stream's low-latency CBR configuration must also produce decodable
/// reference frames; CQP-only checks do not exercise its rate-control path.
#[test]
#[ignore = "needs a Vulkan Video GPU and ffmpeg: MOONSHINE_TEST_GPU=1"]
fn stream_encode_decode_matches_reference_on_gpu() {
	gpu::require();
	gpu::encode_decode_matrix(true);
}

/// The VA-API backend end to end: convert into the imported VA surfaces,
/// encode with the stream's CBR configuration (with an IDR request and a
/// reference invalidation mid-stream), decode with `ffmpeg` and compare with
/// the reference for every profile its startup probe accepts.
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core vaapi_encode_decode -- --ignored --nocapture`
#[test]
#[ignore = "needs a VA-API encoder and ffmpeg: MOONSHINE_TEST_GPU=1"]
fn vaapi_encode_decode_matches_reference_on_gpu() {
	gpu::require();
	gpu::native_encode_decode_matrix(EncoderBackend::Vaapi);
}

/// [`vaapi_encode_decode_matches_reference_on_gpu`] for the NVENC backend
/// (CUDA-imported input buffers, reference invalidation through NVENC).
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core nvenc_encode_decode -- --ignored --nocapture`
#[test]
#[ignore = "needs an NVIDIA GPU with NVENC and ffmpeg: MOONSHINE_TEST_GPU=1"]
fn nvenc_encode_decode_matches_reference_on_gpu() {
	gpu::require();
	gpu::native_encode_decode_matrix(EncoderBackend::Nvenc);
}

/// Pins the Pixelforge input-slot contract modelled by `FakeSlots` in the
/// pipeline tests: two slots, `encode` advances to the next one. A change in
/// the pinned encoder must fail here rather than silently change recovery.
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core encoder_input_slots -- --ignored`
#[test]
#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
fn encoder_input_slots_rotate_on_gpu() {
	gpu::require();
	gpu::slot_rotation();
}

/// Review 2026-10-05 BUG-002 on real hardware: encode one, two or five
/// distinct solid scenes, then a static-scene recovery keyframe through the
/// production `encode_static_recovery`, decode the stream with `ffmpeg` and
/// compare the recovered picture with the latest scene.
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core static_recovery_decodes -- --ignored --nocapture`
#[test]
#[ignore = "needs a Vulkan Video GPU and ffmpeg: MOONSHINE_TEST_GPU=1"]
fn static_recovery_decodes_to_the_latest_frame_on_gpu() {
	gpu::require();
	gpu::static_recovery_decode();
}

/// Each codec's 8-bit 4:2:0 profile on this GPU: an encoder is created at
/// exactly the profile's maximum bitrate, one above it is rejected before the
/// driver sees it, and a profile without constant-bitrate rate control
/// cannot create the stream's CBR encoder (Pixelforge patch 0010).
/// `MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core encoder_rate_control_limits -- --ignored --nocapture`
#[test]
#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
fn encoder_rate_control_limits_on_gpu() {
	gpu::require();
	gpu::rate_control_limits();
}

/// Informational: GPU and wall time per conversion for aligned and
/// unaligned extents (`MOONSHINE_TEST_GPU=1 ... converter_timing -- --ignored --nocapture`).
#[test]
#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
fn converter_timing_on_gpu() {
	gpu::require();
	gpu::timing();
}

mod gpu {
	use pixelforge::{EncodeBitDepth, EncodeConfig, Encoder, PixelFormat, RateControlMode};

	use super::*;
	use crate::session::stream::video::pipeline::encode_caps::check_frame;
	use pixelforge::EncoderCapabilities;

	pub(super) fn require() {
		assert!(
			std::env::var_os("MOONSHINE_TEST_GPU").is_some(),
			"set MOONSHINE_TEST_GPU=1"
		);
	}

	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> u32 {
			self.0 = self
				.0
				.wrapping_mul(6364136223846793005)
				.wrapping_add(1442695040888963407);
			(self.0 >> 33) as u32
		}
	}

	pub(super) fn rate_control_limits() {
		use crate::session::stream::video::pipeline::encode_caps::cbr_target;
		let context = context();
		let mut checked = 0;
		for codec in [Codec::H264, Codec::H265, Codec::AV1] {
			if !context.supports_encode(codec) {
				continue;
			}
			let base = match codec {
				Codec::H264 => EncodeConfig::h264(1920, 1080),
				Codec::H265 => EncodeConfig::h265(1920, 1080),
				Codec::AV1 => EncodeConfig::av1(1920, 1080),
			}
			.with_rate_control(RateControlMode::Cbr);
			let caps = EncoderCapabilities::query(&context, &base).unwrap();
			let cbr = caps
				.rate_control_modes
				.contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR);
			eprintln!(
				"{codec:?}: rate control {:?}, max {} bps",
				caps.rate_control_modes, caps.max_bitrate
			);
			if !cbr {
				assert!(cbr_target(&caps, codec, 10_000_000).is_err());
				assert!(Encoder::new(context.clone(), base.with_target_bitrate(10_000_000)).is_err());
				checked += 1;
				continue;
			}
			let max = cbr_target(&caps, codec, u64::MAX).unwrap();
			assert_eq!(u64::from(max), caps.max_bitrate.min(u64::from(u32::MAX)));
			drop(Encoder::new(context.clone(), base.clone().with_target_bitrate(max)).expect("maximum bitrate"));
			if let Some(above) = max.checked_add(1).filter(|_| caps.max_bitrate < u64::from(u32::MAX)) {
				let error = Encoder::new(context.clone(), base.with_target_bitrate(above))
					.err()
					.expect("a bitrate above the maximum must be rejected");
				assert!(error.to_string().contains("at most"), "{error}");
			}
			checked += 1;
		}
		assert!(checked > 0, "no Vulkan Video codec on this GPU");
	}

	/// Use the opened capture GPU, exactly as production setup does.
	fn context() -> VideoContext {
		let path = std::env::var("MOONSHINE_TEST_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
		let file = std::fs::File::open(path).expect("open capture render node");
		crate::gpu::capture_context(&file).expect("a verified capture/encode GPU")
	}

	fn device_name(context: &VideoContext) -> String {
		// SAFETY: `device_name` is NUL-terminated by the implementation.
		unsafe { std::ffi::CStr::from_ptr(context.device_properties().device_name.as_ptr()) }
			.to_string_lossy()
			.into_owned()
	}

	fn host_buffer(
		context: &VideoContext,
		size: u64,
		usage: vk::BufferUsageFlags,
	) -> (vk::Buffer, vk::DeviceMemory, *mut u8) {
		let device = context.device();
		// SAFETY: test-owned objects on a live device.
		unsafe {
			let buffer = device
				.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None)
				.unwrap();
			let req = device.get_buffer_memory_requirements(buffer);
			let ty = find_memory_type(
				context,
				req.memory_type_bits,
				vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
			)
			.unwrap();
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.unwrap();
			device.bind_buffer_memory(buffer, memory, 0).unwrap();
			let ptr = device
				.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
				.unwrap()
				.cast();
			(buffer, memory, ptr)
		}
	}

	fn free_buffer(context: &VideoContext, (buffer, memory, _): (vk::Buffer, vk::DeviceMemory, *mut u8)) {
		// SAFETY: every submission using it completed.
		unsafe {
			context.device().destroy_buffer(buffer, None);
			context.device().free_memory(memory, None);
		}
	}

	/// One-shot command buffer on the compute queue.
	fn submit(context: &VideoContext, record: impl FnOnce(vk::CommandBuffer)) {
		let device = context.device();
		// SAFETY: test-owned objects; waits for completion before returning.
		unsafe {
			let pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default().queue_family_index(context.compute_queue_family()),
					None,
				)
				.unwrap();
			let cmd = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(pool)
						.command_buffer_count(1),
				)
				.unwrap()[0];
			device
				.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
				.unwrap();
			record(cmd);
			device.end_command_buffer(cmd).unwrap();
			let cmds = [cmd];
			device
				.queue_submit(
					context.compute_queue(),
					&[vk::SubmitInfo::default().command_buffers(&cmds)],
					vk::Fence::null(),
				)
				.unwrap();
			device.queue_wait_idle(context.compute_queue()).unwrap();
			device.destroy_command_pool(pool, None);
		}
	}

	fn image_barrier(
		image: vk::Image,
		old: vk::ImageLayout,
		new: vk::ImageLayout,
		aspect: vk::ImageAspectFlags,
	) -> vk::ImageMemoryBarrier<'static> {
		vk::ImageMemoryBarrier::default()
			.old_layout(old)
			.new_layout(new)
			.src_access_mask(vk::AccessFlags::MEMORY_WRITE)
			.dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
			.src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
			.image(image)
			.subresource_range(vk::ImageSubresourceRange {
				aspect_mask: aspect,
				..color_range()
			})
	}

	fn barrier(context: &VideoContext, cmd: vk::CommandBuffer, barriers: &[vk::ImageMemoryBarrier<'_>]) {
		// SAFETY: recording command buffer of this device.
		unsafe {
			context.device().cmd_pipeline_barrier(
				cmd,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::PipelineStageFlags::ALL_COMMANDS,
				vk::DependencyFlags::empty(),
				&[],
				&[],
				barriers,
			);
		}
	}

	/// A sampled source image in GENERAL layout holding `bytes`.
	fn source_image(context: &VideoContext, (w, h): (u32, u32), format: vk::Format, bytes: &[u8]) -> Arc<CachedImport> {
		let device = context.device();
		// SAFETY: test-owned objects on a live device.
		let (image, memory) = unsafe {
			let image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(format)
						.extent(vk::Extent3D {
							width: w,
							height: h,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.unwrap();
			let req = device.get_image_memory_requirements(image);
			let ty = find_memory_type(context, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).unwrap();
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.unwrap();
			device.bind_image_memory(image, memory, 0).unwrap();
			(image, memory)
		};
		let staging = host_buffer(context, bytes.len() as u64, vk::BufferUsageFlags::TRANSFER_SRC);
		// SAFETY: the mapping holds bytes.len() bytes.
		unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), staging.2, bytes.len()) };
		submit(context, |cmd| unsafe {
			barrier(
				context,
				cmd,
				&[image_barrier(
					image,
					vk::ImageLayout::UNDEFINED,
					vk::ImageLayout::TRANSFER_DST_OPTIMAL,
					vk::ImageAspectFlags::COLOR,
				)],
			);
			device.cmd_copy_buffer_to_image(
				cmd,
				staging.0,
				image,
				vk::ImageLayout::TRANSFER_DST_OPTIMAL,
				&[vk::BufferImageCopy {
					image_subresource: vk::ImageSubresourceLayers {
						aspect_mask: vk::ImageAspectFlags::COLOR,
						mip_level: 0,
						base_array_layer: 0,
						layer_count: 1,
					},
					image_extent: vk::Extent3D {
						width: w,
						height: h,
						depth: 1,
					},
					..Default::default()
				}],
			);
			barrier(
				context,
				cmd,
				&[image_barrier(
					image,
					vk::ImageLayout::TRANSFER_DST_OPTIMAL,
					vk::ImageLayout::GENERAL,
					vk::ImageAspectFlags::COLOR,
				)],
			);
		});
		free_buffer(context, staging);
		Arc::new(CachedImport::for_test(context.clone(), image, memory, w, h, format))
	}

	fn half_to_f32(bits: u16) -> f32 {
		let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
		let exponent = i32::from((bits >> 10) & 0x1f);
		let mantissa = f32::from(bits & 0x3ff);
		sign * match exponent {
			0 => mantissa * 2f32.powi(-24),
			31 => f32::INFINITY,
			e => (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
		}
	}

	fn f32_to_half(v: f32) -> u16 {
		// Exact for the fixture's values (multiples of 1/1024 below 4).
		let bits = v.to_bits();
		let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
		if v == 0.0 || exponent <= 0 {
			return 0;
		}
		(((exponent as u32) << 10) | ((bits >> 13) & 0x3ff)) as u16
	}

	/// The source formats the compositor exports, with their texel decoders.
	const SOURCE_FORMATS: [vk::Format; 5] = [
		vk::Format::R8G8B8A8_UNORM,
		vk::Format::B8G8R8A8_UNORM,
		vk::Format::A2B10G10R10_UNORM_PACK32,
		vk::Format::A2R10G10B10_UNORM_PACK32,
		vk::Format::R16G16B16A16_SFLOAT,
	];

	fn texel_bytes(format: vk::Format) -> usize {
		if format == vk::Format::R16G16B16A16_SFLOAT {
			8
		} else {
			4
		}
	}

	/// RGB of one texel as the GPU samples it.
	fn decode_texel(format: vk::Format, t: &[u8]) -> [f32; 3] {
		let word = |i: usize| u32::from_le_bytes([t[i], t[i + 1], t[i + 2], t[i + 3]]);
		let ten = |shift: u32| ((word(0) >> shift) & 0x3ff) as f32 / 1023.0;
		match format {
			vk::Format::R8G8B8A8_UNORM => [t[0], t[1], t[2]].map(|c| f32::from(c) / 255.0),
			vk::Format::B8G8R8A8_UNORM => [t[2], t[1], t[0]].map(|c| f32::from(c) / 255.0),
			vk::Format::A2B10G10R10_UNORM_PACK32 => [ten(0), ten(10), ten(20)],
			vk::Format::A2R10G10B10_UNORM_PACK32 => [ten(20), ten(10), ten(0)],
			vk::Format::R16G16B16A16_SFLOAT => [0, 2, 4].map(|i| half_to_f32(u16::from_le_bytes([t[i], t[i + 1]]))),
			_ => unreachable!(),
		}
	}

	/// Deterministic test picture: random texels mixed with structure that
	/// exposes layout defects (checkerboards, row/column ramps, saturated bars
	/// with sharp chroma transitions on odd columns).
	fn pattern(rng: &mut Rng, format: vk::Format, (w, h): (u32, u32)) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(w as usize * h as usize * texel_bytes(format));
		for y in 0..h {
			for x in 0..w {
				let rgb: [f32; 3] = match (x / 3 + y / 2) % 4 {
					0 => [
						(rng.next() % 1024) as f32 / 1023.0,
						(rng.next() % 1024) as f32 / 1023.0,
						(rng.next() % 1024) as f32 / 1023.0,
					],
					1 => {
						// Magenta/green checkerboard. Pure green lands 1e-4 above a
						// rounding boundary in full-range 8-bit PQ, where f32 and
						// f64 legitimately round apart; this green does not.
						if (x + y) % 2 == 0 {
							[1.0, 0.0, 1.0]
						} else {
							[0.0, 0.94, 0.06]
						}
					},
					2 => [
						(x % 256) as f32 / 255.0,
						(y % 256) as f32 / 255.0,
						((x + y) % 256) as f32 / 255.0,
					],
					_ => [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 0.0], [0.2, 0.2, 0.2]][(x % 4) as usize],
				};
				match format {
					vk::Format::R8G8B8A8_UNORM => {
						bytes.extend(rgb.map(|c| (c * 255.0).round() as u8).into_iter().chain([255]))
					},
					vk::Format::B8G8R8A8_UNORM => {
						let [r, g, b] = rgb.map(|c| (c * 255.0).round() as u8);
						bytes.extend([b, g, r, (rng.next() & 0xff) as u8]);
					},
					vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32 => {
						let [r, g, b] = rgb.map(|c| (c * 1023.0).round() as u32);
						let (low, high) = if format == vk::Format::A2B10G10R10_UNORM_PACK32 {
							(r, b)
						} else {
							(b, r)
						};
						bytes.extend((low | (g << 10) | (high << 20) | ((rng.next() & 3) << 30)).to_le_bytes());
					},
					_ => {
						// scRGB-like values, including highlights above 1.0.
						let scale = 1.0 + (rng.next() % 3) as f32;
						for c in rgb
							.map(|c| ((c * scale * 1024.0).round() / 1024.0).min(3.99))
							.into_iter()
							.chain([1.0])
						{
							bytes.extend(f32_to_half(c).to_le_bytes());
						}
					},
				}
			}
		}
		bytes
	}

	/// A CPU-texel or imported layer with its reference sampler.
	struct TestLayer {
		x: i32,
		y: i32,
		width: u32,
		height: u32,
		opacity: f32,
		opaque: bool,
		/// Premultiplied RGBA per texel, as the GPU samples it.
		texel: Box<dyn Fn(u32, u32) -> [f32; 4]>,
	}

	/// Late composition in the source encoding, as GLES blends it.
	fn composite(rgb: [f32; 3], (x, y): (u32, u32), layers: &[TestLayer]) -> [f32; 3] {
		let mut dst = rgb.map(f64::from);
		for layer in layers {
			let (rx, ry) = (x as i32 - layer.x, y as i32 - layer.y);
			if rx < 0 || ry < 0 || rx >= layer.width as i32 || ry >= layer.height as i32 {
				continue;
			}
			let mut o = layer.texel(rx as u32, ry as u32).map(f64::from);
			if layer.opaque {
				o[3] = 1.0;
			}
			let o = o.map(|c| c * f64::from(layer.opacity));
			for c in 0..3 {
				dst[c] = o[c] + dst[c] * (1.0 - o[3]);
			}
		}
		dst.map(|c| c as f32)
	}

	impl TestLayer {
		fn texel(&self, x: u32, y: u32) -> [f32; 4] {
			(self.texel)(x, y)
		}
	}

	/// One converter run compared with the reference.
	struct Case {
		format: ConvertFormat,
		frame: (u32, u32),
		extent: (u32, u32),
		color: Color,
		source_format: vk::Format,
		source_extent: (u32, u32),
	}

	/// Rows compared for a frame: all of them up to ~4 MP, else a sample
	/// including every edge and padding row. Both rows of a 4:2:0 pair are
	/// selected together.
	fn rows(extent: (u32, u32)) -> impl Fn(u32) -> bool {
		let all = u64::from(extent.0) * u64::from(extent.1) <= 4_000_000;
		let height = extent.1;
		move |y| {
			let pair = y & !1;
			all || pair < 4 || pair + 8 >= height || pair % 98 == 0
		}
	}

	/// Convert `source` (bytes of `case.source_format`) with `layers` and
	/// compare the output buffer with the reference. The buffer is filled
	/// with a sentinel first, so unwritten samples fail.
	fn check(
		context: &VideoContext,
		case: &Case,
		source: &Arc<CachedImport>,
		bytes: &[u8],
		layers: (&Layers<'_>, &[TestLayer]),
	) -> Result<reference::Agreement, String> {
		let mut converter = InputConverter::create(
			context.clone(),
			ConversionQueueMode::Auto,
			case.frame,
			case.extent,
			case.format,
			4,
			case.color.space,
			case.color.full_range,
			true,
		)?;
		converter.set_color(case.color.space, case.color.full_range, case.color.sdr_white_nits);
		// SAFETY: host-coherent mapping of the whole buffer; no GPU work uses it.
		unsafe { std::ptr::write_bytes(converter.readback.cast_mut(), 0xa5, converter.layout.size as usize) };
		let view = converter.source_view(source, case.source_format)?;
		converter
			.dispatch(
				&SourceBinding {
					image: source.image(),
					view,
					extent: case.source_extent,
					scaled: false,
					first_use: false,
				},
				layers.0,
				None,
			)
			.map_err(|e| format!("dispatch: {e}"))?;
		let actual = converter.layout.read_planes(converter.readback())?;
		let texel_size = texel_bytes(case.source_format);
		let (sw, sh) = case.source_extent;
		let rgb = |x: u32, y: u32| {
			let (sx, sy) = (x.min(sw - 1), y.min(sh - 1));
			let offset = (sy as usize * sw as usize + sx as usize) * texel_size;
			composite(
				decode_texel(case.source_format, &bytes[offset..offset + texel_size]),
				(x, y),
				layers.1,
			)
		};
		let row = rows(case.extent);
		let expected = reference::convert(case.format, case.color, case.frame, case.extent, &rgb, &row);
		reference::compare(&expected, &actual, 1, &row)
	}

	/// At most this share of samples may sit on the other side of a rounding
	/// boundary (f32 GPU vs f64 reference); anything systematic fails.
	fn assert_agreement(label: &str, result: Result<reference::Agreement, String>, failures: &mut Vec<String>) {
		match result {
			Ok(agreement) => {
				eprintln!(
					"{label}: {} of {} samples differ by one",
					agreement.differing, agreement.compared
				);
				if agreement.differing * 200 > agreement.compared.max(1) {
					failures.push(format!("{label}: {} one-code differences", agreement.differing));
				}
			},
			Err(e) => {
				eprintln!("{label}: FAILED {e}");
				failures.push(format!("{label}: {e}"));
			},
		}
	}

	pub(super) fn reference_matrix() {
		let context = context();
		eprintln!("device: {}", device_name(&context));
		let mut failures = Vec::new();
		let mut rng = Rng(0x5eed);

		// Self-check of every format through the production constructor.
		for format in FORMATS {
			let extent = tight_extent(format, (1365, 767));
			let started = std::time::Instant::now();
			let created = InputConverter::create(
				context.clone(),
				ConversionQueueMode::Auto,
				(1365, 767),
				extent,
				format,
				4,
				ColorSpace::Bt709,
				false,
				false,
			);
			let create = started.elapsed();
			drop(created);
			let started = std::time::Instant::now();
			let checked = self_check(&context, ConversionQueueMode::Auto, format);
			eprintln!(
				"{format:?}: converter creation {:.2} ms, self-check {:.2} ms",
				create.as_secs_f64() * 1e3,
				started.elapsed().as_secs_f64() * 1e3
			);
			if let Err(e) = checked {
				failures.push(format!("{format:?} self-check: {e}"));
			}
			if let Err(e) = InputConverter::new(
				context.clone(),
				ConversionQueueMode::Auto,
				(1365, 767),
				extent,
				format,
				4,
				ColorSpace::Bt709,
				false,
			) {
				failures.push(format!("{format:?} self-check: {e}"));
			}
		}

		// Dimension matrix, BT.709 limited, RGBA8 source.
		let mut frames = vec![
			(1, 1),
			(2, 2),
			(3, 5),
			(5, 7),
			(4, 2),
			(7, 3),
			(33, 17),
			(1366, 768),
			(1920, 1081),
			(1365, 767),
			(1920, 1080),
			(2560, 1440),
			(3440, 1440),
			(2880, 1920),
			(3840, 2160),
			(7680, 4320),
		];
		for _ in 0..10 {
			// Around workgroup (32x16 px), chroma and row-alignment boundaries.
			let near = |rng: &mut Rng, unit: u32, max: u32| (1 + rng.next() % (max / unit)) * unit + rng.next() % 3 - 1;
			frames.push((near(&mut rng, 32, 2048).max(1), near(&mut rng, 16, 1200).max(1)));
		}
		for &frame in &frames {
			let source = pattern(&mut rng, vk::Format::R8G8B8A8_UNORM, frame);
			let image = source_image(&context, frame, vk::Format::R8G8B8A8_UNORM, &source);
			for format in FORMATS {
				// Tight extent, and a codec-padded one (edge replication).
				let mut extents = vec![tight_extent(format, frame)];
				let padded = conversion_extent(Codec::AV1, frame.0, frame.1).unwrap();
				if padded != extents[0] && u64::from(padded.0) * u64::from(padded.1) <= 40_000_000 {
					extents.push(padded);
				}
				for extent in extents {
					let case = Case {
						format,
						frame,
						extent,
						color: BT709_LIMITED,
						source_format: vk::Format::R8G8B8A8_UNORM,
						source_extent: frame,
					};
					let label = format!("{format:?} {}x{} -> {}x{}", frame.0, frame.1, extent.0, extent.1);
					match check(&context, &case, &image, &source, (&[None, None], &[])) {
						Err(e) if e.contains("storage buffer") || e.contains("workgroups") => {
							eprintln!("{label}: skipped, beyond this GPU's limits: {e}");
						},
						result => assert_agreement(&label, result, &mut failures),
					}
				}
			}
		}

		// Every source format x color conversion x range, at an odd extent.
		let frame = (257, 131);
		for source_format in SOURCE_FORMATS {
			let source = pattern(&mut rng, source_format, frame);
			let image = source_image(&context, frame, source_format, &source);
			for format in FORMATS {
				for space in [
					ColorSpace::Bt709,
					ColorSpace::Bt2020,
					ColorSpace::SrgbToBt2020Pq,
					ColorSpace::Bt709LinearToBt2020Pq,
				] {
					for full_range in [false, true] {
						let case = Case {
							format,
							frame,
							extent: tight_extent(format, frame),
							color: Color {
								space,
								full_range,
								sdr_white_nits: if space == ColorSpace::Bt709LinearToBt2020Pq {
									80.0
								} else {
									203.0
								},
							},
							source_format,
							source_extent: frame,
						};
						let label = format!("{format:?} {source_format:?} {space:?} full={full_range}");
						assert_agreement(
							&label,
							check(&context, &case, &image, &source, (&[None, None], &[])),
							&mut failures,
						);
					}
				}
			}
		}

		// A source smaller than the frame (capture raced an extent change).
		let source = pattern(&mut rng, vk::Format::B8G8R8A8_UNORM, (100, 50));
		let image = source_image(&context, (100, 50), vk::Format::B8G8R8A8_UNORM, &source);
		for format in FORMATS {
			let case = Case {
				format,
				frame: (121, 61),
				extent: tight_extent(format, (121, 61)),
				color: BT709_LIMITED,
				source_format: vk::Format::B8G8R8A8_UNORM,
				source_extent: (100, 50),
			};
			assert_agreement(
				&format!("{format:?} 100x50 source in 121x61 frame"),
				check(&context, &case, &image, &source, (&[None, None], &[])),
				&mut failures,
			);
		}

		layers_at_frame_edges(&context, &mut rng, &mut failures);
		copy_into_planar_images(&context, &mut rng, &mut failures);
		scaled_sources(&context, &mut rng, &mut failures);
		assert!(failures.is_empty(), "converter mismatches: {failures:#?}");
	}

	/// Desktop Mode's scaled path (`ExportedFrame::scale_to_stream`): at an
	/// exact 2:1 downscale the four bilinear taps fall on texel centers, so
	/// every output pixel is the box average of its 2x2 source texels, for
	/// odd frames and every output format; a source of another aspect ratio
	/// keeps it, centered between black bars.
	fn scaled_sources(context: &VideoContext, rng: &mut Rng, failures: &mut Vec<String>) {
		// Same aspect ratio (fills the frame), then a 4:1 source letterboxed
		// into 16:9 (black bars above and below the 320x80 viewport).
		for (frame, source_extent) in [((321, 181), (642, 362)), ((320, 180), (640, 160))] {
			let viewport = crate::session::capture::frame::fit_viewport(source_extent, frame);
			let source_format = vk::Format::B8G8R8A8_UNORM;
			let bytes = pattern(rng, source_format, source_extent);
			let image = source_image(context, source_extent, source_format, &bytes);
			let texel_size = texel_bytes(source_format);
			let texel = |x: u32, y: u32| {
				let offset = (y as usize * source_extent.0 as usize + x as usize) * texel_size;
				decode_texel(source_format, &bytes[offset..offset + texel_size])
			};
			for format in FORMATS {
				let extent = tight_extent(format, frame);
				let label = format!(
					"{format:?} scaled {}x{} -> {}x{} (viewport {viewport:?})",
					source_extent.0, source_extent.1, frame.0, frame.1
				);
				let result = (|| -> Result<reference::Agreement, String> {
					let mut converter = InputConverter::create(
						context.clone(),
						ConversionQueueMode::Auto,
						frame,
						extent,
						format,
						4,
						BT709_LIMITED.space,
						BT709_LIMITED.full_range,
						true,
					)?;
					let view = converter.source_view(&image, source_format)?;
					converter
						.dispatch(
							&SourceBinding {
								image: image.image(),
								view,
								extent: source_extent,
								scaled: true,
								first_use: false,
							},
							&[None, None],
							None,
						)
						.map_err(|e| format!("dispatch: {e}"))?;
					let actual = converter.layout.read_planes(converter.readback())?;
					let (vx, vy, vw, vh) = viewport;
					let rgb = |x: u32, y: u32| {
						// Padding replicates the frame's edge pixel.
						let (x, y) = (x.min(frame.0 - 1), y.min(frame.1 - 1));
						if x < vx || y < vy || x >= vx + vw || y >= vy + vh {
							return [0.0; 3];
						}
						let (x, y) = (x - vx, y - vy);
						let taps = [
							texel(2 * x, 2 * y),
							texel(2 * x + 1, 2 * y),
							texel(2 * x, 2 * y + 1),
							texel(2 * x + 1, 2 * y + 1),
						];
						std::array::from_fn(|c| taps.iter().map(|t| t[c]).sum::<f32>() / 4.0)
					};
					let row = rows(extent);
					let expected = reference::convert(format, BT709_LIMITED, frame, extent, &rgb, &row);
					reference::compare(&expected, &actual, 1, &row)
				})();
				assert_agreement(&label, result, failures);
			}
		}
	}

	/// A notification (imported BGRX, alpha ignored, 75% window opacity)
	/// clipped by the right edge and a cursor with every alpha level clipped by
	/// the bottom-left corner of an odd-sized frame, so both reach the
	/// replicated padding.
	fn layers_at_frame_edges(context: &VideoContext, rng: &mut Rng, failures: &mut Vec<String>) {
		let frame = (1365u32, 767u32);
		let source = pattern(rng, vk::Format::R8G8B8A8_UNORM, frame);
		let image = source_image(context, frame, vk::Format::R8G8B8A8_UNORM, &source);
		let (cw, ch) = (32u32, 24u32);
		let mut cursor = vec![0u8; (cw * ch * 4) as usize];
		for (i, texel) in cursor.as_chunks_mut::<4>().0.iter_mut().enumerate() {
			let a = ((i * 7) % 256) as u8;
			for channel in &mut texel[..3] {
				*channel = (rng.next() % (u32::from(a) + 1)) as u8;
			}
			texel[3] = a;
		}
		let (nw, nh) = (120u32, 60u32);
		let notification: Vec<u8> = (0..nw * nh * 4).map(|_| rng.next() as u8).collect();
		let notification_image = source_image(context, (nw, nh), vk::Format::B8G8R8A8_UNORM, &notification);
		let cursor_image = OverlayImage {
			generation: 1,
			width: cw,
			height: ch,
			format: OverlayFormat::Rgba8,
			pixels: cursor.clone().into_boxed_slice(),
		};
		let (nx, ny, cx, cy) = (frame.0 as i32 - 70, 30, -5, frame.1 as i32 - 10);
		let reference_layers = [
			TestLayer {
				x: nx,
				y: ny,
				width: nw,
				height: nh,
				opacity: 0.75,
				opaque: true,
				texel: Box::new(move |x, y| {
					let t = &notification[((y * nw + x) * 4) as usize..][..4];
					[t[2], t[1], t[0], t[3]].map(|c| f32::from(c) / 255.0)
				}),
			},
			TestLayer {
				x: cx,
				y: cy,
				width: cw,
				height: ch,
				opacity: 1.0,
				opaque: false,
				texel: Box::new(move |x, y| {
					let t = &cursor[((y * cw + x) * 4) as usize..][..4];
					[t[0], t[1], t[2], t[3]].map(|c| f32::from(c) / 255.0)
				}),
			},
		];
		let layers: Layers<'_> = [
			Some(Layer {
				input: LayerInput::Imported {
					import: &notification_image,
					format: vk::Format::B8G8R8A8_UNORM,
					first_use: false,
					opaque: true,
				},
				x: nx,
				y: ny,
				width: nw,
				height: nh,
				opacity: 0.75,
			}),
			Some(Layer {
				input: LayerInput::Pixels(&cursor_image),
				x: cx,
				y: cy,
				width: cw,
				height: ch,
				opacity: 1.0,
			}),
		];
		for format in FORMATS {
			let case = Case {
				format,
				frame,
				extent: conversion_extent(Codec::H264, frame.0, frame.1).unwrap(),
				color: BT709_LIMITED,
				source_format: vk::Format::R8G8B8A8_UNORM,
				source_extent: frame,
			};
			assert_agreement(
				&format!("{format:?} notification + cursor at the edges of 1365x767"),
				check(context, &case, &image, &source, (&layers, &reference_layers)),
				failures,
			);
		}
	}

	/// A planar image with the usage of an encoder input plus readback.
	struct PlanarImage(vk::Image, vk::DeviceMemory, VideoContext);
	impl Drop for PlanarImage {
		fn drop(&mut self) {
			// SAFETY: every submission using it completed.
			unsafe {
				self.2.device().destroy_image(self.0, None);
				self.2.device().free_memory(self.1, None);
			}
		}
	}

	fn planar_image(context: &VideoContext, (w, h): (u32, u32), format: vk::Format) -> Option<PlanarImage> {
		let device = context.device();
		let usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC;
		// SAFETY: plain physical-device query; creating an unsupported format
		// is invalid usage (and crashes some drivers).
		let _ = unsafe {
			context.instance().get_physical_device_image_format_properties(
				context.physical_device(),
				format,
				vk::ImageType::TYPE_2D,
				vk::ImageTiling::OPTIMAL,
				usage,
				vk::ImageCreateFlags::empty(),
			)
		}
		.ok()?;
		// SAFETY: test-owned objects.
		unsafe {
			let image = device
				.create_image(
					&vk::ImageCreateInfo::default()
						.image_type(vk::ImageType::TYPE_2D)
						.format(format)
						.extent(vk::Extent3D {
							width: w,
							height: h,
							depth: 1,
						})
						.mip_levels(1)
						.array_layers(1)
						.samples(vk::SampleCountFlags::TYPE_1)
						.tiling(vk::ImageTiling::OPTIMAL)
						.usage(usage)
						.initial_layout(vk::ImageLayout::UNDEFINED),
					None,
				)
				.ok()?;
			let req = device.get_image_memory_requirements(image);
			let ty = find_memory_type(context, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).ok()?;
			let memory = device
				.allocate_memory(
					&vk::MemoryAllocateInfo::default()
						.allocation_size(req.size)
						.memory_type_index(ty),
					None,
				)
				.ok()?;
			device.bind_image_memory(image, memory, 0).ok()?;
			Some(PlanarImage(image, memory, context.clone()))
		}
	}

	/// The production copy regions write exactly the conversion extent of a
	/// (larger, coded-size) planar image: read both planes back and compare
	/// them with the reference, and the untouched padding with its sentinel.
	fn copy_into_planar_images(context: &VideoContext, rng: &mut Rng, failures: &mut Vec<String>) {
		for (format, frame, image_extent) in [
			(ConvertFormat::Nv12, (1366, 768), (1376, 768)),
			(ConvertFormat::Nv12, (1920, 1080), (1920, 1088)),
			(ConvertFormat::P010, (1365, 767), (1408, 768)),
			(ConvertFormat::P010, (3, 5), (64, 64)),
			(ConvertFormat::Yuv444, (1365, 767), (1376, 768)),
			(ConvertFormat::Yuv444P10, (5, 7), (16, 16)),
		] {
			let label = format!(
				"{format:?} {}x{} copied into {}x{}",
				frame.0, frame.1, image_extent.0, image_extent.1
			);
			let Some(target) = planar_image(context, image_extent, vulkan_format(format)) else {
				eprintln!("{label}: skipped, {:?} images unsupported", vulkan_format(format));
				continue;
			};
			let source_bytes = pattern(rng, vk::Format::R8G8B8A8_UNORM, frame);
			let source = source_image(context, frame, vk::Format::R8G8B8A8_UNORM, &source_bytes);
			let extent = tight_extent(format, frame);
			let image_layout = Layout::new(format, image_extent).unwrap();
			// Fill the whole image with a sentinel through the layout's own
			// regions over the image extent.
			let sentinel = host_buffer(context, image_layout.size, vk::BufferUsageFlags::TRANSFER_SRC);
			// SAFETY: host-coherent mapping of `size` bytes.
			unsafe { std::ptr::write_bytes(sentinel.2, 0x5a, image_layout.size as usize) };
			submit(context, |cmd| unsafe {
				let all = vk::ImageAspectFlags::COLOR;
				barrier(
					context,
					cmd,
					&[image_barrier(
						target.0,
						vk::ImageLayout::UNDEFINED,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						all,
					)],
				);
				context.device().cmd_copy_buffer_to_image(
					cmd,
					sentinel.0,
					target.0,
					vk::ImageLayout::TRANSFER_DST_OPTIMAL,
					&image_layout.copy_regions(),
				);
				barrier(
					context,
					cmd,
					&[image_barrier(
						target.0,
						vk::ImageLayout::TRANSFER_DST_OPTIMAL,
						vk::ImageLayout::GENERAL,
						all,
					)],
				);
			});
			free_buffer(context, sentinel);

			let mut converter = InputConverter::new(
				context.clone(),
				ConversionQueueMode::Auto,
				frame,
				extent,
				format,
				4,
				ColorSpace::Bt709,
				false,
			)
			.unwrap();
			converter
				.convert(
					&source,
					vk::Format::R8G8B8A8_UNORM,
					false,
					false,
					&[None, None],
					Target::Image {
						image: target.0,
						layout: vk::ImageLayout::GENERAL,
					},
				)
				.unwrap();
			// Read the whole image back with the image-extent layout.
			let readback = host_buffer(context, image_layout.size, vk::BufferUsageFlags::TRANSFER_DST);
			submit(context, |cmd| unsafe {
				let regions = image_layout.copy_regions();
				context.device().cmd_copy_image_to_buffer(
					cmd,
					target.0,
					vk::ImageLayout::GENERAL,
					readback.0,
					&regions,
				);
			});
			// SAFETY: the copy completed; the mapping holds `size` bytes.
			let bytes = unsafe { std::slice::from_raw_parts(readback.2, image_layout.size as usize) }.to_vec();
			free_buffer(context, readback);
			// `read_planes` rejects the 10-bit sentinel's low bits, so decode the
			// image-extent planes without that check: compare raw samples.
			let actual = decode_lenient(&image_layout, &bytes);
			let rgb = |x: u32, y: u32| {
				let offset = (y as usize * frame.0 as usize + x as usize) * 4;
				decode_texel(vk::Format::R8G8B8A8_UNORM, &source_bytes[offset..offset + 4])
			};
			let expected = reference::convert(format, BT709_LIMITED, frame, extent, &rgb, &|_| true);
			let sentinel_sample: u16 = if image_layout.info.luma_bytes == 2 {
				0x5a5a
			} else {
				0x5a
			};
			let mut mismatches = 0u64;
			let mut sentinel_overwritten = 0u64;
			let (cw, ch) = image_layout.chroma_extent();
			let (ecw, ech) = expected.chroma_extent;
			for y in 0..image_extent.1 {
				for x in 0..image_extent.0 {
					let got = actual.luma[(y * image_extent.0 + x) as usize];
					if x < extent.0 && y < extent.1 {
						let want = expected.luma[(y * extent.0 + x) as usize];
						mismatches += u64::from(want.abs_diff(got >> if sentinel_sample > 0xff { 6 } else { 0 }) > 1);
					} else {
						sentinel_overwritten += u64::from(got != sentinel_sample);
					}
				}
			}
			for y in 0..ch {
				for x in 0..cw {
					let got = actual.chroma[(y * cw + x) as usize];
					if x < ecw && y < ech {
						let want = expected.chroma[(y * ecw + x) as usize];
						let shift = if sentinel_sample > 0xff { 6 } else { 0 };
						mismatches +=
							u64::from(want[0].abs_diff(got[0] >> shift) > 1 || want[1].abs_diff(got[1] >> shift) > 1);
					} else {
						sentinel_overwritten += u64::from(got != [sentinel_sample; 2]);
					}
				}
			}
			eprintln!("{label}: {mismatches} wrong samples, {sentinel_overwritten} padding samples overwritten");
			if mismatches != 0 || sentinel_overwritten != 0 {
				failures.push(format!(
					"{label}: {mismatches} wrong samples, {sentinel_overwritten} padding samples overwritten"
				));
			}
		}
	}

	/// Raw sample values (no 10-bit shift) of a layout's extent.
	fn decode_lenient(layout: &Layout, bytes: &[u8]) -> Planes {
		let component = layout.info.luma_bytes as usize;
		let sample = |offset: usize| {
			if component == 1 {
				u16::from(bytes[offset])
			} else {
				u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
			}
		};
		let (w, h) = layout.extent;
		let (cw, ch) = layout.chroma_extent();
		let mut planes = Planes::new((w, h), (cw, ch));
		for y in 0..h as usize {
			for x in 0..w as usize {
				planes.luma[y * w as usize + x] = sample(y * layout.luma_pitch as usize * 4 + x * component);
			}
		}
		for y in 0..ch as usize {
			let row = (layout.chroma_offset as usize + y * layout.chroma_pitch as usize) * 4;
			for x in 0..cw as usize {
				let pair = row + x * 2 * component;
				planes.chroma[y * cw as usize + x] = [sample(pair), sample(pair + component)];
			}
		}
		planes
	}

	fn block_on<F: std::future::Future>(future: F) -> F::Output {
		tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap()
			.block_on(future)
	}

	/// One conventional encoder configuration.
	#[derive(Clone, Copy, Debug)]
	struct CodecCase {
		codec: Codec,
		depth: EncodeBitDepth,
		pixel_format: PixelFormat,
		output: ConvertFormat,
		demuxer: &'static str,
		/// ffmpeg raw pixel format of the decoded planes.
		pix_fmt: &'static str,
	}

	const CODEC_CASES: [CodecCase; 9] = [
		CodecCase {
			codec: Codec::H264,
			depth: EncodeBitDepth::Eight,
			pixel_format: PixelFormat::Yuv420,
			output: ConvertFormat::Nv12,
			demuxer: "h264",
			pix_fmt: "yuv420p",
		},
		CodecCase {
			codec: Codec::H264,
			depth: EncodeBitDepth::Eight,
			pixel_format: PixelFormat::Yuv444,
			output: ConvertFormat::Yuv444,
			demuxer: "h264",
			pix_fmt: "yuv444p",
		},
		CodecCase {
			codec: Codec::H265,
			depth: EncodeBitDepth::Eight,
			pixel_format: PixelFormat::Yuv420,
			output: ConvertFormat::Nv12,
			demuxer: "hevc",
			pix_fmt: "yuv420p",
		},
		CodecCase {
			codec: Codec::H265,
			depth: EncodeBitDepth::Ten,
			pixel_format: PixelFormat::Yuv420,
			output: ConvertFormat::P010,
			demuxer: "hevc",
			pix_fmt: "yuv420p10le",
		},
		CodecCase {
			codec: Codec::H265,
			depth: EncodeBitDepth::Eight,
			pixel_format: PixelFormat::Yuv444,
			output: ConvertFormat::Yuv444,
			demuxer: "hevc",
			pix_fmt: "yuv444p",
		},
		CodecCase {
			codec: Codec::H265,
			depth: EncodeBitDepth::Ten,
			pixel_format: PixelFormat::Yuv444,
			output: ConvertFormat::Yuv444P10,
			demuxer: "hevc",
			pix_fmt: "yuv444p10le",
		},
		CodecCase {
			codec: Codec::AV1,
			depth: EncodeBitDepth::Eight,
			pixel_format: PixelFormat::Yuv420,
			output: ConvertFormat::Nv12,
			demuxer: "obu",
			pix_fmt: "yuv420p",
		},
		CodecCase {
			codec: Codec::AV1,
			depth: EncodeBitDepth::Ten,
			pixel_format: PixelFormat::Yuv420,
			output: ConvertFormat::P010,
			demuxer: "obu",
			pix_fmt: "yuv420p10le",
		},
		CodecCase {
			codec: Codec::AV1,
			depth: EncodeBitDepth::Ten,
			pixel_format: PixelFormat::Yuv444,
			output: ConvertFormat::Yuv444P10,
			demuxer: "obu",
			pix_fmt: "yuv444p10le",
		},
	];

	fn codec_context(codec: Codec) -> Option<VideoContext> {
		let context = context();
		context.supports_encode(codec).then_some(context)
	}

	fn encoder(case: CodecCase, (w, h): (u32, u32)) -> Option<(VideoContext, Encoder)> {
		encoder_on(codec_context(case.codec)?, case, (w, h), false)
	}

	/// An encoder on `context`, if the GPU encodes this case at this extent.
	fn encoder_on(
		context: VideoContext,
		case: CodecCase,
		(w, h): (u32, u32),
		stream: bool,
	) -> Option<(VideoContext, Encoder)> {
		let config = encoder_config(case, (w, h), stream);
		let caps = EncoderCapabilities::query(&context, &config).ok()?;
		check_frame(&caps, case.codec, w, h).ok()?;
		let encoder = Encoder::new(context.clone(), config)
			.map_err(|error| {
				eprintln!(
					"{:?} {:?} {:?} {w}x{h}: encoder creation failed: {error}",
					case.codec, case.pixel_format, case.depth
				);
			})
			.ok()?;
		assert_eq!(
			encoder.input_image_extent(),
			caps.input_image_extent(case.codec, w, h).unwrap()
		);
		assert_eq!(encoder.coded_extent(), caps.coded_extent(case.codec, w, h).unwrap());
		let conversion = conversion_extent(case.codec, w, h).unwrap();
		let input = encoder.input_image_extent();
		assert!(conversion.0 <= input.width && conversion.1 <= input.height);

		Some((context, encoder))
	}

	fn encoder_config(case: CodecCase, (w, h): (u32, u32), stream: bool) -> EncodeConfig {
		let config = match case.codec {
			Codec::H264 => EncodeConfig::h264(w, h),
			Codec::H265 => EncodeConfig::h265(w, h),
			Codec::AV1 => EncodeConfig::av1(w, h),
		}
		.with_pixel_format(case.pixel_format)
		.with_bit_depth(case.depth)
		.with_color_description(pixelforge::ColorDescription::bt709().with_full_range(stream))
		.with_rate_control(RateControlMode::Cqp)
		.with_quality_level(12)
		.with_gop_size(0)
		.with_b_frames(0);
		if stream {
			config
				.with_encoder_tuning_mode(pixelforge::EncoderTuningMode::LowLatency)
				.with_rate_control(RateControlMode::Cbr)
				// RTX 3080 H.264 caps maxBitrate at 120 Mbps.
				.with_target_bitrate(100_000_000)
				.with_frame_rate(116, 1)
				.with_max_reference_frames(1)
				.with_virtual_buffer_size_ms(1000 / 116)
				.with_initial_virtual_buffer_size_ms(0)
		} else {
			config
		}
	}

	fn video_format(codec: Codec) -> crate::session::stream::video::VideoFormat {
		use crate::session::stream::video::VideoFormat;
		match codec {
			Codec::H264 => VideoFormat::H264,
			Codec::H265 => VideoFormat::Hevc,
			Codec::AV1 => VideoFormat::Av1,
		}
	}

	/// Planar Y, Cb and Cr samples of one decoded frame.
	type DecodedFrame = [Vec<u16>; 3];

	/// Decode a raw elementary stream; returns (width, height, frames).
	fn decode(stream: &[u8], case: CodecCase) -> Result<(u32, u32, Vec<DecodedFrame>), String> {
		// Unique per call: tests run in parallel within one process.
		static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
		let path = std::env::temp_dir().join(format!(
			"pyroshine-convert-{}-{}.{}",
			std::process::id(),
			NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
			case.demuxer
		));
		std::fs::write(&path, stream).unwrap();
		let probe = std::process::Command::new("ffprobe")
			.args(["-v", "error", "-f", case.demuxer, "-select_streams", "v:0"])
			.args(["-show_entries", "stream=width,height", "-of", "csv=p=0"])
			.arg(&path)
			.output()
			.map_err(|e| format!("ffprobe: {e}"))?;
		let dims = String::from_utf8_lossy(&probe.stdout);
		let mut dims = dims.trim().split(',').map(|v| v.parse::<u32>());
		let (Some(Ok(w)), Some(Ok(h))) = (dims.next(), dims.next()) else {
			return Err(format!("ffprobe: {}", String::from_utf8_lossy(&probe.stderr)));
		};
		let output = std::process::Command::new("ffmpeg")
			.args(["-hide_banner", "-loglevel", "error", "-f", case.demuxer, "-i"])
			.arg(&path)
			.args([
				"-fps_mode",
				"passthrough",
				"-f",
				"rawvideo",
				"-pix_fmt",
				case.pix_fmt,
				"-",
			])
			.output();
		let _ = std::fs::remove_file(&path);
		let output = output.map_err(|e| format!("ffmpeg: {e}"))?;
		if !output.status.success() {
			return Err(format!("ffmpeg: {}", String::from_utf8_lossy(&output.stderr)));
		}
		let ten = case.pix_fmt.ends_with("10le");
		let samples: Vec<u16> = if ten {
			output
				.stdout
				.as_chunks::<2>()
				.0
				.iter()
				.map(|c| u16::from_le_bytes(*c))
				.collect()
		} else {
			output.stdout.iter().map(|&b| u16::from(b)).collect()
		};
		let (cw, ch) = if case.pix_fmt.starts_with("yuv420") {
			(w.div_ceil(2), h.div_ceil(2))
		} else {
			(w, h)
		};
		let luma = (w * h) as usize;
		let chroma = (cw * ch) as usize;
		let frames = samples
			.chunks_exact(luma + 2 * chroma)
			.map(|f| {
				[
					f[..luma].to_vec(),
					f[luma..luma + chroma].to_vec(),
					f[luma + chroma..].to_vec(),
				]
			})
			.collect();
		Ok((w, h, frames))
	}

	/// Smooth, encodable picture with strong colors: gradients plus color
	/// bars whose edges fall on odd columns.
	fn smooth(frame: (u32, u32)) -> impl Fn(u32, u32) -> [f32; 3] {
		move |x, y| {
			let (u, v) = (x as f32 / frame.0 as f32, y as f32 / frame.1 as f32);
			match (x * 7 / frame.0.max(1)) % 4 {
				0 => [u, v, 1.0 - u],
				1 => [0.9, 0.2 + 0.6 * v, 0.1],
				2 => [0.1, 0.8, 0.3 * u],
				_ => [0.5 * v, 0.1, 0.9],
			}
		}
	}

	pub(super) fn encode_decode_matrix(stream_config: bool) {
		let mut failures = Vec::new();
		let mut checked = 0;
		// Production shares one capture device across codecs and stream epochs.
		// Avoid repeated instance/device creation masking encoder coverage.
		let capture_context = context();
		let frame_count = if stream_config { 12 } else { 3 };
		let color = Color {
			full_range: stream_config,
			..BT709_LIMITED
		};
		// `MOONSHINE_TEST_E2E_FRAMES="1344x768,1408x768"` replaces the extents.
		let frames: Vec<(u32, u32)> = match std::env::var("MOONSHINE_TEST_E2E_FRAMES") {
			Ok(list) => list
				.split(',')
				.filter_map(|f| f.split_once('x'))
				.map(|(w, h)| (w.trim().parse().unwrap(), h.trim().parse().unwrap()))
				.collect(),
			// 1440 and 2400 (width % 64 == 32) caught NVIDIA reading 8-bit 4:4:4
			// chroma with the wrong pitch (vendor/pixelforge-patches/0001).
			Err(_) => vec![
				(1920, 1080),
				(1360, 768),
				(1440, 768),
				(2400, 1080),
				(1366, 768),
				(1280, 720),
				(2880, 1920),
				(3, 5),
				(1365, 767),
				(2, 2),
				(64, 48),
			],
		};
		for case in CODEC_CASES {
			for &frame in &frames {
				let odd = frame.0 % 2 != 0 || frame.1 % 2 != 0;
				let label = format!(
					"{:?} {:?} {:?} {}x{}",
					case.codec, case.pixel_format, case.depth, frame.0, frame.1
				);
				if frame == (1920, 1080)
					&& let Some(caps) =
						EncoderCapabilities::query(&capture_context, &encoder_config(case, frame, stream_config)).ok()
				{
					eprintln!(
						"{:?} {:?} {:?} capabilities: {caps:?}",
						case.codec, case.pixel_format, case.depth
					);
				}
				// `MOONSHINE_TEST_E2E_FILTER="AV1 Yuv420 Eight 1366x768"` reproduces one case.
				if std::env::var("MOONSHINE_TEST_E2E_FILTER").is_ok_and(|filter| !label.contains(&filter)) {
					continue;
				}
				if odd && case.pixel_format == PixelFormat::Yuv420 && case.codec != Codec::AV1 {
					// Not representable: H.264/HEVC crop 4:2:0 in 2-sample units
					// (the stream negotiation rejects these).
					continue;
				}
				if !capture_context.supports_encode(case.codec) {
					eprintln!("{label}: not encoded by this GPU; skipped");
					continue;
				};
				let context = capture_context.clone();
				let Ok(caps) = EncoderCapabilities::query(&context, &encoder_config(case, frame, stream_config)) else {
					eprintln!("{label}: not encoded by this GPU; skipped");
					continue;
				};
				// The stream setup's capability check: frames below the minimum
				// coded extent get a precise error (encoding them can hang the GPU).
				if let Err(e) = check_frame(&caps, case.codec, frame.0, frame.1) {
					eprintln!("{label}: rejected as the stream setup does: {e}");
					continue;
				}
				let Some((context, mut encoder)) = encoder_on(context, case, frame, stream_config) else {
					eprintln!("{label}: not encoded by this GPU; skipped");
					continue;
				};
				checked += 1;
				let rgb = smooth(frame);
				let bytes: Vec<u8> = (0..frame.1)
					.flat_map(|y| (0..frame.0).map(move |x| (x, y)))
					.flat_map(|(x, y)| rgb(x, y).map(|c| (c * 255.0).round() as u8).into_iter().chain([255]))
					.collect();
				let decoded_rgb = |x: u32, y: u32| {
					let offset = (y as usize * frame.0 as usize + x as usize) * 4;
					decode_texel(vk::Format::R8G8B8A8_UNORM, &bytes[offset..offset + 4])
				};
				let source = source_image(&context, frame, vk::Format::R8G8B8A8_UNORM, &bytes);
				let extent = conversion_extent(case.codec, frame.0, frame.1).unwrap();
				let mut converter = InputConverter::new(
					context.clone(),
					ConversionQueueMode::Auto,
					frame,
					extent,
					case.output,
					4,
					ColorSpace::Bt709,
					color.full_range,
				)
				.unwrap();
				let mut stream = Vec::new();
				let mut padding_bytes = 0;
				for _ in 0..frame_count {
					converter
						.convert(
							&source,
							vk::Format::R8G8B8A8_UNORM,
							false,
							false,
							&[None, None],
							Target::encoder_input(encoder.input_image()),
						)
						.unwrap();
					let image = encoder.input_image();
					let mut data = block_on(encoder.encode(image).unwrap()).unwrap().data;
					// Decode what production transmits.
					padding_bytes +=
						crate::session::stream::video::pipeline::padding::strip(&mut data, video_format(case.codec));
					stream.extend(data);
				}
				if padding_bytes > 0 {
					eprintln!("{label}: removed {padding_bytes} bytes of rate-control padding");
				}
				if let Some(directory) = std::env::var_os("MOONSHINE_TEST_E2E_SAVE_DIR") {
					std::fs::create_dir_all(&directory).unwrap();
					let path =
						std::path::Path::new(&directory).join(format!("{}.{}", label.replace(' ', "-"), case.demuxer));
					std::fs::write(path, &stream).unwrap();
				}
				let (w, h, frames) = match decode(&stream, case) {
					Ok(decoded) => decoded,
					Err(e) => {
						failures.push(format!("{label}: {e}"));
						continue;
					},
				};
				// H.264/HEVC crop to the frame exactly. AV1 frames are coded on
				// the implementation's picture grid (render_size carries the
				// requested size); the extra columns/rows hold replicated edges.
				let coded = match case.codec {
					Codec::AV1 => {
						let extent = caps.av1_frame_extent(frame.0, frame.1).unwrap();
						(extent.width, extent.height)
					},
					_ => frame,
				};
				if (w, h) != coded || frames.len() != frame_count {
					failures.push(format!(
						"{label}: decoded {} frames of {w}x{h}, expected {coded:?}",
						frames.len()
					));
					continue;
				}
				let expected = reference::convert(case.output, color, frame, coded, &decoded_rgb, &|_| true);
				compare_decoded(
					&label,
					&expected,
					&frames,
					coded,
					case.depth == EncodeBitDepth::Ten,
					&mut failures,
				);
			}
		}
		assert!(checked > 0, "no conventional codec could be checked");
		assert!(failures.is_empty(), "encode/decode mismatches: {failures:#?}");
	}

	/// Native (VA-API or NVENC) counterpart of [`encode_decode_matrix`] with
	/// the stream configuration: CBR, full range, one IDR request at frame 4
	/// and a reference invalidation at frame 8.
	pub(super) fn native_encode_decode_matrix(backend: EncoderBackend) {
		use crate::session::stream::video::pipeline::backend::{ConventionalEncoder, EncoderInput, NativeFormat};
		use crate::session::stream::video::pipeline::{nvenc, vaapi};
		use crate::session::stream::video::{BitDepth, ChromaFormat, VideoCodec};

		const FRAMES: usize = 12;
		let mut failures = Vec::new();
		let mut checked = 0;
		let capture_context = context();
		eprintln!("{} on {}", backend.label(), device_name(&capture_context));
		let color = Color {
			full_range: true,
			..BT709_LIMITED
		};
		let frames: Vec<(u32, u32)> = match std::env::var("MOONSHINE_TEST_E2E_FRAMES") {
			Ok(list) => list
				.split(',')
				.filter_map(|f| f.split_once('x'))
				.map(|(w, h)| (w.trim().parse().unwrap(), h.trim().parse().unwrap()))
				.collect(),
			Err(_) => vec![
				(1920, 1080),
				(1366, 768),
				(1440, 768),
				(1280, 720),
				(2880, 1920),
				(256, 144),
			],
		};
		let profiles = [
			(VideoCodec::H264, ChromaFormat::Yuv420, BitDepth::Eight),
			(VideoCodec::H264, ChromaFormat::Yuv444, BitDepth::Eight),
			(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Eight),
			(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Ten),
			(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Eight),
			(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Ten),
			(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Eight),
			(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Ten),
		];
		for (codec, chroma, depth) in profiles {
			let probe = match backend {
				EncoderBackend::Vaapi => vaapi::probe(&capture_context, codec, chroma, depth),
				EncoderBackend::Nvenc => nvenc::probe(&capture_context, codec, chroma, depth),
				EncoderBackend::Vulkan => unreachable!("covered by encode_decode_matrix"),
			};
			let profile = format!("{} {codec:?} {chroma:?} {depth:?}", backend.label());
			if let Err(error) = probe {
				eprintln!("{profile}: not encoded ({error}); skipped");
				continue;
			}
			let case = CODEC_CASES
				.into_iter()
				.find(|case| {
					let pixel_format = match chroma {
						ChromaFormat::Yuv420 => PixelFormat::Yuv420,
						ChromaFormat::Yuv444 => PixelFormat::Yuv444,
					};
					let bits = match depth {
						BitDepth::Eight => EncodeBitDepth::Eight,
						BitDepth::Ten => EncodeBitDepth::Ten,
					};
					let case_codec = match case.codec {
						Codec::H264 => VideoCodec::H264,
						Codec::H265 => VideoCodec::Hevc,
						Codec::AV1 => VideoCodec::Av1,
					};
					case_codec == codec && case.pixel_format == pixel_format && case.depth == bits
				})
				.expect("a decoder case for every native profile");
			for &frame in &frames {
				let label = format!("{profile} {}x{}", frame.0, frame.1);
				if std::env::var("MOONSHINE_TEST_E2E_FILTER").is_ok_and(|filter| !label.contains(&filter)) {
					continue;
				}
				let format = NativeFormat {
					codec,
					chroma,
					bit_depth: depth,
					width: frame.0,
					height: frame.1,
					fps: 60,
					bitrate: 50_000_000,
					color: pixelforge::ColorDescription::bt709().with_full_range(true),
				};
				let created = match backend {
					EncoderBackend::Vaapi => vaapi::VaapiEncoder::new(&capture_context, format).map(|encoder| {
						let (format, extent) = encoder.conversion();
						(ConventionalEncoder::Vaapi(encoder), format, extent, 4)
					}),
					_ => nvenc::NvencEncoder::new(&capture_context, format).map(|encoder| {
						let (format, extent, alignment) = encoder.conversion();
						(ConventionalEncoder::Nvenc(encoder), format, extent, alignment)
					}),
				};
				let (mut encoder, output, extent, alignment) = match created {
					Ok(created) => created,
					// The probe accepted the profile: a stream-sized encoder must work.
					Err(error) if frame == (1920, 1080) => {
						failures.push(format!("{label}: encoder creation failed: {error}"));
						continue;
					},
					Err(error) => {
						eprintln!("{label}: encoder creation failed ({error}); skipped");
						continue;
					},
				};
				checked += 1;
				let rgb = smooth(frame);
				let bytes: Vec<u8> = (0..frame.1)
					.flat_map(|y| (0..frame.0).map(move |x| (x, y)))
					.flat_map(|(x, y)| rgb(x, y).map(|c| (c * 255.0).round() as u8).into_iter().chain([255]))
					.collect();
				let decoded_rgb = |x: u32, y: u32| {
					let offset = (y as usize * frame.0 as usize + x as usize) * 4;
					decode_texel(vk::Format::R8G8B8A8_UNORM, &bytes[offset..offset + 4])
				};
				let source = source_image(&capture_context, frame, vk::Format::R8G8B8A8_UNORM, &bytes);
				let mut converter = InputConverter::new(
					capture_context.clone(),
					ConversionQueueMode::Auto,
					frame,
					extent,
					output,
					alignment,
					ColorSpace::Bt709,
					color.full_range,
				)
				.unwrap();
				let mut stream = Vec::new();
				let mut keys = Vec::new();
				let mut error = None;
				for index in 0..FRAMES {
					match index {
						4 => encoder.request_idr(),
						8 => encoder.invalidate_reference_frames(6),
						_ => {},
					}
					let packet = encoder.input_target().map_err(|e| e.to_string()).and_then(|target| {
						converter
							.convert(&source, vk::Format::R8G8B8A8_UNORM, false, false, &[None, None], target)
							.map_err(|e| format!("convert: {e:?}"))?;
						let future = encoder.encode(target).map_err(|e| e.to_string())?;
						block_on(future).map_err(|e| e.to_string())
					});
					match packet {
						Ok(mut packet) => {
							crate::session::stream::video::pipeline::padding::strip(
								&mut packet.data,
								video_format(case.codec),
							);
							keys.push(packet.is_key_frame);
							stream.extend(packet.data);
						},
						Err(e) => {
							error = Some(format!("{label} frame {index}: {e}"));
							break;
						},
					}
				}
				drop(converter);
				drop(encoder);
				if let Some(error) = error {
					failures.push(error);
					continue;
				}
				if !keys[0] || !keys[4] || keys[1..4].iter().any(|&key| key) {
					failures.push(format!("{label}: key frames {keys:?}, expected frames 0 and 4"));
				}
				let key_frames: Vec<usize> = (0..keys.len()).filter(|&i| keys[i]).collect();
				eprintln!("{label}: {} bytes, key frames {key_frames:?}", stream.len());
				if let Some(directory) = std::env::var_os("MOONSHINE_TEST_E2E_SAVE_DIR") {
					std::fs::create_dir_all(&directory).unwrap();
					let path =
						std::path::Path::new(&directory).join(format!("{}.{}", label.replace(' ', "-"), case.demuxer));
					std::fs::write(path, &stream).unwrap();
				}
				let (w, h, decoded) = match decode(&stream, case) {
					Ok(decoded) => decoded,
					Err(e) => {
						failures.push(format!("{label}: {e}"));
						continue;
					},
				};
				if (w, h) != frame || decoded.len() != FRAMES {
					failures.push(format!(
						"{label}: decoded {} frames of {w}x{h}, expected {FRAMES} of {frame:?}",
						decoded.len()
					));
					continue;
				}
				let expected = reference::convert(output, color, frame, frame, &decoded_rgb, &|_| true);
				compare_decoded(
					&label,
					&expected,
					&decoded,
					frame,
					depth == BitDepth::Ten,
					&mut failures,
				);
			}
		}
		assert!(checked > 0, "{} encoded no profile on this GPU", backend.label());
		assert!(failures.is_empty(), "encode/decode mismatches: {failures:#?}");
	}

	/// Compare decoded frames with the reference planes: lossy coding error
	/// only (see the thresholds below), per plane and frame.
	fn compare_decoded(
		label: &str,
		expected: &Planes,
		frames: &[DecodedFrame],
		coded: (u32, u32),
		ten: bool,
		failures: &mut Vec<String>,
	) {
		let expected_planes = [
			expected.luma.clone(),
			expected.chroma.iter().map(|c| c[0]).collect::<Vec<_>>(),
			expected.chroma.iter().map(|c| c[1]).collect::<Vec<_>>(),
		];
		let scale = if ten { 4.0 } else { 1.0 };
		for (index, decoded) in frames.iter().enumerate() {
			let mut line = format!("{label} frame {index}:");
			let mut failed = false;
			for plane in 0..3 {
				let (want, got) = (&expected_planes[plane], &decoded[plane]);
				if want.len() != got.len() {
					failures.push(format!(
						"{label}: plane {plane} has {} samples, expected {}",
						got.len(),
						want.len()
					));
					continue;
				}
				let n = want.len() as f64;
				let mean = |v: &[u16]| v.iter().map(|&s| f64::from(s)).sum::<f64>() / n;
				let errors = || want.iter().zip(got).map(|(&a, &b)| f64::from(a.abs_diff(b)));
				let mae = errors().sum::<f64>() / n;
				let (worst, max) = errors()
					.enumerate()
					.fold((0, 0.0), |best, (i, e)| if e > best.1 { (i, e) } else { best });
				let width = if want.len() == expected_planes[0].len() {
					coded.0
				} else {
					coded.0.div_ceil(2)
				} as usize;
				let bias = mean(got) - mean(want);
				line += &format!(
					" {} mae {:.2} max {:.0} at {:?} bias {:+.2};",
					["Y", "Cb", "Cr"][plane],
					mae / scale,
					max / scale,
					(worst % width, worst / width),
					bias / scale
				);
				// Lossy coding error only: a green tint shifts mean chroma
				// by tens of codes, a layout defect raises the MAE, and a
				// corrupted block or an unencoded padding row is off by
				// ~200 codes in a few samples (lossy coding stays within
				// ~32 codes at color-bar edges). The mean is only
				// meaningful over enough samples: below one 8x8 transform
				// block (a 2x2 4:2:0 frame has one chroma sample) it is a
				// single DC quantization error, up to 8 codes at the rate
				// controller's starting QP of 24 (AMD H.264).
				let wrong =
					max / scale > 64.0 || (n >= 64.0 && mae / scale > 2.5) || (n >= 4096.0 && bias.abs() / scale > 1.0);
				if wrong {
					failed = true;
					if plane > 0 && index == 0 {
						let dims = if want.len() == expected_planes[0].len() {
							coded
						} else {
							(coded.0.div_ceil(2), coded.1.div_ceil(2))
						};
						eprintln!("{label}: {}", diagnose(want, got, &expected_planes[0], dims));
					}
				}
			}
			if failed {
				failures.push(line.clone());
			}
			eprintln!("{line}");
		}
	}

	/// Locate a wrong decoded plane: the best integer shift against the
	/// expected plane, and its error against the luma plane.
	fn diagnose(want: &[u16], got: &[u16], luma: &[u16], (w, h): (u32, u32)) -> String {
		let (w, h) = (w as i64, h as i64);
		let mae = |dx: i64, dy: i64, reference: &[u16]| {
			let (mut sum, mut n) = (0u64, 0u64);
			for y in (h / 8..h - h / 8).step_by(3) {
				for x in (w / 8..w - w / 8).step_by(3) {
					let (sx, sy) = (x + dx, y + dy);
					if sx < 0 || sy < 0 || sx >= w || sy >= h {
						continue;
					}
					sum += u64::from(reference[(sy * w + sx) as usize].abs_diff(got[(y * w + x) as usize]));
					n += 1;
				}
			}
			if n == 0 { f64::INFINITY } else { sum as f64 / n as f64 }
		};
		let mut best = (f64::MAX, 0, 0);
		for dy in -4..=4 {
			for dx in -24..=24 {
				let error = mae(dx, dy, want);
				if error < best.0 {
					best = (error, dx, dy);
				}
			}
		}
		let row_samples = 8.min(w - w / 2) as usize;
		let rows: Vec<String> = (0..4.min(h - h / 2))
			.map(|y| {
				let y = (h / 2 + y) * w + w / 2;
				format!(
					"{:?} vs {:?}",
					&got[y as usize..y as usize + row_samples],
					&want[y as usize..y as usize + row_samples]
				)
			})
			.collect();
		let versus_luma = if luma.len() == want.len() {
			format!("{:.2}", mae(0, 0, luma))
		} else {
			"-".to_string()
		};
		format!(
			"best shift ({}, {}) mae {:.2}; mae vs luma {versus_luma}; centre rows got vs want: {}",
			best.1,
			best.2,
			best.0,
			rows.join(" | ")
		)
	}

	/// Encoder for the slot/recovery fixtures, or `None` when the GPU does
	/// not encode this configuration.
	fn recovery_encoder(case: CodecCase) -> Option<(VideoContext, Encoder, (u32, u32))> {
		let size = (256u32, 128u32);
		let (context, encoder) = encoder(case, size)?;
		Some((context, encoder, size))
	}

	pub(super) fn slot_rotation() {
		let (_context, mut encoder, _) = recovery_encoder(CODEC_CASES[2]).expect("HEVC encoder");
		let mut inputs = vec![encoder.input_image()];
		for _ in 0..4 {
			let image = encoder.input_image();
			block_on(encoder.encode(image).unwrap()).unwrap();
			inputs.push(encoder.input_image());
		}
		assert_ne!(inputs[0], inputs[1], "encode advances the input slot");
		assert_eq!(inputs[0], inputs[2], "the pinned encoder has two input slots");
		assert_eq!(inputs[1], inputs[3]);
		assert_eq!(inputs[2], inputs[4]);
	}

	fn mean(plane: &[u16]) -> f64 {
		plane.iter().map(|&v| f64::from(v)).sum::<f64>() / plane.len() as f64
	}

	pub(super) fn static_recovery_decode() {
		// Distinct grey levels; each run's last level differs from every slot
		// a stale recovery could read.
		const LEVELS: [u8; 5] = [200, 40, 120, 160, 80];
		let mut failures = Vec::new();
		let mut checked = 0;
		for (case, preceding) in [CODEC_CASES[0], CODEC_CASES[2], CODEC_CASES[3], CODEC_CASES[6]]
			.into_iter()
			.flat_map(|case| [1usize, 2, 5].map(|preceding| (case, preceding)))
		{
			let Some((context, encoder, (w, h))) = recovery_encoder(case) else {
				eprintln!("{case:?}: not encoded by this GPU; skipped");
				continue;
			};
			// Through the production encoder interface, as the loop uses it.
			let mut encoder = super::super::super::backend::ConventionalEncoder::Vulkan(encoder);
			checked += 1;
			let extent = conversion_extent(case.codec, w, h).unwrap();
			let mut converter = InputConverter::new(
				context.clone(),
				ConversionQueueMode::Auto,
				(w, h),
				extent,
				case.output,
				4,
				ColorSpace::Bt709,
				true,
			)
			.unwrap();
			let mut stream = Vec::new();
			for &level in &LEVELS[..preceding] {
				let pixels: Vec<u8> = (0..w * h).flat_map(|_| [level, level, level, 255]).collect();
				let source = source_image(&context, (w, h), vk::Format::R8G8B8A8_UNORM, &pixels);
				// The conventional loop's per-frame step.
				use super::super::super::backend::EncoderInput;
				let target = encoder.input_target().unwrap();
				converter
					.convert(&source, vk::Format::R8G8B8A8_UNORM, false, false, &[None, None], target)
					.unwrap();
				stream.extend(block_on(encoder.encode(target).unwrap()).unwrap().data);
			}
			// Static scene: two recovery keyframes in a row, no new conversion.
			for _ in 0..2 {
				encoder.request_idr();
				let restore = |_: &mut _, target| match converter.copy_last_output(target) {
					Ok(true) => Ok(()),
					Ok(false) => panic!("a conversion completed"),
					Err(e) => Err(super::super::super::backend::EncodeError::Vulkan(
						pixelforge::PixelForgeError::Vulkan(e),
					)),
				};
				let recovery =
					block_on(super::super::super::encode_static_recovery(&mut encoder, restore).unwrap()).unwrap();
				assert!(recovery.is_key_frame, "the recovery frame is an IDR");
				stream.extend(recovery.data);
			}
			let (_, _, frames) = decode(&stream, case).unwrap();
			assert_eq!(frames.len(), preceding + 2, "decoded frame count");
			let latest = mean(&frames[preceding - 1][0]);
			for recovered in [mean(&frames[preceding][0]), mean(&frames[preceding + 1][0])] {
				let line = format!(
					"{:?} {:?}: after {preceding} frame(s): latest scene luma {latest:.1}, recovery luma {recovered:.1}",
					case.codec, case.depth
				);
				eprintln!("{line}");
				let scale = if case.depth == EncodeBitDepth::Ten { 4.0 } else { 1.0 };
				if (latest - recovered).abs() > 4.0 * scale {
					failures.push(line);
				}
			}
		}
		assert!(checked > 0, "no conventional codec could be checked");
		assert!(
			failures.is_empty(),
			"review 2026-10-05 BUG-002: static recovery did not decode to the latest scene: {failures:#?}"
		);
	}

	/// Production-shaped timing: conversion into a real encoder input slot.
	/// The same frames and formats are measured by the pre-change converter
	/// for before/after comparisons (aligned extents only).
	pub(super) fn timing() {
		let mut rng = Rng(7);
		for (frame, codec) in [
			((1920u32, 1080u32), Codec::H265),
			((2560, 1440), Codec::H265),
			((3840, 2160), Codec::H265),
			((3440, 1440), Codec::H265),
			((1366, 768), Codec::H265),
			((1365, 767), Codec::AV1),
		] {
			for (format, depth) in [
				(ConvertFormat::Nv12, EncodeBitDepth::Eight),
				(ConvertFormat::P010, EncodeBitDepth::Ten),
			] {
				let case = CodecCase {
					codec,
					depth,
					pixel_format: PixelFormat::Yuv420,
					output: format,
					demuxer: "",
					pix_fmt: "",
				};
				let Some((context, encoder)) = encoder(case, frame) else {
					eprintln!(
						"{format:?} {codec:?} {}x{}: not encoded by this GPU; skipped",
						frame.0, frame.1
					);
					continue;
				};
				let bytes = pattern(&mut rng, vk::Format::R8G8B8A8_UNORM, frame);
				let source = source_image(&context, frame, vk::Format::R8G8B8A8_UNORM, &bytes);
				let extent = conversion_extent(codec, frame.0, frame.1).unwrap();
				let target = Target::encoder_input(encoder.input_image());
				for mode in [ConversionQueueMode::Graphics, ConversionQueueMode::Auto] {
					let mut converter = InputConverter::new(
						context.clone(),
						mode,
						frame,
						extent,
						format,
						4,
						ColorSpace::Bt709,
						false,
					)
					.unwrap();
					for _ in 0..20 {
						converter
							.convert(&source, vk::Format::R8G8B8A8_UNORM, false, false, &[None, None], target)
							.unwrap();
					}
					const N: u32 = 300;
					let (mut wall_ns, mut gpu_ns) = (0u128, 0u64);
					for _ in 0..N {
						let t = std::time::Instant::now();
						gpu_ns += converter
							.convert(&source, vk::Format::R8G8B8A8_UNORM, false, false, &[None, None], target)
							.unwrap()
							.gpu_ns
							.unwrap_or(0);
						wall_ns += t.elapsed().as_nanos();
					}
					eprintln!(
						"TIMING {} {format:?} {}x{} (conversion {}x{}) {mode:?} async={}: {:.1} us GPU, {:.1} us wall",
						device_name(&context),
						frame.0,
						frame.1,
						extent.0,
						extent.1,
						converter.async_compute,
						gpu_ns as f64 / f64::from(N) / 1e3,
						wall_ns as f64 / f64::from(N) / 1e3
					);
				}
			}
		}
	}
}
