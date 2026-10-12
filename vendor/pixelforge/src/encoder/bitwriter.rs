//! Shared bit writer for video codec utilities (used by H.264 and H.265).
/// Small, efficient bit writer used to assemble RBSP and other bitstreams.
#[derive(Default)]
pub struct BitWriter {
    data: Vec<u8>,
    current_byte: u8,
    bit_position: u8, // number of bits currently written into current_byte (0-7)
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            current_byte: 0,
            bit_position: 0,
        }
    }

    pub fn write_bits(&mut self, mut value: u64, mut num_bits: u8) {
        if num_bits == 0 {
            return;
        }

        while num_bits > 0 {
            let space = 8 - self.bit_position;
            let take = std::cmp::min(space, num_bits);

            // shift value so the bits we need are in the high positions
            let shift = num_bits - take;
            let bits = ((value >> shift) & ((1u64 << take) - 1)) as u8;

            // Avoid shifting a u8 by >= 8 which panics. If we're taking a full.
            // byte (take == 8) we can assign the bits directly; otherwise
            // shift the partial byte left by `take` and OR-in the new bits.
            if take == 8 {
                self.current_byte = bits;
            } else {
                self.current_byte = (self.current_byte << take) | bits;
            }
            self.bit_position += take;
            num_bits -= take;
            if shift > 0 {
                value &= (1u64 << shift) - 1;
            } else {
                value = 0;
            }

            if self.bit_position == 8 {
                self.data.push(self.current_byte);
                self.current_byte = 0;
                self.bit_position = 0;
            }
        }
    }

    pub fn write_ue(&mut self, mut value: u32) {
        value += 1;
        let num_bits = 32u8 - value.leading_zeros() as u8;
        let leading_zeros = num_bits - 1;

        // write leading zeros
        for _ in 0..leading_zeros {
            self.write_bits(0, 1);
        }

        // write value bits
        self.write_bits(value as u64, num_bits);
    }

    pub fn write_se(&mut self, value: i32) {
        let code = if value <= 0 {
            (-2 * value) as u32
        } else {
            (2 * value - 1) as u32
        };
        self.write_ue(code);
    }

    pub fn rbsp_trailing_bits(&mut self) {
        // write rbsp_stop_one_bit = 1
        self.write_bits(1, 1);
        // pad with zeros to byte-align
        if self.bit_position != 0 {
            let pad = 8 - self.bit_position;
            self.write_bits(0, pad);
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.bit_position != 0 {
            // left-align the current partial byte
            self.current_byte <<= 8 - self.bit_position;
            self.data.push(self.current_byte);
        }
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_bits_single_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b10101010, 8);
        assert_eq!(writer.finish(), vec![0b10101010]);
    }

    #[test]
    fn test_write_bits_partial_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b101, 3);
        // Partial byte gets left-aligned on finish: 101 -> 10100000
        assert_eq!(writer.finish(), vec![0b10100000]);
    }

    #[test]
    fn test_write_bits_across_bytes() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b1111, 4);
        writer.write_bits(0b00001111, 8);
        // First 4 bits: 1111, next 8 bits: 00001111.
        // Result: 11110000 11110000 (after left-align)
        assert_eq!(writer.finish(), vec![0b11110000, 0b11110000]);
    }

    #[test]
    fn test_write_bits_zero() {
        let mut writer = BitWriter::new();
        writer.write_bits(0, 0); // Should be a no-op
        assert_eq!(writer.finish(), vec![]);
    }

    #[test]
    fn test_write_bits_multiple_bytes() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xABCD, 16);
        assert_eq!(writer.finish(), vec![0xAB, 0xCD]);
    }

    #[test]
    fn test_write_ue_zero() {
        // ue(0) = 1 (single bit '1')
        let mut writer = BitWriter::new();
        writer.write_ue(0);
        // 0 -> code = 1 -> binary '1' -> 1 bit
        assert_eq!(writer.finish(), vec![0b10000000]);
    }

    #[test]
    fn test_write_ue_one() {
        // ue(1) = 010 (leading 0, then 10)
        let mut writer = BitWriter::new();
        writer.write_ue(1);
        // 1 -> code = 2 -> binary '10' -> 0 + 10 = 010 (3 bits)
        assert_eq!(writer.finish(), vec![0b01000000]);
    }

    #[test]
    fn test_write_ue_two() {
        // ue(2) = 011 (leading 0, then 11)
        let mut writer = BitWriter::new();
        writer.write_ue(2);
        // 2 -> code = 3 -> binary '11' -> 0 + 11 = 011 (3 bits)
        assert_eq!(writer.finish(), vec![0b01100000]);
    }

    #[test]
    fn test_write_ue_larger_values() {
        // ue(3) = 00100 (2 leading zeros, then 100)
        let mut writer = BitWriter::new();
        writer.write_ue(3);
        // 3 -> code = 4 -> binary '100' -> 00 + 100 = 00100 (5 bits)
        assert_eq!(writer.finish(), vec![0b00100000]);

        // ue(6) = 00111 (2 leading zeros, then 111)
        let mut writer = BitWriter::new();
        writer.write_ue(6);
        // 6 -> code = 7 -> binary '111' -> 00 + 111 = 00111 (5 bits)
        assert_eq!(writer.finish(), vec![0b00111000]);

        // ue(7) = 0001000 (3 leading zeros, then 1000)
        let mut writer = BitWriter::new();
        writer.write_ue(7);
        // 7 -> code = 8 -> binary '1000' -> 000 + 1000 = 0001000 (7 bits)
        assert_eq!(writer.finish(), vec![0b00010000]);
    }

    #[test]
    fn test_write_se_zero() {
        // se(0) = ue(0) = 1
        let mut writer = BitWriter::new();
        writer.write_se(0);
        assert_eq!(writer.finish(), vec![0b10000000]);
    }

    #[test]
    fn test_write_se_positive() {
        // se(1) = ue(1) = 010
        let mut writer = BitWriter::new();
        writer.write_se(1);
        // 1 -> code = 2*1 - 1 = 1 -> ue(1) = 010
        assert_eq!(writer.finish(), vec![0b01000000]);

        // se(2) = ue(3) = 00100
        let mut writer = BitWriter::new();
        writer.write_se(2);
        // 2 -> code = 2*2 - 1 = 3 -> ue(3) = 00100
        assert_eq!(writer.finish(), vec![0b00100000]);
    }

    #[test]
    fn test_write_se_negative() {
        // se(-1) = ue(2) = 011
        let mut writer = BitWriter::new();
        writer.write_se(-1);
        // -1 -> code = -2*(-1) = 2 -> ue(2) = 011
        assert_eq!(writer.finish(), vec![0b01100000]);

        // se(-2) = ue(4) = 00101
        let mut writer = BitWriter::new();
        writer.write_se(-2);
        // -2 -> code = -2*(-2) = 4 -> ue(4) = 00101
        assert_eq!(writer.finish(), vec![0b00101000]);
    }

    #[test]
    fn test_rbsp_trailing_bits_aligned() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xFF, 8); // Already byte-aligned
        writer.rbsp_trailing_bits();
        // Writes '1' then pads to next byte.
        assert_eq!(writer.finish(), vec![0xFF, 0b10000000]);
    }

    #[test]
    fn test_rbsp_trailing_bits_unaligned() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b111, 3); // 3 bits written
        writer.rbsp_trailing_bits();
        // Current: 111xxxxx, writes 1 -> 1111xxxx, then pad with 4 zeros -> 11110000
        assert_eq!(writer.finish(), vec![0b11110000]);
    }

    #[test]
    fn test_rbsp_trailing_bits_almost_aligned() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b1111111, 7); // 7 bits written
        writer.rbsp_trailing_bits();
        // Current: 1111111x, writes 1 -> 11111111, byte complete, no padding needed
        assert_eq!(writer.finish(), vec![0b11111111]);
    }

    #[test]
    fn test_complex_bitstream() {
        // Simulate writing a simple NAL unit header + some data.
        let mut writer = BitWriter::new();

        // Write forbidden_zero_bit (1 bit)
        writer.write_bits(0, 1);
        // Write nal_ref_idc (2 bits)
        writer.write_bits(3, 2);
        // Write nal_unit_type (5 bits) - SPS type 7.
        writer.write_bits(7, 5);

        // First byte should be 0b01100111 = 0x67 (SPS NAL header)
        let result = writer.finish();
        assert_eq!(result[0], 0x67);
    }

    #[test]
    fn test_default_impl() {
        let writer = BitWriter::default();
        assert_eq!(writer.finish(), vec![]);
    }
}
