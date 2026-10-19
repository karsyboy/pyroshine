//! Vulkan Video encode capabilities that bound the frame extents a
//! conventional stream may use.
//!
//! Pixelforge aligns the coded extent up to its codec block and the device's
//! picture access granularity and clamps it up to the device minimum, keeping
//! the requested frame size. H.264 and HEVC crop that frame out of the coded
//! picture, so any frame size works. AV1 frames below the minimum coded
//! extent are not supported: on RADV a
//! 3x5 AV1 frame times out the VCN ring. Stream setup checks frames against
//! the queried limits and reports a precise error instead.
//!
//! Conventional streams always use constant-bitrate rate control, so a
//! profile without it is not usable (ANV's Gen12 encoder offers only disabled
//! rate control), and the encoder's target never exceeds the profile's
//! maximum bitrate (NVIDIA: 120 Mbps for H.264 and HEVC on an RTX 3080).

use pixelforge::{Codec, EncoderCapabilities};

/// Whether a frame is within the coded extents this profile supports.
pub(crate) fn check_frame(caps: &EncoderCapabilities, codec: Codec, width: u32, height: u32) -> Result<(), String> {
	let min = caps.min_coded_extent;
	if codec == Codec::AV1 && (width < min.width || height < min.height) {
		return Err(format!(
			"{codec:?} on this GPU encodes frames of at least {}x{}; {width}x{height} is smaller",
			min.width, min.height
		));
	}
	caps.coded_extent(codec, width, height)
		.map(|_| ())
		.map_err(|error| error.to_string())
}

/// The encoder's constant-bitrate target for a stream that requested
/// `requested` bps: the request, limited to the profile's maximum.
///
/// The request is a ceiling the client can receive, so encoding below it is
/// always valid; the stream simply uses less of it. Fails when the profile has
/// no CBR rate control.
pub(crate) fn cbr_target(caps: &EncoderCapabilities, codec: Codec, requested: u64) -> Result<u32, String> {
	if !caps
		.rate_control_modes
		.contains(ash::vk::VideoEncodeRateControlModeFlagsKHR::CBR)
	{
		return Err(format!(
			"{codec:?} on this GPU has no constant-bitrate rate control (supported: {:?})",
			caps.rate_control_modes
		));
	}
	let target = requested.min(caps.max_bitrate);
	u32::try_from(target).map_err(|_| format!("Bitrate {target} bps exceeds the encoder API range"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use ash::vk;

	fn caps(min: (u32, u32), granularity: (u32, u32)) -> EncoderCapabilities {
		EncoderCapabilities {
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
			rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR::DISABLED
				| vk::VideoEncodeRateControlModeFlagsKHR::CBR,
			max_bitrate: 120_000_000,
		}
	}

	#[test]
	fn cbr_targets_stay_within_the_profile_limit() {
		let caps = caps((128, 128), (16, 16));
		assert_eq!(cbr_target(&caps, Codec::H264, 50_000_000), Ok(50_000_000));
		assert_eq!(cbr_target(&caps, Codec::H264, 150_000_000), Ok(120_000_000));
		let unlimited = EncoderCapabilities {
			max_bitrate: u64::MAX,
			..caps
		};
		assert!(
			cbr_target(&unlimited, Codec::H265, 1 << 33)
				.unwrap_err()
				.contains("API range")
		);
		let disabled_only = EncoderCapabilities {
			rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR::DISABLED,
			..caps
		};
		assert!(
			cbr_target(&disabled_only, Codec::H264, 10_000_000)
				.unwrap_err()
				.contains("no constant-bitrate")
		);
	}

	#[test]
	fn frames_outside_the_coded_extents_are_rejected() {
		let caps = caps((128, 128), (16, 16));
		assert!(
			check_frame(&caps, Codec::AV1, 3, 5)
				.unwrap_err()
				.contains("at least 128x128")
		);
		assert!(check_frame(&caps, Codec::AV1, 128, 128).is_ok());
		// H.264 and HEVC crop small frames out of a minimum-size coded picture.
		assert!(check_frame(&caps, Codec::H264, 2, 2).is_ok());
		assert!(check_frame(&caps, Codec::H265, 3, 5).is_ok());
		assert!(check_frame(&caps, Codec::H264, 1920, 1080).is_ok());
		assert!(check_frame(&caps, Codec::H264, 8192, 8192).is_ok());
		assert!(
			check_frame(&caps, Codec::H264, 8192, 8193)
				.unwrap_err()
				.contains("at most")
		);
	}
}
