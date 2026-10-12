//! H.264/AVC-specific DPB implementation.
//!
//! This module implements the H.264 DPB according to the specification,
//! including:
//! - POC calculation (Type 0 and Type 2)
//! - Picture number (PicNum) derivation
//! - Sliding window and adaptive memory management
//! - Reference picture list construction for P and B frames
//! - Gap handling for frame_num discontinuities

use super::entry::{DpbEntry, DpbState, MarkingState};
use super::reference_lists::{H264ReferenceListBuilder, ReferenceList};
use super::types::{DpbConfig, MmcoOperation, PictureStartInfo, PocType};
use super::{DecodedPictureBufferTrait, MAX_DPB_SIZE, PictureType};

/// H.264-specific DPB implementation.
#[derive(Debug)]
pub struct DpbH264 {
    /// DPB entries.
    entries: [DpbEntry; MAX_DPB_SIZE],
    /// Current DPB slot index (-1 if none).
    current_slot: i8,
    /// Maximum DPB size.
    max_dpb_size: i8,
    /// Maximum long-term frame index (-1 means no LTR allowed).
    max_long_term_frame_idx: i32,
    /// POC type.
    poc_type: PocType,
    /// Previous POC MSB (for POC Type 0).
    prev_poc_msb: i32,
    /// Previous POC LSB (for POC Type 0).
    prev_poc_lsb: i32,
    /// Previous frame_num offset (for POC Type 2).
    prev_frame_num_offset: i32,
    /// Previous frame_num.
    prev_frame_num: u32,
    /// Previous reference frame_num.
    prev_ref_frame_num: u32,
    /// Maximum frame_num value.
    max_frame_num: u32,
    /// Maximum POC LSB value.
    max_poc_lsb: i32,
    /// Whether to use multiple references.
    use_multiple_refs: bool,
    /// Last IDR timestamp.
    last_idr_timestamp: u64,
    /// Number of temporal layers.
    num_temporal_layers: u32,
}

impl Default for DpbH264 {
    fn default() -> Self {
        Self::new()
    }
}

impl DpbH264 {
    /// Create a new H.264 DPB.
    pub fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| DpbEntry::new()),
            current_slot: -1,
            max_dpb_size: 0,
            max_long_term_frame_idx: -1,
            poc_type: PocType::Type0,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            prev_ref_frame_num: 0,
            max_frame_num: 16,
            max_poc_lsb: 16,
            use_multiple_refs: true,
            last_idr_timestamp: 0,
            num_temporal_layers: 1,
        }
    }

    /// Get reference to the entries array.
    pub fn entries(&self) -> &[DpbEntry; MAX_DPB_SIZE] {
        &self.entries
    }

    /// Get the maximum DPB size.
    pub fn dpb_size(&self) -> i8 {
        self.max_dpb_size
    }

    /// Get the maximum frame_num.
    pub fn max_frame_num(&self) -> u32 {
        self.max_frame_num
    }

    /// Set the POC type.
    pub fn set_poc_type(&mut self, poc_type: PocType) {
        self.poc_type = poc_type;
    }

    /// Get reference picture list for P-frames.
    pub fn get_ref_list_p(&self) -> ReferenceList {
        H264ReferenceListBuilder::init_ref_list_p_frame(
            &self.entries,
            self.max_dpb_size,
            self.prev_frame_num,
            self.max_frame_num,
        )
    }

    /// Get reference picture lists for B-frames.
    pub fn get_ref_lists_b(&self, current_poc: i32) -> (ReferenceList, ReferenceList) {
        H264ReferenceListBuilder::init_ref_lists_b_frame(
            &self.entries,
            self.max_dpb_size,
            current_poc,
        )
    }

    /// Calculate POC for the current picture.
    fn calculate_poc(&mut self, info: &PictureStartInfo) {
        match self.poc_type {
            PocType::Type0 => self.calculate_poc_type0(info),
            PocType::Type1 => {
                // POC Type 1 not commonly used, fallback to Type 2.
                self.calculate_poc_type2(info);
            }
            PocType::Type2 => self.calculate_poc_type2(info),
        }

        // Derive PicOrderCnt (8-1)
        let entry = &mut self.entries[self.current_slot as usize];
        entry.pic_order_cnt = std::cmp::min(entry.top_foc, entry.bottom_foc);
    }

    /// Calculate POC Type 0 (8.2.1.1).
    fn calculate_poc_type0(&mut self, info: &PictureStartInfo) {
        if info.pic_type == PictureType::Idr {
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
        }

        let poc_lsb = info.pic_order_cnt;

        // (8-3)
        let poc_msb = if (poc_lsb < self.prev_poc_lsb)
            && ((self.prev_poc_lsb - poc_lsb) >= (self.max_poc_lsb / 2))
        {
            self.prev_poc_msb + self.max_poc_lsb
        } else if (poc_lsb > self.prev_poc_lsb)
            && ((poc_lsb - self.prev_poc_lsb) > (self.max_poc_lsb / 2))
        {
            self.prev_poc_msb - self.max_poc_lsb
        } else {
            self.prev_poc_msb
        };

        let entry = &mut self.entries[self.current_slot as usize];

        // (8-4) and (8-5)
        entry.top_foc = poc_msb + poc_lsb;
        entry.bottom_foc = poc_msb + poc_lsb;

        if info.is_reference {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = poc_lsb;
        }
    }

    /// Calculate POC Type 2 (8.2.1.3).
    fn calculate_poc_type2(&mut self, info: &PictureStartInfo) {
        // FrameNumOffset (8-12)
        let frame_num_offset = if info.pic_type == PictureType::Idr {
            0
        } else if self.prev_frame_num > info.frame_num {
            self.prev_frame_num_offset + self.max_frame_num as i32
        } else {
            self.prev_frame_num_offset
        };

        // tempPicOrderCnt (8-13)
        let temp_poc = if info.pic_type == PictureType::Idr {
            0
        } else if !info.is_reference {
            2 * (frame_num_offset + info.frame_num as i32) - 1
        } else {
            2 * (frame_num_offset + info.frame_num as i32)
        };

        let entry = &mut self.entries[self.current_slot as usize];

        // (8-14)
        entry.top_foc = temp_poc;
        entry.bottom_foc = temp_poc;

        self.prev_frame_num_offset = frame_num_offset;
        self.prev_frame_num = info.frame_num;
    }

    /// Calculate PicNum for all DPB entries (8.2.4.1).
    fn calculate_pic_num(&mut self, frame_num: u32) {
        for i in 0..self.max_dpb_size as usize {
            let entry = &mut self.entries[i];

            // (8-28) FrameNumWrap
            if entry.frame_num > frame_num {
                entry.frame_num_wrap = entry.frame_num as i32 - self.max_frame_num as i32;
            } else {
                entry.frame_num_wrap = entry.frame_num as i32;
            }

            // For frame coding (not field):
            // (8-29) PicNum = FrameNumWrap
            entry.top_pic_num = entry.frame_num_wrap;
            entry.bottom_pic_num = entry.frame_num_wrap;

            // (8-30) LongTermPicNum = LongTermFrameIdx
            entry.top_long_term_pic_num = entry.long_term_frame_idx;
            entry.bottom_long_term_pic_num = entry.long_term_frame_idx;
        }
    }

    /// Execute Memory Management Control Operations (8.2.5.4).
    pub fn execute_mmco(&mut self, operations: &[MmcoOperation], current_pic_num: i32) {
        for op in operations {
            match op {
                MmcoOperation::End => break,

                MmcoOperation::UnmarkShortTerm {
                    difference_of_pic_nums_minus1,
                } => {
                    // (8-40) picNumX
                    let pic_num_x = current_pic_num - (*difference_of_pic_nums_minus1 as i32 + 1);
                    for i in 0..self.max_dpb_size as usize {
                        let entry = &mut self.entries[i];
                        if entry.marking == MarkingState::ShortTerm
                            && entry.top_pic_num == pic_num_x
                        {
                            entry.marking = MarkingState::Unused;
                        }
                        if entry.bottom_field_marking == MarkingState::ShortTerm
                            && entry.bottom_pic_num == pic_num_x
                        {
                            entry.bottom_field_marking = MarkingState::Unused;
                        }
                    }
                }

                MmcoOperation::UnmarkLongTerm { long_term_pic_num } => {
                    for i in 0..self.max_dpb_size as usize {
                        let entry = &mut self.entries[i];
                        if entry.marking == MarkingState::LongTerm
                            && entry.top_long_term_pic_num == *long_term_pic_num as i32
                        {
                            entry.marking = MarkingState::Unused;
                        }
                        if entry.bottom_field_marking == MarkingState::LongTerm
                            && entry.bottom_long_term_pic_num == *long_term_pic_num as i32
                        {
                            entry.bottom_field_marking = MarkingState::Unused;
                        }
                    }
                }

                MmcoOperation::MarkLongTerm {
                    difference_of_pic_nums_minus1,
                    long_term_frame_idx,
                } => {
                    let pic_num_x = current_pic_num - (*difference_of_pic_nums_minus1 as i32 + 1);

                    // First unmark any existing LTR with this index.
                    for i in 0..self.max_dpb_size as usize {
                        let entry = &mut self.entries[i];
                        if entry.marking == MarkingState::LongTerm
                            && entry.long_term_frame_idx == *long_term_frame_idx as i32
                        {
                            entry.marking = MarkingState::Unused;
                        }
                    }

                    // Mark the short-term as long-term.
                    for i in 0..self.max_dpb_size as usize {
                        let entry = &mut self.entries[i];
                        if entry.marking == MarkingState::ShortTerm
                            && entry.top_pic_num == pic_num_x
                        {
                            entry.mark_long_term(*long_term_frame_idx as i32);
                        }
                    }
                }

                MmcoOperation::SetMaxLongTermIndex {
                    max_long_term_frame_idx_plus1,
                } => {
                    self.max_long_term_frame_idx = *max_long_term_frame_idx_plus1 as i32 - 1;

                    // Mark all LTR with index > max as unused.
                    for i in 0..self.max_dpb_size as usize {
                        let entry = &mut self.entries[i];
                        if entry.marking == MarkingState::LongTerm
                            && entry.long_term_frame_idx > self.max_long_term_frame_idx
                        {
                            entry.marking = MarkingState::Unused;
                        }
                    }
                }

                MmcoOperation::UnmarkAll => {
                    for i in 0..self.max_dpb_size as usize {
                        self.entries[i].mark_unused();
                    }
                    self.max_long_term_frame_idx = -1;

                    // Reset frame_num and POC for current.
                    if self.current_slot >= 0 {
                        let entry = &mut self.entries[self.current_slot as usize];
                        entry.frame_num = 0;
                        let poc = entry.pic_order_cnt;
                        entry.top_foc -= poc;
                        entry.bottom_foc -= poc;
                        entry.pic_order_cnt = 0;
                    }
                }

                MmcoOperation::MarkCurrentAsLongTerm {
                    long_term_frame_idx,
                } => {
                    // Unmark any existing LTR with this index.
                    for i in 0..self.max_dpb_size as usize {
                        if i != self.current_slot as usize {
                            let entry = &mut self.entries[i];
                            if entry.marking == MarkingState::LongTerm
                                && entry.long_term_frame_idx == *long_term_frame_idx as i32
                            {
                                entry.marking = MarkingState::Unused;
                            }
                        }
                    }

                    // Mark current as long-term.
                    if self.current_slot >= 0 {
                        let entry = &mut self.entries[self.current_slot as usize];
                        entry.mark_long_term(*long_term_frame_idx as i32);
                    }
                }
            }
        }
    }

    /// DPB bumping process (C.4.5.3).
    fn dpb_bumping(&mut self) {
        // Find picture with smallest POC that needs output.
        let mut min_poc = i32::MAX;
        let mut min_idx: Option<usize> = None;

        for i in 0..self.max_dpb_size as usize {
            let entry = &self.entries[i];
            if entry.state == DpbState::InUse && entry.output && entry.pic_order_cnt < min_poc {
                min_poc = entry.pic_order_cnt;
                min_idx = Some(i);
            }
        }

        if let Some(idx) = min_idx {
            self.entries[idx].output = false;
            // If also not a reference, mark slot as empty.
            if !self.entries[idx].is_reference() {
                self.entries[idx].state = DpbState::Empty;
            }
        }
    }

    /// Fill frame_num gaps (8.2.5.2).
    pub fn fill_frame_num_gaps(&mut self, current_frame_num: u32, info: &PictureStartInfo) {
        if info.pic_type == PictureType::Idr {
            self.prev_ref_frame_num = 0;
        }

        if current_frame_num != self.prev_ref_frame_num {
            let mut unused_frame_num = (self.prev_ref_frame_num + 1) % self.max_frame_num;

            while unused_frame_num != current_frame_num {
                // Create non-existing frame.
                while self.is_full() {
                    self.dpb_bumping();
                }

                // Find empty slot.
                let mut slot: Option<usize> = None;
                for i in 0..self.max_dpb_size as usize {
                    if self.entries[i].state == DpbState::Empty {
                        slot = Some(i);
                        break;
                    }
                }

                if let Some(idx) = slot {
                    let entry = &mut self.entries[idx];
                    entry.state = DpbState::InUse;
                    entry.frame_num = unused_frame_num;
                    entry.not_existing = true;
                    entry.output = false;
                    entry.mark_short_term();
                }

                unused_frame_num = (unused_frame_num + 1) % self.max_frame_num;
            }
        }

        if info.is_reference {
            self.prev_ref_frame_num = current_frame_num;
        }
    }

    /// Get an entry by index.
    pub fn get_entry(&self, index: usize) -> Option<&DpbEntry> {
        if index < MAX_DPB_SIZE && self.entries[index].state == DpbState::InUse {
            Some(&self.entries[index])
        } else {
            None
        }
    }

    /// Get mutable entry by index.
    pub fn get_entry_mut(&mut self, index: usize) -> Option<&mut DpbEntry> {
        if index < MAX_DPB_SIZE && self.entries[index].state == DpbState::InUse {
            Some(&mut self.entries[index])
        } else {
            None
        }
    }
}

impl DecodedPictureBufferTrait for DpbH264 {
    fn sequence_start(&mut self, config: DpbConfig) {
        // Reset all entries.
        for entry in &mut self.entries {
            entry.reset();
        }

        self.max_dpb_size = std::cmp::min(config.dpb_size as i8, MAX_DPB_SIZE as i8);
        self.use_multiple_refs = config.use_multiple_references;
        self.max_long_term_frame_idx = -1;
        self.max_frame_num = config.max_frame_num();
        self.max_poc_lsb = config.max_pic_order_cnt_lsb();
        self.num_temporal_layers = config.num_temporal_layers;
        self.current_slot = -1;
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = 0;
        self.prev_frame_num_offset = 0;
        self.prev_frame_num = 0;
        self.prev_ref_frame_num = 0;
    }

    fn picture_start(&mut self, info: PictureStartInfo) -> i8 {
        // Handle IDR.
        if info.pic_type == PictureType::Idr {
            if !info.no_output_of_prior_pics_flag {
                while !self.is_empty() {
                    self.dpb_bumping();
                }
            }
            // Clear DPB for IDR.
            for entry in &mut self.entries {
                entry.state = DpbState::Empty;
                entry.mark_unused();
            }
            self.last_idr_timestamp = info.timestamp;
        }

        // Ensure space.
        while self.is_full() {
            self.dpb_bumping();
        }

        // Find empty slot.
        self.current_slot = -1;
        for i in 0..self.max_dpb_size as usize {
            if self.entries[i].state == DpbState::Empty {
                self.current_slot = i as i8;
                break;
            }
        }

        if self.current_slot < 0 {
            // Force bumping if no slot found.
            self.dpb_bumping();
            for i in 0..self.max_dpb_size as usize {
                if self.entries[i].state == DpbState::Empty {
                    self.current_slot = i as i8;
                    break;
                }
            }
        }

        if self.current_slot >= 0 {
            let entry = &mut self.entries[self.current_slot as usize];
            entry.state = DpbState::InUse;
            entry.frame_id = info.frame_id;
            entry.frame_num = info.frame_num;
            entry.pic_type = info.pic_type;
            entry.temporal_id = info.temporal_id as i32;
            entry.timestamp = info.timestamp;
            entry.output = info.pic_output_flag;
            entry.corrupted = false;
            entry.not_existing = false;

            // Calculate POC.
            self.calculate_poc(&info);

            // Calculate PicNum.
            self.calculate_pic_num(info.frame_num);
        }

        self.current_slot
    }

    fn picture_end(&mut self, is_reference: bool) {
        if self.current_slot < 0 {
            return;
        }

        let entry = &mut self.entries[self.current_slot as usize];

        // Mark as reference if needed.
        if is_reference {
            if entry.marking == MarkingState::Unused {
                entry.marking = MarkingState::ShortTerm;
            }
            if entry.bottom_field_marking == MarkingState::Unused {
                entry.bottom_field_marking = MarkingState::ShortTerm;
            }

            // Apply sliding window memory management to remove oldest reference.
            // if we exceed the max number of reference frames
            let num_short_term = self.num_short_term_refs();
            let num_long_term = self.num_long_term_refs();
            let max_refs = (self.max_dpb_size - 1) as u32; // Reserve one slot for current

            if (num_short_term + num_long_term) > max_refs && num_short_term > 0 {
                // Find short-term ref with lowest FrameNumWrap (oldest)
                let mut oldest_idx: Option<usize> = None;
                let mut lowest_frame_num_wrap = i32::MAX;

                for i in 0..self.max_dpb_size as usize {
                    let e = &self.entries[i];
                    if e.is_short_term_reference() && i as i8 != self.current_slot {
                        // frame_num_wrap is a rough approximation based on frame_num
                        let frame_num_wrap = e.frame_num as i32;
                        if frame_num_wrap < lowest_frame_num_wrap {
                            lowest_frame_num_wrap = frame_num_wrap;
                            oldest_idx = Some(i);
                        }
                    }
                }

                if let Some(idx) = oldest_idx {
                    self.entries[idx].mark_unused();
                    if !self.entries[idx].output {
                        self.entries[idx].state = DpbState::Empty;
                    }
                }
            }
        } else {
            entry.mark_unused();
        }
    }

    fn current_slot(&self) -> i8 {
        self.current_slot
    }

    fn is_full(&self) -> bool {
        let mut count = 0;
        for i in 0..self.max_dpb_size as usize {
            if self.entries[i].state == DpbState::InUse {
                count += 1;
            }
        }
        count >= self.max_dpb_size as usize
    }

    fn is_empty(&self) -> bool {
        for i in 0..self.max_dpb_size as usize {
            if self.entries[i].state == DpbState::InUse {
                return false;
            }
        }
        true
    }

    fn flush(&mut self) {
        // Mark all as unused for reference.
        for entry in &mut self.entries {
            entry.mark_unused();
        }
        // Bump until empty.
        while !self.is_empty() {
            self.dpb_bumping();
        }
    }

    fn num_short_term_refs(&self) -> u32 {
        let mut count = 0;
        for i in 0..self.max_dpb_size as usize {
            if self.entries[i].is_short_term_reference() {
                count += 1;
            }
        }
        count
    }

    fn num_long_term_refs(&self) -> u32 {
        let mut count = 0;
        for i in 0..self.max_dpb_size as usize {
            if self.entries[i].is_long_term_reference() {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_h264_dpb_sequence_start() {
        let mut dpb = DpbH264::new();
        dpb.sequence_start(DpbConfig {
            dpb_size: 4,
            ..Default::default()
        });

        assert_eq!(dpb.max_dpb_size, 4);
        assert!(dpb.is_empty());
        assert!(!dpb.is_full());
    }

    #[test]
    fn test_h264_dpb_picture_start_end() {
        let mut dpb = DpbH264::new();
        dpb.sequence_start(DpbConfig {
            dpb_size: 4,
            ..Default::default()
        });

        // Add IDR.
        let slot = dpb.picture_start(PictureStartInfo {
            frame_id: 0,
            pic_order_cnt: 0,
            frame_num: 0,
            pic_type: PictureType::Idr,
            is_reference: true,
            ..Default::default()
        });
        assert_eq!(slot, 0);
        dpb.picture_end(true);

        assert!(!dpb.is_empty());
        assert_eq!(dpb.num_short_term_refs(), 1);
    }

    #[test]
    fn test_h264_dpb_ref_lists_p() {
        let mut dpb = DpbH264::new();
        dpb.sequence_start(DpbConfig {
            dpb_size: 4,
            ..Default::default()
        });

        // Add IDR.
        dpb.picture_start(PictureStartInfo {
            frame_id: 0,
            pic_order_cnt: 0,
            frame_num: 0,
            pic_type: PictureType::Idr,
            is_reference: true,
            ..Default::default()
        });
        dpb.picture_end(true);

        // Get ref list for P-frame.
        let list = dpb.get_ref_list_p();
        assert_eq!(list.count, 1);
        assert_eq!(list.refs[0].dpb_index, 0);
    }
}
