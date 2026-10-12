//! Profile construction and frame/image extents shared by queries and encoders.
use ash::vk;
use ash::vk::TaggedStructure;

use super::codec::{DeviceVideoCaps, input_image_width, query_video_caps};
use super::resources::gcd;
use super::{BitDepth, Codec, EncodeConfig, PixelFormat};
use crate::VideoContext;
use crate::error::{PixelForgeError, Result};

/// Codec syntax block used when aligning coded pictures.
pub const fn codec_block_size(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => super::h264::MB_SIZE,
        Codec::H265 => super::h265::CTB_SIZE,
        Codec::AV1 => super::av1::SUPERBLOCK_SIZE,
    }
}

/// Device limits for the exact chroma, bit depth and usage of an encode profile.
#[derive(Clone, Copy, Debug)]
pub struct EncoderCapabilities {
    /// Minimum coded picture extent supported by the device.
    pub min_coded_extent: vk::Extent2D,
    /// Maximum coded picture extent supported by the device.
    pub max_coded_extent: vk::Extent2D,
    /// Device image-access granularity used along with the codec block size.
    pub picture_access_granularity: vk::Extent2D,
    /// AV1's picture grid. Other codecs crop directly to the requested frame.
    pub av1_coded_picture_alignment: Option<vk::Extent2D>,
}

impl EncoderCapabilities {
    /// Query without allocating a session or encoder images. Uses the same
    /// profile, usage hints and capability chain as encoder initialization.
    pub fn query(context: &VideoContext, config: &EncodeConfig) -> Result<Self> {
        let mut profile = EncodeProfile::new(config)?;
        let caps = query_video_caps(context, &profile.info())?;
        Ok(Self::from_caps(&caps))
    }

    pub(crate) fn from_caps(caps: &DeviceVideoCaps) -> Self {
        Self {
            min_coded_extent: caps.min_coded_extent,
            max_coded_extent: caps.max_coded_extent,
            picture_access_granularity: caps.picture_access_granularity,
            av1_coded_picture_alignment: caps.av1_coded_picture_alignment,
        }
    }

    /// Align to both codec blocks and device granularity, clamp to the minimum
    /// and re-align. Reject zero dimensions, overflow and extents above the max.
    pub fn coded_extent(&self, codec: Codec, width: u32, height: u32) -> Result<vk::Extent2D> {
        let align = |value: u32, granularity: u32, minimum: u32| {
            let block = codec_block_size(codec);
            let granularity = granularity.max(1);
            let unit = (block / gcd(block, granularity)).checked_mul(granularity)?;
            value
                .checked_next_multiple_of(unit)?
                .max(minimum)
                .checked_next_multiple_of(unit)
        };
        let extent = match (
            align(
                width,
                self.picture_access_granularity.width,
                self.min_coded_extent.width,
            ),
            align(
                height,
                self.picture_access_granularity.height,
                self.min_coded_extent.height,
            ),
        ) {
            (Some(w), Some(h))
                if width != 0
                    && height != 0
                    && w <= self.max_coded_extent.width
                    && h <= self.max_coded_extent.height =>
            {
                vk::Extent2D {
                    width: w,
                    height: h,
                }
            }
            _ => {
                return Err(PixelForgeError::InvalidInput(format!(
                    "{codec:?} on this GPU encodes frames of at most {}x{}; {width}x{height} has no valid coded extent",
                    self.max_coded_extent.width, self.max_coded_extent.height
                )));
            }
        };
        Ok(extent)
    }

    /// AV1 frame size (`max_frame_*` and every picture's `codedExtent`): the
    /// requested size on the implementation's coded picture grid, within the
    /// coded extent. `render_size` carries the requested size.
    ///
    /// Implementations code AV1 pictures on that grid regardless, but only
    /// read the source within `codedExtent` and fill the rest themselves:
    /// RADV (8x2 grid) coded a 1365x767 frame as 1368x768 with a black last
    /// row. Covering the whole grid encodes the caller's padding instead,
    /// which decoders that ignore `render_size` display.
    pub fn av1_frame_extent(&self, width: u32, height: u32) -> Result<vk::Extent2D> {
        let coded = self.coded_extent(Codec::AV1, width, height)?;
        let grid = self.av1_coded_picture_alignment.unwrap_or(vk::Extent2D {
            width: 1,
            height: 1,
        });
        let on_grid = |value: u32, unit: u32, limit: u32| {
            value
                .checked_next_multiple_of(unit.max(1))
                .map_or(limit, |aligned| aligned.min(limit))
        };
        Ok(vk::Extent2D {
            width: on_grid(width, grid.width, coded.width),
            height: on_grid(height, grid.height, coded.height),
        })
    }

    /// Allocated input-image extent, including the maintained NVIDIA width
    /// padding. This can be wider than the coded picture; it never exceeds max.
    pub fn input_image_extent(
        &self,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<vk::Extent2D> {
        let coded = self.coded_extent(codec, width, height)?;
        Ok(vk::Extent2D {
            width: input_image_width(coded.width, self.max_coded_extent.width),
            height: coded.height,
        })
    }
}

/// Own every structure referenced by the profile chain until the Vulkan call
/// or encoder initialization completes. Never expose pointers beyond this borrow.
pub(crate) struct EncodeProfile {
    codec: Codec,
    chroma: vk::VideoChromaSubsamplingFlagsKHR,
    depth: vk::VideoComponentBitDepthFlagsKHR,
    pub(crate) profile_idc: u32,
    h264: vk::VideoEncodeH264ProfileInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: vk::VideoEncodeAV1ProfileInfoKHR<'static>,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
}

impl EncodeProfile {
    pub(crate) fn new(config: &EncodeConfig) -> Result<Self> {
        let profile_idc = match (config.codec, config.pixel_format, config.bit_depth) {
            (Codec::H264, PixelFormat::Yuv444, _) => vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE,
            (Codec::H264, _, _) => vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
            (Codec::H265, PixelFormat::Yuv420, BitDepth::Eight) => vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            (Codec::H265, PixelFormat::Yuv420, BitDepth::Ten) => vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
            (Codec::H265, PixelFormat::Yuv444, _) => vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
            (Codec::H265, _, _) => return Err(PixelForgeError::InvalidInput(format!(
                "Unsupported pixel format / bit depth for H.265: {:?} / {:?}", config.pixel_format, config.bit_depth))),
            (Codec::AV1, PixelFormat::Yuv420, _) => vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            (Codec::AV1, _, _) => vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH,
        };
        Ok(Self {
            codec: config.codec,
            chroma: config.pixel_format.into(),
            depth: config.bit_depth.into(),
            profile_idc,
            h264: vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(profile_idc),
            h265: vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(profile_idc),
            av1: vk::VideoEncodeAV1ProfileInfoKHR::default().std_profile(profile_idc),
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(config.encode_usage_hint.into())
                .video_content_hints(config.encode_content_hint.into())
                .tuning_mode(config.encoder_tuning_mode.into()),
        })
    }

    pub(crate) fn info(&mut self) -> vk::VideoProfileInfoKHR<'_> {
        let info = vk::VideoProfileInfoKHR::default()
            .chroma_subsampling(self.chroma)
            .luma_bit_depth(self.depth)
            .chroma_bit_depth(self.depth);
        let info = match self.codec {
            Codec::H264 => info
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                .push(&mut self.h264),
            Codec::H265 => info
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                .push(&mut self.h265),
            Codec::AV1 => info
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_AV1)
                .push(&mut self.av1),
        };
        info.push(&mut self.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(minimum: (u32, u32), granularity: (u32, u32), max_width: u32) -> EncoderCapabilities {
        EncoderCapabilities {
            min_coded_extent: vk::Extent2D {
                width: minimum.0,
                height: minimum.1,
            },
            max_coded_extent: vk::Extent2D {
                width: max_width,
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
    fn coded_and_allocated_extents_are_distinct() {
        let limits = caps((1, 1), (1, 1), 8192);
        let coded = limits.coded_extent(Codec::H265, 1440, 1080).unwrap();
        let image = limits.input_image_extent(Codec::H265, 1440, 1080).unwrap();
        assert_eq!((coded.width, coded.height), (1440, 1088));
        assert_eq!((image.width, image.height), (1472, 1088));
        assert_eq!(
            limits
                .input_image_extent(Codec::H264, 1360, 768)
                .unwrap()
                .width,
            1408
        );
        assert_eq!(
            limits
                .input_image_extent(Codec::H265, 2400, 1080)
                .unwrap()
                .width,
            2432
        );
    }

    #[test]
    fn respects_minimum_granularity_and_maximum() {
        let limits = caps((1, 1), (64, 16), 8192);
        let extent = limits.coded_extent(Codec::H265, 130, 130).unwrap();
        assert_eq!((extent.width, extent.height), (192, 160));
        let extent = limits.input_image_extent(Codec::H264, 1920, 1080).unwrap();
        assert_eq!((extent.width, extent.height), (1920, 1088));
        let minimum = caps((128, 128), (16, 16), 8192);
        let extent = minimum.coded_extent(Codec::AV1, 3, 5).unwrap();
        assert_eq!((extent.width, extent.height), (128, 128));
        assert_eq!(
            caps((1, 1), (1, 1), 4080)
                .input_image_extent(Codec::H264, 4080, 1080)
                .unwrap()
                .width,
            4080
        );
    }

    #[test]
    fn av1_frames_cover_the_coded_picture_grid() {
        let mut limits = caps((1, 1), (1, 1), 8192);
        // Without a reported grid the frame keeps the requested size.
        let extent = limits.av1_frame_extent(1365, 767).unwrap();
        assert_eq!((extent.width, extent.height), (1365, 767));
        // RADV: 8x2.
        limits.av1_coded_picture_alignment = Some(vk::Extent2D {
            width: 8,
            height: 2,
        });
        let extent = limits.av1_frame_extent(1365, 767).unwrap();
        assert_eq!((extent.width, extent.height), (1368, 768));
        let extent = limits.av1_frame_extent(1366, 766).unwrap();
        assert_eq!((extent.width, extent.height), (1368, 766));
        let extent = limits.av1_frame_extent(1920, 1080).unwrap();
        assert_eq!((extent.width, extent.height), (1920, 1080));
        // Never beyond the coded extent (64x64 superblocks: 1408x768).
        limits.av1_coded_picture_alignment = Some(vk::Extent2D {
            width: 256,
            height: 256,
        });
        let extent = limits.av1_frame_extent(1366, 766).unwrap();
        assert_eq!((extent.width, extent.height), (1408, 768));
        assert!(limits.av1_frame_extent(0, 766).is_err());
    }

    #[test]
    fn rejects_zero_overflow_and_aligned_extent_past_maximum() {
        let limits = caps((1, 1), (1, 1), 8192);
        for (w, h) in [(0, 1), (1, 0), (u32::MAX, 1), (1, u32::MAX), (8193, 1)] {
            assert!(limits.coded_extent(Codec::H264, w, h).is_err());
        }
        assert!(
            caps((1, 1), (u32::MAX, 1), 8192)
                .coded_extent(Codec::H265, 1, 1)
                .is_err()
        );
    }

    #[test]
    fn profile_chain_preserves_chroma_depth_and_usage_hints() {
        for codec in [Codec::H264, Codec::H265, Codec::AV1] {
            for chroma in [PixelFormat::Yuv420, PixelFormat::Yuv444] {
                for depth in [BitDepth::Eight, BitDepth::Ten] {
                    let mut config = EncodeConfig::h264(1920, 1080);
                    config.codec = codec;
                    config.pixel_format = chroma;
                    config.bit_depth = depth;
                    config.encoder_tuning_mode = super::super::EncoderTuningMode::LowLatency;
                    let mut profile = EncodeProfile::new(&config).unwrap();
                    let info = profile.info();
                    assert_eq!(info.chroma_subsampling, chroma.into());
                    assert_eq!(info.luma_bit_depth, depth.into());
                    assert_eq!(info.chroma_bit_depth, depth.into());
                    // SAFETY: info borrows profile; these are the exact structures it owns.
                    let usage = unsafe { &*info.p_next.cast::<vk::VideoEncodeUsageInfoKHR>() };
                    assert_eq!(usage.video_usage_hints, config.encode_usage_hint.into());
                    assert_eq!(usage.video_content_hints, config.encode_content_hint.into());
                    assert_eq!(usage.tuning_mode, vk::VideoEncodeTuningModeKHR::LOW_LATENCY);
                    assert!(!usage.p_next.is_null());
                    let codec_info = unsafe { &*usage.p_next.cast::<vk::BaseInStructure>() };
                    let expected = match codec {
                        Codec::H264 => vk::StructureType::VIDEO_ENCODE_H264_PROFILE_INFO_KHR,
                        Codec::H265 => vk::StructureType::VIDEO_ENCODE_H265_PROFILE_INFO_KHR,
                        Codec::AV1 => vk::StructureType::VIDEO_ENCODE_AV1_PROFILE_INFO_KHR,
                    };
                    assert_eq!(codec_info.s_type, expected);
                }
            }
        }
    }
}
