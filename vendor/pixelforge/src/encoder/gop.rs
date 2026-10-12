//! GOP (Group of Pictures) structure for video encoding.
//!
//! This module provides GOP structure management for H.264 and H.265 encoders.
//! The GOP structure determines frame types (IDR/I/P/B) and their ordering.

/// Frame type in GOP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GopFrameType {
    /// IDR frame.
    Idr,
    /// I frame (intra).
    I,
    /// P frame (predicted).
    P,
    /// B frame (bidirectional).
    B,
}

impl GopFrameType {
    /// Whether this frame type is a reference.
    pub fn is_reference(&self) -> bool {
        matches!(self, Self::Idr | Self::I | Self::P)
    }

    /// Whether this is an IDR frame.
    pub fn is_idr(&self) -> bool {
        matches!(self, Self::Idr)
    }

    /// Whether this is an intra frame (I or IDR).
    pub fn is_intra(&self) -> bool {
        matches!(self, Self::Idr | Self::I)
    }
}

/// Position in the GOP.
#[derive(Debug, Clone)]
pub struct GopPosition {
    /// Frame number within the GOP (0-based).
    pub gop_frame_num: u32,
    /// Overall frame index in the sequence.
    pub frame_index: u64,
    /// Frame type.
    pub frame_type: GopFrameType,
    /// POC (Picture Order Count).
    pub pic_order_cnt: i32,
    /// Display order (for B-frames).
    pub display_order: u64,
    /// Whether this frame is a reference.
    pub is_reference: bool,
    /// Temporal layer ID.
    pub temporal_id: u8,
}

/// GOP structure manager.
///
/// Manages the GOP structure for video encoding, determining frame types.
/// and their ordering. This is codec-agnostic and can be used for both
/// H.264 and H.265 encoding.
pub struct GopStructure {
    /// GOP size (number of frames between IDR frames).
    gop_size: u32,
    /// Number of B-frames between I/P frames.
    b_frame_count: u32,
    /// Current frame index.
    frame_index: u64,
    /// Current frame number.
    frame_num: u32,
    /// Current POC.
    poc: i32,
    /// IDR period.
    idr_period: u32,
    /// IDR frame count.
    idr_count: u32,
    /// Max frame_num value (log2_max_frame_num_minus4 + 4).
    max_frame_num: u32,
    /// Max POC LSB value (log2_max_pic_order_cnt_lsb_minus4 + 4).
    max_poc_lsb: u32,
    /// Flag to force next frame to be an IDR.
    force_idr: bool,
}

impl GopStructure {
    /// Create a new GOP structure.
    ///
    /// # Arguments
    /// * `gop_size` - Number of frames in each GOP.
    /// * `b_frame_count` - Number of B-frames between I/P frames (set to 0 for I-P only).
    /// * `idr_period` - Period between IDR frames (0 means only first frame is IDR).
    pub fn new(gop_size: u32, b_frame_count: u32, idr_period: u32) -> Self {
        Self {
            gop_size: gop_size.max(1),
            b_frame_count,
            frame_index: 0,
            frame_num: 0,
            poc: 0,
            idr_period: if idr_period == 0 {
                gop_size
            } else {
                idr_period
            },
            idr_count: 0,
            max_frame_num: 16, // log2_max_frame_num_minus4=0 -> 2^4 = 16
            max_poc_lsb: 16,   // log2_max_pic_order_cnt_lsb_minus4=0 -> 2^4 = 16
            force_idr: false,
        }
    }

    /// Create a simple I-P GOP structure.
    pub fn new_ip_only(gop_size: u32) -> Self {
        Self::new(gop_size, 0, gop_size)
    }

    /// Set max frame_num (2^(log2_max_frame_num_minus4+4)).
    pub fn set_max_frame_num(&mut self, log2_max_frame_num_minus4: u8) {
        self.max_frame_num = 1 << (log2_max_frame_num_minus4 + 4);
    }

    /// Set max POC LSB (2^(log2_max_pic_order_cnt_lsb_minus4+4)).
    pub fn set_max_poc_lsb(&mut self, log2_max_pic_order_cnt_lsb_minus4: u8) {
        self.max_poc_lsb = 1 << (log2_max_pic_order_cnt_lsb_minus4 + 4);
    }

    /// Get the next frame position in the GOP.
    pub fn get_next_frame(&mut self) -> GopPosition {
        let is_idr = self.frame_index == 0
            || (self.idr_period > 0 && self.frame_index.is_multiple_of(self.idr_period as u64))
            || self.force_idr;

        // Clear the force IDR flag after checking it.
        self.force_idr = false;

        // If IDR, reset counters.
        if is_idr {
            self.frame_num = 0;
            self.poc = 0;
            self.idr_count += 1;
        }

        // Determine frame type.
        let frame_type = if is_idr {
            GopFrameType::Idr
        } else if self.b_frame_count > 0 {
            // With B-frames, pattern is: I B B P B B P...
            let gop_pos = (self.frame_index % self.gop_size as u64) as u32;
            if gop_pos == 0 {
                GopFrameType::Idr // Start of GOP is always IDR
            } else if gop_pos.is_multiple_of(self.b_frame_count + 1) {
                GopFrameType::P
            } else {
                GopFrameType::B
            }
        } else {
            // No B-frames, so I-P-P-P-...
            GopFrameType::P
        };

        let position = GopPosition {
            gop_frame_num: (self.frame_index % self.gop_size as u64) as u32,
            frame_index: self.frame_index,
            frame_type,
            pic_order_cnt: self.poc,
            display_order: self.frame_index,
            is_reference: frame_type.is_reference(),
            temporal_id: if frame_type == GopFrameType::B { 1 } else { 0 },
        };

        // Update counters for next frame.
        if frame_type.is_reference() {
            self.frame_num = (self.frame_num + 1) % self.max_frame_num;
        }
        self.poc = (self.poc + 2) % (self.max_poc_lsb as i32 * 2);
        self.frame_index += 1;

        position
    }

    /// Get current frame number.
    pub fn current_frame_num(&self) -> u32 {
        self.frame_num
    }

    /// Get current POC.
    pub fn current_poc(&self) -> i32 {
        self.poc
    }

    /// Get total frames encoded.
    pub fn total_frames(&self) -> u64 {
        self.frame_index
    }

    /// Reset the GOP structure.
    pub fn reset(&mut self) {
        self.frame_index = 0;
        self.frame_num = 0;
        self.poc = 0;
        self.idr_count = 0;
    }

    /// Request that the next frame be an IDR frame.
    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_only_gop() {
        let mut gop = GopStructure::new_ip_only(30);

        // First frame should be IDR.
        let pos = gop.get_next_frame();
        assert_eq!(pos.frame_type, GopFrameType::Idr);
        assert_eq!(pos.frame_index, 0);
        assert!(pos.is_reference);

        // Next frames should be P.
        for i in 1..30 {
            let pos = gop.get_next_frame();
            assert_eq!(pos.frame_type, GopFrameType::P, "Frame {i} should be P");
            assert_eq!(pos.frame_index, i);
            assert!(pos.is_reference);
        }

        // Frame 30 should be IDR (new GOP)
        let pos = gop.get_next_frame();
        assert_eq!(pos.frame_type, GopFrameType::Idr);
        assert_eq!(pos.frame_index, 30);
    }
}
