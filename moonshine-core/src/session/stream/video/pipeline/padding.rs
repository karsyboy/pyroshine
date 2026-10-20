//! Removal of rate-control padding from encoded frames.
//!
//! In CBR mode some encoders pad every frame up to the per-frame bit budget:
//! RADV (verified on an RX 9070 XT) emits H.264 filler data (NAL type 12),
//! HEVC FD_NUT (38) and AV1 OBU_PADDING (15); Intel's VA-API driver (iHD,
//! verified on an Arc B390) appends zero bytes after the last H.264/HEVC NAL
//! unit (Annex B `trailing_zero_8bits`) and after the last AV1 tile inside
//! the tile group OBU. With the stream's one-frame virtual
//! buffer that pads each frame to `bitrate / fps`, so a static desktop was
//! sent at the full negotiated bitrate, plus FEC. Decoders discard padding,
//! so it is removed before packetization; the remaining bytes are unchanged.

use crate::session::stream::video::VideoFormat;

const H264_FILLER_DATA: u8 = 12;
const HEVC_FD_NUT: u8 = 38;
const AV1_OBU_TILE_GROUP: u8 = 4;
const AV1_OBU_FRAME: u8 = 6;
const AV1_OBU_PADDING: u8 = 15;

/// Remove padding NAL units/OBUs from one encoded frame in place; returns the
/// number of bytes removed. Malformed or unknown data is left untouched.
pub(super) fn strip(data: &mut Vec<u8>, format: VideoFormat) -> usize {
	let before = data.len();
	if matches!(format, VideoFormat::H264 | VideoFormat::Hevc) {
		trim_trailing_zeros(data);
	}
	let keep = match format {
		VideoFormat::H264 => annex_b_units(data, |header| header & 0x1F != H264_FILLER_DATA),
		VideoFormat::Hevc => annex_b_units(data, |header| (header >> 1) & 0x3F != HEVC_FD_NUT),
		VideoFormat::Av1 => {
			strip_av1(data);
			None
		},
		VideoFormat::PyroWave => None,
	};
	let Some(keep) = keep else {
		return before - data.len();
	};
	let mut write = 0;
	for range in keep {
		data.copy_within(range.clone(), write);
		write += range.len();
	}
	data.truncate(write);
	before - write
}

/// Drop the zero bytes after the last NAL unit. A NAL unit never ends in a
/// zero byte (H.264 7.4.1, H.265 7.4.2: a trailing `cabac_zero_word` is
/// followed by 0x03), so they are always `trailing_zero_8bits`. Data without
/// a nonzero byte is left untouched.
fn trim_trailing_zeros(data: &mut Vec<u8>) {
	if let Some(last) = data.iter().rposition(|&byte| byte != 0) {
		data.truncate(last + 1);
	}
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

/// Remove AV1 OBU_PADDING and the zero bytes after the last tile of each
/// tile group (OBU_TILE_GROUP, OBU_FRAME), in place. The last tile's size is
/// implicit and the symbol decoder reads past its end as zeros; its trailing
/// one bit is in the last nonzero byte, so the trimmed tile decodes
/// identically (AV1 8.2.4) and `obu_size` is rewritten in its original
/// length. Requires size fields on every OBU but the last; anything
/// unparseable keeps the rest of the frame.
fn strip_av1(data: &mut Vec<u8>) {
	let (mut read, mut write) = (0, 0);
	while read < data.len() {
		let header = data[read];
		let size_at = read + 1 + usize::from(header & 0x04 != 0);
		let parsed = if header & 0x02 != 0 {
			leb128(data.get(size_at..).unwrap_or_default()).and_then(|(size, length)| {
				let payload = size_at.checked_add(length)?;
				let end = payload.checked_add(usize::try_from(size).ok()?)?;
				(end <= data.len()).then_some((length, payload, end))
			})
		} else {
			None
		};
		let Some((length, payload, end)) = parsed else {
			// No size field (the OBU extends to the end) or a malformed one.
			data.copy_within(read.., write);
			write += data.len() - read;
			break;
		};
		match (header >> 3) & 0x0F {
			AV1_OBU_PADDING => {},
			AV1_OBU_TILE_GROUP | AV1_OBU_FRAME => {
				let kept = data[payload..end]
					.iter()
					.rposition(|&byte| byte != 0)
					.map_or(end - payload, |last| last + 1);
				data.copy_within(read..size_at, write);
				let size_field = write + (size_at - read);
				write_leb128(&mut data[size_field..size_field + length], kept as u64);
				data.copy_within(payload..payload + kept, size_field + length);
				write = size_field + length + kept;
			},
			_ => {
				data.copy_within(read..end, write);
				write += end - read;
			},
		}
		read = end;
	}
	data.truncate(write);
}

/// Encode `value` as LEB128 filling `field` exactly (padded with
/// continuation bytes); `value` fits, being at most the value it replaces.
fn write_leb128(field: &mut [u8], mut value: u64) {
	let last = field.len() - 1;
	for (index, byte) in field.iter_mut().enumerate() {
		*byte = (value & 0x7F) as u8 | if index < last { 0x80 } else { 0 };
		value >>= 7;
	}
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
	fn removes_trailing_zero_bytes_after_the_last_nal_unit() {
		let sps = [0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F];
		// Ends in an escaped cabac_zero_word: the 0x03 stays.
		let slice = [0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x03];
		let stream = [&sps[..], &slice, &[0; 1000]].concat();
		let (out, removed) = stripped(stream, VideoFormat::H264);
		assert_eq!(out, [&sps[..], &slice].concat());
		assert_eq!(removed, 1000);

		let idr = [0, 0, 0, 1, 0x26, 0x01, 0xAF, 0x09];
		let fd = [0, 0, 0, 1, 0x4C, 0x01, 0xFF, 0xFF, 0x80];
		let stream = [&idr[..], &fd, &[0; 7]].concat();
		assert_eq!(stripped(stream, VideoFormat::Hevc), (idr.to_vec(), fd.len() + 7));

		// AV1 metadata ending in zero bytes is left alone.
		let obu = vec![0x2A, 0x02, 0xAA, 0x00];
		assert_eq!(stripped(obu.clone(), VideoFormat::Av1), (obu, 0));
		// Only zeros: not a stream.
		assert_eq!(stripped(vec![0; 16], VideoFormat::H264).1, 0);
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
	fn trims_zero_padding_after_the_last_av1_tile_and_keeps_the_size_length() {
		let temporal_delimiter = [0x12, 0x00];
		let frame_header = [0x1A, 0x02, 0x10, 0x80];
		// OBU_TILE_GROUP with a four-byte size of 6: data, then 3 zero bytes.
		let tile_group = [0x22, 0x86, 0x80, 0x80, 0x00, 0xAB, 0x00, 0x01, 0x00, 0x00, 0x00];
		let stream = [&temporal_delimiter[..], &frame_header, &tile_group].concat();
		let (out, removed) = stripped(stream, VideoFormat::Av1);
		assert_eq!(removed, 3);
		let trimmed = [0x22, 0x83, 0x80, 0x80, 0x00, 0xAB, 0x00, 0x01];
		assert_eq!(out, [&temporal_delimiter[..], &frame_header, &trimmed].concat());

		// OBU_FRAME (header and tile) followed by padding, then a second
		// temporal unit's frame: both trimmed, the padding removed.
		let frame = [0x32, 0x04, 0x10, 0x55, 0x00, 0x00];
		let padding = [0x7A, 0x01, 0xFF];
		let stream = [&frame[..], &padding, &frame].concat();
		let (out, removed) = stripped(stream, VideoFormat::Av1);
		assert_eq!(out, [0x32, 0x02, 0x10, 0x55, 0x32, 0x02, 0x10, 0x55]);
		assert_eq!(removed, 2 + padding.len() + 2);
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
