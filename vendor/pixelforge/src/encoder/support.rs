//! Device support queries for encode formats.

use super::codec::query_video_capabilities;
use super::profile::{av1_profile, h264_profile_idc, h265_profile_idc};
use super::{BitDepth, Codec, PixelFormat};
use crate::vulkan::VideoContext;
use ash::vk;
use ash::vk::TaggedStructure;
use tracing::{debug, warn};

impl VideoContext {
    /// Check whether the device can encode `codec` in the given pixel format and
    /// bit depth.
    ///
    /// [`supports_encode`](Self::supports_encode) only answers for 8-bit 4:2:0.
    /// Many devices lack other combinations; AMD's VCN, for instance, has no 4:4:4
    /// profiles. This asks the driver for the capabilities of the profile an
    /// [`Encoder`](crate::Encoder) would request, without creating an encoder.
    /// Resolution and rate control are not considered.
    pub fn supports_encode_format(
        &self,
        codec: Codec,
        pixel_format: PixelFormat,
        bit_depth: BitDepth,
    ) -> bool {
        if !self.supports_encode(codec) {
            return false;
        }

        let chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR = pixel_format.into();
        let component_bit_depth: vk::VideoComponentBitDepthFlagsKHR = bit_depth.into();
        let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();

        // Each arm keeps its profile and capability structs alive across the call.
        let result = match codec {
            Codec::H264 => {
                let mut profile = vk::VideoEncodeH264ProfileInfoKHR::default()
                    .std_profile_idc(h264_profile_idc(pixel_format));
                let profile_info = vk::VideoProfileInfoKHR::default()
                    .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                    .chroma_subsampling(chroma_subsampling)
                    .luma_bit_depth(component_bit_depth)
                    .chroma_bit_depth(component_bit_depth)
                    .push(&mut profile);
                let mut codec_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
                let mut caps = vk::VideoCapabilitiesKHR::default()
                    .push(&mut encode_caps)
                    .push(&mut codec_caps);
                query_video_capabilities(self, &profile_info, &mut caps)
            }
            Codec::H265 => {
                let Some(profile_idc) = h265_profile_idc(pixel_format, bit_depth) else {
                    debug!(
                        "{:?} encode {:?} {:?}: no profile",
                        codec, pixel_format, bit_depth
                    );
                    return false;
                };
                let mut profile =
                    vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(profile_idc);
                let profile_info = vk::VideoProfileInfoKHR::default()
                    .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                    .chroma_subsampling(chroma_subsampling)
                    .luma_bit_depth(component_bit_depth)
                    .chroma_bit_depth(component_bit_depth)
                    .push(&mut profile);
                let mut codec_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
                let mut caps = vk::VideoCapabilitiesKHR::default()
                    .push(&mut encode_caps)
                    .push(&mut codec_caps);
                query_video_capabilities(self, &profile_info, &mut caps)
            }
            Codec::AV1 => {
                let mut profile = vk::VideoEncodeAV1ProfileInfoKHR::default()
                    .std_profile(av1_profile(pixel_format));
                let profile_info = vk::VideoProfileInfoKHR::default()
                    .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_AV1)
                    .chroma_subsampling(chroma_subsampling)
                    .luma_bit_depth(component_bit_depth)
                    .chroma_bit_depth(component_bit_depth)
                    .push(&mut profile);
                let mut codec_caps = vk::VideoEncodeAV1CapabilitiesKHR::default();
                let mut caps = vk::VideoCapabilitiesKHR::default()
                    .push(&mut encode_caps)
                    .push(&mut codec_caps);
                query_video_capabilities(self, &profile_info, &mut caps)
            }
        };

        match result {
            vk::Result::SUCCESS => {
                debug!(
                    "{:?} encode {:?} {:?}: supported",
                    codec, pixel_format, bit_depth
                );
                true
            }
            vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR
            | vk::Result::ERROR_VIDEO_PROFILE_FORMAT_NOT_SUPPORTED_KHR
            | vk::Result::ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR => {
                debug!(
                    "{:?} encode {:?} {:?}: not supported ({:?})",
                    codec, pixel_format, bit_depth, result
                );
                false
            }
            err => {
                warn!(
                    "Failed to query {:?} encode {:?} {:?} capabilities: {:?}",
                    codec, pixel_format, bit_depth, err
                );
                false
            }
        }
    }
}
