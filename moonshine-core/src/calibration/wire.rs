//! Datagram formats of the UDP calibration probe (version 1).
//!
//! Probe datagrams (host → client) carry a fixed header followed by filler up
//! to the stream's shard size, so they occupy the link exactly as video shards
//! of the same `packetSize` would. They are not authenticated: the receiver
//! matches them to the run it started and the host only sends them to an
//! endpoint that proved, below, that it holds the run key.
//!
//! Control datagrams (client → host) are authenticated with a truncated
//! HMAC-SHA256 over the whole datagram, keyed by the secret the host returned
//! over the paired HTTPS connection. The host accepts them only from the TLS
//! peer's IP address.
use aws_lc_rs::hmac;

/// Probe datagram magic, version 1.
pub const PROBE_MAGIC: [u8; 4] = *b"PYB1";
/// Control datagram magic, version 1.
pub const CONTROL_MAGIC: [u8; 4] = *b"PYC1";
/// Bytes of the probe header; the rest of a probe datagram is filler.
pub const PROBE_HEADER_BYTES: usize = 24;
/// Filler byte. Non-zero so no link-layer compression can shrink the probe.
pub const FILLER: u8 = 0xa5;
/// Steps a run can contain (bounded so a control report fits a fixed size).
pub const MAX_STEPS: usize = 8;
/// Run identifier length (public; carried in every control datagram).
pub const RUN_ID_BYTES: usize = 16;
/// Secret length; known only to the host and the paired TLS peer.
pub const KEY_BYTES: usize = 32;
const TAG_BYTES: usize = 16;
const CONTROL_BODY_BYTES: usize = 4 + RUN_ID_BYTES + 4 + 4 + 4 * MAX_STEPS;
/// Size of every control datagram.
pub const CONTROL_BYTES: usize = CONTROL_BODY_BYTES + TAG_BYTES;

/// Last packet of its frame.
pub const FLAG_END_OF_FRAME: u8 = 0x1;
/// Last packet of its step.
pub const FLAG_END_OF_STEP: u8 = 0x2;
/// Last packet of the run.
pub const FLAG_END_OF_RUN: u8 = 0x4;

/// Header of one probe datagram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProbeHeader {
	/// Low 32 bits of the run identifier, to discard strays from older runs.
	pub run_tag: u32,
	/// Sequence number across the whole run, starting at 0.
	pub sequence: u32,
	pub step: u8,
	pub flags: u8,
	/// Index of this packet within its frame.
	pub index: u16,
	/// Frame number across the whole run, starting at 0.
	pub frame: u32,
	/// Scheduled start of this packet's frame, in microseconds from the run start.
	pub frame_offset_us: u32,
}

impl ProbeHeader {
	pub fn write(&self, out: &mut [u8]) {
		out[0..4].copy_from_slice(&PROBE_MAGIC);
		out[4..8].copy_from_slice(&self.run_tag.to_be_bytes());
		out[8..12].copy_from_slice(&self.sequence.to_be_bytes());
		out[12] = self.step;
		out[13] = self.flags;
		out[14..16].copy_from_slice(&self.index.to_be_bytes());
		out[16..20].copy_from_slice(&self.frame.to_be_bytes());
		out[20..24].copy_from_slice(&self.frame_offset_us.to_be_bytes());
	}

	pub fn parse(datagram: &[u8]) -> Option<Self> {
		if datagram.len() < PROBE_HEADER_BYTES || datagram[0..4] != PROBE_MAGIC {
			return None;
		}
		let u32_at = |at: usize| u32::from_be_bytes(datagram[at..at + 4].try_into().unwrap());
		Some(Self {
			run_tag: u32_at(4),
			sequence: u32_at(8),
			step: datagram[12],
			flags: datagram[13],
			index: u16::from_be_bytes([datagram[14], datagram[15]]),
			frame: u32_at(16),
			frame_offset_us: u32_at(20),
		})
	}
}

/// Run tag carried in probe datagrams.
pub fn run_tag(run_id: &[u8; RUN_ID_BYTES]) -> u32 {
	u32::from_be_bytes(run_id[0..4].try_into().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlKind {
	/// Opens the run and fixes the client's UDP endpoint. Sent until the first
	/// probe datagram arrives.
	Hello = 1,
	/// Consent and progress. The host stops sending when reports stop.
	Report = 2,
	/// Ends the run immediately.
	Stop = 3,
}

/// One authenticated client → host datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
	pub run_id: [u8; RUN_ID_BYTES],
	pub kind: ControlKind,
	/// Strictly increasing per run; a replayed or reordered report is ignored.
	pub counter: u32,
	/// Probe datagrams received so far in each step.
	pub received: [u32; MAX_STEPS],
}

impl Control {
	pub fn new(run_id: [u8; RUN_ID_BYTES], kind: ControlKind, counter: u32) -> Self {
		Self {
			run_id,
			kind,
			counter,
			received: [0; MAX_STEPS],
		}
	}

	pub fn encode(&self, key: &[u8; KEY_BYTES]) -> [u8; CONTROL_BYTES] {
		let mut out = [0u8; CONTROL_BYTES];
		out[0..4].copy_from_slice(&CONTROL_MAGIC);
		out[4..20].copy_from_slice(&self.run_id);
		out[20] = self.kind as u8;
		out[24..28].copy_from_slice(&self.counter.to_be_bytes());
		for (step, received) in self.received.iter().enumerate() {
			out[28 + step * 4..32 + step * 4].copy_from_slice(&received.to_be_bytes());
		}
		let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &out[..CONTROL_BODY_BYTES]);
		out[CONTROL_BODY_BYTES..].copy_from_slice(&tag.as_ref()[..TAG_BYTES]);
		out
	}

	/// Parse and authenticate a datagram for `run_id`. Anything else (another
	/// run, a bad tag, a wrong size) is `None`.
	pub fn decode(datagram: &[u8], run_id: &[u8; RUN_ID_BYTES], key: &[u8; KEY_BYTES]) -> Option<Self> {
		if datagram.len() != CONTROL_BYTES || datagram[0..4] != CONTROL_MAGIC || datagram[4..20] != run_id[..] {
			return None;
		}
		let expected = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &datagram[..CONTROL_BODY_BYTES]);
		// Constant-time comparison of the truncated tag.
		let difference = expected.as_ref()[..TAG_BYTES]
			.iter()
			.zip(&datagram[CONTROL_BODY_BYTES..])
			.fold(0u8, |acc, (a, b)| acc | (a ^ b));
		if difference != 0 {
			return None;
		}
		let kind = match datagram[20] {
			1 => ControlKind::Hello,
			2 => ControlKind::Report,
			3 => ControlKind::Stop,
			_ => return None,
		};
		let mut received = [0u32; MAX_STEPS];
		for (step, value) in received.iter_mut().enumerate() {
			*value = u32::from_be_bytes(datagram[28 + step * 4..32 + step * 4].try_into().unwrap());
		}
		Some(Self {
			run_id: *run_id,
			kind,
			counter: u32::from_be_bytes(datagram[24..28].try_into().unwrap()),
			received,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn probe_header_round_trips() {
		let header = ProbeHeader {
			run_tag: 0xdead_beef,
			sequence: 123_456,
			step: 7,
			flags: FLAG_END_OF_FRAME | FLAG_END_OF_RUN,
			index: 65_000,
			frame: 999,
			frame_offset_us: 11_999_000,
		};
		let mut buffer = [FILLER; 64];
		header.write(&mut buffer);
		assert_eq!(ProbeHeader::parse(&buffer), Some(header));
		assert_eq!(ProbeHeader::parse(&buffer[..PROBE_HEADER_BYTES - 1]), None);
		buffer[0] = b'X';
		assert_eq!(ProbeHeader::parse(&buffer), None);
	}

	/// Shared with Pyrolight's receiver test; computed independently with
	/// Python's `hmac` module.
	#[test]
	fn control_matches_the_cross_implementation_vector() {
		let mut report = Control::new([7u8; RUN_ID_BYTES], ControlKind::Report, 42);
		report.received = [1, 2, 3, 4, 5, 6, 7, u32::MAX];
		assert_eq!(
			hex::encode(report.encode(&[9u8; KEY_BYTES])),
			"5059433107070707070707070707070707070707020000000000002a00000001000000020000000300000004000000050000000600\
			 000007ffffffff798b9b0ab53af8617b131f1300cf0eb1"
		);
	}

	#[test]
	fn control_is_authenticated_and_bound_to_its_run() {
		let run = [7u8; RUN_ID_BYTES];
		let key = [9u8; KEY_BYTES];
		let mut report = Control::new(run, ControlKind::Report, 42);
		report.received = [1, 2, 3, 4, 5, 6, 7, u32::MAX];
		let datagram = report.encode(&key);
		assert_eq!(Control::decode(&datagram, &run, &key), Some(report));
		// Wrong key, wrong run, any flipped bit, truncation: rejected.
		assert_eq!(Control::decode(&datagram, &run, &[8u8; KEY_BYTES]), None);
		assert_eq!(Control::decode(&datagram, &[6u8; RUN_ID_BYTES], &key), None);
		for byte in 0..CONTROL_BYTES {
			let mut tampered = datagram;
			tampered[byte] ^= 0x10;
			assert_eq!(Control::decode(&tampered, &run, &key), None, "byte {byte}");
		}
		assert_eq!(Control::decode(&datagram[..CONTROL_BYTES - 1], &run, &key), None);
	}
}
