//! Numeric domains shared by HTTP launch/resume and RTSP ANNOUNCE.
//!
//! Parsing succeeds for many values that downstream consumers cannot use
//! (zero frame rates divide, oversize extents overflow allocation arithmetic,
//! unknown channel counts silently became stereo). Every entry path validates
//! against these domains *before* it changes the session, so a rejected request
//! leaves a working stream untouched.
//!
//! The bounds are representational limits of the consumers, not quality
//! policy: they deliberately admit 8K, high-refresh and multi-gigabit requests.

use crate::session::stream::audio::AudioChannels;

/// Largest accepted width or height. This is the 2D image limit of the Mesa
/// (RADV/ANV) drivers that back the compositor's GBM targets and the Vulkan
/// import path, and exceeds every Vulkan Video coded extent (at most 8192).
/// It also keeps `width * height * bytes_per_pixel` far inside `u32`/`i32`
/// for the smithay output mode, GBM and Vulkan extents. Codec-specific coded
/// extents are still enforced by the encoder.
pub(crate) const MAX_VIDEO_DIMENSION: u32 = 16_384;

/// Largest refresh rate the compositor can advertise: `wl_output` modes carry
/// the refresh in millihertz as a signed 32-bit integer.
pub(crate) const MAX_REFRESH_RATE_HZ: u32 = (i32::MAX / 1000) as u32;

/// Audio packet durations the mixer clock and Opus framing implement
/// (Moonlight negotiates 5 ms, or 10 ms for slow decoders/networks).
pub(crate) const SUPPORTED_AUDIO_PACKET_DURATIONS_MS: [u32; 2] = [5, 10];

/// Validate a stream extent and refresh rate.
pub(crate) fn validate_display_mode(width: u32, height: u32, refresh_rate: u32) -> Result<(), String> {
	for (name, value) in [("width", width), ("height", height)] {
		if !(1..=MAX_VIDEO_DIMENSION).contains(&value) {
			return Err(format!("{name} {value} is outside 1..={MAX_VIDEO_DIMENSION}"));
		}
	}
	if !(1..=MAX_REFRESH_RATE_HZ).contains(&refresh_rate) {
		return Err(format!(
			"refresh rate {refresh_rate} is outside 1..={MAX_REFRESH_RATE_HZ}"
		));
	}
	Ok(())
}

/// A launch/resume `mode=WIDTHxHEIGHTxREFRESH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
}

impl DisplayMode {
	pub(crate) fn parse(mode: &str) -> Result<Self, String> {
		let parts: Vec<&str> = mode.split('x').collect();
		let [width, height, refresh_rate] = parts.as_slice() else {
			return Err(format!("expected mode in format WxHxR, got '{mode}'"));
		};
		let parse = |name: &str, value: &str| {
			value
				.parse::<u32>()
				.map_err(|error| format!("invalid {name} '{value}' in mode: {error}"))
		};
		let mode = Self {
			width: parse("width", width)?,
			height: parse("height", height)?,
			refresh_rate: refresh_rate_hz(refresh_rate)?,
		};
		validate_display_mode(mode.width, mode.height, mode.refresh_rate)?;
		Ok(mode)
	}
}

/// The refresh of a launch/resume mode, rounded to whole hertz.
///
/// Besides plain hertz (`60`), clients for Apollo-derived hosts send
/// fractional rates in two forms that those hosts define: a decimal
/// (`59.94`, Nightjar, Vibepollo clients) and, from Artemis, an integer in
/// millihertz (`59940`). Apollo reads any value of 1000 or more as
/// millihertz, so this does too; no client requests a refresh of 1000 Hz or
/// more in plain hertz. Integer hertz values keep their exact meaning.
fn refresh_rate_hz(value: &str) -> Result<u32, String> {
	let invalid = || format!("invalid refresh rate '{value}' in mode");
	let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
	let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
	// A decimal point needs digits on both sides; millihertz is the finest
	// precision any client sends.
	if !digits(whole) || (value.contains('.') && !(digits(fraction) && fraction.len() <= 3)) {
		return Err(invalid());
	}
	// Thousandths of the value as written, so 59.94 and 59940 compare exactly.
	let thousandths = whole
		.parse::<u64>()
		.ok()
		.and_then(|whole| whole.checked_mul(1000))
		.and_then(|whole| whole.checked_add(format!("{fraction:0<3}").parse::<u64>().unwrap_or(0)))
		.ok_or_else(invalid)?;
	let millihertz = if thousandths < 1_000_000 {
		thousandths
	} else {
		thousandths / 1000
	};
	u32::try_from(millihertz.saturating_add(500) / 1000).map_err(|_| invalid())
}

/// Parse a GameStream channel count (stereo, 5.1 or 7.1).
pub(crate) fn audio_channels(count: u32) -> Result<AudioChannels, String> {
	match count {
		2 => Ok(AudioChannels::Stereo),
		6 => Ok(AudioChannels::Surround51),
		8 => Ok(AudioChannels::Surround71),
		_ => Err(format!("unsupported audio channel count {count} (expected 2, 6 or 8)")),
	}
}

/// Parse launch/resume `surroundAudioInfo`: channel mask in the high 16 bits,
/// channel count in the low 16 bits.
pub(crate) fn surround_audio_info(value: &str) -> Result<(AudioChannels, u32), String> {
	let value: u32 = value
		.parse()
		.map_err(|error| format!("invalid surroundAudioInfo '{value}': {error}"))?;
	Ok((audio_channels(value & 0xffff)?, value >> 16))
}

pub(crate) fn validate_audio_packet_duration(packet_duration_ms: u32) -> Result<(), String> {
	if SUPPORTED_AUDIO_PACKET_DURATIONS_MS.contains(&packet_duration_ms) {
		Ok(())
	} else {
		Err(format!(
			"unsupported audio packet duration {packet_duration_ms} ms (expected 5 or 10)"
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn display_modes_admit_high_end_and_reject_degenerate_values() {
		for mode in [
			"1280x720x60",
			"1920x1080x144",
			"2560x1440x240",
			"3840x2160x120",
			"3840x2160x240",
			"5120x1440x240",
			"7680x4320x60",
			"16384x16384x1",
			"1x1x1",
			"1920x1080x999",
		] {
			assert!(DisplayMode::parse(mode).is_ok(), "{mode}");
		}
		for mode in [
			"",
			"1920x1080",
			"1920x1080x60x1",
			"0x1080x60",
			"1920x0x60",
			"1920x1080x0",
			"16385x1080x60",
			"1920x16385x60",
			"4294967295x4294967295x4294967295",
			"4294967296x1080x60",
			"-1920x1080x60",
			" 1920x1080x60",
		] {
			assert!(DisplayMode::parse(mode).is_err(), "{mode}");
		}
		// The compositor's millihertz conversion stays representable.
		assert!(i32::try_from(MAX_REFRESH_RATE_HZ * 1000).is_ok());
	}

	#[test]
	fn fractional_and_millihertz_refresh_rates_round_to_hertz() {
		for (mode, hertz) in [
			// Nightjar and Vibepollo clients: decimal hertz.
			("1920x1080x59.94", 60),
			("1920x1080x119.88", 120),
			("1920x1080x60.0", 60),
			("1920x1080x143.856", 144),
			("1920x1080x23.976", 24),
			// Artemis: millihertz from 1000 on, as Apollo reads it.
			("1920x1080x59940", 60),
			("1920x1080x119880", 120),
			("1920x1080x1000", 1),
			("1920x1080x2147483", 2147),
			("1920x1080x2147483000", MAX_REFRESH_RATE_HZ),
		] {
			assert_eq!(
				DisplayMode::parse(mode).map(|mode| mode.refresh_rate),
				Ok(hertz),
				"{mode}"
			);
		}
		for mode in [
			// Rounds to zero.
			"1920x1080x0.4",
			"1920x1080x.5",
			"1920x1080x60.",
			"1920x1080x59.9401",
			"1920x1080x+60",
			"1920x1080x-60",
			"1920x1080x6e1",
			"1920x1080x59,94",
			"1920x1080x60.0.0",
			"1920x1080x2147484000",
			"1920x1080x99999999999999999999999",
		] {
			assert!(DisplayMode::parse(mode).is_err(), "{mode}");
		}
	}

	#[test]
	fn surround_audio_info_requires_a_supported_channel_count() {
		assert_eq!(surround_audio_info("196610"), Ok((AudioChannels::Stereo, 0x3)));
		assert_eq!(
			surround_audio_info(&((0x3f << 16) | 6).to_string()),
			Ok((AudioChannels::Surround51, 0x3f))
		);
		assert_eq!(
			surround_audio_info(&((0x63f << 16) | 8).to_string()),
			Ok((AudioChannels::Surround71, 0x63f))
		);
		for value in ["0", "1", "3", "-1", "x", "4294967296"] {
			assert!(surround_audio_info(value).is_err(), "{value}");
		}
		// The count is the whole low 16 bits, not a truncated byte: 0x102 is not 2.
		assert!(surround_audio_info(&0x0003_0102u32.to_string()).is_err());
	}

	#[test]
	fn only_implemented_audio_durations_are_accepted() {
		assert!(validate_audio_packet_duration(5).is_ok());
		assert!(validate_audio_packet_duration(10).is_ok());
		for duration in [0, 1, 2, 20, 40, u32::MAX] {
			assert!(validate_audio_packet_duration(duration).is_err(), "{duration}");
		}
	}
}
