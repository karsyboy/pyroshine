//! Frame reordering buffer for B-frame support.
//!
//! This module handles frame reordering for video encoding with B-frames.
//! Frames arrive in display order but must be encoded in a different order.
//! because B-frames reference both past and future frames.
//!
//! This is codec-agnostic and can be used for both H.264 and H.265 encoding.
//!
//! For example, with 1 B-frame between P-frames:
//! - Display order: I(0), B(1), P(2), B(3), P(4), ...
//! - Encode order:  I(0), P(2), B(1), P(4), B(3), ...
//!
//! The P-frame must be encoded before the B-frame that references it.

use super::gop::GopFrameType;

/// Represents a buffered frame waiting to be encoded.
#[derive(Clone)]
pub struct BufferedFrame {
    /// Raw frame data (YUV).
    pub data: Vec<u8>,
    /// Display order (input order).
    pub display_order: u64,
    /// Frame type determined by GOP structure.
    pub frame_type: GopFrameType,
    /// Picture Order Count for this frame.
    pub poc: i32,
    /// Whether this frame should be used as a reference.
    pub is_reference: bool,
}

/// Frame ready to be encoded (output from reorder buffer).
pub struct FrameToEncode {
    /// Raw frame data.
    pub data: Vec<u8>,
    /// Display order.
    pub display_order: u64,
    /// Encode order (order in which this frame is encoded).
    pub encode_order: u64,
    /// Frame type.
    pub frame_type: GopFrameType,
    /// POC.
    pub poc: i32,
    /// Whether this is a reference frame.
    pub is_reference: bool,
    /// For B-frames: display order of the forward reference (L0).
    pub forward_ref_display_order: Option<u64>,
    /// For B-frames: display order of the backward reference (L1).
    pub backward_ref_display_order: Option<u64>,
}

/// Frame reorder buffer for B-frame encoding.
///
/// Handles the reordering of frames from display order to encode order.
/// This is codec-agnostic and works for both H.264 and H.265 encoding.
pub struct FrameReorderBuffer {
    /// Number of consecutive B-frames between anchor frames.
    b_frame_count: u32,
    /// Buffer holding frames waiting to be encoded.
    buffer: Vec<BufferedFrame>,
    /// Next encode order to assign.
    next_encode_order: u64,
    /// Display order of the last anchor frame (I or P).
    last_anchor_display_order: Option<u64>,
    /// Whether we're flushing (no more input frames).
    flushing: bool,
}

impl FrameReorderBuffer {
    /// Create a new frame reorder buffer.
    ///
    /// # Arguments
    /// * `b_frame_count` - Number of consecutive B-frames between I/P frames.
    pub fn new(b_frame_count: u32) -> Self {
        Self {
            b_frame_count,
            buffer: Vec::with_capacity((b_frame_count + 2) as usize),
            next_encode_order: 0,
            last_anchor_display_order: None,
            flushing: false,
        }
    }

    /// Submit a frame in display order.
    ///
    /// Returns frames ready to encode (may be 0 or more).
    pub fn submit_frame(
        &mut self,
        data: Vec<u8>,
        display_order: u64,
        frame_type: GopFrameType,
        poc: i32,
        is_reference: bool,
    ) -> Vec<FrameToEncode> {
        // Add frame to buffer.
        self.buffer.push(BufferedFrame {
            data,
            display_order,
            frame_type,
            poc,
            is_reference,
        });

        // Get frames ready to encode.
        self.get_frames_to_encode()
    }

    /// Flush remaining frames when encoding ends.
    ///
    /// Call this when there are no more input frames to get any remaining.
    /// buffered frames.
    pub fn flush(&mut self) -> Vec<FrameToEncode> {
        self.flushing = true;
        self.get_frames_to_encode()
    }

    /// Get frames that are ready to be encoded.
    fn get_frames_to_encode(&mut self) -> Vec<FrameToEncode> {
        if self.b_frame_count == 0 {
            // No B-frames: just output frames in order.
            return self.drain_all_frames();
        }

        let mut result = Vec::new();

        // For B-frame encoding, we need to wait for the anchor frame.
        // before we can encode the B-frames that precede it.
        //
        // Pattern: I, B, B, P, B, B, P, ...
        // When we receive P, we can encode P first, then the B-frames before it.

        loop {
            // Find if we have an anchor frame (I or P) after some B-frames.
            let anchor_pos = self.buffer.iter().position(|f| {
                f.frame_type == GopFrameType::Idr
                    || f.frame_type == GopFrameType::I
                    || f.frame_type == GopFrameType::P
            });

            match anchor_pos {
                Some(0) => {
                    // First frame is anchor - output it immediately.
                    let frame = self.buffer.remove(0);
                    let encode_order = self.next_encode_order;
                    self.next_encode_order += 1;
                    self.last_anchor_display_order = Some(frame.display_order);

                    result.push(FrameToEncode {
                        data: frame.data,
                        display_order: frame.display_order,
                        encode_order,
                        frame_type: frame.frame_type,
                        poc: frame.poc,
                        is_reference: frame.is_reference,
                        forward_ref_display_order: None,
                        backward_ref_display_order: None,
                    });
                }
                Some(pos) if pos > 0 => {
                    // We have B-frames followed by an anchor.
                    // First, output the anchor (it needs to be encoded before B-frames)
                    let anchor = self.buffer.remove(pos);
                    let anchor_display_order = anchor.display_order;
                    let anchor_encode_order = self.next_encode_order;
                    self.next_encode_order += 1;

                    result.push(FrameToEncode {
                        data: anchor.data,
                        display_order: anchor.display_order,
                        encode_order: anchor_encode_order,
                        frame_type: anchor.frame_type,
                        poc: anchor.poc,
                        is_reference: anchor.is_reference,
                        forward_ref_display_order: self.last_anchor_display_order,
                        backward_ref_display_order: None,
                    });

                    // Now output the B-frames that were before the anchor.
                    // They reference the previous anchor (L0) and this anchor (L1)
                    let forward_ref = self.last_anchor_display_order;
                    let backward_ref = Some(anchor_display_order);

                    // Drain the B-frames (now at positions 0..pos-1 since we removed anchor)
                    for _ in 0..(pos) {
                        if self.buffer.is_empty() {
                            break;
                        }
                        let b_frame = self.buffer.remove(0);
                        let encode_order = self.next_encode_order;
                        self.next_encode_order += 1;

                        result.push(FrameToEncode {
                            data: b_frame.data,
                            display_order: b_frame.display_order,
                            encode_order,
                            frame_type: b_frame.frame_type,
                            poc: b_frame.poc,
                            is_reference: b_frame.is_reference,
                            forward_ref_display_order: forward_ref,
                            backward_ref_display_order: backward_ref,
                        });
                    }

                    self.last_anchor_display_order = Some(anchor_display_order);
                }
                None if self.flushing && !self.buffer.is_empty() => {
                    // Flushing: output remaining B-frames.
                    // They can only reference the previous anchor (L0 only)
                    let forward_ref = self.last_anchor_display_order;

                    while !self.buffer.is_empty() {
                        let frame = self.buffer.remove(0);
                        let encode_order = self.next_encode_order;
                        self.next_encode_order += 1;

                        result.push(FrameToEncode {
                            data: frame.data,
                            display_order: frame.display_order,
                            encode_order,
                            frame_type: frame.frame_type,
                            poc: frame.poc,
                            is_reference: frame.is_reference,
                            forward_ref_display_order: forward_ref,
                            // No backward ref available when flushing.
                            backward_ref_display_order: None,
                        });
                    }
                }
                _ => break, // No complete group ready
            }
        }

        result
    }

    /// Drain all frames without B-frame reordering.
    fn drain_all_frames(&mut self) -> Vec<FrameToEncode> {
        self.buffer
            .drain(..)
            .map(|frame| {
                let encode_order = self.next_encode_order;
                self.next_encode_order += 1;

                let forward_ref = if frame.frame_type == GopFrameType::P {
                    self.last_anchor_display_order
                } else {
                    None
                };

                if frame.frame_type.is_reference() {
                    self.last_anchor_display_order = Some(frame.display_order);
                }

                FrameToEncode {
                    data: frame.data,
                    display_order: frame.display_order,
                    encode_order,
                    frame_type: frame.frame_type,
                    poc: frame.poc,
                    is_reference: frame.is_reference,
                    forward_ref_display_order: forward_ref,
                    backward_ref_display_order: None,
                }
            })
            .collect()
    }

    /// Check if buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get number of buffered frames.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_b_frames() {
        let mut buffer = FrameReorderBuffer::new(0);

        // Submit I, P, P.
        let frames = buffer.submit_frame(vec![1], 0, GopFrameType::Idr, 0, true);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].display_order, 0);
        assert_eq!(frames[0].encode_order, 0);

        let frames = buffer.submit_frame(vec![2], 1, GopFrameType::P, 2, true);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].display_order, 1);
        assert_eq!(frames[0].encode_order, 1);
    }

    #[test]
    fn test_one_b_frame() {
        let mut buffer = FrameReorderBuffer::new(1);

        // Submit I, B, P in display order.
        let frames = buffer.submit_frame(vec![1], 0, GopFrameType::Idr, 0, true);
        // I is output immediately.
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_type, GopFrameType::Idr);
        assert_eq!(frames[0].encode_order, 0);

        // Submit B - should buffer.
        let frames = buffer.submit_frame(vec![2], 1, GopFrameType::B, 2, false);
        assert_eq!(frames.len(), 0); // B is buffered

        // Submit P - should output P then B.
        let frames = buffer.submit_frame(vec![3], 2, GopFrameType::P, 4, true);
        assert_eq!(frames.len(), 2);
        // First: P (encode order 1)
        assert_eq!(frames[0].frame_type, GopFrameType::P);
        assert_eq!(frames[0].display_order, 2);
        assert_eq!(frames[0].encode_order, 1);
        // Second: B (encode order 2)
        assert_eq!(frames[1].frame_type, GopFrameType::B);
        assert_eq!(frames[1].display_order, 1);
        assert_eq!(frames[1].encode_order, 2);
        assert_eq!(frames[1].forward_ref_display_order, Some(0)); // References I
        assert_eq!(frames[1].backward_ref_display_order, Some(2)); // References P
    }
}
