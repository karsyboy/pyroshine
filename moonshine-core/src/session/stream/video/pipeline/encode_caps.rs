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
		}
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
