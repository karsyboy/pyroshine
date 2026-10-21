//! AV1 Main (4:2:0 8/10-bit) OBUs and VA-API parameter buffers for a
//! low-latency stream: a key frame, then inter frames predicting from the
//! previous frame only, no reordering (AV1 Bitstream & Decoding Process
//! Specification).
//!
//! Inter frames inherit entropy contexts (CDFs) from the previous frame.
//!
//! Each frame is sent as separate packed units, in order: the sequence
//! header (key frames) and a frame header OBU (`OBU_FRAME_HEADER`); the
//! driver appends the tile group OBU. One OBU per packed buffer: Mesa parses
//! only the first OBU of a buffer, and Intel's driver locates the frame
//! header by its first byte. The temporal delimiter that starts each
//! temporal unit is prepended to the coded frame by the backend, as FFmpeg's
//! muxers do, so the driver needs no raw packed data. Under constant-bitrate
//! control the driver rewrites `base_q_idx`, the loop filter and CDEF
//! parameters and `obu_size` in the frame header at the offsets reported in
//! the picture parameters, so the header carries placeholder values there
//! and a fixed four-byte `obu_size` (as libva requires). The loop filter and
//! CDEF placeholders are zero levels and a single zero strength, and the
//! picture parameters leave both to the driver, as FFmpeg does.

use super::bits::BitWriter;
use super::sys::*;
use super::{Frame, Setup};

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
/// `OBU_TEMPORAL_DELIMITER` with a zero `obu_size`.
pub(super) const TEMPORAL_DELIMITER: [u8; 2] = [(2 << 3) | 0x02, 0];
const KEY_FRAME: u32 = 0;
const INTER_FRAME: u32 = 1;
const PRIMARY_REF_NONE: u32 = 7;
/// Inter frames take their CDFs from LAST (`ref_frame_idx[0]`, the previous
/// frame), which they predict from anyway. `PRIMARY_REF_NONE` would be
/// equivalent, but Mesa's VCN 5 encoder (RX 9070 XT, Mesa 26.2) inherits the
/// reference's CDFs regardless and its inter frames were undecodable.
const PRIMARY_REF_LAST: u32 = 0;
const ORDER_HINT_BITS: u32 = 8;
const REFS_PER_FRAME: usize = 7;
const ALL_FRAMES: u8 = 0xff;
/// `interpolation_filter` value of a switchable filter.
const SWITCHABLE: u32 = 4;
/// Placeholder quantizer: nonzero, so `delta_q_present` is coded and the
/// header length does not depend on the value the driver writes.
const BASE_Q_IDX: u32 = 128;
/// Placeholder `cdef_params()`: damping 3, `cdef_bits` 0, one zero strength.
const CDEF_BITS: u32 = 0;
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
/// libva: the frame header OBU has a four-byte `obu_size`.
const OBU_SIZE_BYTES: usize = 4;

/// Level limits (Annex A.3): seq_level_idx, MaxPicSize, MaxHSize, MaxVSize,
/// MaxDisplayRate, and MainMbps and HighMbps in kbit/s (0: no High tier
/// below level 4.0). Main profile's BitrateProfileFactor is 1.
const LEVELS: [(u8, u64, u32, u32, u64, u64, u64); 14] = [
	(0, 147_456, 2048, 1152, 4_423_680, 1_500, 0),
	(1, 278_784, 2816, 1584, 8_363_520, 3_000, 0),
	(4, 665_856, 4352, 2448, 19_975_680, 6_000, 0),
	(5, 1_065_024, 5504, 3096, 31_950_720, 10_000, 0),
	(8, 2_359_296, 6144, 3456, 70_778_880, 12_000, 30_000),
	(9, 2_359_296, 6144, 3456, 141_557_760, 20_000, 50_000),
	(12, 8_912_896, 8192, 4352, 267_386_880, 30_000, 100_000),
	(13, 8_912_896, 8192, 4352, 534_773_760, 40_000, 160_000),
	(14, 8_912_896, 8192, 4352, 1_069_547_520, 60_000, 240_000),
	(15, 8_912_896, 8192, 4352, 1_069_547_520, 60_000, 240_000),
	(16, 35_651_584, 16384, 8704, 1_069_547_520, 60_000, 240_000),
	(17, 35_651_584, 16384, 8704, 2_139_095_040, 100_000, 480_000),
	(18, 35_651_584, 16384, 8704, 4_278_190_080, 160_000, 800_000),
	(19, 35_651_584, 16384, 8704, 4_278_190_080, 160_000, 800_000),
];

/// The level and tier (`true`: High) of a stream: the lowest Main tier level
/// whose picture size, sample rate and bitrate (the stream's constant target)
/// fit, else the lowest High tier level that fits; 31 (no level constraints)
/// beyond the table.
pub(super) fn seq_level_tier(width: u32, height: u32, fps: u32, bitrate: u32) -> (u8, bool) {
	let picture = u64::from(width) * u64::from(height);
	let rate = picture * u64::from(fps);
	let kbps = u64::from(bitrate).div_ceil(1000);
	let fits = |&&(_, max_picture, max_width, max_height, max_rate, ..): &&(u8, u64, u32, u32, u64, u64, u64)| {
		picture <= max_picture && width <= max_width && height <= max_height && rate <= max_rate
	};
	LEVELS
		.iter()
		.filter(fits)
		.find(|&&(.., main_kbps, _)| kbps <= main_kbps)
		.map(|&(level, ..)| (level, false))
		.or_else(|| {
			LEVELS
				.iter()
				.filter(fits)
				.find(|&&(.., high_kbps)| kbps <= high_kbps)
				.map(|&(level, ..)| (level, true))
		})
		.unwrap_or((31, false))
}

/// Smallest `k` with `block << k >= target` (spec `tile_log2`).
fn tile_log2(block: u32, target: u32) -> u32 {
	let mut k = 0;
	while (block << k) < target {
		k += 1;
	}
	k
}

/// Coding tools from `VAConfigAttribEncAV1`/`Ext1`/`Ext2`. Only tools the
/// driver requires are enabled; everything optional stays off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tools {
	pub sb128: bool,
	pub filter_intra: bool,
	pub intra_edge_filter: bool,
	pub interintra_compound: bool,
	pub masked_compound: bool,
	pub warped_motion: bool,
	pub dual_filter: bool,
	pub jnt_comp: bool,
	pub ref_frame_mvs: bool,
	/// `interpolation_filter`: 0..3 fixed, [`SWITCHABLE`].
	pub interpolation_filter: u32,
	/// `TX_MODE_SELECT` (otherwise `TX_MODE_LARGEST`).
	pub tx_mode_select: bool,
}

const VA_FEATURE_REQUIRED: u32 = 2;

impl Tools {
	/// `None` when the driver offers neither selectable nor largest
	/// transform sizes.
	pub(super) fn from_attributes(features: u32, ext1: u32, ext2: u32) -> Option<Self> {
		let required =
			|shift: u32| features != VA_ATTRIB_NOT_SUPPORTED && (features >> shift) & 3 == VA_FEATURE_REQUIRED;
		let interpolation_filter = if ext1 == VA_ATTRIB_NOT_SUPPORTED {
			0
		} else {
			let filters = ext1 & 0x1f;
			if filters & 0x10 != 0 {
				SWITCHABLE
			} else {
				// EIGHTTAP, EIGHTTAP_SMOOTH, EIGHTTAP_SHARP, BILINEAR.
				(0..4).find(|bit| filters & (1 << bit) != 0).unwrap_or(0)
			}
		};
		let tx_mode_select = if ext2 == VA_ATTRIB_NOT_SUPPORTED {
			true
		} else {
			let modes = (ext2 >> 4) & 7;
			if modes & 4 != 0 {
				true
			} else if modes & 2 != 0 {
				false
			} else {
				return None;
			}
		};
		Some(Self {
			sb128: required(0),
			filter_intra: required(2),
			intra_edge_filter: required(4),
			interintra_compound: required(6),
			masked_compound: required(8),
			warped_motion: required(10),
			dual_filter: required(14),
			jnt_comp: required(16),
			ref_frame_mvs: required(18),
			interpolation_filter,
			tx_mode_select,
		})
	}
}

/// A frame header OBU and the positions the driver patches.
pub(super) struct FrameHeader {
	pub obu: Vec<u8>,
	/// Bits from the start of the OBU to `base_q_idx`.
	pub qindex: u32,
	pub loop_filter: u32,
	pub cdef: u32,
	pub cdef_bits: u32,
	/// Bits up to and including the trailing one bit.
	pub size_bits: u32,
}

pub(super) struct Av1 {
	tools: Tools,
	level: u8,
	high_tier: bool,
	sb_cols: u32,
	sb_rows: u32,
	max_log2_tile_cols: u32,
	max_log2_tile_rows: u32,
}

impl Av1 {
	pub(super) fn new(setup: &Setup, tools: Tools) -> Result<Self, String> {
		// MiCols/MiRows: the frame in 4x4 units, rounded to 8x8.
		let mi_cols = 2 * setup.width.div_ceil(8);
		let mi_rows = 2 * setup.height.div_ceil(8);
		let sb_shift = if tools.sb128 { 5 } else { 4 };
		let sb_cols = mi_cols.div_ceil(1 << sb_shift);
		let sb_rows = mi_rows.div_ceil(1 << sb_shift);
		let sb_size = sb_shift + 2;
		let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
		let max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
		let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
		let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
		if min_log2_tiles > 0 {
			return Err(format!(
				"{}x{} needs several AV1 tiles, which the VA-API backend does not code",
				setup.width, setup.height
			));
		}
		let (level, high_tier) = seq_level_tier(setup.width, setup.height, setup.fps, setup.bitrate);
		Ok(Self {
			tools,
			level,
			high_tier,
			sb_cols,
			sb_rows,
			max_log2_tile_cols: tile_log2(1, sb_cols.min(MAX_TILE_COLS)),
			max_log2_tile_rows: tile_log2(1, sb_rows.min(MAX_TILE_ROWS)),
		})
	}

	/// The sequence header OBU.
	pub(super) fn sequence_header(&self, setup: &Setup) -> Vec<u8> {
		let t = &self.tools;
		let mut w = BitWriter::default();
		w.u(3, 0); // seq_profile: Main
		w.flag(false); // still_picture
		w.flag(false); // reduced_still_picture_header
		w.flag(false); // timing_info_present_flag
		w.flag(false); // initial_display_delay_present_flag
		w.u(5, 0); // operating_points_cnt_minus_1
		w.u(12, 0); // operating_point_idc[0]
		w.u(5, u32::from(self.level)); // seq_level_idx[0]
		if self.level > 7 {
			w.flag(self.high_tier); // seq_tier[0]
		}
		let width_bits = 32 - (setup.width - 1).max(1).leading_zeros();
		let height_bits = 32 - (setup.height - 1).max(1).leading_zeros();
		w.u(4, width_bits - 1); // frame_width_bits_minus_1
		w.u(4, height_bits - 1); // frame_height_bits_minus_1
		w.u(width_bits, setup.width - 1); // max_frame_width_minus_1
		w.u(height_bits, setup.height - 1); // max_frame_height_minus_1
		w.flag(false); // frame_id_numbers_present_flag
		w.flag(t.sb128); // use_128x128_superblock
		w.flag(t.filter_intra);
		w.flag(t.intra_edge_filter);
		w.flag(t.interintra_compound);
		w.flag(t.masked_compound);
		w.flag(t.warped_motion);
		w.flag(t.dual_filter);
		w.flag(true); // enable_order_hint
		w.flag(t.jnt_comp);
		w.flag(t.ref_frame_mvs);
		w.flag(false); // seq_choose_screen_content_tools
		w.flag(false); // seq_force_screen_content_tools
		w.u(3, ORDER_HINT_BITS - 1); // order_hint_bits_minus_1
		w.flag(false); // enable_superres
		w.flag(true); // enable_cdef
		w.flag(false); // enable_restoration
		// color_config(): Main profile, so 4:2:0 and no twelve_bit.
		w.flag(setup.bit_depth == 10); // high_bitdepth
		w.flag(false); // mono_chrome
		w.flag(true); // color_description_present_flag
		w.u(8, u32::from(setup.color.color_primaries));
		w.u(8, u32::from(setup.color.transfer_characteristics));
		w.u(8, u32::from(setup.color.matrix_coefficients));
		w.flag(setup.color.full_range); // color_range
		w.u(2, 0); // chroma_sample_position: unknown
		w.flag(false); // separate_uv_delta_q
		w.flag(false); // film_grain_params_present
		w.trailing_bits();
		let payload = w.into_bytes();
		let mut obu = vec![obu_header(OBU_SEQUENCE_HEADER)];
		leb128(&mut obu, payload.len() as u32, None);
		obu.extend(payload);
		obu
	}

	pub(super) fn sequence(&self, setup: &Setup) -> VAEncSequenceParameterBufferAV1 {
		let t = &self.tools;
		let mut seq = VAEncSequenceParameterBufferAV1 {
			seq_profile: 0,
			seq_level_idx: self.level,
			seq_tier: u8::from(self.high_tier),
			hierarchical_flag: 0,
			// Infinite GOP: key frames only when requested.
			intra_period: 0,
			ip_period: 1,
			bits_per_second: setup.bitrate,
			order_hint_bits_minus_1: (ORDER_HINT_BITS - 1) as u8,
			..Default::default()
		};
		// SAFETY: writing a plain-data bitfield view of a union.
		let fields = unsafe { &mut seq.seq_fields.bits };
		fields.set_use_128x128_superblock(u32::from(t.sb128));
		fields.set_enable_filter_intra(u32::from(t.filter_intra));
		fields.set_enable_intra_edge_filter(u32::from(t.intra_edge_filter));
		fields.set_enable_interintra_compound(u32::from(t.interintra_compound));
		fields.set_enable_masked_compound(u32::from(t.masked_compound));
		fields.set_enable_warped_motion(u32::from(t.warped_motion));
		fields.set_enable_dual_filter(u32::from(t.dual_filter));
		fields.set_enable_order_hint(1);
		fields.set_enable_jnt_comp(u32::from(t.jnt_comp));
		fields.set_enable_ref_frame_mvs(u32::from(t.ref_frame_mvs));
		fields.set_enable_cdef(1);
		fields.set_bit_depth_minus8(setup.bit_depth - 8);
		fields.set_subsampling_x(1);
		fields.set_subsampling_y(1);
		seq
	}

	/// The frame header OBU of `frame`. A key frame refreshes every slot;
	/// an inter frame predicts from slot 0 (all `ref_frame_idx`) and
	/// refreshes it.
	pub(super) fn frame_header(&self, frame: &Frame) -> FrameHeader {
		let t = &self.tools;
		let key = frame.idr;
		let mut w = BitWriter::default();
		w.flag(false); // show_existing_frame
		w.u(2, if key { KEY_FRAME } else { INTER_FRAME });
		w.flag(true); // show_frame
		if !key {
			w.flag(false); // error_resilient_mode (inferred 1 for shown key frames)
		}
		w.flag(false); // disable_cdf_update
		// seq_force_screen_content_tools = 0: no allow_screen_content_tools.
		w.flag(false); // frame_size_override_flag
		w.u(ORDER_HINT_BITS, order_hint(frame.index_since_idr));
		if !key {
			w.u(3, PRIMARY_REF_LAST); // primary_ref_frame
			w.u(8, 1); // refresh_frame_flags: slot 0
			// error_resilient_mode = 0: no ref_order_hint.
			w.flag(false); // frame_refs_short_signaling
			for _ in 0..REFS_PER_FRAME {
				w.u(3, 0); // ref_frame_idx[i]
			}
		}
		// frame_size(): no override, no superres; render_size():
		w.flag(false); // render_and_frame_size_different
		if !key {
			w.flag(false); // allow_high_precision_mv
			let switchable = t.interpolation_filter == SWITCHABLE;
			w.flag(switchable); // is_filter_switchable
			if !switchable {
				w.u(2, t.interpolation_filter);
			}
			w.flag(false); // is_motion_mode_switchable
			// use_ref_frame_mvs coded only when error_resilient_mode is 0
			// and the sequence enables it.
			if t.ref_frame_mvs {
				w.flag(false);
			}
		}
		w.flag(false); // disable_frame_end_update_cdf
		// tile_info(): one uniform tile.
		w.flag(true); // uniform_tile_spacing_flag
		if self.max_log2_tile_cols > 0 {
			w.flag(false); // increment_tile_cols_log2
		}
		if self.max_log2_tile_rows > 0 {
			w.flag(false); // increment_tile_rows_log2
		}
		// quantization_params()
		let qindex = w.bit_position();
		w.u(8, BASE_Q_IDX); // base_q_idx
		w.flag(false); // DeltaQYDc: delta_coded
		w.flag(false); // DeltaQUDc: delta_coded (separate_uv_delta_q = 0)
		w.flag(false); // DeltaQUAc: delta_coded
		w.flag(false); // using_qmatrix
		w.flag(false); // segmentation_enabled
		w.flag(false); // delta_q_present (base_q_idx > 0)
		// loop_filter_params(): zero luma levels, so no chroma levels.
		let loop_filter = w.bit_position();
		w.u(6, 0); // loop_filter_level[0]
		w.u(6, 0); // loop_filter_level[1]
		w.u(3, 0); // loop_filter_sharpness
		w.flag(false); // loop_filter_delta_enabled
		// cdef_params()
		let cdef = w.bit_position();
		w.u(2, 0); // cdef_damping_minus_3
		w.u(2, CDEF_BITS);
		for _ in 0..1 << CDEF_BITS {
			w.u(4, 0); // cdef_y_pri_strength
			w.u(2, 0); // cdef_y_sec_strength
			w.u(4, 0); // cdef_uv_pri_strength
			w.u(2, 0); // cdef_uv_sec_strength
		}
		let cdef_bits = w.bit_position() - cdef;
		// lr_params(): restoration disabled.
		w.flag(t.tx_mode_select); // tx_mode_select
		if !key {
			w.flag(false); // reference_select: single reference
			// skip_mode_params(): not allowed without reference_select.
			// allow_warped_motion coded only when the sequence enables it.
			if t.warped_motion {
				w.flag(false);
			}
		}
		w.flag(false); // reduced_tx_set
		if !key {
			// global_motion_params(): is_global for LAST..ALTREF.
			for _ in 0..REFS_PER_FRAME {
				w.flag(false);
			}
		}
		// film_grain_params(): not present.
		let payload_bits = w.bit_position() + 1;
		w.trailing_bits();
		let payload = w.into_bytes();

		let prefix = (1 + OBU_SIZE_BYTES) as u32 * 8;
		let mut obu = vec![obu_header(OBU_FRAME_HEADER)];
		leb128(&mut obu, payload.len() as u32, Some(OBU_SIZE_BYTES));
		obu.extend(payload);
		FrameHeader {
			obu,
			qindex: prefix + qindex,
			loop_filter: prefix + loop_filter,
			cdef: prefix + cdef,
			cdef_bits,
			size_bits: prefix + payload_bits,
		}
	}

	/// Picture parameters of `frame`; `header` is its frame header OBU and
	/// `header_offset` the bytes of packed units submitted before it.
	pub(super) fn picture(
		&self,
		setup: &Setup,
		frame: &Frame,
		coded: VABufferID,
		header: &FrameHeader,
		header_offset: u32,
	) -> VAEncPictureParameterBufferAV1 {
		let t = &self.tools;
		let key = frame.idr;
		let mut picture = VAEncPictureParameterBufferAV1 {
			frame_width_minus_1: (setup.width - 1) as u16,
			frame_height_minus_1: (setup.height - 1) as u16,
			reconstructed_frame: frame.recon,
			coded_buf: coded,
			reference_frames: [VA_INVALID_SURFACE; 8],
			ref_frame_idx: [0; REFS_PER_FRAME],
			primary_ref_frame: if key { PRIMARY_REF_NONE } else { PRIMARY_REF_LAST } as u8,
			order_hint: order_hint(frame.index_since_idr) as u8,
			refresh_frame_flags: if key { ALL_FRAMES } else { 1 },
			interpolation_filter: t.interpolation_filter as u8,
			base_qindex: BASE_Q_IDX as u8,
			min_base_qindex: 1,
			max_base_qindex: 255,
			tile_cols: 1,
			tile_rows: 1,
			bit_offset_qindex: header.qindex,
			bit_offset_loopfilter_params: header.loop_filter,
			bit_offset_cdef_params: header.cdef,
			size_in_bits_cdef_params: header.cdef_bits,
			byte_offset_frame_hdr_obu_size: header_offset + 1,
			size_in_bits_frame_hdr_obu: header.size_bits,
			..Default::default()
		};
		picture.width_in_sbs_minus_1[0] = (self.sb_cols - 1) as u16;
		picture.height_in_sbs_minus_1[0] = (self.sb_rows - 1) as u16;
		if let (false, Some((surface, _))) = (key, frame.reference) {
			picture.reference_frames[0] = surface;
			// SAFETY: writing a plain-data bitfield view of a union.
			unsafe { picture.ref_frame_ctrl_l0.fields.set_search_idx0(1) }; // LAST_FRAME
		}
		// SAFETY: writing plain-data bitfield views of unions.
		let flags = unsafe { &mut picture.picture_flags.bits };
		flags.set_frame_type(if key { KEY_FRAME } else { INTER_FRAME });
		flags.set_error_resilient_mode(u32::from(key));
		// SAFETY: as above.
		let mode = unsafe { &mut picture.mode_control_flags.bits };
		mode.set_tx_mode(if t.tx_mode_select { 2 } else { 1 });
		// SAFETY: as above.
		unsafe { picture.tile_group_obu_hdr_info.bits.set_obu_has_size_field(1) };
		picture
	}

	pub(super) fn tile_group() -> VAEncTileGroupBufferAV1 {
		VAEncTileGroupBufferAV1 {
			tg_start: 0,
			tg_end: 0,
			..Default::default()
		}
	}
}

fn order_hint(index_since_idr: u64) -> u32 {
	(index_since_idr % (1 << ORDER_HINT_BITS)) as u32
}

/// `obu_header()` without extension, with `obu_has_size_field`.
fn obu_header(obu_type: u8) -> u8 {
	(obu_type << 3) | 0x02
}

/// LEB128 `value`, padded to `bytes` bytes when given.
fn leb128(out: &mut Vec<u8>, mut value: u32, bytes: Option<usize>) {
	let length = bytes.unwrap_or_else(|| (1..=5).find(|n| u64::from(value) < 1 << (7 * n)).unwrap_or(5));
	for index in 0..length {
		let more = if index + 1 < length { 0x80 } else { 0 };
		out.push((value & 0x7f) as u8 | more);
		value >>= 7;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pixelforge::ColorDescription;

	fn setup(width: u32, height: u32, bit_depth: u32) -> Setup {
		Setup {
			width,
			height,
			coded_width: width.next_multiple_of(16),
			coded_height: height.next_multiple_of(16),
			fps: 60,
			bitrate: 20_000_000,
			max_references: 1,
			chroma_444: false,
			bit_depth,
			color: ColorDescription::bt709(),
		}
	}

	const TOOLS: Tools = Tools {
		sb128: false,
		filter_intra: false,
		intra_edge_filter: false,
		interintra_compound: false,
		masked_compound: false,
		warped_motion: false,
		dual_filter: false,
		jnt_comp: false,
		ref_frame_mvs: false,
		interpolation_filter: 0,
		tx_mode_select: true,
	};

	fn frame(idr: bool, index_since_idr: u64) -> Frame {
		Frame {
			idr,
			index_since_idr,
			idr_count: 0,
			recon: 7,
			retained: [None; 4],
			reference: (!idr).then(|| (6, index_since_idr - 1)),
		}
	}

	fn bits(data: &[u8], offset: u32, count: u32) -> u32 {
		(offset..offset + count).fold(0, |value, bit| {
			(value << 1) | u32::from(data[(bit / 8) as usize] >> (7 - bit % 8) & 1)
		})
	}

	#[test]
	fn level_follows_picture_size_and_rate() {
		let level = |width, height, fps| seq_level_tier(width, height, fps, 10_000_000);
		assert_eq!(level(1920, 1080, 30), (8, false)); // 4.0
		assert_eq!(level(1920, 1080, 60), (9, false)); // 4.1
		assert_eq!(level(1920, 1080, 120), (12, false)); // 5.0
		assert_eq!(level(2560, 1440, 120), (13, false)); // 5.1
		assert_eq!(level(3840, 2160, 120), (14, false)); // 5.2
		assert_eq!(level(16384, 16384, 60), (31, false));
	}

	#[test]
	fn level_and_tier_bound_the_bitrate() {
		// 1080p60 is 4.1 by size; Main tier 4.1 carries 20 Mbit/s, 5.2 60.
		assert_eq!(seq_level_tier(1920, 1080, 60, 20_000_000), (9, false));
		assert_eq!(seq_level_tier(1920, 1080, 60, 50_000_000), (14, false));
		assert_eq!(seq_level_tier(1920, 1080, 60, 100_000_000), (17, false));
		// Beyond Main tier 6.3 (160 Mbit/s): the lowest High tier level.
		assert_eq!(seq_level_tier(1920, 1080, 60, 200_000_000), (14, true));
		// Beyond every level: no level constraints.
		assert_eq!(seq_level_tier(1920, 1080, 60, 900_000_000), (31, false));
	}

	#[test]
	fn leb128_pads_to_the_requested_length() {
		let mut out = Vec::new();
		leb128(&mut out, 300, Some(4));
		assert_eq!(out, [0xac, 0x82, 0x80, 0x00]);
		out.clear();
		leb128(&mut out, 300, None);
		assert_eq!(out, [0xac, 0x02]);
	}

	#[test]
	fn frame_header_offsets_point_at_their_syntax_elements() {
		for (key, index) in [(true, 0), (false, 5)] {
			let av1 = Av1::new(&setup(1920, 1080, 8), TOOLS).unwrap();
			let header = av1.frame_header(&frame(key, index));
			let obu = &header.obu;
			assert_eq!(obu[0], 0x1a, "OBU_FRAME_HEADER with a size field");
			// The four-byte obu_size covers the payload.
			let size = (0..4).map(|i| u32::from(obu[1 + i] & 0x7f) << (7 * i)).sum::<u32>();
			assert_eq!(size as usize, obu.len() - 5);
			assert_eq!(bits(obu, header.qindex, 8), BASE_Q_IDX);
			// Two zero levels, sharpness, delta_enabled: then cdef_params().
			assert_eq!(header.cdef - header.loop_filter, 16);
			assert_eq!(bits(obu, header.loop_filter, 16), 0);
			assert_eq!(header.cdef_bits, 4 + (1 << CDEF_BITS) * 12);
			// The size ends at the trailing one bit; only zeros follow.
			assert_eq!(bits(obu, header.size_bits - 1, 1), 1);
			let total = obu.len() as u32 * 8;
			assert_eq!(bits(obu, header.size_bits, total - header.size_bits), 0);
		}
	}

	#[test]
	fn small_frames_code_no_tile_increments_and_large_ones_need_tiles() {
		let av1 = Av1::new(&setup(64, 64, 8), TOOLS).unwrap();
		assert_eq!((av1.max_log2_tile_cols, av1.max_log2_tile_rows), (0, 0));
		let av1 = Av1::new(&setup(1920, 1080, 8), TOOLS).unwrap();
		assert_eq!((av1.sb_cols, av1.sb_rows), (30, 17));
		assert!(Av1::new(&setup(7680, 4320, 8), TOOLS).is_err());
	}

	#[test]
	fn attributes_enable_only_required_tools() {
		let tools = Tools::from_attributes(
			VA_ATTRIB_NOT_SUPPORTED,
			VA_ATTRIB_NOT_SUPPORTED,
			VA_ATTRIB_NOT_SUPPORTED,
		)
		.unwrap();
		assert_eq!(tools, TOOLS);
		// 128x128 required (2 at bits 0-1), filter intra supported only;
		// switchable interpolation; TX_MODE_LARGEST only.
		let tools = Tools::from_attributes(2 | (1 << 2), 0x1f, 2 << 4).unwrap();
		assert!(tools.sb128 && !tools.filter_intra && !tools.tx_mode_select);
		assert_eq!(tools.interpolation_filter, SWITCHABLE);
		assert!(Tools::from_attributes(0, 0, 1 << 4).is_none());
	}

	#[test]
	fn sequence_header_carries_size_and_bit_depth() {
		let av1 = Av1::new(&setup(1366, 768, 10), TOOLS).unwrap();
		let obu = av1.sequence_header(&setup(1366, 768, 10));
		assert_eq!(obu[0], 0x0a, "OBU_SEQUENCE_HEADER with a size field");
		assert_eq!(obu[1] as usize, obu.len() - 2);
		let payload = &obu[2..];
		// seq_profile..operating_point_idc: 3+1+1+1+1+5+12 bits, then the
		// level (4.1 for 20 Mbit/s), a tier bit (levels above 7) and the
		// frame size.
		assert_eq!(bits(payload, 24, 5), 9);
		assert_eq!(bits(payload, 29, 1), 0);
		// frame_width_bits_minus_1: 1365 needs 11 bits; 767 needs 10.
		assert_eq!(bits(payload, 30, 4), 10);
		assert_eq!(bits(payload, 34, 4), 9);
		assert_eq!(bits(payload, 38, 11), 1365);
		assert_eq!(bits(payload, 49, 10), 767);
	}
}
