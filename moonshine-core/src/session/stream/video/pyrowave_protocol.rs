//! PyroWave setup contract. Codec API and transport dialect are independent.
/// Shared block format, verified against Nonary's vendored codec.
pub const BITSTREAM_ID: &str = "186f0393";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PyroWaveDialect {
	NativeWireV1,
	RecordFramed,
}
impl PyroWaveDialect {
	/// Older native peers echo version 1. Nonary advertises record support by
	/// feature bit 1 or its documented adaptive-FEC attribute. No UA heuristics.
	pub fn negotiate(
		version: Option<&str>,
		dialect: Option<&str>,
		bitstream: Option<&str>,
		features: Option<&str>,
		adaptive_fec: Option<&str>,
	) -> Result<Self, &'static str> {
		if bitstream.is_some_and(|id| id != BITSTREAM_ID) {
			return Err("PyroWave bitstream revision incompatible: host=186f0393");
		}
		let features = features
			.map(|s| s.parse::<u32>().map_err(|_| "Malformed PyroWave feature bits"))
			.transpose()?;
		if adaptive_fec.is_some_and(|s| s != "0" && s != "1") {
			return Err("Malformed PyroWave adaptive FEC attribute");
		}
		let record = features.is_some_and(|f| f & 1 != 0) || adaptive_fec.is_some();
		if let Some(version) = version {
			if version != "1" {
				return Err("Unsupported PyroWave native wire version; expected 1");
			}
			if record || dialect.is_some_and(|d| d != "native-wire-v1") {
				return Err("Contradictory PyroWave transport attributes");
			}
			return Ok(Self::NativeWireV1);
		}
		if record && dialect.is_none_or(|d| d == "record-framed") {
			return Ok(Self::RecordFramed);
		}
		Err("Unsupported PyroWave protocol dialect; native requires version 1, records require feature bit 1")
	}
}
/// SDP attribute of the extended FEC block layout. The host advertises the
/// layout it supports in DESCRIBE; a client that implements it echoes the
/// same value in ANNOUNCE, and only then does the host use it.
pub const FEC_BLOCKS_ATTRIBUTE: &str = "x-ss-pyrowave.fecBlocks";

/// How many Reed-Solomon blocks one video frame may span, and how their
/// indices are written to `NV_VIDEO_PACKET::multiFecBlocks`.
///
/// GameStream has two bits each for the current (bits 4–5) and last
/// (bits 6–7) block index, so at most four blocks of 255 shards protect a
/// frame. At 1392-byte packets that keeps 20% parity only up to 848 data
/// shards (about 1.1 Gb/s at 120 FPS); larger frames lose most of their
/// parity. The extended layout carries the high two bits of each index in the
/// byte's low nibble (bits 0–1 current, bits 2–3 last), which GameStream
/// senders leave zero, for up to sixteen blocks. It is never inferred from
/// packets: only a client that echoed [`FEC_BLOCKS_ATTRIBUTE`] receives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FecBlockLayout {
	#[default]
	GameStream,
	Extended,
}
impl FecBlockLayout {
	/// The value both sides exchange for [`Self::Extended`].
	pub const EXTENDED_VALUE: &'static str = "16";

	/// Select the layout from the client's ANNOUNCE. Conventional codecs and
	/// PyroWave clients that do not echo the attribute keep GameStream's.
	pub fn negotiate(pyrowave: bool, attribute: Option<&str>) -> Result<Self, &'static str> {
		match attribute {
			_ if !pyrowave => Ok(Self::GameStream),
			None => Ok(Self::GameStream),
			Some(Self::EXTENDED_VALUE) => Ok(Self::Extended),
			Some(_) => Err("Unsupported PyroWave FEC block layout; expected 16"),
		}
	}

	pub const fn max_blocks(self) -> usize {
		match self {
			Self::GameStream => 4,
			Self::Extended => 16,
		}
	}

	/// The `multiFecBlocks` byte of a shard in block `block` of `last + 1`.
	/// Identical to GameStream's encoding whenever both indices are below 4.
	pub(crate) const fn encode(self, block: usize, last: usize) -> u8 {
		debug_assert!(block <= last && last < self.max_blocks());
		(((block & 3) << 4) | ((last & 3) << 6) | ((block >> 2) & 3) | (((last >> 2) & 3) << 2)) as u8
	}
}

/// Whether a record-framed client converts PyroWave planes by the control
/// stream's HDR mode rather than by the sequence header, so SDR frames of an
/// HDR10 stream can be sent as SDR with HDR mode off (the Rubylight host's
/// convention).
///
/// Nightjar does, and announces records by feature bit 1 alone. Pyrolight's
/// record mode (explicit `x-ss-pyrowave.dialect` echo) and Nonary builds that
/// send the adaptive FEC or bitrate attributes treat zero header color bits
/// as "the negotiated format" and would show such SDR frames as PQ, so they
/// keep every frame in HDR10.
pub fn record_client_follows_hdr_mode(
	dialect: Option<&str>,
	adaptive_fec: Option<&str>,
	adaptive_bitrate: Option<&str>,
) -> bool {
	dialect.is_none() && adaptive_fec.is_none() && adaptive_bitrate.is_none()
}

/// Nonary accepts unpadded and straddling records. Prefix completeness permits
/// partial recovery; ordinary GameStream FEC and encryption remain unchanged.
pub(crate) fn record_boundaries(frame: &[u8], payload: usize) -> Result<(Vec<usize>, u16), ()> {
	if frame.len() < 8 || payload < 16 {
		return Err(());
	}
	let word = |at| u32::from_le_bytes(frame[at..at + 4].try_into().unwrap());
	let a = word(0);
	if a >> 31 == 0 || (word(4) >> 24) & 3 != 0 {
		return Err(());
	}
	let blocks = |extent: usize| ((extent.div_ceil(32) * 32).max(128) >> 5).div_ceil(32);
	let coarse = blocks((a & 0x3fff) as usize + 1) * blocks(((a >> 14) & 0x3fff) as usize + 1) * 12;
	let mut starts = vec![0];
	let mut offset = 8;
	let mut critical_end = 8;
	let mut detail_seen = false;
	while offset < frame.len() {
		if frame.len() - offset < 8 {
			return Err(());
		}
		let bytes = ((word(offset) >> 16) & 0xfff) as usize * 4;
		if word(offset) >> 31 != 0 || bytes < 8 || bytes > frame.len() - offset {
			return Err(());
		}
		if (word(offset + 4) >> 8) as usize >= coarse {
			detail_seen = true;
		} else if detail_seen {
			return Err(());
		} else {
			critical_end = offset + bytes;
		}
		starts.push(offset);
		offset += bytes;
	}
	Ok((
		starts,
		u16::try_from((8 + critical_end).div_ceil(payload)).map_err(|_| ())?,
	))
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_bare_feature_bit_record_clients_follow_hdr_mode() {
		assert!(record_client_follows_hdr_mode(None, None, None));
		assert!(!record_client_follows_hdr_mode(Some("record-framed"), None, None));
		assert!(!record_client_follows_hdr_mode(None, Some("0"), None));
		assert!(!record_client_follows_hdr_mode(None, None, Some("1")));
	}
	#[test]
	fn fec_block_layout_is_explicit_and_pyrowave_only() {
		assert_eq!(FecBlockLayout::negotiate(true, None), Ok(FecBlockLayout::GameStream));
		assert_eq!(
			FecBlockLayout::negotiate(true, Some("16")),
			Ok(FecBlockLayout::Extended)
		);
		assert!(FecBlockLayout::negotiate(true, Some("4")).is_err());
		assert!(FecBlockLayout::negotiate(true, Some("32")).is_err());
		assert!(FecBlockLayout::negotiate(true, Some("sixteen")).is_err());
		// Conventional codecs never change their GameStream wire format.
		assert_eq!(
			FecBlockLayout::negotiate(false, Some("16")),
			Ok(FecBlockLayout::GameStream)
		);
	}
	#[test]
	fn extended_block_indices_keep_the_gamestream_bits() {
		for layout in [FecBlockLayout::GameStream, FecBlockLayout::Extended] {
			for last in 0..4 {
				for block in 0..=last {
					assert_eq!(layout.encode(block, last), ((block << 4) | (last << 6)) as u8);
				}
			}
		}
		let extended = FecBlockLayout::Extended;
		for last in 0..16usize {
			for block in 0..=last {
				let byte = extended.encode(block, last) as usize;
				assert_eq!((byte >> 4) & 3 | (byte & 3) << 2, block);
				assert_eq!((byte >> 6) & 3 | ((byte >> 2) & 3) << 2, last);
			}
		}
	}
	#[test]
	fn native_and_nonary_are_explicit() {
		assert_eq!(
			PyroWaveDialect::negotiate(Some("1"), None, None, None, None),
			Ok(PyroWaveDialect::NativeWireV1)
		);
		assert_eq!(
			PyroWaveDialect::negotiate(None, None, None, Some("1"), Some("0")),
			Ok(PyroWaveDialect::RecordFramed)
		);
		assert!(PyroWaveDialect::negotiate(None, None, None, None, None).is_err());
		assert!(PyroWaveDialect::negotiate(Some("2"), None, None, None, None).is_err());
		assert!(PyroWaveDialect::negotiate(Some("1"), None, None, Some("1"), None).is_err());
		assert!(PyroWaveDialect::negotiate(None, None, Some("unknown"), Some("1"), None).is_err());
		assert!(PyroWaveDialect::negotiate(None, None, None, Some("junk"), None).is_err());
	}
	#[test]
	fn prefix_and_record_starts_are_bounded() {
		let mut frame = Vec::new();
		frame.extend_from_slice(&(0x80000000u32 | 127 | (127 << 14)).to_le_bytes());
		frame.extend_from_slice(&2u32.to_le_bytes());
		frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
		frame.extend_from_slice(&0u32.to_le_bytes());
		frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
		frame.extend_from_slice(&(12u32 << 8).to_le_bytes());
		assert_eq!(record_boundaries(&frame, 16).unwrap(), (vec![0, 8, 16], 2));
		assert!(record_boundaries(&frame[..23], 16).is_err());
	}
}
