//! RBSP bit writing and NAL unit framing for the packed headers.

/// MSB-first bit writer with Exp-Golomb codes (ITU-T H.264/H.265 clause 9.1).
#[derive(Default)]
pub(super) struct BitWriter {
	bytes: Vec<u8>,
	/// Pending bits, left-aligned in the low `pending` bits.
	accumulator: u64,
	pending: u32,
}

impl BitWriter {
	/// `bits` (at most 32) low bits of `value`, most significant first.
	pub(super) fn u(&mut self, bits: u32, value: u32) {
		debug_assert!(
			bits <= 32 && (bits == 32 || value >> bits == 0),
			"{value} does not fit {bits} bits"
		);
		if bits == 0 {
			return;
		}
		self.accumulator = (self.accumulator << bits) | u64::from(value) & ((1u64 << bits) - 1);
		self.pending += bits;
		while self.pending >= 8 {
			self.pending -= 8;
			self.bytes.push((self.accumulator >> self.pending) as u8);
		}
		self.accumulator &= (1u64 << self.pending) - 1;
	}

	pub(super) fn flag(&mut self, value: bool) {
		self.u(1, u32::from(value));
	}

	/// Unsigned Exp-Golomb `ue(v)`.
	pub(super) fn ue(&mut self, value: u32) {
		let coded = u64::from(value) + 1;
		let length = 64 - coded.leading_zeros();
		// length - 1 leading zeros, then `coded` in `length` bits.
		let zeros = length - 1;
		if zeros > 0 {
			self.u(zeros.min(32), 0);
		}
		if length > 32 {
			self.u(length - 32, (coded >> 32) as u32);
			self.u(32, coded as u32);
		} else {
			self.u(length, coded as u32);
		}
	}

	/// Signed Exp-Golomb `se(v)`.
	pub(super) fn se(&mut self, value: i32) {
		let mapped = if value > 0 {
			(value as u32) * 2 - 1
		} else {
			value.unsigned_abs() * 2
		};
		self.ue(mapped);
	}

	/// Bits written so far.
	pub(super) fn bit_position(&self) -> u32 {
		self.bytes.len() as u32 * 8 + self.pending
	}

	pub(super) fn byte_aligned(&self) -> bool {
		self.pending == 0
	}

	/// `rbsp_trailing_bits()`: a one bit, then zero bits to the byte boundary.
	pub(super) fn trailing_bits(&mut self) {
		self.u(1, 1);
		while !self.byte_aligned() {
			self.u(1, 0);
		}
	}

	pub(super) fn into_bytes(self) -> Vec<u8> {
		assert!(self.pending == 0, "RBSP must end byte-aligned");
		self.bytes
	}

	/// The bits written, zero-padded to a byte boundary, and the number of
	/// padding bits: an H.264 slice header ends unaligned.
	pub(super) fn into_padded_bytes(mut self) -> (Vec<u8>, u32) {
		let padding = (8 - self.pending) % 8;
		self.u(padding, 0);
		(self.bytes, padding)
	}
}

/// A NAL unit with a 4-byte start code: `header` then `rbsp` with emulation
/// prevention bytes inserted (ITU-T H.264 7.4.1, H.265 7.4.2).
pub(super) fn nal_unit(header: &[u8], rbsp: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(4 + header.len() + rbsp.len() + rbsp.len() / 64);
	out.extend_from_slice(&[0, 0, 0, 1]);
	out.extend_from_slice(header);
	let mut zeros = 0;
	for &byte in rbsp {
		if zeros >= 2 && byte <= 3 {
			out.push(3);
			zeros = 0;
		}
		out.push(byte);
		zeros = if byte == 0 { zeros + 1 } else { 0 };
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn exp_golomb_codes_match_the_standard_table() {
		// H.264 Table 9-2: 0 -> 1, 1 -> 010, 2 -> 011, 3 -> 00100, 7 -> 0001000.
		let mut w = BitWriter::default();
		for value in [0, 1, 2, 3, 7] {
			w.ue(value);
		}
		// 1 010 011 00100 0001000 + trailing 1 + 000 = 24 bits
		w.trailing_bits();
		assert_eq!(w.into_bytes(), [0b1010_0110, 0b0100_0001, 0b0001_0000]);

		// se(v): 0 -> 1, 1 -> 010, -1 -> 011, 2 -> 00100.
		let mut w = BitWriter::default();
		for value in [0, 1, -1, 2] {
			w.se(value);
		}
		w.u(4, 0);
		assert_eq!(w.into_bytes(), [0b1010_0110, 0b0100_0000]);
	}

	#[test]
	fn large_exp_golomb_values_round_trip_through_their_length() {
		let mut w = BitWriter::default();
		w.ue(u32::MAX);
		// 32 zeros, then 2^32 - 1 + 1 = 2^32 in 33 bits.
		w.u(7, 0);
		let bytes = w.into_bytes();
		assert_eq!(bytes.len(), 9);
		assert_eq!(&bytes[..5], &[0, 0, 0, 0, 0x80]);
		assert!(bytes[5..].iter().all(|&b| b == 0));
	}

	#[test]
	fn emulation_prevention_escapes_start_code_prefixes() {
		assert_eq!(
			nal_unit(&[0x67], &[0, 0, 1, 0, 0, 0, 0, 0, 3, 5]),
			[0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 3, 5]
		);
		assert_eq!(nal_unit(&[0x40, 0x01], &[0, 0, 4]), [0, 0, 0, 1, 0x40, 0x01, 0, 0, 4]);
	}
}
