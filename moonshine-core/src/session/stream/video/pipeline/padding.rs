//! Removal of rate-control padding from encoded frames.
//!
//! In CBR mode some encoders pad every frame up to the per-frame bit budget:
//! RADV (verified on an RX 9070 XT) emits H.264 filler data (NAL type 12),
//! HEVC FD_NUT (38) and AV1 OBU_PADDING (15). With the stream's one-frame
//! virtual buffer that pads each frame to `bitrate / fps`, so a static desktop
//! was sent at the full negotiated bitrate, plus FEC. Decoders discard
//! padding, so it is removed before packetization; the remaining bytes are
//! unchanged.

use crate::session::stream::video::VideoFormat;

const H264_FILLER_DATA: u8 = 12;
const HEVC_FD_NUT: u8 = 38;
const AV1_OBU_PADDING: u8 = 15;

/// Remove padding NAL units/OBUs from one encoded frame in place; returns the
/// number of bytes removed. Malformed or unknown data is left untouched.
pub(super) fn strip(data: &mut Vec<u8>, format: VideoFormat) -> usize {
	let keep = match format {
		VideoFormat::H264 => annex_b_units(data, |header| header & 0x1F != H264_FILLER_DATA),
		VideoFormat::Hevc => annex_b_units(data, |header| (header >> 1) & 0x3F != HEVC_FD_NUT),
		VideoFormat::Av1 => av1_units(data),
		VideoFormat::PyroWave => None,
	};
	let Some(keep) = keep else {
		return 0;
	};
	let before = data.len();
	let mut write = 0;
	for range in keep {
		data.copy_within(range.clone(), write);
		write += range.len();
	}
	data.truncate(write);
	before - write
}

/// Byte ranges to keep, or `None` when there is nothing to remove.
type Keep = Option<Vec<std::ops::Range<usize>>>;

/// Split an Annex B stream at its start codes and keep the NAL units whose
/// first header byte `keep` accepts. A unit spans its start code (with the
/// `zero_byte` of a 4-byte start code) up to the next unit.
fn annex_b_units(data: &[u8], keep: impl Fn(u8) -> bool) -> Keep {
	// Positions of the `00 00 01` start-code prefixes. Emulation prevention
	// guarantees the pattern never occurs inside a NAL unit.
	let mut starts = Vec::new();
	let mut i = 2;
	while let Some(offset) = data.get(i..).and_then(|rest| rest.iter().position(|&b| b == 1)) {
		let one = i + offset;
		if data[one - 1] == 0 && data[one - 2] == 0 {
			let begin = if one >= 3 && data[one - 3] == 0 {
				one - 3
			} else {
				one - 2
			};
			starts.push((begin, one + 1));
		}
		i = one + 3;
	}
	let mut removed = false;
	let mut ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(starts.len() + 1);
	// Bytes before the first start code are never removed.
	let first = starts.first().map_or(data.len(), |&(begin, _)| begin);
	if first > 0 {
		ranges.push(0..first);
	}
	for (index, &(begin, header)) in starts.iter().enumerate() {
		let end = starts.get(index + 1).map_or(data.len(), |&(next, _)| next);
		match data.get(header) {
			Some(&byte) if header < end && !keep(byte) => removed = true,
			_ => push(&mut ranges, begin..end),
		}
	}
	removed.then_some(ranges)
}

/// Keep every AV1 OBU except OBU_PADDING. Requires size fields on every OBU
/// but the last; anything unparseable keeps the rest of the frame.
fn av1_units(data: &[u8]) -> Keep {
	let mut removed = false;
	let mut ranges = Vec::new();
	let mut i = 0;
	while i < data.len() {
		let header = data[i];
		let has_extension = header & 0x04 != 0;
		let has_size = header & 0x02 != 0;
		let size_at = i + 1 + usize::from(has_extension);
		let end = if has_size {
			match leb128(data.get(size_at..).unwrap_or_default()) {
				Some((size, length)) => size_at
					.checked_add(length)
					.and_then(|payload| payload.checked_add(usize::try_from(size).ok()?))
					.filter(|&end| end <= data.len()),
				None => None,
			}
		} else {
			None
		};
		let Some(end) = end else {
			// No size field (the OBU extends to the end) or a malformed one.
			push(&mut ranges, i..data.len());
			break;
		};
		if (header >> 3) & 0x0F == AV1_OBU_PADDING {
			removed = true;
		} else {
			push(&mut ranges, i..end);
		}
		i = end;
	}
	removed.then_some(ranges)
}

/// Append a range, merging it with an adjacent previous one.
fn push(ranges: &mut Vec<std::ops::Range<usize>>, range: std::ops::Range<usize>) {
	match ranges.last_mut() {
		Some(last) if last.end == range.start => last.end = range.end,
		_ => ranges.push(range),
	}
}

/// Decode an unsigned LEB128 value of at most 8 bytes: (value, bytes read).
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
	let mut value = 0u64;
	for (index, &byte) in data.iter().take(8).enumerate() {
		value |= u64::from(byte & 0x7F) << (7 * index);
		if byte & 0x80 == 0 {
			return Some((value, index + 1));
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stripped(mut data: Vec<u8>, format: VideoFormat) -> (Vec<u8>, usize) {
		let removed = strip(&mut data, format);
		(data, removed)
	}

	#[test]
	fn removes_h264_filler_data_and_keeps_other_units() {
		let sps = [0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F];
		let slice = [0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01];
		let filler = [0, 0, 0, 1, 0x0C, 0xFF, 0xFF, 0xFF, 0x80];
		let stream = [&sps[..], &slice, &filler, &filler].concat();
		let (out, removed) = stripped(stream, VideoFormat::H264);
		assert_eq!(out, [&sps[..], &slice].concat());
		assert_eq!(removed, 2 * filler.len());

		// Filler between slices; units after it keep their start codes.
		let stream = [&slice[..], &filler, &slice].concat();
		assert_eq!(stripped(stream, VideoFormat::H264).0, [&slice[..], &slice].concat());
	}

	#[test]
	fn removes_hevc_fd_nut_only() {
		let vps = [0, 0, 0, 1, 0x40, 0x01, 0x0C];
		let idr = [0, 0, 0, 1, 0x26, 0x01, 0xAF, 0x09];
		let fd = [0, 0, 0, 1, 0x4C, 0x01, 0xFF, 0xFF, 0x80];
		let sei = [0, 0, 0, 1, 0x4E, 0x01, 0x89, 0x80];
		let stream = [&vps[..], &sei, &idr, &fd].concat();
		let (out, removed) = stripped(stream, VideoFormat::Hevc);
		assert_eq!(out, [&vps[..], &sei, &idr].concat());
		assert_eq!(removed, fd.len());
	}

	#[test]
	fn leaves_streams_without_padding_unchanged() {
		let stream = vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 3, 1, 0, 0, 1, 0x41, 0x9A];
		assert_eq!(stripped(stream.clone(), VideoFormat::H264), (stream.clone(), 0));
		// Not Annex B at all.
		assert_eq!(stripped(vec![0x0C, 0xFF, 1], VideoFormat::H264).1, 0);
		assert_eq!(stripped(stream.clone(), VideoFormat::PyroWave), (stream, 0));
	}

	#[test]
	fn removes_av1_padding_obus() {
		let temporal_delimiter = [0x12, 0x00];
		let frame = [0x32, 0x03, 0xAA, 0xBB, 0xCC];
		// OBU_PADDING with a two-byte LEB128 size (130 bytes).
		let padding: Vec<u8> = [0x7A, 0x82, 0x01].into_iter().chain([0xFF; 130]).collect();
		// Extension header present.
		let padding_ext = [0x7E, 0x00, 0x01, 0xFF];
		let stream = [&temporal_delimiter[..], &frame, &padding, &padding_ext].concat();
		let (out, removed) = stripped(stream, VideoFormat::Av1);
		assert_eq!(out, [&temporal_delimiter[..], &frame].concat());
		assert_eq!(removed, padding.len() + padding_ext.len());
	}

	#[test]
	fn av1_keeps_everything_after_an_unparseable_obu() {
		let padding = [0x7A, 0x01, 0xFF];
		// Truncated size: claims 9 bytes, has 2.
		let truncated = [0x32, 0x09, 0xAA, 0xBB];
		let stream = [&padding[..], &truncated, &padding].concat();
		let (out, removed) = stripped(stream, VideoFormat::Av1);
		assert_eq!(out, [&truncated[..], &padding].concat());
		assert_eq!(removed, padding.len());
	}
}
