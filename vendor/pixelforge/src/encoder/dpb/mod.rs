//! Decoded Picture Buffer (DPB) management for video encoding.
//!
//! This module provides full-featured DPB management for H.264 and H.265 video encoding,
//! including:
//! - B-frame support with categorization of frames before/after current frame in display order
//! - Complex GOP structures with hierarchical B-frames and multiple reference frames
//! - Long-term reference (LTR) support for keeping golden frames in the DPB
//! - Bitstream compliance for correctly populating slice headers and parameter sets
//!
//! The DPB is codec-agnostic at the interface level, with codec-specific implementations.
//! for H.264 and H.265 in separate modules.

mod entry;
mod h264;
mod h265;
mod reference_lists;
mod types;

pub use entry::{DpbEntry, MarkingState};
pub use h264::DpbH264;
pub use h265::DpbH265;
pub use reference_lists::{ReferenceList, ReferencePicture};
pub use types::*;

/// Maximum DPB size (16 slots for H.264, 15 for H.265, use 16 for both).
pub const MAX_DPB_SIZE: usize = 16;

/// Alias for MAX_DPB_SIZE for backwards compatibility.
pub const MAX_DPB_SLOTS: usize = MAX_DPB_SIZE;

/// Maximum number of reference pictures in a list.
pub const MAX_REF_LIST_SIZE: usize = 16;

/// Default DPB size for encoding.
pub const DEFAULT_DPB_SIZE: u32 = 4;

/// Default maximum number of reference frames.
pub const DEFAULT_MAX_NUM_REF_FRAMES: u32 = 4;

/// Picture type for encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureType {
    /// IDR picture (Instantaneous Decoder Refresh).
    Idr,
    /// I picture (Intra).
    I,
    /// P picture (Predicted, uses L0 references).
    P,
    /// B picture (Bi-predicted, uses L0 and L1 references).
    B,
}

impl PictureType {
    /// Returns true if this is an IRAP (Intra Random Access Point) picture.
    pub fn is_irap(&self) -> bool {
        matches!(self, Self::Idr | Self::I)
    }

    /// Returns true if this picture type uses reference pictures.
    pub fn uses_references(&self) -> bool {
        matches!(self, Self::P | Self::B)
    }

    /// Returns true if this is a reference picture (can be used by other frames).
    pub fn is_reference(&self) -> bool {
        // B-frames can also be references in hierarchical B-frame structures.
        // This is determined by the encoder, not the picture type alone.
        matches!(self, Self::Idr | Self::I | Self::P)
    }

    /// Returns true if this is a B picture.
    pub fn is_b_frame(&self) -> bool {
        matches!(self, Self::B)
    }
}

/// Common DPB interface for both H.264 and H.265.
///
/// This trait provides a codec-agnostic interface for DPB operations.
pub trait DecodedPictureBufferTrait {
    /// Initialize the DPB for a new sequence.
    fn sequence_start(&mut self, config: DpbConfig);

    /// Start encoding a new picture.
    ///
    /// Returns the DPB slot index allocated for this picture.
    fn picture_start(&mut self, info: PictureStartInfo) -> i8;

    /// End encoding a picture and update the DPB.
    fn picture_end(&mut self, is_reference: bool);

    /// Get the current DPB slot index.
    fn current_slot(&self) -> i8;

    /// Check if the DPB is full.
    fn is_full(&self) -> bool;

    /// Check if the DPB is empty.
    fn is_empty(&self) -> bool;

    /// Flush all frames from the DPB.
    fn flush(&mut self);

    /// Get the number of short-term reference frames.
    fn num_short_term_refs(&self) -> u32;

    /// Get the number of long-term reference frames.
    fn num_long_term_refs(&self) -> u32;
}

/// Unified DPB that works for both H.264 and H.265.
///
/// This provides a simple interface for encoders while delegating.
/// to codec-specific implementations internally.
pub struct DecodedPictureBuffer {
    /// H.264-specific DPB (used when codec is H.264).
    pub h264: DpbH264,
    /// H.265-specific DPB (used when codec is H.265).
    pub h265: DpbH265,
}

impl DecodedPictureBuffer {
    /// Create a new DPB.
    pub fn new() -> Self {
        Self {
            h264: DpbH264::new(),
            h265: DpbH265::new(),
        }
    }
}

impl Default for DecodedPictureBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_picture_type() {
        assert!(PictureType::Idr.is_irap());
        assert!(PictureType::I.is_irap());
        assert!(!PictureType::P.is_irap());
        assert!(!PictureType::B.is_irap());

        assert!(!PictureType::Idr.uses_references());
        assert!(!PictureType::I.uses_references());
        assert!(PictureType::P.uses_references());
        assert!(PictureType::B.uses_references());
    }
}
