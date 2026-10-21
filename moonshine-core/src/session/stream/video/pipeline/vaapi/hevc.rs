//! HEVC Main, Main 10 and Main 4:4:4 (8/10-bit) parameter sets and VA-API
//! parameter buffers for a low-latency stream: IDR then P frames (or
//! low-delay B frames where the driver requires them), one reference, no
//! reordering (ITU-T H.265).
//!
//! The VPS, SPS and PPS are packed here so the VUI carries the stream's color
//! description and the profile's constraint flags are complete. Slice segment
//! headers are packed too where the driver takes them (Mesa codes each slice
//! from its packed header); the parameter buffers mirror every field of the
//! packed headers.

use super::bits::{BitWriter, nal_unit};
use super::sys::*;
use super::{Frame, Setup};

const LOG2_MAX_POC_LSB_MINUS4: u32 = 4;
const SLICE_TYPE_B: u8 = 0;
const SLICE_TYPE_P: u8 = 1;
const SLICE_TYPE_I: u8 = 2;
const NAL_TRAIL_R: u8 = 1;
const NAL_IDR_W_RADL: u8 = 19;
const PROFILE_MAIN: u32 = 1;
const PROFILE_MAIN10: u32 = 2;
const PROFILE_RANGE_EXTENSIONS: u32 = 4;

/// Level limits (Tables A.8 and A.9): general_level_idc, MaxLumaPs,
/// MaxLumaSr, and MaxBR of the Main and High tiers (in units of
/// `CpbVclFactor` bit/s; 0 where the level has no High tier).
const LEVELS: [(u8, u64, u64, u64, u64); 13] = [
	(30, 36_864, 552_960, 128, 0),
	(60, 122_880, 3_686_400, 1_500, 0),
	(63, 245_760, 7_372_800, 3_000, 0),
	(90, 552_960, 16_588_800, 6_000, 0),
	(93, 983_040, 33_177_600, 10_000, 0),
	(120, 2_228_224, 66_846_720, 12_000, 30_000),
	(123, 2_228_224, 133_693_440, 20_000, 50_000),
	(150, 8_912_896, 267_386_880, 25_000, 100_000),
	(153, 8_912_896, 534_773_760, 40_000, 160_000),
	(156, 8_912_896, 1_069_547_520, 60_000, 240_000),
	(180, 35_651_584, 1_069_547_520, 60_000, 240_000),
	(183, 35_651_584, 2_139_095_040, 120_000, 480_000),
	(186, 35_651_584, 4_278_190_080, 240_000, 800_000),
];

fn poc_lsb(index_since_idr: u64) -> u32 {
	(index_since_idr % (1 << (LOG2_MAX_POC_LSB_MINUS4 + 4))) as u32
}

/// `CpbVclFactor` of a profile (Table A.9 and the range extensions' table):
/// MaxBR is in units of this many bit/s.
fn cpb_vcl_factor(chroma_444: bool, bit_depth: u32) -> u64 {
	match (chroma_444, bit_depth > 8) {
		(false, _) => 1000,
		(true, false) => 2000,
		(true, true) => 2500,
	}
}

/// The level and tier (`true`: High) of a stream: the lowest Main tier level
/// whose picture size, sample rate and bitrate (the stream's constant target)
/// fit, as decoders widely support Main tier, else the lowest High tier level
/// that fits; level 6.2 High beyond every limit.
pub(super) fn level_tier(width: u32, height: u32, fps: u32, bitrate: u32, br_factor: u64) -> (u8, bool) {
	let picture = u64::from(width) * u64::from(height);
	let rate = picture * u64::from(fps);
	let fits = |max_picture: u64, max_rate: u64, max_br: u64| {
		picture <= max_picture && rate <= max_rate && u64::from(bitrate) <= max_br * br_factor
	};
	LEVELS
		.iter()
		.find(|&&(_, max_picture, max_rate, main_br, _)| fits(max_picture, max_rate, main_br))
		.map(|&(level, ..)| (level, false))
		.or_else(|| {
			LEVELS
				.iter()
				.find(|&&(_, max_picture, max_rate, _, high_br)| fits(max_picture, max_rate, high_br))
				.map(|&(level, ..)| (level, true))
		})
		.unwrap_or((186, true))
}

/// Coding tools and block sizes the driver supports (from
/// `VAConfigAttribEncHEVCFeatures`/`VAConfigAttribEncHEVCBlockSizes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tools {
	pub log2_min_cb: u32,
	pub log2_ctb: u32,
	pub log2_min_tb: u32,
	pub log2_max_tb: u32,
	pub depth_inter: u32,
	pub depth_intra: u32,
	pub amp: bool,
	pub sao: bool,
	pub temporal_mvp: bool,
	pub strong_intra_smoothing: bool,
	pub transform_skip: bool,
	pub cu_qp_delta: bool,
	/// P frames must be coded as B frames with a non-empty list 1 (low-delay B).
	pub low_delay_b: bool,
}

impl Tools {
	/// `diff_cu_qp_delta_depth`: one QP per minimum coding block. Intel's
	/// VDEnc rate control (Gen12, verified on a UHD 730) codes QP deltas at
	/// that granularity whatever the picture parameters say; a coarser
	/// depth decodes with wrong QPs. FFmpeg signals the same depth.
	pub(super) fn cu_qp_delta_depth(&self) -> u32 {
		self.log2_ctb - self.log2_min_cb
	}

	/// Defaults for drivers that report neither attribute.
	pub(super) const DEFAULT: Self = Self {
		log2_min_cb: 3,
		log2_ctb: 5,
		log2_min_tb: 2,
		log2_max_tb: 5,
		depth_inter: 3,
		depth_intra: 3,
		amp: false,
		sao: false,
		temporal_mvp: false,
		strong_intra_smoothing: false,
		transform_skip: false,
		cu_qp_delta: true,
		low_delay_b: false,
	};

	/// Tools from the raw attribute values (`VA_ATTRIB_NOT_SUPPORTED` keeps
	/// the defaults). Each feature is two bits: 0 unsupported, 1 supported,
	/// 2 required; supported features are used.
	pub(super) fn from_attributes(features: u32, block_sizes: u32, prediction_direction: u32) -> Self {
		let mut tools = Self::DEFAULT;
		if features != VA_ATTRIB_NOT_SUPPORTED {
			let feature = |index: u32| (features >> (2 * index)) & 3 != 0;
			// va.h VAConfigAttribValEncHEVCFeatures order.
			tools.amp = feature(2);
			tools.sao = feature(3);
			tools.temporal_mvp = feature(5);
			tools.strong_intra_smoothing = feature(6);
			tools.transform_skip = feature(10);
			tools.cu_qp_delta = feature(11);
		}
		if block_sizes != VA_ATTRIB_NOT_SUPPORTED {
			let field = |index: u32| (block_sizes >> (2 * index)) & 3;
			// va.h VAConfigAttribValEncHEVCBlockSizes order.
			tools.log2_ctb = field(0) + 3;
			tools.log2_min_cb = field(2) + 3;
			tools.log2_max_tb = field(3) + 2;
			tools.log2_min_tb = field(4) + 2;
			tools.depth_inter = field(5);
			tools.depth_intra = field(7);
		}
		if prediction_direction != VA_ATTRIB_NOT_SUPPORTED {
			tools.low_delay_b = prediction_direction & VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0;
		}
		tools
	}
}

const VA_PREDICTION_DIRECTION_BI_NOT_EMPTY: u32 = 4;

pub(super) struct Hevc {
	profile: u32,
	level: u8,
	high_tier: bool,
	tools: Tools,
	ctus: u32,
	max_references: usize,
}

impl Hevc {
	pub(super) fn new(setup: &Setup, tools: Tools) -> Self {
		let profile = match (setup.chroma_444, setup.bit_depth) {
			(true, _) => PROFILE_RANGE_EXTENSIONS,
			(false, 10) => PROFILE_MAIN10,
			(false, _) => PROFILE_MAIN,
		};
		let ctb = 1 << tools.log2_ctb;
		let (level, high_tier) = level_tier(
			setup.coded_width,
			setup.coded_height,
			setup.fps,
			setup.bitrate,
			cpb_vcl_factor(setup.chroma_444, setup.bit_depth),
		);
		Self {
			max_references: setup.max_references,
			profile,
			level,
			high_tier,
			tools,
			ctus: setup.coded_width.div_ceil(ctb) * setup.coded_height.div_ceil(ctb),
		}
	}

	/// Packed sequence header (VPS and SPS) and picture header (PPS).
	pub(super) fn headers(&self, setup: &Setup) -> (Vec<u8>, Vec<u8>) {
		let mut sequence = self.vps(setup);
		sequence.extend(self.sps(setup));
		(sequence, self.pps())
	}

	fn profile_tier_level(&self, w: &mut BitWriter, setup: &Setup) {
		w.u(2, 0); // general_profile_space
		w.flag(self.high_tier); // general_tier_flag
		w.u(5, self.profile);
		// general_profile_compatibility_flag[j]: Main streams are also Main 10.
		let mut compatibility = 1u32 << (31 - self.profile);
		if self.profile == PROFILE_MAIN {
			compatibility |= 1 << (31 - PROFILE_MAIN10);
		}
		w.u(32, compatibility);
		w.flag(true); // general_progressive_source_flag
		w.flag(false); // general_interlaced_source_flag
		w.flag(false); // general_non_packed_constraint_flag
		w.flag(true); // general_frame_only_constraint_flag
		if self.profile == PROFILE_RANGE_EXTENSIONS {
			// Main 4:4:4 (Table A.2): max_12bit, max_10bit, max_8bit (8-bit
			// only), not 4:2:2/4:2:0/monochrome, not intra, not one picture,
			// lower bit rate.
			let eight = setup.bit_depth == 8;
			for flag in [true, true, eight, false, false, false, false, false, true] {
				w.flag(flag);
			}
			w.u(32, 0); // general_reserved_zero_34bits
			w.u(2, 0);
		} else {
			w.u(32, 0); // general_reserved_zero_43bits
			w.u(11, 0);
		}
		w.flag(false); // general_inbld_flag / reserved
		w.u(8, u32::from(self.level));
	}

	fn vps(&self, setup: &Setup) -> Vec<u8> {
		let mut w = BitWriter::default();
		w.u(4, 0); // vps_video_parameter_set_id
		w.flag(true); // vps_base_layer_internal_flag
		w.flag(true); // vps_base_layer_available_flag
		w.u(6, 0); // vps_max_layers_minus1
		w.u(3, 0); // vps_max_sub_layers_minus1
		w.flag(true); // vps_temporal_id_nesting_flag
		w.u(16, 0xffff); // vps_reserved_0xffff_16bits
		self.profile_tier_level(&mut w, setup);
		w.flag(true); // vps_sub_layer_ordering_info_present_flag
		w.ue(self.max_references as u32); // retained references plus current
		w.ue(0); // vps_max_num_reorder_pics
		w.ue(0); // vps_max_latency_increase_plus1
		w.u(6, 0); // vps_max_layer_id
		w.ue(0); // vps_num_layer_sets_minus1
		w.flag(false); // vps_timing_info_present_flag
		w.flag(false); // vps_extension_flag
		w.trailing_bits();
		nal_unit(&[0x40, 0x01], &w.into_bytes())
	}

	fn sps(&self, setup: &Setup) -> Vec<u8> {
		let t = &self.tools;
		let mut w = BitWriter::default();
		w.u(4, 0); // sps_video_parameter_set_id
		w.u(3, 0); // sps_max_sub_layers_minus1
		w.flag(true); // sps_temporal_id_nesting_flag
		self.profile_tier_level(&mut w, setup);
		w.ue(0); // sps_seq_parameter_set_id
		w.ue(if setup.chroma_444 { 3 } else { 1 }); // chroma_format_idc
		if setup.chroma_444 {
			w.flag(false); // separate_colour_plane_flag
		}
		w.ue(setup.coded_width);
		w.ue(setup.coded_height);
		let (crop_right, crop_bottom) = setup.crop();
		let cropping = crop_right != 0 || crop_bottom != 0;
		w.flag(cropping); // conformance_window_flag
		if cropping {
			// In chroma sample units (SubWidthC/SubHeightC).
			let unit = if setup.chroma_444 { 1 } else { 2 };
			w.ue(0);
			w.ue(crop_right / unit);
			w.ue(0);
			w.ue(crop_bottom / unit);
		}
		w.ue(setup.bit_depth - 8); // bit_depth_luma_minus8
		w.ue(setup.bit_depth - 8); // bit_depth_chroma_minus8
		w.ue(LOG2_MAX_POC_LSB_MINUS4);
		w.flag(true); // sps_sub_layer_ordering_info_present_flag
		w.ue(self.max_references as u32); // retained references plus current
		w.ue(0); // sps_max_num_reorder_pics
		w.ue(0); // sps_max_latency_increase_plus1
		w.ue(t.log2_min_cb - 3);
		w.ue(t.log2_ctb - t.log2_min_cb);
		w.ue(t.log2_min_tb - 2);
		w.ue(t.log2_max_tb - t.log2_min_tb);
		w.ue(t.depth_inter);
		w.ue(t.depth_intra);
		w.flag(false); // scaling_list_enabled_flag
		w.flag(t.amp);
		w.flag(t.sao);
		w.flag(false); // pcm_enabled_flag
		w.ue(0); // num_short_term_ref_pic_sets: coded in each slice header
		w.flag(false); // long_term_ref_pics_present_flag
		w.flag(t.temporal_mvp);
		w.flag(t.strong_intra_smoothing);
		w.flag(true); // vui_parameters_present_flag
		self.vui(&mut w, setup);
		w.flag(false); // sps_extension_present_flag
		w.trailing_bits();
		nal_unit(&[0x42, 0x01], &w.into_bytes())
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
		w.flag(false); // neutral_chroma_indication_flag
		w.flag(false); // field_seq_flag
		w.flag(false); // frame_field_info_present_flag
		w.flag(false); // default_display_window_flag
		w.flag(true); // vui_timing_info_present_flag
		w.u(32, 1); // vui_num_units_in_tick
		w.u(32, setup.fps); // vui_time_scale
		w.flag(false); // vui_poc_proportional_to_timing_flag
		w.flag(false); // vui_hrd_parameters_present_flag
		w.flag(true); // bitstream_restriction_flag
		w.flag(false); // tiles_fixed_structure_flag
		w.flag(true); // motion_vectors_over_pic_boundaries_flag
		w.flag(true); // restricted_ref_pic_lists_flag
		w.ue(0); // min_spatial_segmentation_idc
		w.ue(0); // max_bytes_per_pic_denom
		w.ue(0); // max_bits_per_min_cu_denom
		w.ue(15); // log2_max_mv_length_horizontal
		w.ue(15); // log2_max_mv_length_vertical
	}

	fn pps(&self) -> Vec<u8> {
		let t = &self.tools;
		let mut w = BitWriter::default();
		w.ue(0); // pps_pic_parameter_set_id
		w.ue(0); // pps_seq_parameter_set_id
		w.flag(false); // dependent_slice_segments_enabled_flag
		w.flag(false); // output_flag_present_flag
		w.u(3, 0); // num_extra_slice_header_bits
		w.flag(false); // sign_data_hiding_enabled_flag
		w.flag(false); // cabac_init_present_flag
		w.ue(0); // num_ref_idx_l0_default_active_minus1
		w.ue(0); // num_ref_idx_l1_default_active_minus1
		w.se(0); // init_qp_minus26
		w.flag(false); // constrained_intra_pred_flag
		w.flag(t.transform_skip);
		w.flag(t.cu_qp_delta);
		if t.cu_qp_delta {
			w.ue(t.cu_qp_delta_depth());
		}
		w.se(0); // pps_cb_qp_offset
		w.se(0); // pps_cr_qp_offset
		w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
		w.flag(false); // weighted_pred_flag
		w.flag(false); // weighted_bipred_flag
		w.flag(false); // transquant_bypass_enabled_flag
		w.flag(false); // tiles_enabled_flag
		w.flag(false); // entropy_coding_sync_enabled_flag
		w.flag(true); // pps_loop_filter_across_slices_enabled_flag
		w.flag(false); // deblocking_filter_control_present_flag
		w.flag(false); // pps_scaling_list_data_present_flag
		w.flag(false); // lists_modification_present_flag
		w.ue(0); // log2_parallel_merge_level_minus2
		w.flag(false); // slice_segment_header_extension_present_flag
		w.flag(false); // pps_extension_present_flag
		w.trailing_bits();
		nal_unit(&[0x44, 0x01], &w.into_bytes())
	}

	/// Packed slice segment header NAL unit of `frame` (no slice data) and
	/// its length in bits; it matches the parameters of [`Self::frame`].
	pub(super) fn slice_header(&self, frame: &Frame) -> (Vec<u8>, u32) {
		let t = &self.tools;
		let reference = frame.reference.filter(|_| !frame.idr);
		let temporal_mvp = t.temporal_mvp && reference.is_some();
		let slice_type = self.slice_type(frame);
		let mut w = BitWriter::default();
		w.flag(true); // first_slice_segment_in_pic_flag
		if frame.idr {
			w.flag(false); // no_output_of_prior_pics_flag
		}
		w.ue(0); // slice_pic_parameter_set_id
		w.ue(u32::from(slice_type));
		if !frame.idr {
			w.u(LOG2_MAX_POC_LSB_MINUS4 + 4, poc_lsb(frame.index_since_idr));
			w.flag(false); // short_term_ref_pic_set_sps_flag: st_ref_pic_set(0) follows
			let retained = frame.retained.iter().flatten().rev();
			w.ue(retained.clone().count() as u32); // num_negative_pics
			w.ue(0); // num_positive_pics
			let mut previous = frame.index_since_idr;
			for &(surface, index) in retained {
				w.ue((previous - index - 1) as u32); // negative POC deltas, newest first
				w.flag(reference.is_some_and(|r| r.0 == surface));
				previous = index;
			}
			if t.temporal_mvp {
				w.flag(temporal_mvp); // slice_temporal_mvp_enabled_flag
			}
		}
		if t.sao {
			w.flag(true); // slice_sao_luma_flag
			w.flag(true); // slice_sao_chroma_flag
		}
		if slice_type != SLICE_TYPE_I {
			w.flag(false); // num_ref_idx_active_override_flag: one reference
			if slice_type == SLICE_TYPE_B {
				w.flag(false); // mvd_l1_zero_flag
			}
			if temporal_mvp && slice_type == SLICE_TYPE_B {
				w.flag(true); // collocated_from_l0_flag
			}
			w.ue(0); // five_minus_max_num_merge_cand
		}
		w.se(0); // slice_qp_delta
		// pps_loop_filter_across_slices_enabled_flag with deblocking enabled.
		w.flag(true); // slice_loop_filter_across_slices_enabled_flag
		w.trailing_bits(); // byte_alignment(): a one bit, then zero bits
		let nal_type = if frame.idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
		let nal = nal_unit(&[nal_type << 1, 0x01], &w.into_bytes());
		let bits = (nal.len() * 8) as u32;
		(nal, bits)
	}

	fn slice_type(&self, frame: &Frame) -> u8 {
		match (frame.idr, self.tools.low_delay_b) {
			(true, _) => SLICE_TYPE_I,
			(false, false) => SLICE_TYPE_P,
			(false, true) => SLICE_TYPE_B,
		}
	}

	pub(super) fn sequence(&self, setup: &Setup) -> VAEncSequenceParameterBufferHEVC {
		let t = &self.tools;
		let mut seq = VAEncSequenceParameterBufferHEVC {
			general_profile_idc: self.profile as u8,
			general_level_idc: self.level,
			general_tier_flag: u8::from(self.high_tier),
			intra_period: 0,
			intra_idr_period: 0,
			ip_period: 1,
			bits_per_second: setup.bitrate,
			pic_width_in_luma_samples: setup.coded_width as u16,
			pic_height_in_luma_samples: setup.coded_height as u16,
			log2_min_luma_coding_block_size_minus3: (t.log2_min_cb - 3) as u8,
			log2_diff_max_min_luma_coding_block_size: (t.log2_ctb - t.log2_min_cb) as u8,
			log2_min_transform_block_size_minus2: (t.log2_min_tb - 2) as u8,
			log2_diff_max_min_transform_block_size: (t.log2_max_tb - t.log2_min_tb) as u8,
			max_transform_hierarchy_depth_inter: t.depth_inter as u8,
			max_transform_hierarchy_depth_intra: t.depth_intra as u8,
			vui_parameters_present_flag: 1,
			aspect_ratio_idc: 1,
			vui_num_units_in_tick: 1,
			vui_time_scale: setup.fps,
			..Default::default()
		};
		// SAFETY: writing plain-data bitfield views of unions.
		let fields = unsafe { &mut seq.seq_fields.bits };
		fields.set_chroma_format_idc(if setup.chroma_444 { 3 } else { 1 });
		fields.set_bit_depth_luma_minus8(setup.bit_depth - 8);
		fields.set_bit_depth_chroma_minus8(setup.bit_depth - 8);
		fields.set_strong_intra_smoothing_enabled_flag(u32::from(t.strong_intra_smoothing));
		fields.set_amp_enabled_flag(u32::from(t.amp));
		fields.set_sample_adaptive_offset_enabled_flag(u32::from(t.sao));
		fields.set_sps_temporal_mvp_enabled_flag(u32::from(t.temporal_mvp));
		fields.set_low_delay_seq(1);
		// SAFETY: as above.
		let vui = unsafe { &mut seq.vui_fields.bits };
		vui.set_aspect_ratio_info_present_flag(1);
		vui.set_vui_timing_info_present_flag(1);
		vui.set_bitstream_restriction_flag(1);
		vui.set_motion_vectors_over_pic_boundaries_flag(1);
		vui.set_restricted_ref_pic_lists_flag(1);
		vui.set_log2_max_mv_length_horizontal(15);
		vui.set_log2_max_mv_length_vertical(15);
		seq
	}

	fn invalid() -> VAPictureHEVC {
		VAPictureHEVC {
			picture_id: VA_INVALID_SURFACE,
			flags: VA_PICTURE_HEVC_INVALID,
			..Default::default()
		}
	}

	/// Picture and slice parameters of one frame.
	pub(super) fn frame(
		&self,
		frame: &Frame,
		coded: VABufferID,
	) -> (VAEncPictureParameterBufferHEVC, VAEncSliceParameterBufferHEVC) {
		let t = &self.tools;
		let reference = frame
			.reference
			.filter(|_| !frame.idr)
			.map(|(surface, index)| VAPictureHEVC {
				picture_id: surface,
				pic_order_cnt: index as i32,
				flags: VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
				..Default::default()
			});
		let temporal_mvp = t.temporal_mvp && reference.is_some();
		let mut picture = VAEncPictureParameterBufferHEVC {
			decoded_curr_pic: VAPictureHEVC {
				picture_id: frame.recon,
				pic_order_cnt: frame.index_since_idr as i32,
				..Default::default()
			},
			reference_frames: [Self::invalid(); 15],
			coded_buf: coded,
			collocated_ref_pic_index: if temporal_mvp { 0 } else { 0xff },
			pic_init_qp: 26,
			diff_cu_qp_delta_depth: if t.cu_qp_delta { t.cu_qp_delta_depth() as u8 } else { 0 },
			nal_unit_type: if frame.idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R },
			..Default::default()
		};
		for (slot, &(surface, index)) in picture
			.reference_frames
			.iter_mut()
			.zip(frame.retained.iter().flatten().rev())
		{
			*slot = VAPictureHEVC {
				picture_id: surface,
				pic_order_cnt: index as i32,
				flags: if reference.is_some_and(|r| r.picture_id == surface) {
					VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
				} else {
					0
				},
				..Default::default()
			};
		}
		if temporal_mvp {
			picture.collocated_ref_pic_index = picture
				.reference_frames
				.iter()
				.position(|r| reference.is_some_and(|a| a.picture_id == r.picture_id))
				.expect("retained safe reference") as u8;
		}
		let slice_type = self.slice_type(frame);
		let coding_type = match slice_type {
			SLICE_TYPE_I => 1,
			SLICE_TYPE_P => 2,
			_ => 3,
		};
		// SAFETY: writing plain-data bitfield views of unions.
		let fields = unsafe { &mut picture.pic_fields.bits };
		fields.set_idr_pic_flag(u32::from(frame.idr));
		fields.set_coding_type(coding_type);
		fields.set_reference_pic_flag(1);
		fields.set_transform_skip_enabled_flag(u32::from(t.transform_skip));
		fields.set_cu_qp_delta_enabled_flag(u32::from(t.cu_qp_delta));
		fields.set_pps_loop_filter_across_slices_enabled_flag(1);

		let mut slice = VAEncSliceParameterBufferHEVC {
			num_ctu_in_slice: self.ctus,
			slice_type,
			ref_pic_list0: [Self::invalid(); 15],
			ref_pic_list1: [Self::invalid(); 15],
			max_num_merge_cand: 5,
			..Default::default()
		};
		if let Some(reference) = reference {
			slice.ref_pic_list0[0] = reference;
			if t.low_delay_b {
				slice.ref_pic_list1[0] = reference;
			}
		}
		// SAFETY: as above.
		let fields = unsafe { &mut slice.slice_fields.bits };
		fields.set_last_slice_of_pic_flag(1);
		fields.set_slice_temporal_mvp_enabled_flag(u32::from(temporal_mvp));
		fields.set_slice_sao_luma_flag(u32::from(t.sao));
		fields.set_slice_sao_chroma_flag(u32::from(t.sao));
		fields.set_slice_loop_filter_across_slices_enabled_flag(1);
		fields.set_collocated_from_l0_flag(1);
		(picture, slice)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn level_follows_picture_size_and_rate() {
		let level = |width, height, fps| level_tier(width, height, fps, 10_000_000, 1000);
		assert_eq!(level(1920, 1088, 60), (123, false)); // 4.1
		assert_eq!(level(2560, 1472, 120), (153, false)); // 5.1
		assert_eq!(level(3840, 2176, 60), (153, false)); // 5.1
		assert_eq!(level(3840, 2176, 120), (156, false)); // 5.2
		assert_eq!(level(8192, 8192, 240), (186, true));
	}

	#[test]
	fn level_and_tier_bound_the_bitrate() {
		let main = cpb_vcl_factor(false, 8);
		// 1080p60 Main: 4.1 carries 20 Mbit/s, 5.1 40, 5.2 60.
		assert_eq!(level_tier(1920, 1088, 60, 20_000_000, main), (123, false));
		assert_eq!(level_tier(1920, 1088, 60, 50_000_000, main), (156, false));
		// Beyond Main tier 6.2 (240 Mbit/s): the lowest High tier level.
		assert_eq!(level_tier(1920, 1088, 60, 300_000_000, main), (183, true));
		// Main 4:4:4 10 counts MaxBR in 2500 bit/s units.
		assert_eq!(cpb_vcl_factor(true, 10), 2500);
		assert_eq!(
			level_tier(1920, 1088, 60, 50_000_000, cpb_vcl_factor(true, 10)),
			(123, false)
		);
		assert_eq!(level_tier(1920, 1088, 60, u32::MAX, main), (186, true));
	}

	#[test]
	fn attributes_select_tools_and_unsupported_ones_keep_defaults() {
		let tools = Tools::from_attributes(
			VA_ATTRIB_NOT_SUPPORTED,
			VA_ATTRIB_NOT_SUPPORTED,
			VA_ATTRIB_NOT_SUPPORTED,
		);
		assert_eq!(tools, Tools::DEFAULT);
		// SAO supported (index 3), TMVP required (index 5), CU QP delta
		// supported (index 11); CTB 64 (3), min CB 8 (0), TB 4..32.
		let features = (1 << 6) | (2 << 10) | (1 << 22);
		let blocks = 3 | (3 << 6) | (2 << 10) | (2 << 14);
		let tools = Tools::from_attributes(features, blocks, 3);
		assert!(tools.sao && tools.temporal_mvp && tools.cu_qp_delta && !tools.amp);
		assert_eq!((tools.log2_ctb, tools.log2_min_cb), (6, 3));
		assert_eq!((tools.log2_min_tb, tools.log2_max_tb), (2, 5));
		assert_eq!((tools.depth_inter, tools.depth_intra), (2, 2));
		assert!(!tools.low_delay_b);
		assert!(Tools::from_attributes(features, blocks, 4 | 1).low_delay_b);
	}
}
