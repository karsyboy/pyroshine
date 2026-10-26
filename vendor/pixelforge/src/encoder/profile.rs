//! Codec profile selection.
//!
//! The encoders' `create` functions and [`VideoContext::supports_encode_format`](crate::VideoContext::supports_encode_format)
//! both choose their profile here, so the query answers for exactly the profile an
//! encoder would request.

use super::{BitDepth, PixelFormat};

/// H.264 profile for a pixel format: High 4:4:4 Predictive for 4:4:4, High otherwise.
pub(crate) fn h264_profile_idc(pixel_format: PixelFormat) -> u32 {
    match pixel_format {
        PixelFormat::Yuv444 => {
            ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
        }
        _ => ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
    }
}

/// H.265 profile for a pixel format and bit depth, or `None` for combinations the
/// encoder does not support (4:2:2).
pub(crate) fn h265_profile_idc(pixel_format: PixelFormat, bit_depth: BitDepth) -> Option<u32> {
    match (pixel_format, bit_depth) {
        (PixelFormat::Yuv420, BitDepth::Eight) => {
            Some(ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN)
        }
        (PixelFormat::Yuv420, BitDepth::Ten) => {
            Some(ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10)
        }
        (PixelFormat::Yuv444, _) => Some(
            ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
        ),
        _ => None,
    }
}

/// AV1 profile for a pixel format: Main is 4:2:0, High adds 4:4:4.
pub(crate) fn av1_profile(pixel_format: PixelFormat) -> u32 {
    match pixel_format {
        PixelFormat::Yuv420 => ash::vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
        _ => ash::vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH,
    }
}
