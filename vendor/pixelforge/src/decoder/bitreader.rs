//! A bit reader for parsing codec bitstream syntax (Exp-Golomb, fixed-width).
//!
//! Operates on the *encapsulated* byte payload (EBSP) of a NAL unit and strips
//! `0x03` emulation-prevention bytes on the fly, so callers can hand it a raw
//! NAL payload without first converting to RBSP.

use crate::error::{PixelForgeError, Result};

/// Reads bits MSB-first from a NAL payload, skipping emulation-prevention bytes.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the next byte to load.
    byte_pos: usize,
    /// Bit offset (0..8) within the current byte.
    bit_pos: u32,
    /// Number of consecutive zero bytes seen (for emulation prevention).
    zero_run: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
            zero_run: 0,
        }
    }

    /// Read a single bit.
    pub fn bit(&mut self) -> Result<u32> {
        if self.bit_pos == 0 {
            // About to start a new byte: skip an emulation prevention byte
            // (0x03 after two zero bytes).
            if self.zero_run >= 2
                && self.byte_pos < self.data.len()
                && self.data[self.byte_pos] == 0x03
            {
                self.byte_pos += 1;
                self.zero_run = 0;
            }
            if self.byte_pos >= self.data.len() {
                return Err(PixelForgeError::InvalidInput(
                    "bitstream: read past end of NAL unit".to_string(),
                ));
            }
            if self.data[self.byte_pos] == 0 {
                self.zero_run += 1;
            } else {
                self.zero_run = 0;
            }
        }
        let byte = self.data[self.byte_pos];
        let bit = (byte >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(bit as u32)
    }

    /// Read `n` bits (n <= 32) as an unsigned value. `u(n)` in spec notation.
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// Read a boolean flag. `u(1)`.
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? != 0)
    }

    /// Read an unsigned Exp-Golomb value. `ue(v)`.
    pub fn ue(&mut self) -> Result<u32> {
        let mut leading_zeros = 0u32;
        while self.bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(PixelForgeError::InvalidInput(
                    "bitstream: invalid Exp-Golomb code".to_string(),
                ));
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.bits(leading_zeros)?;
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Read a signed Exp-Golomb value. `se(v)`.
    pub fn se(&mut self) -> Result<i32> {
        let code = self.ue()?;
        // Mapping: 0 -> 0, 1 -> 1, 2 -> -1, 3 -> 2, 4 -> -2, ...
        let value = code.div_ceil(2) as i32;
        if code % 2 == 0 { Ok(-value) } else { Ok(value) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bits() {
        let data = [0b1010_1100, 0b0111_0001];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(4).unwrap(), 0b1010);
        assert_eq!(r.bits(8).unwrap(), 0b1100_0111);
        assert_eq!(r.bits(4).unwrap(), 0b0001);
        assert!(r.bit().is_err());
    }

    // The literals below are grouped by exp-Golomb codeword, not by nibble,
    // which is the whole point of writing them out in binary.
    #[allow(clippy::unusual_byte_groupings)]
    #[test]
    fn test_ue() {
        // ue codes: 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let data = [0b1_010_011_0, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
    }

    #[allow(clippy::unusual_byte_groupings)]
    #[test]
    fn test_se() {
        // se: code 1 -> 0, 010 -> +1, 011 -> -1
        let data = [0b1_010_011_0];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se().unwrap(), 0);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
    }

    #[test]
    fn test_emulation_prevention() {
        // 0x00 0x00 0x03 0x01 -> RBSP bytes 0x00 0x00 0x01
        let data = [0x00, 0x00, 0x03, 0x01];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x01);
        assert!(r.bits(8).is_err());
    }
}
