//! Storage for the structures one H.264 decode command is recorded from.
//!
//! NVIDIA's decoder does not copy `VkVideoDecodeInfoKHR` and the structures
//! hanging off it when the command is recorded: it reads them again at
//! `vkQueueSubmit`. They therefore have to stay alive and unmoved until the
//! submission has finished, which a stack frame in the recording function does
//! not guarantee. Left on the stack they are garbage by submit time, which
//! crashed the driver or made it decode B-pictures from stale reference data.
//!
//! A [`DecodeRecord`] owns all of them. There is one per pipeline slot, and it
//! is rebuilt only once that slot's previous submission has completed.

use ash::vk;
use ash::vk::TaggedStructure;
use ash::vk::native::{StdVideoDecodeH264PictureInfo, StdVideoDecodeH264ReferenceInfo};

use super::dpb::MAX_DPB_SLOTS;

/// Most references one picture can have: every DPB slot but the one being set up.
const MAX_REFS: usize = MAX_DPB_SLOTS;

/// Everything one decode command points at.
///
/// The Vulkan structures here point at each other and at the plain data beside
/// them, so the whole is a self-referential value. That is sound because it lives in
/// heap storage it is never moved out of once built, and every pointer is rebuilt
/// by [`DecodeRecord::link`] after the last write to what it points at. The
/// `'static` lifetimes on the fields are internal: nothing outside this module
/// can obtain a reference that outlives the record.
///
/// Every collection is a fixed-size array holding `ref_count` live entries, so
/// nothing is ever reallocated and an element's address does not change however
/// the record is filled.
pub(super) struct DecodeRecord {
    // Plain data.
    slice_offsets: Vec<u32>,
    ref_count: usize,
    /// DPB slot and image view of each reference.
    refs: [(u8, vk::ImageView); MAX_REFS],
    ref_std: [StdVideoDecodeH264ReferenceInfo; MAX_REFS],
    setup_std: StdVideoDecodeH264ReferenceInfo,
    pic_std: StdVideoDecodeH264PictureInfo,

    // Picture resources.
    ref_resources: [vk::VideoPictureResourceInfoKHR<'static>; MAX_REFS],
    setup_resource: vk::VideoPictureResourceInfoKHR<'static>,
    output_resource: vk::VideoPictureResourceInfoKHR<'static>,

    // Codec-specific slot info. The begin and decode commands each chain their
    // own copy, since chaining takes the structure mutably.
    begin_dpb: [vk::VideoDecodeH264DpbSlotInfoKHR<'static>; MAX_REFS],
    decode_dpb: [vk::VideoDecodeH264DpbSlotInfoKHR<'static>; MAX_REFS],
    setup_dpb: vk::VideoDecodeH264DpbSlotInfoKHR<'static>,

    // Reference slots. `begin_slots` has room for the slot being set up too.
    begin_slots: [vk::VideoReferenceSlotInfoKHR<'static>; MAX_REFS + 1],
    decode_slots: [vk::VideoReferenceSlotInfoKHR<'static>; MAX_REFS],
    setup_slot: vk::VideoReferenceSlotInfoKHR<'static>,

    // Top level.
    pic_info: vk::VideoDecodeH264PictureInfoKHR<'static>,
    begin_info: vk::VideoBeginCodingInfoKHR<'static>,
    decode_info: vk::VideoDecodeInfoKHR<'static>,
}

// SAFETY: the raw pointers inside the Vulkan structures only ever point into
// the same record, which is owned by one decoder and used from one thread at a
// time.
unsafe impl Send for DecodeRecord {}

/// What [`DecodeRecord::link`] needs besides the references already pushed.
pub(super) struct LinkParams {
    pub extent: vk::Extent2D,
    pub session: vk::VideoSessionKHR,
    pub session_params: vk::VideoSessionParametersKHR,
    pub bitstream: vk::Buffer,
    pub bitstream_range: u64,
    /// DPB slot, and view of it, that the picture is reconstructed into.
    pub setup_slot: u8,
    pub setup_view: vk::ImageView,
    /// The decode destination when it is not the DPB slot itself.
    pub output_view: Option<vk::ImageView>,
    pub setup_std: StdVideoDecodeH264ReferenceInfo,
    pub pic_std: StdVideoDecodeH264PictureInfo,
}

/// Extend a borrow to any lifetime.
///
/// # Safety
/// `value` must stay alive and in place for as long as the result is used.
unsafe fn stable<'a, T: ?Sized>(value: &T) -> &'a T {
    unsafe { &*(value as *const T) }
}

/// As [`stable`], for a mutable borrow.
///
/// # Safety
/// As [`stable`], and nothing else may access `value` while the result is used.
unsafe fn stable_mut<'a, T: ?Sized>(value: &mut T) -> &'a mut T {
    unsafe { &mut *(value as *mut T) }
}

impl DecodeRecord {
    pub(super) fn new() -> Self {
        // SAFETY: the standard structures are plain C data, for which all-zero
        // is a valid (if meaningless) value.
        let zeroed_ref: StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        let (setup_std, pic_std) = (zeroed_ref, unsafe { std::mem::zeroed() });
        Self {
            slice_offsets: Vec::new(),
            ref_count: 0,
            refs: [(0, vk::ImageView::null()); MAX_REFS],
            ref_std: [zeroed_ref; MAX_REFS],
            setup_std,
            pic_std,
            ref_resources: [Default::default(); MAX_REFS],
            setup_resource: Default::default(),
            output_resource: Default::default(),
            begin_dpb: [Default::default(); MAX_REFS],
            decode_dpb: [Default::default(); MAX_REFS],
            setup_dpb: Default::default(),
            begin_slots: [Default::default(); MAX_REFS + 1],
            decode_slots: [Default::default(); MAX_REFS],
            setup_slot: Default::default(),
            pic_info: Default::default(),
            begin_info: Default::default(),
            decode_info: Default::default(),
        }
    }

    /// Start a new picture, discarding the last one.
    ///
    /// Only call once the submission that used this record has completed.
    pub(super) fn reset(&mut self, slice_offsets: &[u32]) {
        self.slice_offsets.clear();
        self.slice_offsets.extend_from_slice(slice_offsets);
        self.ref_count = 0;
    }

    /// Add a reference picture, in the order the driver is to see it.
    pub(super) fn push_reference(
        &mut self,
        slot: u8,
        view: vk::ImageView,
        std: StdVideoDecodeH264ReferenceInfo,
    ) {
        assert!(self.ref_count < MAX_REFS, "too many H.264 references");
        self.refs[self.ref_count] = (slot, view);
        self.ref_std[self.ref_count] = std;
        self.ref_count += 1;
    }

    /// Build the structures from what has been pushed, wiring up every pointer.
    ///
    /// Order matters: the plain data and the structures pointed at are written
    /// before the structures that point at them.
    pub(super) fn link(&mut self, p: &LinkParams) {
        let n = self.ref_count;
        self.setup_std = p.setup_std;
        self.pic_std = p.pic_std;

        let resource = |view: vk::ImageView| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(p.extent)
                .base_array_layer(0)
                .image_view_binding(view)
        };
        for i in 0..n {
            self.ref_resources[i] = resource(self.refs[i].1);
        }
        self.setup_resource = resource(p.setup_view);
        self.output_resource = match p.output_view {
            None => self.setup_resource,
            Some(view) => resource(view),
        };

        // SAFETY (for every `stable`/`stable_mut` below): the target is a field
        // of `self`, which is boxed and not written again until the next
        // `reset`/`link`, and fixed arrays never move their elements.
        unsafe {
            for i in 0..n {
                let info = vk::VideoDecodeH264DpbSlotInfoKHR::default()
                    .std_reference_info(stable(&self.ref_std[i]));
                self.begin_dpb[i] = info;
                self.decode_dpb[i] = info;
            }
            self.setup_dpb = vk::VideoDecodeH264DpbSlotInfoKHR::default()
                .std_reference_info(stable(&self.setup_std));

            for i in 0..n {
                let slot = self.refs[i].0 as i32;
                let resource = stable(&self.ref_resources[i]);
                self.begin_slots[i] = vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(slot)
                    .picture_resource(resource)
                    .push(stable_mut(&mut self.begin_dpb[i]));
                self.decode_slots[i] = vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(slot)
                    .picture_resource(resource)
                    .push(stable_mut(&mut self.decode_dpb[i]));
            }
            // The slot being reconstructed is listed to `begin` with slot index
            // -1: it is not active until the decode's setup slot activates it,
            // and a non-negative index here must name an already-active slot
            // (VUID-vkCmdBeginVideoCodingKHR-slotIndex-07239). -1 still binds
            // its picture resource.
            self.begin_slots[n] = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(-1)
                .picture_resource(stable(&self.setup_resource));
            self.setup_slot = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(p.setup_slot as i32)
                .picture_resource(stable(&self.setup_resource))
                .push(stable_mut(&mut self.setup_dpb));

            self.pic_info = vk::VideoDecodeH264PictureInfoKHR::default()
                .std_picture_info(stable(&self.pic_std))
                .slice_offsets(stable(&self.slice_offsets[..]));

            self.begin_info = vk::VideoBeginCodingInfoKHR::default()
                .video_session(p.session)
                .video_session_parameters(p.session_params)
                .reference_slots(stable(&self.begin_slots[..n + 1]));
            self.decode_info = vk::VideoDecodeInfoKHR::default()
                .src_buffer(p.bitstream)
                .src_buffer_offset(0)
                .src_buffer_range(p.bitstream_range)
                .dst_picture_resource(self.output_resource)
                .setup_reference_slot(stable(&self.setup_slot))
                .reference_slots(stable(&self.decode_slots[..n]))
                .push(stable_mut(&mut self.pic_info));
        }
    }

    /// The `vkCmdBeginVideoCodingKHR` argument. Valid after [`Self::link`].
    pub(super) fn begin_info(&self) -> &vk::VideoBeginCodingInfoKHR<'_> {
        &self.begin_info
    }

    /// The `vkCmdDecodeVideoKHR` argument. Valid after [`Self::link`].
    pub(super) fn decode_info(&self) -> &vk::VideoDecodeInfoKHR<'_> {
        &self.decode_info
    }
}
