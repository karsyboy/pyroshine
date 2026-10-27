//! VA-API H.264, HEVC and AV1 encoding on the capture GPU. Raw frames stay
//! on the GPU: the converter's output is copied into the encoder's surfaces.
//!
//! libva is loaded at runtime (`libva.so.2`, `libva-drm.so.2`), so hosts
//! without it are unaffected. The display is opened on the capture GPU's own
//! render node (from its Vulkan DRM identity), never on another GPU.
//!
//! Input surfaces are allocated by the driver in the layout its encoder reads
//! (tiled, possibly a packed 4:4:4 format), exported as DMA-BUF and imported
//! into the capture Vulkan device, one single-plane image per layer: the color
//! converter copies each frame into them ([`Target::ExternalPlanes`]), handing
//! them over through `VK_QUEUE_FAMILY_FOREIGN_EXT` because the VA driver is not
//! the Vulkan driver. The converter waits for its fence before
//! [`VaapiEncoder::encode`] submits the surface, which orders the write before
//! the encoder's read; a surface is handed out again only after its encode was
//! read back, or, when a submission failed after the driver may have accepted
//! it, after the surface was synced. When completion cannot be established
//! the encoder fails closed: the stream ends and the VA objects are never
//! destroyed under the driver.
//!
//! Frames: an IDR (AV1 key frame) on start and on request, otherwise P
//! frames (low-delay B frames where the driver needs them; AV1 inter frames)
//! predicting from the previous frame, constant bitrate with a one-frame
//! buffer, no frame skipping, no filler. Reference invalidation re-anchors
//! with an IDR.

mod av1;
mod bits;
mod h264;
mod hevc;
mod recovery;
pub(crate) mod sys;

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use ash::vk;
use ash::vk::TaggedStructure;
use libloading::Library;
use pixelforge::{ColorDescription, VideoContext};
use sys::*;

use super::backend::{EncodeError, NativeFormat, PROBE_EXTENT, Packet, PacketFuture};
use super::convert::{ConvertFormat, Target};
use crate::session::stream::video::{BitDepth, ChromaFormat, VideoCodec};

/// Input surfaces: one being converted, one encoding, one read back.
const INPUT_SURFACES: usize = 3;
/// How long `input_target`/`flush` wait for the readback thread before the
/// encoder is declared hung.
const SLOT_TIMEOUT: Duration = Duration::from_secs(5);

type GetDisplayDrm = unsafe extern "C" fn(c_int) -> VADisplay;
type Initialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type Terminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type ErrorStr = unsafe extern "C" fn(VAStatus) -> *const c_char;
type QueryVendorString = unsafe extern "C" fn(VADisplay) -> *const c_char;
type MaxNum = unsafe extern "C" fn(VADisplay) -> c_int;
type QueryConfigEntrypoints = unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
type GetConfigAttributes =
	unsafe extern "C" fn(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int) -> VAStatus;
type CreateConfig =
	unsafe extern "C" fn(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus;
type DestroyConfig = unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus;
type CreateSurfaces = unsafe extern "C" fn(
	VADisplay,
	c_uint,
	c_uint,
	c_uint,
	*mut VASurfaceID,
	c_uint,
	*mut VASurfaceAttrib,
	c_uint,
) -> VAStatus;
type DestroySurfaces = unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
type CreateContext = unsafe extern "C" fn(
	VADisplay,
	VAConfigID,
	c_int,
	c_int,
	c_int,
	*mut VASurfaceID,
	c_int,
	*mut VAContextID,
) -> VAStatus;
type DestroyContext = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type CreateBuffer = unsafe extern "C" fn(
	VADisplay,
	VAContextID,
	VABufferType,
	c_uint,
	c_uint,
	*mut c_void,
	*mut VABufferID,
) -> VAStatus;
type DestroyBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type MapBuffer = unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
type UnmapBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type BeginPicture = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
type RenderPicture = unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
type EndPicture = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type SyncSurface = unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus;
type SyncBuffer = unsafe extern "C" fn(VADisplay, VABufferID, u64) -> VAStatus;
type ExportSurfaceHandle = unsafe extern "C" fn(VADisplay, VASurfaceID, u32, u32, *mut c_void) -> VAStatus;
type SetCallback = unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback;

/// libva entry points (VA-API 1.x, whose ABI is stable).
pub(crate) struct Va {
	// Must outlive every copied symbol pointer below.
	_va: Library,
	_drm: Library,
	get_display_drm: GetDisplayDrm,
	initialize: Initialize,
	terminate: Terminate,
	error_str: ErrorStr,
	query_vendor_string: QueryVendorString,
	max_num_entrypoints: MaxNum,
	query_config_entrypoints: QueryConfigEntrypoints,
	get_config_attributes: GetConfigAttributes,
	create_config: CreateConfig,
	destroy_config: DestroyConfig,
	create_surfaces: CreateSurfaces,
	destroy_surfaces: DestroySurfaces,
	create_context: CreateContext,
	destroy_context: DestroyContext,
	create_buffer: CreateBuffer,
	destroy_buffer: DestroyBuffer,
	map_buffer: MapBuffer,
	unmap_buffer: UnmapBuffer,
	begin_picture: BeginPicture,
	render_picture: RenderPicture,
	end_picture: EndPicture,
	sync_surface: SyncSurface,
	/// libva 2.9 or newer; otherwise the input surface is synced.
	sync_buffer: Option<SyncBuffer>,
	export_surface_handle: ExportSurfaceHandle,
	set_error_callback: SetCallback,
	set_info_callback: SetCallback,
}

fn load() -> Result<Va, String> {
	// SAFETY: fixed system library names; every symbol's type mirrors va.h
	// and va_drm.h of VA-API 1.x.
	unsafe {
		let va = Library::new("libva.so.2").map_err(|e| format!("libva.so.2: {e}"))?;
		let drm = Library::new("libva-drm.so.2").map_err(|e| format!("libva-drm.so.2: {e}"))?;
		macro_rules! symbol {
			($library:expr, $name:literal) => {
				*$library
					.get(concat!($name, "\0").as_bytes())
					.map_err(|e| format!("libva is missing {}: {e}", $name))?
			};
		}
		Ok(Va {
			get_display_drm: symbol!(drm, "vaGetDisplayDRM"),
			initialize: symbol!(va, "vaInitialize"),
			terminate: symbol!(va, "vaTerminate"),
			error_str: symbol!(va, "vaErrorStr"),
			query_vendor_string: symbol!(va, "vaQueryVendorString"),
			max_num_entrypoints: symbol!(va, "vaMaxNumEntrypoints"),
			query_config_entrypoints: symbol!(va, "vaQueryConfigEntrypoints"),
			get_config_attributes: symbol!(va, "vaGetConfigAttributes"),
			create_config: symbol!(va, "vaCreateConfig"),
			destroy_config: symbol!(va, "vaDestroyConfig"),
			create_surfaces: symbol!(va, "vaCreateSurfaces"),
			destroy_surfaces: symbol!(va, "vaDestroySurfaces"),
			create_context: symbol!(va, "vaCreateContext"),
			destroy_context: symbol!(va, "vaDestroyContext"),
			create_buffer: symbol!(va, "vaCreateBuffer"),
			destroy_buffer: symbol!(va, "vaDestroyBuffer"),
			map_buffer: symbol!(va, "vaMapBuffer"),
			unmap_buffer: symbol!(va, "vaUnmapBuffer"),
			begin_picture: symbol!(va, "vaBeginPicture"),
			render_picture: symbol!(va, "vaRenderPicture"),
			end_picture: symbol!(va, "vaEndPicture"),
			sync_surface: symbol!(va, "vaSyncSurface"),
			sync_buffer: va.get(b"vaSyncBuffer\0").ok().map(|symbol| *symbol),
			export_surface_handle: symbol!(va, "vaExportSurfaceHandle"),
			set_error_callback: symbol!(va, "vaSetErrorCallback"),
			set_info_callback: symbol!(va, "vaSetInfoCallback"),
			_va: va,
			_drm: drm,
		})
	}
}

/// The process-wide libva, loaded on first use.
fn library() -> Result<&'static Va, String> {
	static VA: OnceLock<Result<Va, String>> = OnceLock::new();
	VA.get_or_init(load).as_ref().map_err(Clone::clone)
}

unsafe extern "C" fn log_error(_: *mut c_void, message: *const c_char) {
	// SAFETY: libva passes a NUL-terminated message.
	let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
	tracing::warn!(message = message.trim_end(), "libva");
}

unsafe extern "C" fn log_info(_: *mut c_void, message: *const c_char) {
	// SAFETY: as above.
	let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
	tracing::debug!(message = message.trim_end(), "libva");
}

/// An initialized VA display on one render node.
///
/// VA-API calls on one display are thread-safe in the drivers this backend
/// supports (iHD, radeonsi); submissions and buffer mapping are additionally
/// serialized by [`Shared::lock`].
struct Display {
	va: &'static Va,
	raw: VADisplay,
	// Keeps the render node open for the display's lifetime.
	_node: std::fs::File,
	vendor: String,
}

// SAFETY: see the type documentation; the raw handle is only used through
// libva, which owns the display state.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
	fn open(node: &Path) -> Result<Self, String> {
		let va = library()?;
		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(node)
			.map_err(|e| format!("cannot open {}: {e}", node.display()))?;
		// SAFETY: the descriptor stays open in `_node` while the display lives.
		let raw = unsafe { (va.get_display_drm)(file.as_raw_fd()) };
		if raw.is_null() {
			return Err(format!("libva has no DRM display for {}", node.display()));
		}
		// SAFETY: valid display; the callbacks only log the message.
		unsafe {
			(va.set_error_callback)(raw, Some(log_error), std::ptr::null_mut());
			(va.set_info_callback)(raw, Some(log_info), std::ptr::null_mut());
		}
		let (mut major, mut minor) = (0, 0);
		// SAFETY: valid display and output pointers.
		let status = unsafe { (va.initialize)(raw, &mut major, &mut minor) };
		if status as u32 != VA_STATUS_SUCCESS {
			// SAFETY: a display that failed to initialize is still terminated.
			unsafe { (va.terminate)(raw) };
			return Err(format!(
				"no VA-API driver for {}: {}",
				node.display(),
				error_string(va, status)
			));
		}
		// SAFETY: initialized display; libva returns a static string.
		let vendor = unsafe { CStr::from_ptr((va.query_vendor_string)(raw)) }
			.to_string_lossy()
			.into_owned();
		Ok(Self {
			va,
			raw,
			_node: file,
			vendor,
		})
	}

	fn check(&self, status: VAStatus, what: &str) -> Result<(), EncodeError> {
		if status as u32 == VA_STATUS_SUCCESS {
			Ok(())
		} else {
			Err(EncodeError::native(format!(
				"VA-API {what}: {}",
				error_string(self.va, status)
			)))
		}
	}
}

impl Drop for Display {
	fn drop(&mut self) {
		// SAFETY: every config, context, surface and buffer was destroyed first.
		unsafe { (self.va.terminate)(self.raw) };
	}
}

fn error_string(va: &Va, status: VAStatus) -> String {
	// SAFETY: vaErrorStr returns a static string for any status.
	let text = unsafe { CStr::from_ptr((va.error_str)(status)) }.to_string_lossy();
	format!("{text} ({status})")
}

/// The VA profile, render-target format and surface fourcc of a format.
struct Profile {
	profile: VAProfile,
	rt_format: u32,
	fourcc: u32,
	convert: ConvertFormat,
	kind: Kind,
	label: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
	H264,
	Hevc,
	Av1,
}

fn profile(codec: VideoCodec, chroma: ChromaFormat, depth: BitDepth) -> Result<Profile, String> {
	let profile = |profile, rt_format, fourcc, convert, kind, label| Profile {
		profile,
		rt_format,
		fourcc,
		convert,
		kind,
		label,
	};
	Ok(match (codec, chroma, depth) {
		(VideoCodec::H264, ChromaFormat::Yuv420, BitDepth::Eight) => profile(
			VAProfileH264High,
			VA_RT_FORMAT_YUV420,
			VA_FOURCC_NV12,
			ConvertFormat::Nv12,
			Kind::H264,
			"H.264 High",
		),
		(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Eight) => profile(
			VAProfileHEVCMain,
			VA_RT_FORMAT_YUV420,
			VA_FOURCC_NV12,
			ConvertFormat::Nv12,
			Kind::Hevc,
			"HEVC Main",
		),
		(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Ten) => profile(
			VAProfileHEVCMain10,
			VA_RT_FORMAT_YUV420_10,
			VA_FOURCC_P010,
			ConvertFormat::P010,
			Kind::Hevc,
			"HEVC Main 10",
		),
		(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Eight) => profile(
			VAProfileHEVCMain444,
			VA_RT_FORMAT_YUV444,
			VA_FOURCC_XYUV,
			ConvertFormat::Xyuv,
			Kind::Hevc,
			"HEVC Main 4:4:4",
		),
		(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Ten) => profile(
			VAProfileHEVCMain444_10,
			VA_RT_FORMAT_YUV444_10,
			VA_FOURCC_Y410,
			ConvertFormat::Y410,
			Kind::Hevc,
			"HEVC Main 4:4:4 10",
		),
		(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Eight) => profile(
			VAProfileAV1Profile0,
			VA_RT_FORMAT_YUV420,
			VA_FOURCC_NV12,
			ConvertFormat::Nv12,
			Kind::Av1,
			"AV1 Main",
		),
		(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Ten) => profile(
			VAProfileAV1Profile0,
			VA_RT_FORMAT_YUV420_10,
			VA_FOURCC_P010,
			ConvertFormat::P010,
			Kind::Av1,
			"AV1 Main 10",
		),
		(VideoCodec::Av1, ..) => return Err("VA-API AV1 encodes 4:2:0 only".into()),
		(VideoCodec::H264, ..) => return Err("VA-API H.264 encodes 4:2:0 8-bit only".into()),
		_ => return Err(format!("{codec} is not a VA-API codec")),
	})
}

/// Frame geometry and rate control shared by the codec writers.
pub(super) struct Setup {
	pub width: u32,
	pub height: u32,
	/// Surface extent: the frame rounded up to 16 luma samples.
	pub coded_width: u32,
	pub coded_height: u32,
	pub fps: u32,
	pub bitrate: u32,
	pub max_references: usize,
	pub chroma_444: bool,
	pub bit_depth: u32,
	pub color: ColorDescription,
}

impl Setup {
	/// Luma samples cropped on the right and at the bottom.
	pub(super) fn crop(&self) -> (u32, u32) {
		(self.coded_width - self.width, self.coded_height - self.height)
	}
}

/// One frame's position in the reference structure.
pub(super) struct Frame {
	pub idr: bool,
	pub retained: [Option<(VASurfaceID, u64)>; recovery::MAX_REFS],
	/// Frames since the last IDR (0 for the IDR itself).
	pub index_since_idr: u64,
	/// IDRs before this one (`idr_pic_id`).
	pub idr_count: u64,
	/// Reconstructed-picture surface.
	pub recon: VASurfaceID,
	/// The reference picture and its `index_since_idr`.
	pub reference: Option<(VASurfaceID, u64)>,
}

enum Codec {
	H264(h264::H264),
	Hevc(hevc::Hevc),
	Av1(av1::Av1),
}

/// Input surface with its Vulkan views and coded buffer.
struct Slot {
	surface: VASurfaceID,
	coded: VABufferID,
	images: [vk::Image; 2],
	memories: Vec<vk::DeviceMemory>,
	planes: usize,
}

/// State shared with the readback thread.
struct Shared {
	display: Arc<Display>,
	/// Serializes submissions with coded-buffer mapping.
	lock: Mutex<()>,
	/// Per input slot: submitted and not yet read back.
	busy: Mutex<Vec<bool>>,
	freed: Condvar,
	/// The completion of a submitted frame could not be established: its
	/// slot stays busy, the encoder fails every later frame and its VA
	/// objects are never destroyed. Set while holding `busy`.
	lost: AtomicBool,
}

impl Shared {
	/// Record that a slot's completion is unknown (see [`Self::lost`]).
	fn mark_lost(&self) {
		let _busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
		self.lost.store(true, Ordering::Release);
		self.freed.notify_all();
	}
}

struct Job {
	slot: usize,
	surface: VASurfaceID,
	coded: VABufferID,
	key_frame: bool,
	/// Bytes the packet starts with before the coded data: the AV1 temporal
	/// delimiter, which no driver is asked to write.
	prefix: &'static [u8],
	/// `None` for a failed submission the driver may have accepted: the
	/// readback thread only establishes its completion before freeing the slot.
	reply: Option<tokio::sync::oneshot::Sender<Result<Packet, EncodeError>>>,
}

/// A failed submission. `accepted`: `vaBeginPicture` succeeded, so the driver
/// may have taken work that still reads the input surface and writes its
/// coded buffer; the slot is freed only once that work is known to be done.
struct SubmitError {
	error: EncodeError,
	accepted: bool,
}

impl From<EncodeError> for SubmitError {
	fn from(error: EncodeError) -> Self {
		Self { error, accepted: false }
	}
}

/// A VA-API encoder of one stream epoch.
pub(crate) struct VaapiEncoder {
	shared: Arc<Shared>,
	vulkan: VideoContext,
	node: PathBuf,
	profile: VAProfile,
	entrypoint: VAEntrypoint,
	config: VAConfigID,
	context: VAContextID,
	slots: Vec<Slot>,
	recon: Vec<VASurfaceID>,
	dpb: recovery::Dpb,
	codec: Codec,
	setup: Setup,
	bitrate_ceiling: u32,
	installed_bitrate: u32,
	packed_headers: u32,
	convert: ConvertFormat,
	current: usize,
	submitted: u64,
	index_since_idr: u64,
	idr_count: u64,
	idr_pending: bool,
	jobs: Option<std::sync::mpsc::Sender<Job>>,
	readback: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: VA handles are plain IDs owned by this encoder; Vulkan handles
// belong to the capture device, which is used from the encoding thread only.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
	/// Create the encoder on `vulkan`'s GPU for `format`, with the first
	/// entry point that initializes (see [`entrypoint_order`]).
	pub(crate) fn new(vulkan: &VideoContext, format: NativeFormat) -> Result<Self, String> {
		Self::create(vulkan, format, |_| Ok(()))
	}

	/// Create the encoder with each usable entry point in order until one
	/// initializes and passes `verify`.
	fn create(
		vulkan: &VideoContext,
		format: NativeFormat,
		verify: impl Fn(&mut Self) -> Result<(), String>,
	) -> Result<Self, String> {
		let profile = profile(format.codec, format.chroma, format.bit_depth)?;
		if !vulkan.has_queue_family_foreign() {
			return Err("the capture GPU's Vulkan driver lacks VK_EXT_queue_family_foreign, \
			            which hands VA-API surfaces between the two drivers"
				.into());
		}
		let node = crate::gpu::render_node(vulkan)
			.ok_or("the capture GPU's Vulkan driver reports no DRM render node for VA-API")?;
		let display = Arc::new(Display::open(&node)?);
		let order = entrypoint_order(
			&listed_entrypoints(&display, profile.profile),
			verified_entrypoint(&node, profile.profile),
		);
		if order.is_empty() {
			return Err(format!("{} has no VA-API {} encoder", display.vendor, profile.label));
		}
		let mut reasons = Vec::new();
		for entrypoint in order {
			let attempt = Self::with_entrypoint(vulkan, format, &profile, display.clone(), &node, entrypoint)
				.and_then(|mut encoder| verify(&mut encoder).map(|()| encoder));
			match attempt {
				Ok(encoder) => return Ok(encoder),
				Err(reason) => {
					tracing::debug!(
						profile = profile.label,
						entrypoint = entrypoint_label(entrypoint),
						reason,
						"VA-API entry point unusable"
					);
					reasons.push(format!("{} entry point: {reason}", entrypoint_label(entrypoint)));
				},
			}
		}
		Err(reasons.join("; "))
	}

	fn with_entrypoint(
		vulkan: &VideoContext,
		format: NativeFormat,
		profile: &Profile,
		display: Arc<Display>,
		node: &Path,
		entrypoint: VAEntrypoint,
	) -> Result<Self, String> {
		let va = display.va;
		let raw = display.raw;

		let mut attributes = [
			VAConfigAttrib {
				type_: VAConfigAttribRTFormat,
				value: 0,
			},
			VAConfigAttrib {
				type_: VAConfigAttribRateControl,
				value: 0,
			},
			VAConfigAttrib {
				type_: VAConfigAttribEncPackedHeaders,
				value: 0,
			},
		];
		// SAFETY: valid display and attribute array.
		let status = unsafe {
			(va.get_config_attributes)(
				raw,
				profile.profile,
				entrypoint,
				attributes.as_mut_ptr(),
				attributes.len() as c_int,
			)
		};
		display.check(status, "config attributes").map_err(|e| e.to_string())?;
		let [rt, rc, packed] = attributes.map(|a| a.value);
		let mut references = VAConfigAttrib {
			type_: VAConfigAttribEncMaxRefFrames,
			value: 0,
		};
		// SAFETY: a single valid attribute and the already selected profile.
		let refs_status = unsafe { (va.get_config_attributes)(raw, profile.profile, entrypoint, &mut references, 1) };
		// EncMaxRefFrames bounds the *active list*, which stays at one.
		// We allocate the retained reconstructions ourselves. Four short-term
		// entries fit H.264/HEVC's DPB at every selected level, and both VA
		// picture parameter arrays; no additional prediction references or
		// reorder delay are enabled. Unknown capability keeps one entry.
		let max_references = if refs_status == VA_STATUS_SUCCESS as i32
			&& references.value != VA_ATTRIB_NOT_SUPPORTED
			&& references.value & 0xffff >= 1
			&& profile.kind != Kind::Av1
		{
			recovery::MAX_REFS
		} else {
			1
		};
		// Codec tools: HEVC features, block sizes and prediction directions;
		// AV1 features and their two extensions.
		let mut codec_attributes = match profile.kind {
			Kind::H264 => Vec::new(),
			Kind::Hevc => vec![
				VAConfigAttribEncHEVCFeatures,
				VAConfigAttribEncHEVCBlockSizes,
				VAConfigAttribPredictionDirection,
			],
			Kind::Av1 => vec![VAConfigAttribEncAV1, VAConfigAttribEncAV1Ext1, VAConfigAttribEncAV1Ext2],
		}
		.into_iter()
		.map(|type_| VAConfigAttrib { type_, value: 0 })
		.collect::<Vec<_>>();
		if !codec_attributes.is_empty() {
			// SAFETY: valid display and attribute array.
			let status = unsafe {
				(va.get_config_attributes)(
					raw,
					profile.profile,
					entrypoint,
					codec_attributes.as_mut_ptr(),
					codec_attributes.len() as c_int,
				)
			};
			display.check(status, "codec attributes").map_err(|e| e.to_string())?;
		}
		let tool = |index: usize| codec_attributes.get(index).map_or(VA_ATTRIB_NOT_SUPPORTED, |a| a.value);
		if rt == VA_ATTRIB_NOT_SUPPORTED || rt & profile.rt_format == 0 {
			return Err(format!(
				"{} cannot encode {} surfaces",
				profile.label,
				format_label(profile.convert)
			));
		}
		if rc == VA_ATTRIB_NOT_SUPPORTED || rc & VA_RC_CBR == 0 {
			return Err(format!("{} has no constant-bitrate rate control", profile.label));
		}
		let required = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE;
		if packed == VA_ATTRIB_NOT_SUPPORTED || packed & required != required {
			return Err(format!(
				"{} does not take packed sequence and picture headers",
				profile.label
			));
		}
		let wanted = if profile.kind == Kind::Av1 {
			required
		} else {
			// Packed slice headers where the driver takes them, as FFmpeg and
			// GStreamer send them: Mesa codes slices only from packed headers.
			required | (packed & VA_ENC_PACKED_HEADER_SLICE)
		};

		let coded_width = format.width.next_multiple_of(16);
		let coded_height = format.height.next_multiple_of(16);
		tracing::debug!(
			reported_refs = references.value,
			retained_refs = max_references,
			"VA-API reference retention"
		);
		let setup = Setup {
			width: format.width,
			height: format.height,
			coded_width,
			coded_height,
			fps: format.fps,
			bitrate: format.bitrate,
			max_references,
			chroma_444: format.chroma == ChromaFormat::Yuv444,
			bit_depth: u32::from(format.bit_depth.bits()),
			color: format.color,
		};
		let codec = match profile.kind {
			Kind::H264 => Codec::H264(h264::H264::new(&setup)),
			Kind::Hevc => Codec::Hevc(hevc::Hevc::new(
				&setup,
				hevc::Tools::from_attributes(tool(0), tool(1), tool(2)),
			)),
			Kind::Av1 => {
				// Mesa codes the frame size rounded up to 8 samples and no
				// render size, so the client would decode a larger picture
				// than negotiated (RX 9070 XT, Mesa 26.2).
				if display.vendor.starts_with("Mesa")
					&& !(format.width.is_multiple_of(8) && format.height.is_multiple_of(8))
				{
					return Err(format!(
						"{} codes AV1 frames in multiples of 8 samples, not {}x{}",
						display.vendor, format.width, format.height
					));
				}
				let tools = av1::Tools::from_attributes(tool(0), tool(1), tool(2))
					.ok_or_else(|| format!("{} offers no usable transform mode", profile.label))?;
				Codec::Av1(av1::Av1::new(&setup, tools)?)
			},
		};

		let mut config_attributes = [
			VAConfigAttrib {
				type_: VAConfigAttribRTFormat,
				value: profile.rt_format,
			},
			VAConfigAttrib {
				type_: VAConfigAttribRateControl,
				value: VA_RC_CBR,
			},
			VAConfigAttrib {
				type_: VAConfigAttribEncPackedHeaders,
				value: wanted,
			},
		];
		let mut config = VA_INVALID_ID;
		// SAFETY: valid display, attributes and output.
		let status = unsafe {
			(va.create_config)(
				raw,
				profile.profile,
				entrypoint,
				config_attributes.as_mut_ptr(),
				config_attributes.len() as c_int,
				&mut config,
			)
		};
		display.check(status, "create config").map_err(|e| e.to_string())?;

		let shared = Arc::new(Shared {
			display,
			lock: Mutex::new(()),
			busy: Mutex::new(vec![false; INPUT_SURFACES]),
			freed: Condvar::new(),
			lost: AtomicBool::new(false),
		});
		let mut encoder = Self {
			shared,
			vulkan: vulkan.clone(),
			node: node.to_path_buf(),
			profile: profile.profile,
			entrypoint,
			config,
			context: VA_INVALID_ID,
			slots: Vec::new(),
			recon: Vec::new(),
			dpb: recovery::Dpb::new(max_references),
			codec,
			setup,
			packed_headers: wanted,
			convert: profile.convert,
			current: 0,
			submitted: 0,
			index_since_idr: 0,
			idr_count: 0,
			bitrate_ceiling: format.bitrate,
			installed_bitrate: format.bitrate,
			idr_pending: true,
			jobs: None,
			readback: None,
		};
		// Drop releases whatever was created if a later step fails.
		encoder.create_resources(profile)?;
		let (jobs, receiver) = std::sync::mpsc::channel();
		let shared = encoder.shared.clone();
		encoder.readback = Some(
			std::thread::Builder::new()
				.name("vaapi-readback".into())
				.spawn(move || readback(shared, receiver))
				.map_err(|e| format!("cannot start the VA-API readback thread: {e}"))?,
		);
		encoder.jobs = Some(jobs);
		tracing::info!(
			driver = %encoder.shared.display.vendor,
			profile = profile.label,
			entrypoint = entrypoint_label(entrypoint),
			coded_width,
			coded_height,
			"Created VA-API encoder"
		);
		Ok(encoder)
	}

	fn create_resources(&mut self, profile: &Profile) -> Result<(), String> {
		let va = self.shared.display.va;
		let raw = self.shared.display.raw;
		let (width, height) = (self.setup.coded_width, self.setup.coded_height);
		let mut attributes = [
			VASurfaceAttrib {
				type_: VASurfaceAttribPixelFormat,
				flags: VA_SURFACE_ATTRIB_SETTABLE,
				value: VAGenericValue {
					type_: VAGenericValueTypeInteger,
					value: _VAGenericValue__bindgen_ty_1 {
						i: profile.fourcc as i32,
					},
				},
			},
			VASurfaceAttrib {
				type_: VASurfaceAttribUsageHint,
				flags: VA_SURFACE_ATTRIB_SETTABLE,
				value: VAGenericValue {
					type_: VAGenericValueTypeInteger,
					value: _VAGenericValue__bindgen_ty_1 {
						i: (VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER | VA_SURFACE_ATTRIB_USAGE_HINT_EXPORT) as i32,
					},
				},
			},
		];
		let mut surfaces = vec![VA_INVALID_SURFACE; INPUT_SURFACES + self.setup.max_references + 1];
		// SAFETY: valid display, output array and attributes.
		let status = unsafe {
			(va.create_surfaces)(
				raw,
				profile.rt_format,
				width,
				height,
				surfaces.as_mut_ptr(),
				surfaces.len() as c_uint,
				attributes.as_mut_ptr(),
				attributes.len() as c_uint,
			)
		};
		self.shared
			.display
			.check(status, "create surfaces")
			.map_err(|e| e.to_string())?;
		self.recon = surfaces[INPUT_SURFACES..].to_vec();
		for &surface in &surfaces[..INPUT_SURFACES] {
			self.slots.push(Slot {
				surface,
				coded: VA_INVALID_ID,
				images: [vk::Image::null(); 2],
				memories: Vec::new(),
				planes: 0,
			});
		}

		// SAFETY: valid display, config and render-target array.
		let status = unsafe {
			(va.create_context)(
				raw,
				self.config,
				width as c_int,
				height as c_int,
				VA_PROGRESSIVE as c_int,
				surfaces.as_mut_ptr(),
				surfaces.len() as c_int,
				&mut self.context,
			)
		};
		self.shared
			.display
			.check(status, "create context")
			.map_err(|e| e.to_string())?;

		// A raw frame bounds any coded frame of these settings.
		let samples = u64::from(width) * u64::from(height);
		let bytes_per_sample = if self.setup.chroma_444 { 3 } else { 2 } * if self.setup.bit_depth > 8 { 2 } else { 1 };
		let coded_size = (samples * bytes_per_sample).clamp(1 << 20, 256 << 20) as c_uint;
		for index in 0..INPUT_SURFACES {
			let mut coded = VA_INVALID_ID;
			// SAFETY: valid display and context; no initial data.
			let status = unsafe {
				(va.create_buffer)(
					raw,
					self.context,
					VAEncCodedBufferType,
					coded_size,
					1,
					std::ptr::null_mut(),
					&mut coded,
				)
			};
			self.shared
				.display
				.check(status, "create coded buffer")
				.map_err(|e| e.to_string())?;
			self.slots[index].coded = coded;
			let (images, memories, planes) = self.import_surface(self.slots[index].surface)?;
			let slot = &mut self.slots[index];
			slot.images = images;
			slot.memories = memories;
			slot.planes = planes;
		}
		Ok(())
	}

	/// Export `surface` and import its layers into the capture Vulkan device.
	fn import_surface(&self, surface: VASurfaceID) -> Result<([vk::Image; 2], Vec<vk::DeviceMemory>, usize), String> {
		let display = &self.shared.display;
		let mut descriptor = VADRMPRIMESurfaceDescriptor::default();
		// SAFETY: valid display and surface; the descriptor receives the fds.
		let status = unsafe {
			(display.va.export_surface_handle)(
				display.raw,
				surface,
				VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
				VA_EXPORT_SURFACE_WRITE_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
				(&mut descriptor as *mut VADRMPRIMESurfaceDescriptor).cast(),
			)
		};
		display.check(status, "export surface").map_err(|e| e.to_string())?;
		let result = import_layers(
			&self.vulkan,
			&descriptor,
			self.convert,
			(self.setup.coded_width, self.setup.coded_height),
		);
		for object in &descriptor.objects[..descriptor.num_objects as usize] {
			// SAFETY: the export transferred these descriptors to us; Vulkan
			// imported duplicates.
			unsafe { libc::close(object.fd) };
		}
		result
	}

	/// The converter output this encoder reads, and the extent to convert.
	pub(crate) fn conversion(&self) -> (ConvertFormat, (u32, u32)) {
		(self.convert, (self.setup.coded_width, self.setup.coded_height))
	}

	pub(crate) fn set_bitrate(&mut self, requested: u32) -> Result<u32, EncodeError> {
		let accepted = super::backend::bounded_bitrate(requested, self.bitrate_ceiling)?;
		self.setup.bitrate = accepted;
		Ok(accepted)
	}

	#[cfg(test)]
	pub(crate) fn reference_capacity(&self) -> usize {
		self.setup.max_references
	}

	pub(crate) fn next_display_order(&self) -> u64 {
		self.submitted
	}

	pub(crate) fn request_idr(&mut self) {
		self.idr_pending = true;
		self.dpb.clear();
	}

	pub(crate) fn invalidate_reference_frames(&mut self, display_order: u64) {
		if matches!(self.codec, Codec::Av1(_)) || display_order >= self.submitted {
			self.request_idr();
			return;
		}
		self.dpb.invalidate(display_order);
		if self.dpb.anchor().is_none() {
			self.request_idr();
		}
	}

	pub(crate) fn set_color_description(&mut self, description: ColorDescription) -> Result<(), EncodeError> {
		if self.setup.color != description {
			self.setup.color = description;
			// The new VUI goes out with the next IDR's sequence header.
			self.idr_pending = true;
		}
		Ok(())
	}

	/// Wait until slot `index` was read back.
	fn wait_free(&self, wanted: impl Fn(&[bool]) -> bool) -> Result<(), EncodeError> {
		let busy = self.shared.busy.lock().unwrap_or_else(|e| e.into_inner());
		let lost = || self.shared.lost.load(Ordering::Acquire);
		let (_busy, timeout) = self
			.shared
			.freed
			.wait_timeout_while(busy, SLOT_TIMEOUT, |busy| !lost() && !wanted(busy))
			.unwrap_or_else(|e| e.into_inner());
		if lost() {
			// Terminal: an input surface may still be in use by the driver.
			return Err(EncodeError::Native {
				message: "VA-API could not establish that a submitted frame completed".into(),
				device_lost: true,
			});
		}
		if timeout.timed_out() {
			return Err(EncodeError::native("VA-API encoder did not finish a frame within 5 s"));
		}
		Ok(())
	}

	pub(crate) fn input_target(&mut self) -> Result<Target, EncodeError> {
		let current = self.current;
		self.wait_free(|busy| !busy[current])?;
		let slot = &self.slots[current];
		Ok(Target::ExternalPlanes {
			images: slot.images,
			count: slot.planes,
		})
	}

	pub(crate) fn flush(&mut self) -> Result<(), EncodeError> {
		self.wait_free(|busy| busy.iter().all(|b| !b))
	}

	pub(crate) fn encode(&mut self, target: Target) -> Result<PacketFuture, EncodeError> {
		let index = self.current;
		let slot = &self.slots[index];
		if !matches!(target, Target::ExternalPlanes { images, .. } if images == slot.images) {
			return Err(EncodeError::native(
				"encode target is not the current VA-API input surface",
			));
		}
		let idr = self.idr_pending || self.dpb.anchor().is_none();
		if idr {
			self.index_since_idr = 0;
			self.dpb.clear();
		}
		// Current output never aliases any decoder-retained reconstruction.
		let recon = *self
			.recon
			.iter()
			.find(|&&surface| !self.dpb.contains(surface))
			.ok_or_else(|| EncodeError::native("no free reconstructed surface"))?;
		let frame = Frame {
			retained: self.dpb.references(),
			idr,
			index_since_idr: self.index_since_idr,
			idr_count: self.idr_count,
			recon,
			reference: if idr {
				None
			} else {
				self.dpb.anchor().map(|r| (r.surface, r.codec_order))
			},
		};
		let (surface, coded) = (slot.surface, slot.coded);
		self.shared.busy.lock().unwrap_or_else(|e| e.into_inner())[index] = true;
		if let Err(SubmitError { error, accepted }) = self.submit(&frame, surface, coded) {
			// The reference chain is unknown after a failed submission.
			self.idr_pending = true;
			self.dpb.clear();
			if !accepted {
				self.shared.busy.lock().unwrap_or_else(|e| e.into_inner())[index] = false;
				return Err(error);
			}
			// The slot stays busy until the readback thread, after every
			// earlier frame, has synced the surface. It stays current, so the
			// next frame waits for that.
			let drain = Job {
				slot: index,
				surface,
				coded,
				key_frame: false,
				prefix: &[],
				reply: None,
			};
			if self.jobs.as_ref().is_none_or(|jobs| jobs.send(drain).is_err()) {
				self.shared.mark_lost();
				return Err(EncodeError::Native {
					message: format!("{error}; VA-API readback thread stopped before the frame was synced"),
					device_lost: true,
				});
			}
			return Err(error);
		}
		let (reply, receiver) = tokio::sync::oneshot::channel();
		let job = Job {
			slot: index,
			surface,
			coded,
			key_frame: idr,
			prefix: if matches!(self.codec, Codec::Av1(_)) {
				&av1::TEMPORAL_DELIMITER
			} else {
				&[]
			},
			reply: Some(reply),
		};
		if self.jobs.as_ref().is_none_or(|jobs| jobs.send(job).is_err()) {
			return Err(EncodeError::native("VA-API readback thread stopped"));
		}
		if idr {
			self.idr_pending = false;
			self.idr_count += 1;
		}
		self.installed_bitrate = self.setup.bitrate;
		self.dpb.push(frame.recon, self.submitted, frame.index_since_idr);
		self.index_since_idr += 1;
		self.submitted += 1;
		self.current = (self.current + 1) % self.slots.len();
		Ok(PacketFuture::Native(receiver))
	}

	fn submit(&self, frame: &Frame, surface: VASurfaceID, coded: VABufferID) -> Result<(), SubmitError> {
		let display = &self.shared.display;
		let mut buffers = Buffers {
			display,
			context: self.context,
			ids: Vec::with_capacity(12),
		};
		if let Codec::Av1(codec) = &self.codec {
			self.av1_buffers(codec, &mut buffers, frame, coded)?;
		} else {
			self.annex_b_buffers(&mut buffers, frame, coded)?;
		}
		if !frame.idr && self.setup.bitrate != self.installed_bitrate {
			// RC/HRD change at the next picture; reset=0 preserves prediction.
			buffers.rate_control(&self.setup)?;
		}
		let _submitting = self.shared.lock.lock().unwrap_or_else(|e| e.into_inner());
		let va = display.va;
		// SAFETY: valid display, context, surface and parameter buffers, which
		// stay alive until after vaEndPicture (`buffers` drops last).
		unsafe {
			display.check((va.begin_picture)(display.raw, self.context, surface), "begin picture")?;
			let rendered = (va.render_picture)(
				display.raw,
				self.context,
				buffers.ids.as_mut_ptr(),
				buffers.ids.len() as c_int,
			);
			let ended = (va.end_picture)(display.raw, self.context);
			display
				.check(rendered, "render picture")
				.and_then(|()| display.check(ended, "end picture"))
				.map_err(|error| SubmitError { error, accepted: true })
		}
	}

	/// Parameter buffers and packed headers of an H.264/HEVC frame.
	fn annex_b_buffers(&self, buffers: &mut Buffers<'_>, frame: &Frame, coded: VABufferID) -> Result<(), EncodeError> {
		if frame.idr {
			let (sequence_header, picture_header) = match &self.codec {
				Codec::H264(codec) => codec.headers(&self.setup),
				Codec::Hevc(codec) => codec.headers(&self.setup),
				Codec::Av1(_) => unreachable!("AV1 has its own buffers"),
			};
			match &self.codec {
				Codec::H264(codec) => buffers.value(VAEncSequenceParameterBufferType, &codec.sequence(&self.setup))?,
				Codec::Hevc(codec) => buffers.value(VAEncSequenceParameterBufferType, &codec.sequence(&self.setup))?,
				Codec::Av1(_) => unreachable!("AV1 has its own buffers"),
			}
			buffers.rate_control(&self.setup)?;
			if self.packed_headers & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
				buffers.packed_header(VAEncPackedHeaderSequence, &sequence_header)?;
			}
			if self.packed_headers & VA_ENC_PACKED_HEADER_PICTURE != 0 {
				buffers.packed_header(VAEncPackedHeaderPicture, &picture_header)?;
			}
		}
		let slice_header = (self.packed_headers & VA_ENC_PACKED_HEADER_SLICE != 0).then(|| match &self.codec {
			Codec::H264(codec) => codec.slice_header(frame),
			Codec::Hevc(codec) => codec.slice_header(frame),
			Codec::Av1(_) => unreachable!("AV1 has its own buffers"),
		});
		match &self.codec {
			Codec::H264(codec) => {
				let (picture, slice) = codec.frame(frame, coded);
				buffers.value(VAEncPictureParameterBufferType, &picture)?;
				if let Some((header, bits)) = &slice_header {
					buffers.packed_header_bits(VAEncPackedHeaderSlice, header, *bits)?;
				}
				buffers.value(VAEncSliceParameterBufferType, &slice)?;
			},
			Codec::Hevc(codec) => {
				let (picture, slice) = codec.frame(frame, coded);
				buffers.value(VAEncPictureParameterBufferType, &picture)?;
				if let Some((header, bits)) = &slice_header {
					buffers.packed_header_bits(VAEncPackedHeaderSlice, header, *bits)?;
				}
				buffers.value(VAEncSliceParameterBufferType, &slice)?;
			},
			Codec::Av1(_) => unreachable!("AV1 has its own buffers"),
		}
		Ok(())
	}

	/// Parameter buffers and packed OBUs of an AV1 frame: the sequence header
	/// (key frames) and the frame header, each its own packed unit, then the
	/// single tile group. The temporal delimiter is prepended on readback.
	fn av1_buffers(
		&self,
		codec: &av1::Av1,
		buffers: &mut Buffers<'_>,
		frame: &Frame,
		coded: VABufferID,
	) -> Result<(), EncodeError> {
		let sequence_header = frame.idr.then(|| codec.sequence_header(&self.setup));
		let header = codec.frame_header(frame);
		// The driver locates obu_size from the start of all packed units.
		let header_offset = sequence_header.as_ref().map_or(0, Vec::len) as u32;
		if frame.idr {
			buffers.value(VAEncSequenceParameterBufferType, &codec.sequence(&self.setup))?;
			buffers.rate_control(&self.setup)?;
		}
		let picture = codec.picture(&self.setup, frame, coded, &header, header_offset);
		buffers.value(VAEncPictureParameterBufferType, &picture)?;
		if let Some(sequence_header) = &sequence_header {
			buffers.packed_header(VAEncPackedHeaderSequence, sequence_header)?;
		}
		buffers.packed_header(VAEncPackedHeaderPicture, &header.obu)?;
		buffers.value(VAEncSliceParameterBufferType, &av1::Av1::tile_group())
	}
}

impl Drop for VaapiEncoder {
	fn drop(&mut self) {
		// The readback thread finishes every submitted frame, then exits.
		self.jobs = None;
		if let Some(thread) = self.readback.take() {
			let _ = thread.join();
		}
		let device = self.vulkan.device();
		for slot in &self.slots {
			// SAFETY: conversions into these images completed (each was
			// fenced). The imports only reference the surfaces' DMA-BUFs, so
			// they are released even if the VA driver may still use them.
			unsafe {
				for &image in &slot.images[..slot.planes] {
					device.destroy_image(image, None);
				}
				for &memory in &slot.memories {
					device.free_memory(memory, None);
				}
			}
		}
		if self.shared.lost.load(Ordering::Acquire) {
			// Fail closed: never destroy surfaces, buffers or the context the
			// driver may still use, nor terminate their display.
			tracing::error!("Keeping VA-API encoder objects: a submitted frame's completion is unknown");
			std::mem::forget(self.shared.clone());
			return;
		}
		let display = &self.shared.display;
		let va = display.va;
		// SAFETY: every frame was read back or synced; IDs are destroyed
		// child-first.
		unsafe {
			for slot in &self.slots {
				if slot.coded != VA_INVALID_ID {
					(va.destroy_buffer)(display.raw, slot.coded);
				}
			}
			if self.context != VA_INVALID_ID {
				(va.destroy_context)(display.raw, self.context);
			}
			let mut surfaces: Vec<VASurfaceID> = self
				.slots
				.iter()
				.map(|slot| slot.surface)
				.chain(self.recon.iter().copied())
				.filter(|&surface| surface != VA_INVALID_SURFACE)
				.collect();
			if !surfaces.is_empty() {
				(va.destroy_surfaces)(display.raw, surfaces.as_mut_ptr(), surfaces.len() as c_int);
			}
			if self.config != VA_INVALID_ID {
				(va.destroy_config)(display.raw, self.config);
			}
		}
	}
}

/// Parameter buffers of one submission, destroyed after it.
struct Buffers<'a> {
	display: &'a Display,
	context: VAContextID,
	ids: Vec<VABufferID>,
}

impl Buffers<'_> {
	fn bytes(&mut self, kind: VABufferType, bytes: &[u8]) -> Result<(), EncodeError> {
		let mut id = VA_INVALID_ID;
		// SAFETY: libva copies `bytes` into the new buffer.
		let status = unsafe {
			(self.display.va.create_buffer)(
				self.display.raw,
				self.context,
				kind,
				bytes.len() as c_uint,
				1,
				bytes.as_ptr().cast_mut().cast(),
				&mut id,
			)
		};
		self.display.check(status, "create parameter buffer")?;
		self.ids.push(id);
		Ok(())
	}

	fn value<T: Copy>(&mut self, kind: VABufferType, value: &T) -> Result<(), EncodeError> {
		// SAFETY: `T` is a plain C parameter structure.
		let bytes = unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>()) };
		self.bytes(kind, bytes)
	}

	/// A `VAEncMiscParameterBuffer`: the type, then the parameter structure.
	fn misc<T: Copy>(&mut self, kind: VAEncMiscParameterType, value: &T) -> Result<(), EncodeError> {
		let mut bytes = kind.to_ne_bytes().to_vec();
		// SAFETY: as above.
		bytes.extend_from_slice(unsafe {
			std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
		});
		self.bytes(VAEncMiscParameterBufferType, &bytes)
	}

	fn rate_control(&mut self, setup: &Setup) -> Result<(), EncodeError> {
		let mut rate_control = VAEncMiscParameterRateControl {
			bits_per_second: setup.bitrate,
			target_percentage: 100,
			window_size: 1000,
			..Default::default()
		};
		// SAFETY: writing a plain-data bitfield view of a union.
		let flags = unsafe { &mut rate_control.rc_flags.bits };
		flags.set_disable_frame_skip(1);
		flags.set_disable_bit_stuffing(1);
		self.misc(VAEncMiscParameterTypeRateControl, &rate_control)?;
		// One frame of buffer, starting full: a frame may use one frame's
		// budget, as the Vulkan Video path's virtual buffer allows.
		let buffer = (u64::from(setup.bitrate) / u64::from(setup.fps.max(1))).min(u64::from(u32::MAX)) as u32;
		self.misc(
			VAEncMiscParameterTypeHRD,
			&VAEncMiscParameterHRD {
				initial_buffer_fullness: buffer,
				buffer_size: buffer,
				..Default::default()
			},
		)?;
		self.misc(
			VAEncMiscParameterTypeFrameRate,
			&VAEncMiscParameterFrameRate {
				framerate: setup.fps | (1 << 16),
				..Default::default()
			},
		)
	}

	fn packed_header(&mut self, kind: VAEncPackedHeaderType, data: &[u8]) -> Result<(), EncodeError> {
		self.packed_header_bits(kind, data, (data.len() * 8) as u32)
	}

	/// A packed header of `bits` bits (a slice header may end unaligned).
	fn packed_header_bits(&mut self, kind: VAEncPackedHeaderType, data: &[u8], bits: u32) -> Result<(), EncodeError> {
		self.value(
			VAEncPackedHeaderParameterBufferType,
			&VAEncPackedHeaderParameterBuffer {
				type_: kind,
				bit_length: bits,
				has_emulation_bytes: 1,
				..Default::default()
			},
		)?;
		self.bytes(VAEncPackedHeaderDataBufferType, data)
	}
}

impl Drop for Buffers<'_> {
	fn drop(&mut self) {
		for &id in &self.ids {
			// SAFETY: created by this submission; drivers copy parameters at
			// vaRenderPicture, so they are no longer needed.
			unsafe { (self.display.va.destroy_buffer)(self.display.raw, id) };
		}
	}
}

/// Wait for each submitted frame, copy its bitstream out and free its slot.
fn readback(shared: Arc<Shared>, jobs: std::sync::mpsc::Receiver<Job>) {
	let display = &shared.display;
	for job in jobs {
		let completed = match job.reply {
			// SAFETY: valid display and surface; the encoder keeps it alive
			// until this thread exits.
			None => unsafe { sync_surface(display, job.surface) },
			Some(reply) => {
				// SAFETY: as above, for the frame's surface and coded buffer.
				let (result, completed) =
					unsafe { read_back(&shared, job.surface, job.coded, job.key_frame, job.prefix) };
				let _ = reply.send(result);
				completed
			},
		};
		if completed {
			shared.busy.lock().unwrap_or_else(|e| e.into_inner())[job.slot] = false;
			shared.freed.notify_all();
		} else {
			tracing::error!(slot = job.slot, "VA-API could not sync a submitted frame");
			shared.mark_lost();
		}
	}
}

/// Whether every operation on `surface` completed.
///
/// # Safety
///
/// `surface` is a valid surface of `display`.
unsafe fn sync_surface(display: &Display, surface: VASurfaceID) -> bool {
	// SAFETY: guaranteed by the caller.
	unsafe { (display.va.sync_surface)(display.raw, surface) as u32 == VA_STATUS_SUCCESS }
}

/// Wait for a submitted frame and copy its bitstream; also whether its
/// completion was established, so its slot can be reused.
///
/// # Safety
///
/// The surface and coded buffer are valid and were submitted together.
unsafe fn read_back(
	shared: &Shared,
	surface: VASurfaceID,
	coded: VABufferID,
	key_frame: bool,
	prefix: &[u8],
) -> (Result<Packet, EncodeError>, bool) {
	let display = &shared.display;
	let va = display.va;
	// SAFETY: guaranteed by the caller.
	let synced = unsafe {
		match va.sync_buffer {
			Some(sync_buffer) => {
				let status = sync_buffer(display.raw, coded, VA_TIMEOUT_INFINITE as u64);
				if status as u32 == VA_STATUS_ERROR_UNIMPLEMENTED {
					(va.sync_surface)(display.raw, surface)
				} else {
					status
				}
			},
			None => (va.sync_surface)(display.raw, surface),
		}
	};
	if let Err(error) = display.check(synced, "sync") {
		// The frame failed; its slot is reusable only once the surface is idle.
		// SAFETY: guaranteed by the caller.
		return (Err(error), unsafe { sync_surface(display, surface) });
	}
	let _mapping = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
	// SAFETY: the encode completed.
	let data = unsafe { read_coded(display, coded, prefix) };
	(
		data.map(|data| Packet {
			data,
			is_key_frame: key_frame,
		}),
		true,
	)
}

/// Copy a finished coded buffer's segments.
///
/// # Safety
///
/// `coded` is a valid coded buffer whose encode completed.
unsafe fn read_coded(display: &Display, coded: VABufferID, prefix: &[u8]) -> Result<Vec<u8>, EncodeError> {
	let mut mapped: *mut c_void = std::ptr::null_mut();
	// SAFETY: guaranteed by the caller.
	display.check(
		unsafe { (display.va.map_buffer)(display.raw, coded, &mut mapped) },
		"map coded buffer",
	)?;
	// SAFETY: the mapped list is valid until vaUnmapBuffer.
	let data = unsafe { coded_segments(mapped.cast(), prefix) };
	// SAFETY: mapped above.
	unsafe { (display.va.unmap_buffer)(display.raw, coded) };
	data
}

/// `prefix` followed by every segment of a coded buffer's list.
///
/// A segment the driver marks as corrupt (`VA_CODED_BUF_STATUS_BAD_BITSTREAM`)
/// or overflowing fails the frame, so it is never sent and the pipeline's
/// recovery re-anchors the stream; the other status bits (QP, frame-size
/// hints) are informational.
///
/// # Safety
///
/// `segment` is null or the head of a valid `VACodedBufferSegment` list whose
/// buffers hold `size` bytes each.
unsafe fn coded_segments(mut segment: *const VACodedBufferSegment, prefix: &[u8]) -> Result<Vec<u8>, EncodeError> {
	let mut data = prefix.to_vec();
	let mut status = 0;
	while !segment.is_null() {
		// SAFETY: guaranteed by the caller.
		let current = unsafe { &*segment };
		status |= current.status;
		if !current.buf.is_null() && current.size > 0 {
			// SAFETY: `size` bytes at `buf` belong to the segment.
			data.extend_from_slice(unsafe {
				std::slice::from_raw_parts(current.buf.cast::<u8>(), current.size as usize)
			});
		}
		segment = current.next.cast();
	}
	if status & VA_CODED_BUF_STATUS_BAD_BITSTREAM != 0 {
		return Err(EncodeError::native("VA-API marked the coded frame as corrupt"));
	}
	if status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0 {
		return Err(EncodeError::native("VA-API coded buffer overflowed"));
	}
	if data.len() == prefix.len() {
		return Err(EncodeError::native("VA-API produced an empty frame"));
	}
	Ok(data)
}

/// The encode entry points the driver lists for `profile`.
fn listed_entrypoints(display: &Display, profile: VAProfile) -> Vec<VAEntrypoint> {
	let va = display.va;
	// SAFETY: valid display.
	let max = unsafe { (va.max_num_entrypoints)(display.raw) }.max(0) as usize;
	let mut entrypoints = vec![0 as VAEntrypoint; max];
	let mut count: c_int = 0;
	// SAFETY: the array holds `max` entries.
	let status = unsafe { (va.query_config_entrypoints)(display.raw, profile, entrypoints.as_mut_ptr(), &mut count) };
	if status as u32 != VA_STATUS_SUCCESS {
		return Vec::new();
	}
	entrypoints.truncate(count.max(0) as usize);
	entrypoints
}

/// The encode entry points to try, in order: low-power (fixed-function)
/// first, which keeps encoding off the 3D engine on Intel Gen12, then the
/// slice encoder. Intel GPUs before Meteor Lake list both, and low-power rate
/// control needs HuC firmware a host may lack, so an unusable low-power
/// encoder must not hide a working slice encoder. An entry point the startup
/// probe verified by encoding a frame goes first.
fn entrypoint_order(listed: &[VAEntrypoint], verified: Option<VAEntrypoint>) -> Vec<VAEntrypoint> {
	let mut order: Vec<VAEntrypoint> = [VAEntrypointEncSliceLP, VAEntrypointEncSlice]
		.into_iter()
		.filter(|entrypoint| listed.contains(entrypoint))
		.collect();
	if let Some(position) = verified.and_then(|verified| order.iter().position(|&e| e == verified)) {
		let verified = order.remove(position);
		order.insert(0, verified);
	}
	order
}

fn entrypoint_label(entrypoint: VAEntrypoint) -> &'static str {
	if entrypoint == VAEntrypointEncSliceLP {
		"low-power"
	} else {
		"slice"
	}
}

/// Entry points the startup probe encoded a frame with, per render node and
/// profile.
static VERIFIED: Mutex<Vec<(PathBuf, VAProfile, VAEntrypoint)>> = Mutex::new(Vec::new());

fn verified_entrypoint(node: &Path, profile: VAProfile) -> Option<VAEntrypoint> {
	VERIFIED
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.iter()
		.find(|(n, p, _)| n == node && *p == profile)
		.map(|(.., entrypoint)| *entrypoint)
}

fn remember_verified(node: &Path, profile: VAProfile, entrypoint: VAEntrypoint) {
	let mut verified = VERIFIED.lock().unwrap_or_else(|e| e.into_inner());
	verified.retain(|(n, p, _)| !(n == node && *p == profile));
	verified.push((node.to_path_buf(), profile, entrypoint));
}

fn format_label(format: ConvertFormat) -> &'static str {
	super::convert::format_label(format)
}

/// Single-plane Vulkan formats and extents of each surface layer.
fn layer_formats(format: ConvertFormat, (width, height): (u32, u32)) -> Vec<(vk::Format, u32, u32)> {
	match format {
		ConvertFormat::Nv12 => vec![
			(vk::Format::R8_UNORM, width, height),
			(vk::Format::R8G8_UNORM, width / 2, height / 2),
		],
		ConvertFormat::P010 => vec![
			(vk::Format::R16_UNORM, width, height),
			(vk::Format::R16G16_UNORM, width / 2, height / 2),
		],
		// Raw 32-bit words: the copy moves bytes, never interprets channels.
		ConvertFormat::Xyuv => vec![(vk::Format::R8G8B8A8_UNORM, width, height)],
		ConvertFormat::Y410 => vec![(vk::Format::A2B10G10R10_UNORM_PACK32, width, height)],
		ConvertFormat::Yuv444 | ConvertFormat::Yuv444P10 => Vec::new(),
	}
}

/// Import every layer of an exported surface as a transfer-destination image.
fn import_layers(
	context: &VideoContext,
	descriptor: &VADRMPRIMESurfaceDescriptor,
	format: ConvertFormat,
	extent: (u32, u32),
) -> Result<([vk::Image; 2], Vec<vk::DeviceMemory>, usize), String> {
	let layers = layer_formats(format, extent);
	if layers.is_empty() || descriptor.num_layers as usize != layers.len() {
		return Err(format!(
			"VA-API exported {} layers for {}; expected {}",
			descriptor.num_layers,
			format_label(format),
			layers.len()
		));
	}
	let device = context.device();
	let instance = context.instance();
	let external_memory_fd = ash::khr::external_memory_fd::Device::load(instance, device);
	let mut images = [vk::Image::null(); 2];
	let mut memories = Vec::new();
	let release = |images: &[vk::Image], memories: &[vk::DeviceMemory]| {
		// SAFETY: created below and not used by any submission yet.
		unsafe {
			for &image in images.iter().filter(|image| **image != vk::Image::null()) {
				device.destroy_image(image, None);
			}
			for &memory in memories {
				device.free_memory(memory, None);
			}
		}
	};
	for (index, &(vk_format, width, height)) in layers.iter().enumerate() {
		let layer = &descriptor.layers[index];
		let object = &descriptor.objects[layer.object_index[0] as usize];
		let modifier = object.drm_format_modifier;
		let usage = vk::ImageUsageFlags::TRANSFER_DST;
		if !modifier_importable(context, vk_format, modifier, usage) {
			release(&images, &memories);
			return Err(format!(
				"the capture GPU's Vulkan driver cannot import VA-API surface layer {index} ({vk_format:?}, modifier 0x{modifier:016x})"
			));
		}
		let plane_layouts: Vec<vk::SubresourceLayout> = (0..layer.num_planes.max(1) as usize)
			.map(|plane| {
				vk::SubresourceLayout::default()
					.offset(u64::from(layer.offset[plane]))
					.row_pitch(u64::from(layer.pitch[plane]))
			})
			.collect();
		let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
			.drm_format_modifier(modifier)
			.plane_layouts(&plane_layouts);
		let mut external =
			vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
		let info = vk::ImageCreateInfo::default()
			.image_type(vk::ImageType::TYPE_2D)
			.format(vk_format)
			.extent(vk::Extent3D {
				width,
				height,
				depth: 1,
			})
			.mip_levels(1)
			.array_layers(1)
			.samples(vk::SampleCountFlags::TYPE_1)
			.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
			.usage(usage)
			.sharing_mode(vk::SharingMode::EXCLUSIVE)
			.initial_layout(vk::ImageLayout::UNDEFINED)
			.push(&mut external)
			.push(&mut modifier_info);
		// SAFETY: valid create info chain on the capture device.
		let image = match unsafe { device.create_image(&info, None) } {
			Ok(image) => image,
			Err(e) => {
				release(&images, &memories);
				return Err(format!("VA-API surface image: {e}"));
			},
		};
		images[index] = image;
		// SAFETY: plain queries on valid handles.
		let requirements = unsafe { device.get_image_memory_requirements(image) };
		let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
		if let Err(e) = unsafe {
			external_memory_fd.get_memory_fd_properties(
				vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				object.fd,
				&mut fd_properties,
			)
		} {
			release(&images, &memories);
			return Err(format!("VA-API surface memory properties: {e}"));
		}
		let Some(memory_type) = context.find_memory_type(
			requirements.memory_type_bits & fd_properties.memory_type_bits,
			vk::MemoryPropertyFlags::empty(),
		) else {
			release(&images, &memories);
			return Err("no Vulkan memory type can import the VA-API surface".into());
		};
		// SAFETY: `object.fd` is open for the duration of this call.
		let fd = match unsafe { BorrowedFd::borrow_raw(object.fd) }.try_clone_to_owned() {
			Ok(fd) => fd,
			Err(e) => {
				release(&images, &memories);
				return Err(format!("cannot duplicate the VA-API surface descriptor: {e}"));
			},
		};
		let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
		let mut import = vk::ImportMemoryFdInfoKHR::default()
			.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
			.fd(fd.as_raw_fd());
		let allocate = vk::MemoryAllocateInfo::default()
			.allocation_size(requirements.size)
			.memory_type_index(memory_type)
			.push(&mut import)
			.push(&mut dedicated);
		// SAFETY: valid allocation chain; Vulkan owns the fd only on success.
		match unsafe { device.allocate_memory(&allocate, None) } {
			Ok(memory) => {
				let _ = fd.into_raw_fd();
				memories.push(memory);
				// SAFETY: fresh image and memory on the same device.
				if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
					release(&images, &memories);
					return Err(format!("VA-API surface memory bind: {e}"));
				}
			},
			Err(e) => {
				release(&images, &memories);
				return Err(format!("VA-API surface memory import: {e}"));
			},
		}
	}
	Ok((images, memories, layers.len()))
}

/// Whether `format` with `modifier` imports from a DMA-BUF with `usage`.
fn modifier_importable(context: &VideoContext, format: vk::Format, modifier: u64, usage: vk::ImageUsageFlags) -> bool {
	let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
		.drm_format_modifier(modifier)
		.sharing_mode(vk::SharingMode::EXCLUSIVE);
	let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
		.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
	let info = vk::PhysicalDeviceImageFormatInfo2::default()
		.format(format)
		.ty(vk::ImageType::TYPE_2D)
		.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
		.usage(usage)
		.push(&mut external_info)
		.push(&mut modifier_info);
	let mut external = vk::ExternalImageFormatProperties::default();
	let mut properties = vk::ImageFormatProperties2::default().push(&mut external);
	// SAFETY: valid input and output chains that live for the call.
	let supported = unsafe {
		context.instance().get_physical_device_image_format_properties2(
			context.physical_device(),
			&info,
			&mut properties,
		)
	};
	supported.is_ok()
		&& external
			.external_memory_properties
			.external_memory_features
			.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}

/// Create an encoder for `codec`/`chroma`/`depth` on the capture GPU and
/// encode one frame: the startup probe of a VA-API profile.
pub(crate) fn probe(
	context: &VideoContext,
	codec: VideoCodec,
	chroma: ChromaFormat,
	depth: BitDepth,
) -> Result<(), String> {
	profile(codec, chroma, depth)?;
	let format = NativeFormat {
		codec,
		chroma,
		bit_depth: depth,
		width: PROBE_EXTENT.0,
		height: PROBE_EXTENT.1,
		fps: 60,
		bitrate: 10_000_000,
		color: ColorDescription::srgb(),
	};
	// Each entry point must encode a frame; streams then use the one that did.
	let encoder = VaapiEncoder::create(context, format, encode_key_frame)?;
	remember_verified(&encoder.node, encoder.profile, encoder.entrypoint);
	Ok(())
}

/// Encode one frame and check that it is a key frame.
fn encode_key_frame(encoder: &mut VaapiEncoder) -> Result<(), String> {
	let target = encoder.input_target().map_err(|e| e.to_string())?;
	let PacketFuture::Native(receiver) = encoder.encode(target).map_err(|e| e.to_string())? else {
		unreachable!("VA-API frames resolve through the readback thread")
	};
	let packet = receiver
		.blocking_recv()
		.map_err(|_| "VA-API readback stopped".to_string())?
		.map_err(|e| e.to_string())?;
	if !packet.is_key_frame || packet.data.len() < 8 {
		return Err("VA-API encoded no key frame".into());
	}
	Ok(())
}

#[cfg(test)]
mod tests;
