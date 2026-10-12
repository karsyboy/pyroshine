//! DPB types and configuration structures.

use super::{DEFAULT_DPB_SIZE, DEFAULT_MAX_NUM_REF_FRAMES};

/// Configuration for DPB initialization.
#[derive(Debug, Clone)]
pub struct DpbConfig {
    /// Maximum DPB size in frames.
    pub dpb_size: u32,
    /// Maximum number of reference frames.
    pub max_num_ref_frames: u32,
    /// Whether to use multiple references (enables full B-frame support).
    pub use_multiple_references: bool,
    /// Maximum number of long-term reference frames (0 to disable LTR).
    pub max_long_term_refs: u32,
    /// Log2 of max_frame_num (for H.264 frame_num calculation).
    pub log2_max_frame_num_minus4: u8,
    /// Log2 of max_pic_order_cnt_lsb (for POC calculation).
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    /// Number of temporal layers (for temporal SVC support).
    pub num_temporal_layers: u32,
}

impl Default for DpbConfig {
    fn default() -> Self {
        Self {
            dpb_size: DEFAULT_DPB_SIZE,
            max_num_ref_frames: DEFAULT_MAX_NUM_REF_FRAMES,
            use_multiple_references: true,
            max_long_term_refs: 0,
            log2_max_frame_num_minus4: 0,         // max_frame_num = 16
            log2_max_pic_order_cnt_lsb_minus4: 0, // max_poc_lsb = 16
            num_temporal_layers: 1,
        }
    }
}

impl DpbConfig {
    /// Get the maximum frame_num value.
    pub fn max_frame_num(&self) -> u32 {
        1 << (self.log2_max_frame_num_minus4 + 4)
    }

    /// Get the maximum POC LSB value.
    pub fn max_pic_order_cnt_lsb(&self) -> i32 {
        1 << (self.log2_max_pic_order_cnt_lsb_minus4 + 4)
    }
}

/// Information for starting a new picture.
#[derive(Debug, Clone)]
pub struct PictureStartInfo {
    /// Unique frame ID (internal tracking).
    pub frame_id: u64,
    /// Picture Order Count.
    pub pic_order_cnt: i32,
    /// Frame number (for H.264).
    pub frame_num: u32,
    /// Picture type (IDR, I, P, B).
    pub pic_type: super::PictureType,
    /// Temporal layer ID.
    pub temporal_id: u8,
    /// Timestamp for this picture.
    pub timestamp: u64,
    /// Whether this picture should be output.
    pub pic_output_flag: bool,
    /// Whether this is a reference picture.
    pub is_reference: bool,
    /// For H.264: whether to use long-term reference flag (IDR only).
    pub long_term_reference_flag: bool,
    /// For H.264: adaptive_ref_pic_marking_mode_flag.
    pub adaptive_ref_pic_marking_mode: bool,
    /// For H.264: no_output_of_prior_pics_flag.
    pub no_output_of_prior_pics_flag: bool,
}

impl Default for PictureStartInfo {
    fn default() -> Self {
        Self {
            frame_id: 0,
            pic_order_cnt: 0,
            frame_num: 0,
            pic_type: super::PictureType::I,
            temporal_id: 0,
            timestamp: 0,
            pic_output_flag: true,
            is_reference: true,
            long_term_reference_flag: false,
            adaptive_ref_pic_marking_mode: false,
            no_output_of_prior_pics_flag: false,
        }
    }
}

/// Reference Picture Set for H.265 encoding.
///
/// Contains indices into the DPB for reference pictures categorized by their.
/// position relative to the current picture in display order.
#[derive(Debug, Clone, Default)]
pub struct RefPicSet {
    /// Short-term references with POC before current (for L0, closest first).
    pub st_curr_before: [i8; super::MAX_REF_LIST_SIZE],
    /// Short-term references with POC after current (for L1, closest first).
    pub st_curr_after: [i8; super::MAX_REF_LIST_SIZE],
    /// Long-term references used by current picture.
    pub lt_curr: [i8; super::MAX_REF_LIST_SIZE],
    /// Short-term references not used by current but kept in DPB.
    pub st_foll: [i8; super::MAX_REF_LIST_SIZE],
    /// Long-term references not used by current but kept in DPB.
    pub lt_foll: [i8; super::MAX_REF_LIST_SIZE],
    /// Count of st_curr_before entries.
    pub num_st_curr_before: u8,
    /// Count of st_curr_after entries.
    pub num_st_curr_after: u8,
    /// Count of lt_curr entries.
    pub num_lt_curr: u8,
    /// Count of st_foll entries.
    pub num_st_foll: u8,
    /// Count of lt_foll entries.
    pub num_lt_foll: u8,
}

impl RefPicSet {
    /// Create a new empty reference picture set.
    pub fn new() -> Self {
        Self {
            st_curr_before: [-1; super::MAX_REF_LIST_SIZE],
            st_curr_after: [-1; super::MAX_REF_LIST_SIZE],
            lt_curr: [-1; super::MAX_REF_LIST_SIZE],
            st_foll: [-1; super::MAX_REF_LIST_SIZE],
            lt_foll: [-1; super::MAX_REF_LIST_SIZE],
            num_st_curr_before: 0,
            num_st_curr_after: 0,
            num_lt_curr: 0,
            num_st_foll: 0,
            num_lt_foll: 0,
        }
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.st_curr_before.fill(-1);
        self.st_curr_after.fill(-1);
        self.lt_curr.fill(-1);
        self.st_foll.fill(-1);
        self.lt_foll.fill(-1);
        self.num_st_curr_before = 0;
        self.num_st_curr_after = 0;
        self.num_lt_curr = 0;
        self.num_st_foll = 0;
        self.num_lt_foll = 0;
    }

    /// Get the total number of reference pictures used by the current picture.
    pub fn num_poc_total_curr(&self) -> u8 {
        self.num_st_curr_before + self.num_st_curr_after + self.num_lt_curr
    }
}

/// H.265 Short-term Reference Picture Set data.
#[derive(Debug, Clone, Default)]
pub struct ShortTermRefPicSet {
    /// Number of negative delta POC values.
    pub num_negative_pics: u8,
    /// Number of positive delta POC values.
    pub num_positive_pics: u8,
    /// Delta POC S0 minus 1 values.
    pub delta_poc_s0_minus1: [u8; super::MAX_REF_LIST_SIZE],
    /// Used by current pic S0 flag (bitmask).
    pub used_by_curr_pic_s0_flag: u16,
    /// Delta POC S1 minus 1 values.
    pub delta_poc_s1_minus1: [u8; super::MAX_REF_LIST_SIZE],
    /// Used by current pic S1 flag (bitmask).
    pub used_by_curr_pic_s1_flag: u16,
    /// Whether to use inter_ref_pic_set_prediction_flag.
    pub inter_ref_pic_set_prediction_flag: bool,
}

/// H.264 Memory Management Control Operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmcoOperation {
    /// End of MMCO list.
    End,
    /// Mark short-term picture as unused for reference.
    UnmarkShortTerm {
        /// difference_of_pic_nums_minus1.
        difference_of_pic_nums_minus1: u32,
    },
    /// Mark long-term picture as unused for reference.
    UnmarkLongTerm {
        /// long_term_pic_num.
        long_term_pic_num: u32,
    },
    /// Assign long-term frame index to short-term reference.
    MarkLongTerm {
        /// difference_of_pic_nums_minus1.
        difference_of_pic_nums_minus1: u32,
        /// long_term_frame_idx.
        long_term_frame_idx: u32,
    },
    /// Set max long-term frame index.
    SetMaxLongTermIndex {
        /// max_long_term_frame_idx_plus1.
        max_long_term_frame_idx_plus1: u32,
    },
    /// Mark all reference pictures as unused.
    UnmarkAll,
    /// Mark current picture as long-term.
    MarkCurrentAsLongTerm {
        /// long_term_frame_idx.
        long_term_frame_idx: u32,
    },
}

/// H.264 reference picture list modification entry.
#[derive(Debug, Clone, Copy)]
pub struct RefListModEntry {
    /// Modification operation (0, 1, 2, 3).
    pub modification_of_pic_nums_idc: u8,
    /// abs_diff_pic_num_minus1 or long_term_pic_num.
    pub value: u32,
}

/// H.264 POC type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PocType {
    /// POC type 0 (explicit signaling via pic_order_cnt_lsb).
    #[default]
    Type0,
    /// POC type 1 (delta POC from frame_num).
    Type1,
    /// POC type 2 (derived from frame_num).
    Type2,
}
