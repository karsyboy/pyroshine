//! H.264 High profile (4:2:0 8-bit) sequence/picture headers and VA-API
//! parameter buffers for a low-latency stream: IDR then P frames, one
//! reference, no reordering (ITU-T H.264).
//!
//! The SPS and PPS are packed here so the VUI carries the stream's color
//! description. Slice headers are packed too where the driver takes them:
//! Mesa derives the slice NAL header and frame numbering from the packed
//! slice header and writes no SPS/PPS without one. The parameter buffers
//! mirror every field of the packed headers.

use super::bits::{BitWriter, nal_unit};
use super::sys::*;
use super::{Frame, Setup};

const MB: u32 = 16;
/// `log2_max_frame_num_minus4`: frame_num wraps at 256.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;
const PROFILE_HIGH: u32 = 100;
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;

/// Level limits (Table A-1): level_idc, MaxMBPS, MaxFS.
const LEVELS: [(u8, u64, u64); 12] = [
	(30, 40_500, 1_620),
	(31, 108_000, 3_600),
	(32, 216_000, 5_120),
	(40, 245_760, 8_192),
	(41, 245_760, 8_192),
	(42, 522_240, 8_704),
	(50, 589_824, 22_080),
	(51, 983_040, 36_864),
	(52, 2_073_600, 36_864),
	(60, 4_177_920, 139_264),
	(61, 8_355_840, 139_264),
	(62, 16_711_680, 139_264),
];

/// The lowest level whose frame size and macroblock rate fit. Bitrate limits
/// are not applied (streams exceed them, like other low-latency encoders).
pub(super) fn level_idc(width_mbs: u32, height_mbs: u32, fps: u32) -> u8 {
	let frame = u64::from(width_mbs) * u64::from(height_mbs);
	let rate = frame * u64::from(fps);
	LEVELS
		.iter()
		.find(|(_, max_rate, max_frame)| frame <= *max_frame && rate <= *max_rate)
		.map_or(62, |(level, _, _)| *level)
}

fn frame_num(index_since_idr: u64) -> u32 {
	(index_since_idr % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4))) as u32
}

fn idr_pic_id(idr_count: u64) -> u32 {
	(idr_count & 0xffff) as u32
}

pub(super) struct H264 {
	width_mbs: u32,
	height_mbs: u32,
	level: u8,
}

impl H264 {
	pub(super) fn new(setup: &Setup) -> Self {
		let width_mbs = setup.coded_width / MB;
		let height_mbs = setup.coded_height / MB;
		Self {
			width_mbs,
			height_mbs,
			level: level_idc(width_mbs, height_mbs, setup.fps),
		}
	}

	/// Packed SPS and PPS NAL units.
	pub(super) fn headers(&self, setup: &Setup) -> (Vec<u8>, Vec<u8>) {
		(self.sps(setup), self.pps())
	}

	fn sps(&self, setup: &Setup) -> Vec<u8> {
		let mut w = BitWriter::default();
		w.u(8, PROFILE_HIGH);
		w.u(8, 0); // constraint_set0..5_flag, reserved_zero_2bits
		w.u(8, u32::from(self.level));
		w.ue(0); // seq_parameter_set_id
		w.ue(1); // chroma_format_idc: 4:2:0
		w.ue(0); // bit_depth_luma_minus8
		w.ue(0); // bit_depth_chroma_minus8
		w.flag(false); // qpprime_y_zero_transform_bypass_flag
		w.flag(false); // seq_scaling_matrix_present_flag
		w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
		w.ue(2); // pic_order_cnt_type: derived from frame_num (no reordering)
		w.ue(1); // max_num_ref_frames
		w.flag(false); // gaps_in_frame_num_value_allowed_flag
		w.ue(self.width_mbs - 1);
		w.ue(self.height_mbs - 1); // pic_height_in_map_units_minus1
		w.flag(true); // frame_mbs_only_flag
		w.flag(true); // direct_8x8_inference_flag
		let (crop_right, crop_bottom) = setup.crop();
		let cropping = crop_right != 0 || crop_bottom != 0;
		w.flag(cropping);
		if cropping {
			// CropUnitX/Y are 2 for 4:2:0 progressive frames.
			w.ue(0);
			w.ue(crop_right / 2);
			w.ue(0);
			w.ue(crop_bottom / 2);
		}
		w.flag(true); // vui_parameters_present_flag
		self.vui(&mut w, setup);
		w.trailing_bits();
		nal_unit(&[0x67], &w.into_bytes())
	}

	fn vui(&self, w: &mut BitWriter, setup: &Setup) {
		w.flag(true); // aspect_ratio_info_present_flag
		w.u(8, 1); // aspect_ratio_idc: 1:1
		w.flag(false); // overscan_info_present_flag
		w.flag(true); // video_signal_type_present_flag
		w.u(3, 5); // video_format: unspecified
		w.flag(setup.color.full_range);
		w.flag(true); // colour_description_present_flag
		w.u(8, u32::from(setup.color.color_primaries));
		w.u(8, u32::from(setup.color.transfer_characteristics));
		w.u(8, u32::from(setup.color.matrix_coefficients));
		w.flag(false); // chroma_loc_info_present_flag
		w.flag(true); // timing_info_present_flag
		w.u(32, 1); // num_units_in_tick
		w.u(32, 2 * setup.fps); // time_scale (two ticks per frame)
		w.flag(false); // fixed_frame_rate_flag
		w.flag(false); // nal_hrd_parameters_present_flag
		w.flag(false); // vcl_hrd_parameters_present_flag
		w.flag(false); // pic_struct_present_flag
		w.flag(true); // bitstream_restriction_flag
		w.flag(true); // motion_vectors_over_pic_boundaries_flag
		w.ue(0); // max_bytes_per_pic_denom
		w.ue(0); // max_bits_per_mb_denom
		w.ue(16); // log2_max_mv_length_horizontal
		w.ue(16); // log2_max_mv_length_vertical
		// Decoders may output every frame as soon as it is decoded.
		w.ue(0); // max_num_reorder_frames
		w.ue(1); // max_dec_frame_buffering
	}

	fn pps(&self) -> Vec<u8> {
		let mut w = BitWriter::default();
		w.ue(0); // pic_parameter_set_id
		w.ue(0); // seq_parameter_set_id
		w.flag(true); // entropy_coding_mode_flag: CABAC
		w.flag(false); // bottom_field_pic_order_in_frame_present_flag
		w.ue(0); // num_slice_groups_minus1
		w.ue(0); // num_ref_idx_l0_default_active_minus1
		w.ue(0); // num_ref_idx_l1_default_active_minus1
		w.flag(false); // weighted_pred_flag
		w.u(2, 0); // weighted_bipred_idc
		w.se(0); // pic_init_qp_minus26
		w.se(0); // pic_init_qs_minus26
		w.se(0); // chroma_qp_index_offset
		w.flag(true); // deblocking_filter_control_present_flag
		w.flag(false); // constrained_intra_pred_flag
		w.flag(false); // redundant_pic_cnt_present_flag
		w.flag(true); // transform_8x8_mode_flag
		w.flag(false); // pic_scaling_matrix_present_flag
		w.se(0); // second_chroma_qp_index_offset
		w.trailing_bits();
		nal_unit(&[0x68], &w.into_bytes())
	}

	/// Packed slice header NAL unit of `frame` (no slice data) and its length
	/// in bits; it matches the slice parameters of [`Self::frame`].
	pub(super) fn slice_header(&self, frame: &Frame) -> (Vec<u8>, u32) {
		let mut w = BitWriter::default();
		w.ue(0); // first_mb_in_slice
		w.ue(u32::from(if frame.idr { SLICE_TYPE_I } else { SLICE_TYPE_P }));
		w.ue(0); // pic_parameter_set_id
		w.u(LOG2_MAX_FRAME_NUM_MINUS4 + 4, frame_num(frame.index_since_idr));
		// frame_mbs_only_flag: no field_pic_flag; pic_order_cnt_type 2: no POC.
		if frame.idr {
			w.ue(idr_pic_id(frame.idr_count));
			// dec_ref_pic_marking(): no_output_of_prior_pics_flag, long_term_reference_flag
			w.flag(false);
			w.flag(false);
			w.se(0); // slice_qp_delta
		} else {
			w.flag(false); // num_ref_idx_active_override_flag: one reference
			w.flag(false); // ref_pic_list_modification_flag_l0
			w.flag(false); // dec_ref_pic_marking(): adaptive_ref_pic_marking_mode_flag
			w.ue(0); // cabac_init_idc
			w.se(0); // slice_qp_delta
		}
		w.ue(0); // disable_deblocking_filter_idc
		w.se(0); // slice_alpha_c0_offset_div2
		w.se(0); // slice_beta_offset_div2
		let (rbsp, padding) = w.into_padded_bytes();
		// nal_ref_idc 3 (IDR) or 2 (reference P frame).
		let nal = nal_unit(&[if frame.idr { 0x65 } else { 0x41 }], &rbsp);
		let bits = (nal.len() * 8) as u32 - padding;
		(nal, bits)
	}

	pub(super) fn sequence(&self, setup: &Setup) -> VAEncSequenceParameterBufferH264 {
		let mut seq = VAEncSequenceParameterBufferH264 {
			level_idc: self.level,
			// Infinite GOP: IDR frames only when requested.
			intra_period: 0,
			intra_idr_period: 0,
			ip_period: 1,
			bits_per_second: setup.bitrate,
			max_num_ref_frames: 1,
			picture_width_in_mbs: self.width_mbs as u16,
			picture_height_in_mbs: self.height_mbs as u16,
			..Default::default()
		};
		// SAFETY: writing a plain-data bitfield view of a union.
		let fields = unsafe { &mut seq.seq_fields.bits };
		fields.set_chroma_format_idc(1);
		fields.set_frame_mbs_only_flag(1);
		fields.set_direct_8x8_inference_flag(1);
		fields.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM_MINUS4);
		fields.set_pic_order_cnt_type(2);
		let (crop_right, crop_bottom) = setup.crop();
		if crop_right != 0 || crop_bottom != 0 {
			seq.frame_cropping_flag = 1;
			seq.frame_crop_right_offset = crop_right / 2;
			seq.frame_crop_bottom_offset = crop_bottom / 2;
		}
		seq.vui_parameters_present_flag = 1;
		// SAFETY: as above.
		let vui = unsafe { &mut seq.vui_fields.bits };
		vui.set_aspect_ratio_info_present_flag(1);
		vui.set_timing_info_present_flag(1);
		vui.set_bitstream_restriction_flag(1);
		vui.set_log2_max_mv_length_horizontal(16);
		vui.set_log2_max_mv_length_vertical(16);
		vui.set_motion_vectors_over_pic_boundaries_flag(1);
		seq.aspect_ratio_idc = 1;
		seq.num_units_in_tick = 1;
		seq.time_scale = 2 * setup.fps;
		seq
	}

	fn picture(surface: VASurfaceID, frame_num: u32, poc: i32) -> VAPictureH264 {
		VAPictureH264 {
			picture_id: surface,
			frame_idx: frame_num,
			flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
			TopFieldOrderCnt: poc,
			BottomFieldOrderCnt: poc,
			..Default::default()
		}
	}

	fn invalid() -> VAPictureH264 {
		VAPictureH264 {
			picture_id: VA_INVALID_SURFACE,
			flags: VA_PICTURE_H264_INVALID,
			..Default::default()
		}
	}

	/// Picture and slice parameters of one frame.
	pub(super) fn frame(
		&self,
		frame: &Frame,
		coded: VABufferID,
	) -> (VAEncPictureParameterBufferH264, VAEncSliceParameterBufferH264) {
		let number = frame_num(frame.index_since_idr);
		let poc = (2 * frame.index_since_idr) as i32;
		let mut picture = VAEncPictureParameterBufferH264 {
			CurrPic: Self::picture(frame.recon, number, poc),
			ReferenceFrames: [Self::invalid(); 16],
			coded_buf: coded,
			frame_num: number as u16,
			pic_init_qp: 26,
			..Default::default()
		};
		let reference = frame
			.reference
			.map(|(surface, index)| Self::picture(surface, frame_num(index), (2 * index) as i32));
		if let Some(reference) = reference {
			picture.ReferenceFrames[0] = reference;
		}
		// SAFETY: writing a plain-data bitfield view of a union.
		let fields = unsafe { &mut picture.pic_fields.bits };
		fields.set_idr_pic_flag(u32::from(frame.idr));
		fields.set_reference_pic_flag(1);
		fields.set_entropy_coding_mode_flag(1);
		fields.set_transform_8x8_mode_flag(1);
		fields.set_deblocking_filter_control_present_flag(1);

		let mut slice = VAEncSliceParameterBufferH264 {
			num_macroblocks: self.width_mbs * self.height_mbs,
			macroblock_info: VA_INVALID_ID,
			slice_type: if frame.idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
			idr_pic_id: idr_pic_id(frame.idr_count) as u16,
			RefPicList0: [Self::invalid(); 32],
			RefPicList1: [Self::invalid(); 32],
			..Default::default()
		};
		if let (false, Some(reference)) = (frame.idr, reference) {
			slice.RefPicList0[0] = reference;
		}
		(picture, slice)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn level_follows_frame_size_and_rate() {
		// 1080p60: 8160 MBs, 489600 MB/s -> 4.2
		assert_eq!(level_idc(120, 68, 60), 42);
		// 1440p120: 14400 MBs, 1728000 MB/s -> 5.2
		assert_eq!(level_idc(160, 90, 120), 52);
		// 2160p60 -> 5.2; 2160p120 -> 6.0
		assert_eq!(level_idc(240, 135, 60), 52);
		assert_eq!(level_idc(240, 135, 120), 60);
		// Beyond every limit: the highest level.
		assert_eq!(level_idc(1000, 1000, 240), 62);
	}
}
