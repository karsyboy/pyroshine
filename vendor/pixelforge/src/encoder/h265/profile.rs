//! Complete the Range Extensions profile constraints unavailable in StdVideo.

use crate::encoder::BitDepth;
use crate::error::{PixelForgeError, Result};

fn invalid(reason: &str) -> PixelForgeError {
    PixelForgeError::SessionParametersCreation(format!("HEVC 4:4:4 header: {reason}"))
}

fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    for i in from..data.len().saturating_sub(2) {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            return Some((i, 4));
        }
        if data[i..].starts_with(&[0, 0, 1]) {
            return Some((i, 3));
        }
    }
    None
}

fn unescape(data: &[u8]) -> Result<Vec<u8>> {
    let mut rbsp = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for (i, &byte) in data.iter().enumerate() {
        if zeros == 2 && byte == 3 {
            if !data.get(i + 1).is_some_and(|b| *b <= 3) {
                return Err(invalid("invalid emulation-prevention byte"));
            }
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    Ok(rbsp)
}

fn escape(data: &[u8], output: &mut Vec<u8>) {
    let mut zeros = 0;
    for &byte in data {
        if zeros == 2 && byte <= 3 {
            output.push(3);
            zeros = 0;
        }
        output.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
}

/// Normalize the general PTL in both parameter sets once, before caching them.
/// Vulkan's StdVideoH265ProfileTierLevel omits the RExt constraint flags.
/// NVIDIA consequently writes zeros, which prevents VAAPI from identifying
/// Main 4:4:4 / Main 4:4:4 10 even though software decoders accept the stream.
/// Re-escape the RBSP after changing bits; editing escaped bytes in place can
/// leave an emulation-prevention byte at the wrong location.
pub(super) fn normalize_444_header(header: &[u8], depth: BitDepth) -> Result<Vec<u8>> {
    let (mut start, mut prefix) =
        start_code(header, 0).ok_or_else(|| invalid("missing Annex B start code"))?;
    if header[..start].iter().any(|b| *b != 0) {
        return Err(invalid("unexpected bytes before start code"));
    }
    let mut output = Vec::with_capacity(header.len());
    output.extend_from_slice(&header[..start]);
    let mut seen = [false; 2];
    loop {
        let begin = start + prefix;
        let next = start_code(header, begin);
        let end = next.map_or(header.len(), |(i, _)| i);
        let nal = &header[begin..end];
        if nal.len() < 2 {
            return Err(invalid("truncated NAL header"));
        }
        output.extend_from_slice(&header[start..begin]);
        let kind = (nal[0] >> 1) & 63;
        if kind == 32 || kind == 33 {
            // VPS has 32 bits before general PTL; SPS has eight. PTL is 96 bits.
            let offset = if kind == 32 { 4 } else { 1 };
            let mut rbsp = unescape(&nal[2..])?;
            if rbsp.len() < offset + 12 {
                return Err(invalid("truncated profile_tier_level"));
            }
            if rbsp[offset] & 0xdf != 4 {
                return Err(invalid("expected Range Extensions profile in space zero"));
            }
            // max_12bit, max_10bit, max_8bit, max_422chroma, max_420chroma,
            // max_monochrome, intra, one_picture_only, lower_bit_rate.
            // These describe this encoder's actual 8/10-bit 4:4:4 output.
            let flags = [
                true,
                true,
                depth == BitDepth::Eight,
                false,
                false,
                false,
                false,
                false,
                true,
            ];
            for (i, set) in flags.into_iter().enumerate() {
                let bit = offset * 8 + 44 + i;
                let mask = 1 << (7 - bit % 8);
                rbsp[bit / 8] = (rbsp[bit / 8] & !mask) | if set { mask } else { 0 };
            }
            output.extend_from_slice(&nal[..2]);
            escape(&rbsp, &mut output);
            seen[usize::from(kind == 33)] = true;
        } else {
            output.extend_from_slice(nal);
        }
        match next {
            Some((i, len)) => {
                start = i;
                prefix = len;
            }
            None => break,
        }
    }
    if !seen.into_iter().all(|b| b) {
        return Err(invalid("missing VPS or SPS"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Vec<u8> {
        let mut result = Vec::new();
        for (kind, offset, prefix) in [(32, 4, &[0, 0, 0, 1][..]), (33, 1, &[0, 0, 1][..])] {
            let mut rbsp = vec![0; offset + 12];
            rbsp[..offset].fill(0x11);
            rbsp[offset] = 4;
            rbsp[offset + 1] = 8; // profile_compatibility_flag[4]
            rbsp[offset + 5] = 0x90; // source flags; preserve these
            rbsp[offset + 11] = 153; // level 5.1
            rbsp.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0x80]);
            result.extend_from_slice(prefix);
            result.extend_from_slice(&[kind << 1, 1]);
            escape(&rbsp, &mut result);
        }
        result.extend_from_slice(&[0, 0, 0, 1, 68, 1, 0x80]); // PPS
        result
    }

    fn payloads(header: &[u8]) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        let mut current = start_code(header, 0);
        while let Some((i, prefix)) = current {
            current = start_code(header, i + prefix);
            let end = current.map_or(header.len(), |(i, _)| i);
            result.push(unescape(&header[i + prefix + 2..end]).unwrap());
        }
        result
    }

    #[test]
    fn identifies_both_depths_preserving_other_header_fields() {
        let input = header();
        let original = payloads(&input);
        for (depth, flags) in [
            (BitDepth::Eight, [1, 1, 1, 0, 0, 0, 0, 0, 1]),
            (BitDepth::Ten, [1, 1, 0, 0, 0, 0, 0, 0, 1]),
        ] {
            let output = normalize_444_header(&input, depth).unwrap();
            let decoded = payloads(&output);
            for (nal, offset) in [(0, 4), (1, 1)] {
                let mut restored = decoded[nal].clone();
                for (i, expected) in flags.into_iter().enumerate() {
                    let bit = offset * 8 + 44 + i;
                    let mask = 1 << (7 - bit % 8);
                    assert_eq!(u8::from(decoded[nal][bit / 8] & mask != 0), expected);
                    restored[bit / 8] &= !mask;
                }
                assert_eq!(restored, original[nal]);
            }
            assert_eq!(decoded[2], original[2]);
            assert!(output.ends_with(&[0, 0, 0, 1, 68, 1, 0x80]));
            assert_eq!(normalize_444_header(&output, depth).unwrap(), output);
            // A new encoder/session with changed depth must replace the old constraints.
            let other = if depth == BitDepth::Eight {
                BitDepth::Ten
            } else {
                BitDepth::Eight
            };
            assert_eq!(
                normalize_444_header(&output, other).unwrap(),
                normalize_444_header(&input, other).unwrap()
            );
        }
    }

    #[test]
    fn rejects_truncated_headers_without_panicking() {
        let input = header();
        for end in 0..22 {
            assert!(normalize_444_header(&input[..end], BitDepth::Eight).is_err());
        }
    }

    #[test]
    fn rejects_unexpected_profile_and_invalid_escape() {
        let mut input = header();
        input[10] = 1; // first VPS PTL byte
        assert!(normalize_444_header(&input, BitDepth::Eight).is_err());
        assert!(unescape(&[0, 0, 3]).is_err());
        assert!(unescape(&[0, 0, 3, 4]).is_err());
    }

    #[test]
    fn preserves_high_tier() {
        let mut input = header();
        input[10] |= 0x20;
        let output = normalize_444_header(&input, BitDepth::Eight).unwrap();
        assert_eq!(payloads(&output)[0][4] & 0x20, 0x20);
    }
}
