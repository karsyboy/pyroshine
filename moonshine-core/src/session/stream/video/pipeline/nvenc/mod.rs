//! NVENC encoding on the capture GPU, zero-copy through CUDA.
//!
//! `libcuda.so.1` and `libnvidia-encode.so.1` (both part of the NVIDIA
//! driver) are loaded at runtime, so hosts without them are unaffected. The
//! CUDA device is the one whose UUID equals the capture Vulkan device's; no
//! other GPU is used.
//!
//! Input buffers are Vulkan buffers exported as opaque file descriptors and
//! imported into CUDA, registered with NVENC as pitched device memory in the
//! converter's own layout: the converter copies each frame into one
//! ([`Target::Buffer`]) and waits for its fence before [`NvencEncoder::encode`]
//! submits it, which orders the write before the encoder's read. A buffer is
//! handed out again only after its frame was read back.
//!
//! Frames: P1 preset with ultra-low-latency tuning, an IDR on start and on
//! request, P frames otherwise, constant bitrate with a one-frame buffer.
//! Lost frames are invalidated in the encoder's reference list (NVENC picks an
//! older reference) when the GPU supports it, else an IDR re-anchors the stream.

mod recovery;
pub(crate) mod sys;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use ash::vk;
use ash::vk::TaggedStructure;
use libloading::Library;
use pixelforge::{ColorDescription, VideoContext};
use sys::*;

use super::backend::{EncodeError, NativeFormat, PROBE_EXTENT, Packet, PacketFuture};
use super::convert::{ConvertFormat, Layout, Target};
use crate::session::stream::video::{BitDepth, ChromaFormat, VideoCodec};

/// Row pitch alignment of the input buffers.
const ROW_ALIGNMENT: u32 = 256;
const SLOT_TIMEOUT: Duration = Duration::from_secs(5);
/// Lost frames NVENC is asked to invalidate before an IDR is cheaper.
const MAX_INVALIDATIONS: u64 = 16;
const CUDA_EXTERNAL_MEMORY_DEDICATED: u32 = 1;

const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const fn struct_version(version: u32) -> u32 {
	NVENCAPI_VERSION | (version << 16) | (0x7 << 28)
}
const FUNCTION_LIST_VER: u32 = struct_version(2);
const OPEN_SESSION_VER: u32 = struct_version(1);
const CAPS_PARAM_VER: u32 = struct_version(1);
const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
const RC_PARAMS_VER: u32 = struct_version(1);
const INITIALIZE_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const REGISTER_RESOURCE_VER: u32 = struct_version(4);
const MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> GUID {
	GUID {
		Data1: data1,
		Data2: data2,
		Data3: data3,
		Data4: data4,
	}
}
// nvEncodeAPI.h 12.1.
const CODEC_H264: GUID = guid(
	0x6bc82762,
	0x4e63,
	0x4ca4,
	[0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const CODEC_HEVC: GUID = guid(
	0x790cdc88,
	0x4522,
	0x4d7b,
	[0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
const CODEC_AV1: GUID = guid(
	0x0a352289,
	0x0aa7,
	0x4759,
	[0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
const PRESET_P1: GUID = guid(
	0xfc0a8d3e,
	0x45f8,
	0x4cf8,
	[0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
const H264_HIGH: GUID = guid(
	0xe7cbc309,
	0x4f7a,
	0x4b89,
	[0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
const H264_HIGH_444: GUID = guid(
	0x7ac663cb,
	0xa598,
	0x4960,
	[0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
);
const HEVC_MAIN: GUID = guid(
	0xb514c39a,
	0xb55b,
	0x40fa,
	[0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
const HEVC_MAIN10: GUID = guid(
	0xfa4d2b6c,
	0x3a5b,
	0x411a,
	[0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5],
);
const HEVC_FREXT: GUID = guid(
	0x51ec32b5,
	0x1b4c,
	0x453c,
	[0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
);
const AV1_MAIN: GUID = guid(
	0x5f2a39f5,
	0xf14e,
	0x4f95,
	[0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
);

fn same_guid(a: &GUID, b: &GUID) -> bool {
	(a.Data1, a.Data2, a.Data3, a.Data4) == (b.Data1, b.Data2, b.Data3, b.Data4)
}

macro_rules! required {
	($library:expr, $name:literal, $ty:ty) => {
		(*$library
			.get::<$ty>(concat!($name, "\0").as_bytes())
			.map_err(|e| format!("missing {}: {e}", $name))?)
		.ok_or(concat!("null ", $name))?
	};
}

/// CUDA driver API entry points.
struct Cuda {
	_library: Library,
	init: unsafe extern "C" fn(u32) -> CUresult,
	device_get_count: unsafe extern "C" fn(*mut c_int) -> CUresult,
	device_get: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
	device_get_uuid: unsafe extern "C" fn(*mut CUuuid, CUdevice) -> CUresult,
	primary_ctx_retain: unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult,
	primary_ctx_release: unsafe extern "C" fn(CUdevice) -> CUresult,
	ctx_push: unsafe extern "C" fn(CUcontext) -> CUresult,
	ctx_pop: unsafe extern "C" fn(*mut CUcontext) -> CUresult,
	import_external_memory:
		unsafe extern "C" fn(*mut CUexternalMemory, *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC) -> CUresult,
	external_memory_get_mapped_buffer:
		unsafe extern "C" fn(*mut CUdeviceptr, CUexternalMemory, *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC) -> CUresult,
	destroy_external_memory: unsafe extern "C" fn(CUexternalMemory) -> CUresult,
	mem_free: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
	get_error_name: unsafe extern "C" fn(CUresult, *mut *const c_char) -> CUresult,
}

fn load_cuda() -> Result<Cuda, String> {
	// SAFETY: the NVIDIA driver's CUDA library; symbol types mirror
	// dynlink_cuda.h of nv-codec-headers 12.1.
	unsafe {
		let library = Library::new("libcuda.so.1").map_err(|e| format!("libcuda.so.1: {e}"))?;
		let cuda = Cuda {
			init: required!(library, "cuInit", tcuInit),
			device_get_count: required!(library, "cuDeviceGetCount", tcuDeviceGetCount),
			device_get: required!(library, "cuDeviceGet", tcuDeviceGet),
			device_get_uuid: required!(library, "cuDeviceGetUuid", tcuDeviceGetUuid),
			primary_ctx_retain: required!(library, "cuDevicePrimaryCtxRetain", tcuDevicePrimaryCtxRetain),
			primary_ctx_release: required!(library, "cuDevicePrimaryCtxRelease_v2", tcuDevicePrimaryCtxRelease),
			ctx_push: required!(library, "cuCtxPushCurrent_v2", tcuCtxPushCurrent_v2),
			ctx_pop: required!(library, "cuCtxPopCurrent_v2", tcuCtxPopCurrent_v2),
			import_external_memory: required!(library, "cuImportExternalMemory", tcuImportExternalMemory),
			external_memory_get_mapped_buffer: required!(
				library,
				"cuExternalMemoryGetMappedBuffer",
				tcuExternalMemoryGetMappedBuffer
			),
			destroy_external_memory: required!(library, "cuDestroyExternalMemory", tcuDestroyExternalMemory),
			mem_free: required!(library, "cuMemFree_v2", tcuMemFree_v2),
			get_error_name: required!(library, "cuGetErrorName", tcuGetErrorName),
			_library: library,
		};
		let status = (cuda.init)(0);
		if status != CUDA_SUCCESS {
			return Err(format!("cuInit: {}", cuda.error(status)));
		}
		Ok(cuda)
	}
}

impl Cuda {
	fn error(&self, status: CUresult) -> String {
		let mut name: *const c_char = std::ptr::null();
		// SAFETY: CUDA returns a static string or leaves it null.
		unsafe { (self.get_error_name)(status, &mut name) };
		if name.is_null() {
			format!("CUDA error {status}")
		} else {
			// SAFETY: a static NUL-terminated name.
			unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
		}
	}

	fn check(&self, status: CUresult, what: &str) -> Result<(), String> {
		if status == CUDA_SUCCESS {
			Ok(())
		} else {
			Err(format!("{what}: {}", self.error(status)))
		}
	}
}

fn cuda() -> Result<&'static Cuda, String> {
	static CUDA: OnceLock<Result<Cuda, String>> = OnceLock::new();
	CUDA.get_or_init(load_cuda).as_ref().map_err(Clone::clone)
}

/// The NVENC function table.
fn nvenc() -> Result<&'static NV_ENCODE_API_FUNCTION_LIST, String> {
	struct Api {
		_library: Library,
		functions: NV_ENCODE_API_FUNCTION_LIST,
	}
	// SAFETY: the table only holds function pointers into the library, which
	// stays loaded with it for the process.
	unsafe impl Send for Api {}
	unsafe impl Sync for Api {}
	static API: OnceLock<Result<Api, String>> = OnceLock::new();
	API.get_or_init(|| {
		// SAFETY: the NVIDIA driver's encode library; NvEncodeAPICreateInstance
		// fills a table whose layout is fixed by its version.
		unsafe {
			let library = Library::new("libnvidia-encode.so.1").map_err(|e| format!("libnvidia-encode.so.1: {e}"))?;
			let create = *library
				.get::<unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS>(
					b"NvEncodeAPICreateInstance\0",
				)
				.map_err(|e| format!("missing NvEncodeAPICreateInstance: {e}"))?;
			let mut functions = NV_ENCODE_API_FUNCTION_LIST {
				version: FUNCTION_LIST_VER,
				..std::mem::zeroed()
			};
			let status = create(&mut functions);
			if status != NV_ENC_SUCCESS {
				return Err(format!(
					"NvEncodeAPICreateInstance: {} (the driver may predate NVENC API 12.1, driver 530)",
					status_name(status)
				));
			}
			Ok(Api {
				_library: library,
				functions,
			})
		}
	})
	.as_ref()
	.map(|api| &api.functions)
	.map_err(Clone::clone)
}

fn status_name(status: NVENCSTATUS) -> String {
	let name = match status {
		NV_ENC_ERR_NO_ENCODE_DEVICE => "NV_ENC_ERR_NO_ENCODE_DEVICE",
		NV_ENC_ERR_UNSUPPORTED_DEVICE => "NV_ENC_ERR_UNSUPPORTED_DEVICE",
		NV_ENC_ERR_INVALID_ENCODERDEVICE => "NV_ENC_ERR_INVALID_ENCODERDEVICE",
		NV_ENC_ERR_INVALID_DEVICE => "NV_ENC_ERR_INVALID_DEVICE",
		NV_ENC_ERR_DEVICE_NOT_EXIST => "NV_ENC_ERR_DEVICE_NOT_EXIST",
		NV_ENC_ERR_INVALID_PTR => "NV_ENC_ERR_INVALID_PTR",
		NV_ENC_ERR_INVALID_PARAM => "NV_ENC_ERR_INVALID_PARAM",
		NV_ENC_ERR_INVALID_CALL => "NV_ENC_ERR_INVALID_CALL",
		NV_ENC_ERR_OUT_OF_MEMORY => "NV_ENC_ERR_OUT_OF_MEMORY",
		NV_ENC_ERR_UNSUPPORTED_PARAM => "NV_ENC_ERR_UNSUPPORTED_PARAM",
		NV_ENC_ERR_INVALID_VERSION => "NV_ENC_ERR_INVALID_VERSION",
		NV_ENC_ERR_ENCODER_BUSY => "NV_ENC_ERR_ENCODER_BUSY",
		NV_ENC_ERR_GENERIC => "NV_ENC_ERR_GENERIC",
		NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
		NV_ENC_ERR_UNIMPLEMENTED => "NV_ENC_ERR_UNIMPLEMENTED",
		NV_ENC_ERR_RESOURCE_REGISTER_FAILED => "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
		NV_ENC_ERR_RESOURCE_NOT_REGISTERED => "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
		NV_ENC_ERR_RESOURCE_NOT_MAPPED => "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
		NV_ENC_ERR_NEED_MORE_INPUT => "NV_ENC_ERR_NEED_MORE_INPUT",
		_ => return format!("NVENC status {status}"),
	};
	name.to_string()
}

/// A retained CUDA primary context of one device.
struct CudaContext {
	cuda: &'static Cuda,
	device: CUdevice,
	context: CUcontext,
}

impl CudaContext {
	/// The CUDA device with the capture Vulkan device's UUID.
	fn for_vulkan(vulkan: &VideoContext) -> Result<Self, String> {
		let cuda = cuda()?;
		let mut ids = vk::PhysicalDeviceIDProperties::default();
		let mut properties = vk::PhysicalDeviceProperties2::default().push(&mut ids);
		// SAFETY: core Vulkan 1.1 query on the live physical device.
		unsafe {
			vulkan
				.instance()
				.get_physical_device_properties2(vulkan.physical_device(), &mut properties);
		}
		let mut count = 0;
		// SAFETY: plain CUDA queries with valid output pointers.
		unsafe {
			cuda.check((cuda.device_get_count)(&mut count), "cuDeviceGetCount")?;
			for ordinal in 0..count {
				let mut device = 0;
				cuda.check((cuda.device_get)(&mut device, ordinal), "cuDeviceGet")?;
				let mut uuid = CUuuid::default();
				cuda.check((cuda.device_get_uuid)(&mut uuid, device), "cuDeviceGetUuid")?;
				if uuid.bytes.map(|b| b as u8) == ids.device_uuid {
					let mut context = std::ptr::null_mut();
					cuda.check(
						(cuda.primary_ctx_retain)(&mut context, device),
						"cuDevicePrimaryCtxRetain",
					)?;
					return Ok(Self { cuda, device, context });
				}
			}
		}
		Err("no CUDA device has the capture GPU's UUID".into())
	}

	/// Make the context current on this thread until the guard drops.
	fn push(&self) -> Result<Current<'_>, EncodeError> {
		// SAFETY: a retained context.
		let status = unsafe { (self.cuda.ctx_push)(self.context) };
		self.cuda
			.check(status, "cuCtxPushCurrent")
			.map_err(EncodeError::native)?;
		Ok(Current(self))
	}
}

impl Drop for CudaContext {
	fn drop(&mut self) {
		// SAFETY: retained in `for_vulkan`; every CUDA object was freed first.
		unsafe { (self.cuda.primary_ctx_release)(self.device) };
	}
}

struct Current<'a>(&'a CudaContext);

impl Drop for Current<'_> {
	fn drop(&mut self) {
		let mut popped = std::ptr::null_mut();
		// SAFETY: pushed by `CudaContext::push` on this thread.
		unsafe { (self.0.cuda.ctx_pop)(&mut popped) };
	}
}

/// What encodes one format: codec and profile GUIDs and the input format.
struct Profile {
	codec: GUID,
	profile: GUID,
	buffer_format: NV_ENC_BUFFER_FORMAT,
	convert: ConvertFormat,
	label: &'static str,
}

fn profile(codec: VideoCodec, chroma: ChromaFormat, depth: BitDepth) -> Result<Profile, String> {
	let profile = |codec, profile, buffer_format, convert, label| Profile {
		codec,
		profile,
		buffer_format,
		convert,
		label,
	};
	use BitDepth::{Eight, Ten};
	use ChromaFormat::{Yuv420, Yuv444};
	Ok(match (codec, chroma, depth) {
		(VideoCodec::H264, Yuv420, Eight) => profile(
			CODEC_H264,
			H264_HIGH,
			NV_ENC_BUFFER_FORMAT_NV12,
			ConvertFormat::Nv12,
			"H.264 High",
		),
		(VideoCodec::H264, Yuv444, Eight) => profile(
			CODEC_H264,
			H264_HIGH_444,
			NV_ENC_BUFFER_FORMAT_AYUV,
			ConvertFormat::Xyuv,
			"H.264 High 4:4:4",
		),
		(VideoCodec::Hevc, Yuv420, Eight) => profile(
			CODEC_HEVC,
			HEVC_MAIN,
			NV_ENC_BUFFER_FORMAT_NV12,
			ConvertFormat::Nv12,
			"HEVC Main",
		),
		(VideoCodec::Hevc, Yuv420, Ten) => profile(
			CODEC_HEVC,
			HEVC_MAIN10,
			NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
			ConvertFormat::P010,
			"HEVC Main 10",
		),
		(VideoCodec::Hevc, Yuv444, Eight) => profile(
			CODEC_HEVC,
			HEVC_FREXT,
			NV_ENC_BUFFER_FORMAT_AYUV,
			ConvertFormat::Xyuv,
			"HEVC Main 4:4:4",
		),
		(VideoCodec::Av1, Yuv420, Eight) => profile(
			CODEC_AV1,
			AV1_MAIN,
			NV_ENC_BUFFER_FORMAT_NV12,
			ConvertFormat::Nv12,
			"AV1 Main",
		),
		(VideoCodec::Av1, Yuv420, Ten) => profile(
			CODEC_AV1,
			AV1_MAIN,
			NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
			ConvertFormat::P010,
			"AV1 Main 10",
		),
		(_, Yuv444, Ten) => return Err("NVENC 4:4:4 10-bit input is not implemented".into()),
		(VideoCodec::Av1, Yuv444, _) => return Err("NVENC encodes AV1 4:2:0 only".into()),
		(VideoCodec::H264, _, Ten) => return Err("H.264 is encoded at 8 bits".into()),
		(VideoCodec::PyroWave, ..) => return Err("PyroWave is not an NVENC codec".into()),
	})
}

/// Input buffer: Vulkan memory shared with CUDA and registered with NVENC.
struct Slot {
	buffer: vk::Buffer,
	memory: vk::DeviceMemory,
	external: CUexternalMemory,
	pointer: CUdeviceptr,
	registered: NV_ENC_REGISTERED_PTR,
	bitstream: NV_ENC_OUTPUT_PTR,
}

/// State shared with the readback thread.
struct Shared {
	api: &'static NV_ENCODE_API_FUNCTION_LIST,
	session: *mut c_void,
	cuda: CudaContext,
	/// Serializes session calls other than waiting for a bitstream.
	lock: Mutex<()>,
	busy: Mutex<Vec<bool>>,
	freed: Condvar,
}

// SAFETY: NVENC sessions and CUDA contexts may be used from several threads
// (each pushes the context); calls that change session state are serialized
// by `lock`.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
	fn check(&self, status: NVENCSTATUS, what: &str) -> Result<(), EncodeError> {
		if status == NV_ENC_SUCCESS {
			return Ok(());
		}
		let detail = self
			.api
			.nvEncGetLastErrorString
			// SAFETY: valid session; NVENC returns a NUL-terminated string.
			.map(|last| {
				unsafe { CStr::from_ptr(last(self.session)) }
					.to_string_lossy()
					.into_owned()
			})
			.unwrap_or_default();
		Err(EncodeError::Native {
			message: format!("NVENC {what}: {} {detail}", status_name(status))
				.trim_end()
				.to_string(),
			device_lost: status == NV_ENC_ERR_DEVICE_NOT_EXIST,
		})
	}
}

struct Job {
	slot: usize,
	mapped: NV_ENC_INPUT_PTR,
	bitstream: NV_ENC_OUTPUT_PTR,
	reply: tokio::sync::oneshot::Sender<Result<Packet, EncodeError>>,
}

// SAFETY: opaque NVENC handles of the shared session, which owns them.
unsafe impl Send for Job {}

/// The color description NVENC encodes with and a requested change it has
/// not accepted yet. NVENC applies a change by reconfiguring before the next
/// frame, which can fail; until it succeeds the change remains pending.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ColorState {
	applied: ColorDescription,
	pending: Option<ColorDescription>,
}

impl ColorState {
	fn new(applied: ColorDescription) -> Self {
		Self { applied, pending: None }
	}

	/// Request `description`; requesting the applied one cancels a pending
	/// change.
	fn request(&mut self, description: ColorDescription) {
		self.pending = (description != self.applied).then_some(description);
	}

	/// NVENC accepted `description`.
	fn accepted(&mut self, description: ColorDescription) {
		self.applied = description;
		if self.pending == Some(description) {
			self.pending = None;
		}
	}
}

/// An NVENC encoder of one stream epoch.
pub(crate) struct NvencEncoder {
	shared: Arc<Shared>,
	vulkan: VideoContext,
	slots: Vec<Slot>,
	layout: Layout,
	extent: (u32, u32),
	buffer_format: NV_ENC_BUFFER_FORMAT,
	config: Box<NV_ENC_CONFIG>,
	init: Box<NV_ENC_INITIALIZE_PARAMS>,
	codec: VideoCodec,
	width: u32,
	height: u32,
	reference_invalidation: bool,
	recovery: recovery::Recovery,
	bitrate_ceiling: u32,
	pending_bitrate: u32,
	/// Input buffers: see `conventional_input_slots`.
	input_slots: usize,
	current: usize,
	submitted: u64,
	idr_pending: bool,
	color: ColorState,
	jobs: Option<std::sync::mpsc::Sender<Job>>,
	readback: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: owned NVENC/CUDA handles and Vulkan handles of the capture device,
// used from the encoding thread; see `Shared` for the readback thread.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
	pub(crate) fn new(vulkan: &VideoContext, format: NativeFormat) -> Result<Self, String> {
		let profile = profile(format.codec, format.chroma, format.bit_depth)?;
		let api = nvenc()?;
		let cuda = CudaContext::for_vulkan(vulkan)?;
		let session = {
			let _current = cuda.push().map_err(|e| e.to_string())?;
			let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
				version: OPEN_SESSION_VER,
				deviceType: NV_ENC_DEVICE_TYPE_CUDA,
				device: cuda.context.cast(),
				apiVersion: NVENCAPI_VERSION,
				// SAFETY: plain-data structure; zero is valid for the rest.
				..unsafe { std::mem::zeroed() }
			};
			let mut session = std::ptr::null_mut();
			let open = api.nvEncOpenEncodeSessionEx.ok_or("NVENC has no session entry point")?;
			// SAFETY: valid parameters and output pointer.
			let status = unsafe { open(&mut params, &mut session) };
			if status != NV_ENC_SUCCESS {
				return Err(format!("NVENC session: {}", status_name(status)));
			}
			session
		};
		let input_slots = super::conventional_input_slots();
		let shared = Arc::new(Shared {
			api,
			session,
			cuda,
			lock: Mutex::new(()),
			busy: Mutex::new(vec![false; input_slots]),
			freed: Condvar::new(),
		});

		let extent = if format.chroma == ChromaFormat::Yuv420 {
			(format.width.next_multiple_of(2), format.height.next_multiple_of(2))
		} else {
			(format.width, format.height)
		};
		let layout = Layout::aligned(profile.convert, extent, ROW_ALIGNMENT)?;
		// SAFETY: plain-data structures; zero is valid until filled below.
		let config: Box<NV_ENC_CONFIG> = Box::new(unsafe { std::mem::zeroed() });
		let init: Box<NV_ENC_INITIALIZE_PARAMS> = Box::new(unsafe { std::mem::zeroed() });
		let mut encoder = Self {
			shared,
			vulkan: vulkan.clone(),
			slots: Vec::new(),
			layout,
			extent,
			buffer_format: profile.buffer_format,
			config,
			init,
			codec: format.codec,
			width: format.width,
			height: format.height,
			reference_invalidation: false,
			recovery: recovery::Recovery::default(),
			bitrate_ceiling: format.bitrate,
			pending_bitrate: format.bitrate,
			input_slots,
			current: 0,
			submitted: 0,
			idr_pending: true,
			color: ColorState::new(format.color),
			jobs: None,
			readback: None,
		};
		// Drop releases everything created so far if a step fails.
		encoder.initialize(&profile, &format).map_err(|e| e.to_string())?;
		encoder.create_buffers().map_err(|e| e.to_string())?;
		let (jobs, receiver) = std::sync::mpsc::channel();
		let shared = encoder.shared.clone();
		encoder.readback = Some(
			std::thread::Builder::new()
				.name("nvenc-readback".into())
				.spawn(move || readback(shared, receiver))
				.map_err(|e| format!("cannot start the NVENC readback thread: {e}"))?,
		);
		encoder.jobs = Some(jobs);
		tracing::info!(
			profile = profile.label,
			width = format.width,
			height = format.height,
			reference_invalidation = encoder.reference_invalidation,
			"Created NVENC encoder"
		);
		Ok(encoder)
	}

	fn caps(&self, codec: GUID, capability: NV_ENC_CAPS) -> i32 {
		let mut param = NV_ENC_CAPS_PARAM {
			version: CAPS_PARAM_VER,
			capsToQuery: capability,
			..Default::default()
		};
		let mut value = 0;
		let Some(get) = self.shared.api.nvEncGetEncodeCaps else {
			return 0;
		};
		// SAFETY: valid session and outputs.
		let status = unsafe { get(self.shared.session, codec, &mut param, &mut value) };
		if status == NV_ENC_SUCCESS { value } else { 0 }
	}

	fn initialize(&mut self, profile: &Profile, format: &NativeFormat) -> Result<(), EncodeError> {
		let shared = self.shared.clone();
		let api = shared.api;
		let _current = shared.cuda.push()?;

		let mut count = 0u32;
		let mut codecs = [GUID::default(); 16];
		// SAFETY: valid session; the array holds 16 entries.
		unsafe {
			shared.check(
				api.nvEncGetEncodeGUIDs.ok_or(EncodeError::native("NVENC table"))?(
					shared.session,
					codecs.as_mut_ptr(),
					codecs.len() as u32,
					&mut count,
				),
				"encode GUIDs",
			)?;
		}
		if !codecs[..count as usize]
			.iter()
			.any(|codec| same_guid(codec, &profile.codec))
		{
			return Err(EncodeError::native(format!(
				"this GPU's NVENC has no {} encoder",
				profile.label
			)));
		}
		if format.chroma == ChromaFormat::Yuv444 && self.caps(profile.codec, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE) == 0 {
			return Err(EncodeError::native(format!(
				"this GPU's NVENC has no {} encoder",
				profile.label
			)));
		}
		if format.bit_depth == BitDepth::Ten && self.caps(profile.codec, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) == 0 {
			return Err(EncodeError::native(format!(
				"this GPU's NVENC has no {} encoder",
				profile.label
			)));
		}
		let (max_width, max_height) = (
			self.caps(profile.codec, NV_ENC_CAPS_WIDTH_MAX),
			self.caps(profile.codec, NV_ENC_CAPS_HEIGHT_MAX),
		);
		if format.width as i32 > max_width || format.height as i32 > max_height {
			return Err(EncodeError::native(format!(
				"NVENC {} encodes at most {max_width}x{max_height}",
				profile.label
			)));
		}
		self.reference_invalidation = self.caps(profile.codec, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0;

		// SAFETY: plain-data structure; the preset query fills it.
		let mut preset: NV_ENC_PRESET_CONFIG = unsafe { std::mem::zeroed() };
		preset.version = PRESET_CONFIG_VER;
		preset.presetCfg.version = CONFIG_VER;
		// SAFETY: valid session, GUIDs and output.
		unsafe {
			shared.check(
				api.nvEncGetEncodePresetConfigEx
					.ok_or(EncodeError::native("NVENC table"))?(
					shared.session,
					profile.codec,
					PRESET_P1,
					NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
					&mut preset,
				),
				"preset configuration",
			)?;
		}
		*self.config = preset.presetCfg;
		let config = &mut *self.config;
		config.version = CONFIG_VER;
		config.profileGUID = profile.profile;
		config.gopLength = NVENC_INFINITE_GOPLENGTH;
		config.frameIntervalP = 1;
		let rc = &mut config.rcParams;
		rc.version = RC_PARAMS_VER;
		rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
		rc.averageBitRate = format.bitrate;
		rc.maxBitRate = format.bitrate;
		// One frame of buffer, starting full, as on the other backends.
		rc.vbvBufferSize = (u64::from(format.bitrate) / u64::from(format.fps.max(1))) as u32;
		rc.vbvInitialDelay = rc.vbvBufferSize;
		rc.set_zeroReorderDelay(1);
		rc.set_enableLookahead(0);
		self.apply_codec_config(format.chroma, format.bit_depth, format.color);

		// SAFETY: apply_codec_config initialized the union member for this codec.
		let retained = unsafe {
			match self.codec {
				VideoCodec::H264 => self.config.encodeCodecConfig.h264Config.maxNumRefFrames,
				VideoCodec::Hevc => self.config.encodeCodecConfig.hevcConfig.maxNumRefFramesInDPB,
				VideoCodec::Av1 => self.config.encodeCodecConfig.av1Config.maxNumRefFramesInDPB,
				VideoCodec::PyroWave => 0,
			}
		};
		self.recovery = recovery::Recovery::new(retained);
		let init = &mut *self.init;
		init.version = INITIALIZE_PARAMS_VER;
		init.encodeGUID = profile.codec;
		init.presetGUID = PRESET_P1;
		init.encodeWidth = format.width;
		init.encodeHeight = format.height;
		init.darWidth = format.width;
		init.darHeight = format.height;
		init.frameRateNum = format.fps;
		init.frameRateDen = 1;
		init.enableEncodeAsync = 0;
		init.enablePTD = 1;
		init.encodeConfig = &mut *self.config;
		init.maxEncodeWidth = format.width;
		init.maxEncodeHeight = format.height;
		init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
		init.bufferFormat = profile.buffer_format;
		// SAFETY: valid session and parameters (the config lives in `self`).
		unsafe {
			shared.check(
				api.nvEncInitializeEncoder.ok_or(EncodeError::native("NVENC table"))?(shared.session, &mut *self.init),
				"initialize encoder",
			)
		}
	}

	/// Codec-specific fields: chroma, bit depth, color description, headers
	/// on every IDR, no periodic IDR.
	fn apply_codec_config(&mut self, chroma: ChromaFormat, depth: BitDepth, color: ColorDescription) {
		let chroma_idc = if chroma == ChromaFormat::Yuv444 { 3 } else { 1 };
		let depth_minus8 = u32::from(depth.bits()) - 8;
		// Chroma position is defined for 4:2:0 only.
		let chroma_location = if chroma == ChromaFormat::Yuv420 {
			color.chroma_location
		} else {
			pixelforge::ChromaLocation::Unspecified
		};
		let loc_type = chroma_location.h26x_loc_type();
		let vui = NV_ENC_CONFIG_H264_VUI_PARAMETERS {
			videoSignalTypePresentFlag: 1,
			videoFormat: NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED,
			videoFullRangeFlag: u32::from(color.full_range),
			colourDescriptionPresentFlag: 1,
			// NVENC's VUI enumerations are the ITU-T H.273 code points.
			colourPrimaries: u32::from(color.color_primaries) as _,
			transferCharacteristics: u32::from(color.transfer_characteristics) as _,
			colourMatrix: u32::from(color.matrix_coefficients) as _,
			chromaSampleLocationFlag: u32::from(loc_type.is_some()),
			chromaSampleLocationTop: u32::from(loc_type.unwrap_or(0)),
			chromaSampleLocationBot: u32::from(loc_type.unwrap_or(0)),
			bitstreamRestrictionFlag: 1,
			..Default::default()
		};
		let codec = &mut self.config.encodeCodecConfig;
		// SAFETY: writing the union member of the session's codec.
		unsafe {
			match self.codec {
				VideoCodec::H264 => {
					let h264 = &mut codec.h264Config;
					h264.set_repeatSPSPPS(1);
					h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
					h264.chromaFormatIDC = chroma_idc;
					h264.level = NV_ENC_LEVEL_AUTOSELECT;
					h264.h264VUIParameters = vui;
				},
				VideoCodec::Hevc => {
					let hevc = &mut codec.hevcConfig;
					hevc.set_repeatSPSPPS(1);
					hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
					hevc.set_chromaFormatIDC(chroma_idc);
					hevc.set_pixelBitDepthMinus8(depth_minus8);
					hevc.level = NV_ENC_LEVEL_AUTOSELECT;
					hevc.hevcVUIParameters = vui;
				},
				VideoCodec::Av1 => {
					let av1 = &mut codec.av1Config;
					av1.set_repeatSeqHdr(1);
					av1.set_outputAnnexBFormat(0);
					av1.idrPeriod = NVENC_INFINITE_GOPLENGTH;
					av1.set_chromaFormatIDC(chroma_idc);
					av1.set_inputPixelBitDepthMinus8(depth_minus8);
					av1.set_pixelBitDepthMinus8(depth_minus8);
					av1.colorPrimaries = vui.colourPrimaries;
					av1.transferCharacteristics = vui.transferCharacteristics;
					av1.matrixCoefficients = vui.colourMatrix;
					av1.colorRange = u32::from(color.full_range);
					av1.chromaSamplePosition = u32::from(chroma_location.av1_sample_position());
				},
				VideoCodec::PyroWave => {},
			}
		}
	}

	fn create_buffers(&mut self) -> Result<(), EncodeError> {
		let shared = self.shared.clone();
		let api = shared.api;
		let cuda = shared.cuda.cuda;
		let _current = shared.cuda.push()?;
		let device = self.vulkan.device();
		let external_memory_fd = ash::khr::external_memory_fd::Device::load(self.vulkan.instance(), device);
		for _ in 0..self.input_slots {
			let mut slot = Slot {
				buffer: vk::Buffer::null(),
				memory: vk::DeviceMemory::null(),
				external: std::ptr::null_mut(),
				pointer: 0,
				registered: std::ptr::null_mut(),
				bitstream: std::ptr::null_mut(),
			};
			let result = (|| -> Result<(), EncodeError> {
				let vk_error = |what: &str| {
					let what = what.to_string();
					move |e: vk::Result| EncodeError::native(format!("NVENC input buffer {what}: {e}"))
				};
				let mut external_info = vk::ExternalMemoryBufferCreateInfo::default()
					.handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
				let info = vk::BufferCreateInfo::default()
					.size(self.layout.size)
					.usage(vk::BufferUsageFlags::TRANSFER_DST)
					.sharing_mode(vk::SharingMode::EXCLUSIVE)
					.push(&mut external_info);
				// SAFETY: valid create info on the capture device.
				slot.buffer = unsafe { device.create_buffer(&info, None) }.map_err(vk_error("creation"))?;
				// SAFETY: plain query on a valid buffer.
				let requirements = unsafe { device.get_buffer_memory_requirements(slot.buffer) };
				let memory_type = self
					.vulkan
					.find_memory_type(requirements.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
					.ok_or_else(|| EncodeError::native("no device-local memory for the NVENC input buffer"))?;
				let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(slot.buffer);
				let mut export =
					vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
				let allocate = vk::MemoryAllocateInfo::default()
					.allocation_size(requirements.size)
					.memory_type_index(memory_type)
					.push(&mut export)
					.push(&mut dedicated);
				// SAFETY: valid allocation chain.
				slot.memory = unsafe { device.allocate_memory(&allocate, None) }.map_err(vk_error("memory"))?;
				// SAFETY: fresh buffer and memory on the same device.
				unsafe { device.bind_buffer_memory(slot.buffer, slot.memory, 0) }.map_err(vk_error("bind"))?;
				let fd_info = vk::MemoryGetFdInfoKHR::default()
					.memory(slot.memory)
					.handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
				// SAFETY: exportable memory; the descriptor is handed to CUDA.
				let fd = unsafe { external_memory_fd.get_memory_fd(&fd_info) }.map_err(vk_error("export"))?;
				let mut handle = CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
					type_: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
					size: requirements.size,
					flags: CUDA_EXTERNAL_MEMORY_DEDICATED,
					..Default::default()
				};
				handle.handle.fd = fd;
				// SAFETY: valid descriptor; CUDA owns `fd` on success.
				let status = unsafe { (cuda.import_external_memory)(&mut slot.external, &handle) };
				if status != CUDA_SUCCESS {
					// SAFETY: CUDA did not take the descriptor.
					unsafe { libc::close(fd) };
					return Err(EncodeError::native(format!(
						"NVENC input buffer CUDA import: {}",
						cuda.error(status)
					)));
				}
				let mapping = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
					offset: 0,
					size: self.layout.size,
					..Default::default()
				};
				// SAFETY: valid external memory covering the mapping.
				let status =
					unsafe { (cuda.external_memory_get_mapped_buffer)(&mut slot.pointer, slot.external, &mapping) };
				cuda.check(status, "NVENC input buffer CUDA mapping")
					.map_err(EncodeError::native)?;
				let mut register = NV_ENC_REGISTER_RESOURCE {
					version: REGISTER_RESOURCE_VER,
					resourceType: NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
					width: self.extent.0,
					height: self.layout.luma_rows(),
					pitch: self.layout.row_pitch_bytes(),
					resourceToRegister: slot.pointer as *mut c_void,
					bufferFormat: self.buffer_format,
					bufferUsage: NV_ENC_INPUT_IMAGE,
					// SAFETY: plain-data structure; zero is valid for the rest.
					..unsafe { std::mem::zeroed() }
				};
				if self.layout.plane_count() > 1 {
					register.chromaOffset[0] = self.layout.chroma_offset * 4;
				}
				// SAFETY: valid session and CUDA allocation.
				unsafe {
					shared.check(
						api.nvEncRegisterResource.ok_or(EncodeError::native("NVENC table"))?(
							shared.session,
							&mut register,
						),
						"register input buffer",
					)?;
				}
				slot.registered = register.registeredResource;
				let mut bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER {
					version: CREATE_BITSTREAM_BUFFER_VER,
					..Default::default()
				};
				// SAFETY: valid session.
				unsafe {
					shared.check(
						api.nvEncCreateBitstreamBuffer
							.ok_or(EncodeError::native("NVENC table"))?(shared.session, &mut bitstream),
						"create bitstream buffer",
					)?;
				}
				slot.bitstream = bitstream.bitstreamBuffer;
				Ok(())
			})();
			// Pushed even on failure so Drop releases the partial slot.
			self.slots.push(slot);
			result?;
		}
		Ok(())
	}

	/// The converter output this encoder reads, the extent to convert and the
	/// row alignment of its input buffers.
	pub(crate) fn conversion(&self) -> (ConvertFormat, (u32, u32), u32) {
		(self.layout.format, self.extent, ROW_ALIGNMENT)
	}

	pub(crate) fn set_bitrate(&mut self, requested: u32) -> Result<u32, EncodeError> {
		let accepted = super::backend::bounded_bitrate(requested, self.bitrate_ceiling)?;
		let previous = self.pending_bitrate;
		self.pending_bitrate = accepted;
		if let Err(error) = self.apply_bitrate() {
			self.pending_bitrate = previous;
			return Err(error);
		}
		Ok(accepted)
	}

	/// Reconfigure a copy, committing parameters only after SDK acceptance.
	/// A bitrate-only change leaves resetEncoder/forceIDR clear.
	fn apply_bitrate(&mut self) -> Result<(), EncodeError> {
		if self.config.rcParams.averageBitRate == self.pending_bitrate {
			return Ok(());
		}
		let mut config = *self.config;
		config.rcParams.averageBitRate = self.pending_bitrate;
		config.rcParams.maxBitRate = self.pending_bitrate;
		config.rcParams.vbvBufferSize =
			self.pending_bitrate / self.init.frameRateNum.max(1) * self.init.frameRateDen.max(1);
		config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
		let mut init = *self.init;
		init.encodeConfig = &mut config;
		let mut params = NV_ENC_RECONFIGURE_PARAMS {
			version: RECONFIGURE_PARAMS_VER,
			reInitEncodeParams: init,
			// SAFETY: zeroed reserved fields and no reset/IDR request.
			..unsafe { std::mem::zeroed() }
		};
		let shared = &self.shared;
		let _current = shared.cuda.push()?;
		let _session = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
		// SAFETY: config and init copies remain live throughout the call.
		unsafe {
			shared.check(
				shared
					.api
					.nvEncReconfigureEncoder
					.ok_or(EncodeError::native("NVENC reconfigure unavailable"))?(shared.session, &mut params),
				"live bitrate reconfigure",
			)?;
		}
		*self.config = config;
		Ok(())
	}

	pub(crate) fn next_display_order(&self) -> u64 {
		self.submitted
	}

	pub(crate) fn request_idr(&mut self) {
		self.idr_pending = true;
		self.recovery.reset(self.submitted);
	}

	pub(crate) fn invalidate_reference_frames(&mut self, display_order: u64) {
		if self.idr_pending || !self.reference_invalidation {
			self.request_idr();
			return;
		}
		let (first, end) = match self.recovery.plan(display_order, self.submitted) {
			recovery::Plan::Idr => {
				self.request_idr();
				return;
			},
			recovery::Plan::Covered => return,
			recovery::Plan::Invalidate { first, end } => (first, end),
		};
		let shared = &self.shared;
		let Some(invalidate) = shared.api.nvEncInvalidateRefFrames else {
			self.idr_pending = true;
			return;
		};
		let Ok(_current) = shared.cuda.push() else {
			self.idr_pending = true;
			return;
		};
		let _session = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
		for timestamp in first..end {
			// SAFETY: valid session; timestamps are the frames' inputTimeStamp.
			if unsafe { invalidate(shared.session, timestamp) } != NV_ENC_SUCCESS {
				self.idr_pending = true;
				return;
			}
		}
		self.recovery.accepted(first, end);
	}

	pub(crate) fn set_color_description(&mut self, description: ColorDescription) -> Result<(), EncodeError> {
		self.color.request(description);
		Ok(())
	}

	fn wait_free(&self, wanted: impl Fn(&[bool]) -> bool) -> Result<(), EncodeError> {
		let busy = self.shared.busy.lock().unwrap_or_else(|e| e.into_inner());
		let (_busy, timeout) = self
			.shared
			.freed
			.wait_timeout_while(busy, SLOT_TIMEOUT, |busy| !wanted(busy))
			.unwrap_or_else(|e| e.into_inner());
		if timeout.timed_out() {
			return Err(EncodeError::native("NVENC did not finish a frame within 5 s"));
		}
		Ok(())
	}

	pub(crate) fn input_target(&mut self) -> Result<Target, EncodeError> {
		let current = self.current;
		self.wait_free(|busy| !busy[current])?;
		Ok(Target::Buffer {
			buffer: self.slots[current].buffer,
		})
	}

	pub(crate) fn flush(&mut self) -> Result<(), EncodeError> {
		self.wait_free(|busy| busy.iter().all(|b| !b))
	}

	/// Apply a changed color description: reinitialize with the new VUI and
	/// start over with an IDR.
	fn reconfigure(&mut self, description: ColorDescription) -> Result<(), EncodeError> {
		let chroma = if self.layout.plane_count() == 1 {
			ChromaFormat::Yuv444
		} else {
			ChromaFormat::Yuv420
		};
		let depth = if self.layout.format.bit_depth() == 10 {
			BitDepth::Ten
		} else {
			BitDepth::Eight
		};
		self.apply_codec_config(chroma, depth, description);
		let shared = self.shared.clone();
		let _current = shared.cuda.push()?;
		let _session = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
		self.init.encodeConfig = &mut *self.config;
		let mut params = NV_ENC_RECONFIGURE_PARAMS {
			version: RECONFIGURE_PARAMS_VER,
			reInitEncodeParams: *self.init,
			// SAFETY: plain-data structure.
			..unsafe { std::mem::zeroed() }
		};
		params.set_resetEncoder(1);
		params.set_forceIDR(1);
		// SAFETY: valid session and parameters.
		unsafe {
			shared.check(
				shared
					.api
					.nvEncReconfigureEncoder
					.ok_or(EncodeError::native("NVENC table"))?(shared.session, &mut params),
				"reconfigure",
			)
		}
	}

	pub(crate) fn encode(&mut self, target: Target) -> Result<PacketFuture, EncodeError> {
		let index = self.current;
		if !matches!(target, Target::Buffer { buffer } if buffer == self.slots[index].buffer) {
			return Err(EncodeError::native(
				"encode target is not the current NVENC input buffer",
			));
		}
		if let Some(description) = self.color.pending {
			// The change stays pending until NVENC accepts it: a failed
			// reconfiguration fails this frame (converted for the new
			// description) and is retried with the next one, so no frame is
			// encoded under the previous VUI.
			self.reconfigure(description)?;
			self.color.accepted(description);
			self.idr_pending = true;
		}
		let shared = self.shared.clone();
		let api = shared.api;
		let slot = &self.slots[index];
		let _current = shared.cuda.push()?;
		shared.busy.lock().unwrap_or_else(|e| e.into_inner())[index] = true;
		let submitted = (|| -> Result<NV_ENC_INPUT_PTR, EncodeError> {
			let _session = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
			let mut map = NV_ENC_MAP_INPUT_RESOURCE {
				version: MAP_INPUT_RESOURCE_VER,
				registeredResource: slot.registered,
				// SAFETY: plain-data structure.
				..unsafe { std::mem::zeroed() }
			};
			// SAFETY: valid session and registered resource.
			unsafe {
				shared.check(
					api.nvEncMapInputResource.ok_or(EncodeError::native("NVENC table"))?(shared.session, &mut map),
					"map input",
				)?;
			}
			let mut picture = NV_ENC_PIC_PARAMS {
				version: PIC_PARAMS_VER,
				inputWidth: self.width,
				inputHeight: self.height,
				inputPitch: self.layout.row_pitch_bytes(),
				encodePicFlags: if self.idr_pending {
					NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
				} else {
					0
				},
				inputTimeStamp: self.submitted,
				inputBuffer: map.mappedResource,
				outputBitstream: slot.bitstream,
				bufferFmt: map.mappedBufferFmt,
				pictureStruct: NV_ENC_PIC_STRUCT_FRAME,
				// SAFETY: plain-data structure.
				..unsafe { std::mem::zeroed() }
			};
			// SAFETY: valid session, mapped input and bitstream buffer.
			let status = unsafe {
				api.nvEncEncodePicture.ok_or(EncodeError::native("NVENC table"))?(shared.session, &mut picture)
			};
			if let Err(error) = shared.check(status, "encode") {
				// SAFETY: mapped above.
				unsafe {
					if let Some(unmap) = api.nvEncUnmapInputResource {
						unmap(shared.session, map.mappedResource);
					}
				}
				return Err(error);
			}
			Ok(map.mappedResource)
		})();
		let mapped = match submitted {
			Ok(mapped) => mapped,
			Err(error) => {
				shared.busy.lock().unwrap_or_else(|e| e.into_inner())[index] = false;
				self.idr_pending = true;
				return Err(error);
			},
		};
		let (reply, receiver) = tokio::sync::oneshot::channel();
		if self.jobs.as_ref().is_none_or(|jobs| {
			jobs.send(Job {
				slot: index,
				mapped,
				bitstream: slot.bitstream,
				reply,
			})
			.is_err()
		}) {
			return Err(EncodeError::native("NVENC readback thread stopped"));
		}
		if self.idr_pending {
			self.recovery.reset(self.submitted);
		}
		self.idr_pending = false;
		self.submitted += 1;
		self.current = (self.current + 1) % self.slots.len();
		Ok(PacketFuture::Native(receiver))
	}
}

impl Drop for NvencEncoder {
	fn drop(&mut self) {
		self.jobs = None;
		if let Some(thread) = self.readback.take() {
			let _ = thread.join();
		}
		let shared = &self.shared;
		let api = shared.api;
		let cuda = shared.cuda.cuda;
		let device = self.vulkan.device();
		{
			let current = shared.cuda.push();
			// SAFETY: every frame was read back; objects are released in
			// reverse order of creation, each only if it was created.
			unsafe {
				for slot in &self.slots {
					if !slot.registered.is_null()
						&& let Some(unregister) = api.nvEncUnregisterResource
					{
						unregister(shared.session, slot.registered);
					}
					if !slot.bitstream.is_null()
						&& let Some(destroy) = api.nvEncDestroyBitstreamBuffer
					{
						destroy(shared.session, slot.bitstream);
					}
				}
				if let Some(destroy) = api.nvEncDestroyEncoder {
					destroy(shared.session);
				}
				for slot in &self.slots {
					if slot.pointer != 0 {
						(cuda.mem_free)(slot.pointer);
					}
					if !slot.external.is_null() {
						(cuda.destroy_external_memory)(slot.external);
					}
				}
			}
			drop(current);
		}
		for slot in &self.slots {
			// SAFETY: conversions into the buffers completed (each was fenced)
			// and CUDA released its imports above.
			unsafe {
				if slot.buffer != vk::Buffer::null() {
					device.destroy_buffer(slot.buffer, None);
				}
				if slot.memory != vk::DeviceMemory::null() {
					device.free_memory(slot.memory, None);
				}
			}
		}
	}
}

/// Wait for each submitted frame, copy its bitstream out and free its buffer.
fn readback(shared: Arc<Shared>, jobs: std::sync::mpsc::Receiver<Job>) {
	for job in jobs {
		let result = read_frame(&shared, job.bitstream, job.mapped);
		let _ = job.reply.send(result);
		shared.busy.lock().unwrap_or_else(|e| e.into_inner())[job.slot] = false;
		shared.freed.notify_all();
	}
}

/// Lock a submitted frame's bitstream (waiting for the encode), copy it, then
/// unlock it and unmap its input.
fn read_frame(shared: &Shared, bitstream: NV_ENC_OUTPUT_PTR, mapped: NV_ENC_INPUT_PTR) -> Result<Packet, EncodeError> {
	let api = shared.api;
	let _current = shared.cuda.push()?;
	let lock_bitstream = api.nvEncLockBitstream.ok_or(EncodeError::native("NVENC table"))?;
	let mut lock = NV_ENC_LOCK_BITSTREAM {
		version: LOCK_BITSTREAM_VER,
		outputBitstream: bitstream,
		// SAFETY: plain-data structure.
		..unsafe { std::mem::zeroed() }
	};
	// Waits for the encode; not serialized with submissions.
	// SAFETY: valid session and a submitted bitstream buffer.
	let status = unsafe { lock_bitstream(shared.session, &mut lock) };
	let result = shared.check(status, "lock bitstream").and_then(|()| {
		if lock.bitstreamBufferPtr.is_null() || lock.bitstreamSizeInBytes == 0 {
			return Err(EncodeError::native("NVENC produced an empty frame"));
		}
		// SAFETY: the locked buffer holds `bitstreamSizeInBytes` bytes until
		// it is unlocked below.
		let data = unsafe {
			std::slice::from_raw_parts(lock.bitstreamBufferPtr.cast::<u8>(), lock.bitstreamSizeInBytes as usize)
		}
		.to_vec();
		Ok(Packet {
			data,
			is_key_frame: matches!(lock.pictureType, NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE_I),
		})
	});
	let _session = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
	// SAFETY: locked above (when successful) and mapped by `encode`.
	unsafe {
		if status == NV_ENC_SUCCESS
			&& let Some(unlock) = api.nvEncUnlockBitstream
		{
			unlock(shared.session, bitstream);
		}
		if let Some(unmap) = api.nvEncUnmapInputResource {
			unmap(shared.session, mapped);
		}
	}
	result
}

/// Create an encoder for `codec`/`chroma`/`depth` on the capture GPU and
/// encode one frame: the startup probe of an NVENC profile.
pub(crate) fn probe(
	context: &VideoContext,
	codec: VideoCodec,
	chroma: ChromaFormat,
	depth: BitDepth,
) -> Result<(), String> {
	profile(codec, chroma, depth)?;
	let mut encoder = NvencEncoder::new(
		context,
		NativeFormat {
			codec,
			chroma,
			bit_depth: depth,
			width: PROBE_EXTENT.0,
			height: PROBE_EXTENT.1,
			fps: 60,
			bitrate: 10_000_000,
			color: ColorDescription::srgb(),
		},
	)?;
	let target = encoder.input_target().map_err(|e| e.to_string())?;
	let PacketFuture::Native(receiver) = encoder.encode(target).map_err(|e| e.to_string())? else {
		unreachable!("NVENC frames resolve through the readback thread")
	};
	let packet = receiver
		.blocking_recv()
		.map_err(|_| "NVENC readback stopped".to_string())?
		.map_err(|e| e.to_string())?;
	if !packet.is_key_frame || packet.data.len() < 8 {
		return Err("NVENC encoded no key frame".into());
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn color_changes_stay_pending_until_accepted() {
		let sdr = ColorDescription::bt709();
		let hdr = ColorDescription::bt2020_pq();
		let mut color = ColorState::new(sdr);
		color.request(sdr);
		assert_eq!(color.pending, None, "the applied description needs no change");
		color.request(hdr);
		assert_eq!(color.pending, Some(hdr));
		// A failed reconfiguration does not call `accepted`: the next frame
		// retries the same change.
		assert_eq!(color.pending, Some(hdr));
		color.accepted(hdr);
		assert_eq!(
			color,
			ColorState {
				applied: hdr,
				pending: None
			}
		);
		// Back to SDR, then to the applied HDR before NVENC accepted SDR.
		color.request(sdr);
		assert_eq!(color.pending, Some(sdr));
		color.request(hdr);
		assert_eq!(color.pending, None);
	}
}
