//! Vulkan Video encode capabilities that bound the frame extents a
//! conventional stream may use.
//!
//! Pixelforge aligns the coded extent up to its codec block and the device's
//! picture access granularity and clamps it up to the device minimum, keeping
//! the requested frame size. H.264 and HEVC crop that frame out of the coded
//! picture, so any frame size works. An AV1 frame size is the coded size
//! itself, and one below the minimum coded extent is not supported: on RADV a
//! 3x5 AV1 frame times out the VCN ring. Stream setup checks frames against
//! the queried limits and reports a precise error instead.

use ash::vk;
use ash::vk::TaggedStructure;
use pixelforge::{Codec, EncodeBitDepth, PixelFormat, VideoContext};

/// Extent capabilities of the encode profile Pixelforge creates for a format.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EncodeCaps {
	pub min_coded_extent: vk::Extent2D,
	pub max_coded_extent: vk::Extent2D,
	pub picture_access_granularity: vk::Extent2D,
	/// AV1 only: the grid on which the implementation codes frames. Other
	/// sizes are coded rounded up and signal the requested size as the AV1
	/// render size.
	pub av1_coded_picture_alignment: Option<vk::Extent2D>,
}

fn gcd(a: u32, b: u32) -> u32 {
	if b == 0 { a } else { gcd(b, a % b) }
}

fn lcm(a: u32, b: u32) -> u32 {
	a / gcd(a, b) * b
}

/// Pixelforge's alignment unit for a codec (macroblock, CTB, superblock).
pub(crate) fn block_size(codec: Codec) -> u32 {
	match codec {
		Codec::H264 => pixelforge::encoder::h264::MB_SIZE,
		Codec::H265 => pixelforge::encoder::h265::CTB_SIZE,
		Codec::AV1 => pixelforge::encoder::av1::SUPERBLOCK_SIZE,
	}
}

impl EncodeCaps {
	/// Query the capabilities of Pixelforge's profile for this format (the
	/// same codec profile, chroma subsampling and bit depth it encodes with).
	pub(crate) fn query(
		context: &VideoContext,
		codec: Codec,
		pixel_format: PixelFormat,
		bit_depth: EncodeBitDepth,
	) -> Result<Self, String> {
		let subsampling = match pixel_format {
			PixelFormat::Yuv420 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
			PixelFormat::Yuv422 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_422,
			PixelFormat::Yuv444 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
		};
		let depth = match bit_depth {
			EncodeBitDepth::Eight => vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
			EncodeBitDepth::Ten => vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
		};
		let yuv444 = pixel_format == PixelFormat::Yuv444;
		let mut h264_profile = vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(if yuv444 {
			ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
		} else {
			ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
		});
		let mut h265_profile =
			vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(match (yuv444, bit_depth) {
				(true, _) => ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
				(false, EncodeBitDepth::Ten) => {
					ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
				},
				(false, EncodeBitDepth::Eight) => {
					ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
				},
			});
		let mut av1_profile = vk::VideoEncodeAV1ProfileInfoKHR::default().std_profile(if yuv444 {
			ash::vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH
		} else {
			ash::vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN
		});
		let profile = vk::VideoProfileInfoKHR::default()
			.chroma_subsampling(subsampling)
			.luma_bit_depth(depth)
			.chroma_bit_depth(depth);
		let profile = match codec {
			Codec::H264 => profile
				.video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
				.push(&mut h264_profile),
			Codec::H265 => profile
				.video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
				.push(&mut h265_profile),
			Codec::AV1 => profile
				.video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_AV1)
				.push(&mut av1_profile),
		};
		// Drivers require the codec-specific capability structure in the chain.
		let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
		let mut h264_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
		let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
		let mut av1_caps = vk::VideoEncodeAV1CapabilitiesKHR::default();
		let capabilities = vk::VideoCapabilitiesKHR::default().push(&mut encode_caps);
		let mut capabilities = match codec {
			Codec::H264 => capabilities.push(&mut h264_caps),
			Codec::H265 => capabilities.push(&mut h265_caps),
			Codec::AV1 => capabilities.push(&mut av1_caps),
		};
		let video_queue = ash::khr::video_queue::Instance::load(context.entry(), context.instance());
		// SAFETY: valid, fully initialized input and output chains that live
		// for the call; the physical device belongs to this instance.
		let result = unsafe {
			(video_queue.fp().get_physical_device_video_capabilities_khr)(
				context.physical_device(),
				&profile,
				&mut capabilities,
			)
		};
		if result != vk::Result::SUCCESS {
			return Err(format!(
				"{codec:?} {pixel_format:?} {bit_depth:?} encode capabilities unavailable: {result}"
			));
		}
		Ok(Self {
			min_coded_extent: capabilities.min_coded_extent,
			max_coded_extent: capabilities.max_coded_extent,
			picture_access_granularity: capabilities.picture_access_granularity,
			av1_coded_picture_alignment: (codec == Codec::AV1).then_some(av1_caps.coded_picture_alignment),
		})
	}

	/// The coded extent Pixelforge (v0.9) gives its input images for a frame:
	/// aligned to lcm(block, granularity), at least the minimum, re-aligned.
	pub(crate) fn input_image_extent(&self, codec: Codec, width: u32, height: u32) -> Option<(u32, u32)> {
		let align = |value: u32, unit: u32, min: u32| {
			let unit = lcm(block_size(codec), unit.max(1));
			value
				.checked_next_multiple_of(unit)?
				.max(min)
				.checked_next_multiple_of(unit)
		};
		Some((
			align(
				width,
				self.picture_access_granularity.width,
				self.min_coded_extent.width,
			)?,
			align(
				height,
				self.picture_access_granularity.height,
				self.min_coded_extent.height,
			)?,
		))
	}

	/// Whether a frame is within the coded extents this profile supports.
	pub(crate) fn check_frame(&self, codec: Codec, width: u32, height: u32) -> Result<(), String> {
		let (min, max) = (self.min_coded_extent, self.max_coded_extent);
		if codec == Codec::AV1 && (width < min.width || height < min.height) {
			return Err(format!(
				"{codec:?} on this GPU encodes frames of at least {}x{}; {width}x{height} is smaller",
				min.width, min.height
			));
		}
		match self.input_image_extent(codec, width, height) {
			Some((w, h)) if w <= max.width && h <= max.height => Ok(()),
			_ => Err(format!(
				"{codec:?} on this GPU encodes frames of at most {}x{}; {width}x{height} aligns past that",
				max.width, max.height
			)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn caps(min: (u32, u32), granularity: (u32, u32)) -> EncodeCaps {
		EncodeCaps {
			min_coded_extent: vk::Extent2D {
				width: min.0,
				height: min.1,
			},
			max_coded_extent: vk::Extent2D {
				width: 8192,
				height: 8192,
			},
			picture_access_granularity: vk::Extent2D {
				width: granularity.0,
				height: granularity.1,
			},
			av1_coded_picture_alignment: None,
		}
	}

	#[test]
	fn input_extent_follows_pixelforge_alignment() {
		// Same cases as Pixelforge's own alignment tests.
		assert_eq!(
			caps((1, 1), (1, 1)).input_image_extent(Codec::H264, 1920, 1080),
			Some((1920, 1088))
		);
		assert_eq!(
			caps((1, 1), (64, 16)).input_image_extent(Codec::H265, 130, 130),
			Some((192, 160))
		);
		assert_eq!(
			caps((128, 128), (16, 16)).input_image_extent(Codec::AV1, 3, 5),
			Some((128, 128))
		);
	}

	#[test]
	fn frames_outside_the_coded_extents_are_rejected() {
		let caps = caps((128, 128), (16, 16));
		assert!(
			caps.check_frame(Codec::AV1, 3, 5)
				.unwrap_err()
				.contains("at least 128x128")
		);
		assert!(caps.check_frame(Codec::AV1, 128, 128).is_ok());
		// H.264 and HEVC crop small frames out of a minimum-size coded picture.
		assert!(caps.check_frame(Codec::H264, 2, 2).is_ok());
		assert!(caps.check_frame(Codec::H265, 3, 5).is_ok());
		assert!(caps.check_frame(Codec::H264, 1920, 1080).is_ok());
		assert!(caps.check_frame(Codec::H264, 8192, 8192).is_ok());
		assert!(
			caps.check_frame(Codec::H264, 8192, 8193)
				.unwrap_err()
				.contains("at most")
		);
	}
}
