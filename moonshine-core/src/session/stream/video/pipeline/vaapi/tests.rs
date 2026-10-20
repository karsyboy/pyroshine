use super::*;

#[test]
fn profiles_map_each_format_to_its_surface_layout() {
	let check = |codec, chroma, depth, rt, fourcc, convert| {
		let p = profile(codec, chroma, depth).unwrap();
		assert_eq!((p.rt_format, p.fourcc, p.convert), (rt, fourcc, convert));
	};
	use BitDepth::{Eight, Ten};
	use ChromaFormat::{Yuv420, Yuv444};
	check(
		VideoCodec::H264,
		Yuv420,
		Eight,
		VA_RT_FORMAT_YUV420,
		VA_FOURCC_NV12,
		ConvertFormat::Nv12,
	);
	check(
		VideoCodec::Hevc,
		Yuv420,
		Ten,
		VA_RT_FORMAT_YUV420_10,
		VA_FOURCC_P010,
		ConvertFormat::P010,
	);
	check(
		VideoCodec::Hevc,
		Yuv444,
		Eight,
		VA_RT_FORMAT_YUV444,
		VA_FOURCC_XYUV,
		ConvertFormat::Xyuv,
	);
	check(
		VideoCodec::Hevc,
		Yuv444,
		Ten,
		VA_RT_FORMAT_YUV444_10,
		VA_FOURCC_Y410,
		ConvertFormat::Y410,
	);
	check(
		VideoCodec::Av1,
		Yuv420,
		Ten,
		VA_RT_FORMAT_YUV420_10,
		VA_FOURCC_P010,
		ConvertFormat::P010,
	);
	assert!(profile(VideoCodec::Av1, Yuv444, Eight).is_err());
	assert!(profile(VideoCodec::H264, Yuv444, Eight).is_err());
}

#[test]
fn surface_layers_match_the_converter_planes() {
	let layers = layer_formats(ConvertFormat::Nv12, (1920, 1088));
	assert_eq!(
		layers,
		[(vk::Format::R8_UNORM, 1920, 1088), (vk::Format::R8G8_UNORM, 960, 544)]
	);
	assert_eq!(layer_formats(ConvertFormat::Y410, (16, 16)).len(), 1);
	assert!(layer_formats(ConvertFormat::Yuv444, (16, 16)).is_empty());
}

/// Read a coded-buffer list of `(status, bytes)` segments.
fn read_segments(segments: &[(u32, &[u8])]) -> Result<Vec<u8>, String> {
	let mut payloads: Vec<Vec<u8>> = segments.iter().map(|(_, bytes)| bytes.to_vec()).collect();
	let mut list: Vec<VACodedBufferSegment> = segments
		.iter()
		.zip(&mut payloads)
		.map(|(&(status, _), payload)| VACodedBufferSegment {
			size: payload.len() as u32,
			status,
			buf: payload.as_mut_ptr().cast(),
			..Default::default()
		})
		.collect();
	for index in 1..list.len() {
		let next: *mut VACodedBufferSegment = &mut list[index];
		list[index - 1].next = next.cast();
	}
	let head = list.first().map_or(std::ptr::null(), |segment| segment as *const _);
	// SAFETY: a valid list whose payloads outlive the call.
	unsafe { coded_segments(head, &[9]) }.map_err(|e| e.to_string())
}

#[test]
fn coded_segments_reject_corrupt_and_overflowing_frames() {
	assert_eq!(read_segments(&[(0, &[1, 2]), (0, &[3])]), Ok(vec![9, 1, 2, 3]));
	// QP and frame-size hints are informational.
	let informational = 0x33 | VA_CODED_BUF_STATUS_LARGE_SLICE_MASK;
	assert_eq!(read_segments(&[(informational, &[1])]), Ok(vec![9, 1]));
	for position in 0..3 {
		let mut segments = [(0, &[1u8][..]), (0, &[2][..]), (0, &[3][..])];
		segments[position].0 = VA_CODED_BUF_STATUS_BAD_BITSTREAM | 0x20;
		assert!(
			read_segments(&segments).unwrap_err().contains("corrupt"),
			"segment {position}"
		);
	}
	assert!(
		read_segments(&[(VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK, &[1])])
			.unwrap_err()
			.contains("overflowed")
	);
	assert!(read_segments(&[(0, &[])]).unwrap_err().contains("empty"));
	assert!(read_segments(&[]).unwrap_err().contains("empty"));
}

#[test]
fn entry_points_prefer_low_power_and_fall_back_to_slice() {
	let both = [VAEntrypointEncSlice, VAEntrypointVLD, VAEntrypointEncSliceLP];
	assert_eq!(
		entrypoint_order(&both, None),
		[VAEntrypointEncSliceLP, VAEntrypointEncSlice]
	);
	// The probe found only the slice encoder working.
	assert_eq!(
		entrypoint_order(&both, Some(VAEntrypointEncSlice)),
		[VAEntrypointEncSlice, VAEntrypointEncSliceLP]
	);
	assert_eq!(entrypoint_order(&[VAEntrypointEncSlice], None), [VAEntrypointEncSlice]);
	// A verified entry point the driver no longer lists is not tried.
	assert_eq!(
		entrypoint_order(&[VAEntrypointEncSliceLP], Some(VAEntrypointEncSlice)),
		[VAEntrypointEncSliceLP]
	);
	assert!(entrypoint_order(&[VAEntrypointVLD], None).is_empty());
}

/// A libva stand-in for the readback thread: syncs return the statuses set
/// here and the coded buffer maps to one three-byte segment.
mod fake {
	use std::sync::atomic::AtomicI32;

	use super::*;

	pub(super) static SYNC_BUFFER: AtomicI32 = AtomicI32::new(0);
	pub(super) static SYNC_SURFACE: AtomicI32 = AtomicI32::new(0);
	static mut PAYLOAD: [u8; 3] = [1, 2, 3];

	unsafe extern "C" fn get_display(_: c_int) -> VADisplay {
		unreachable!()
	}
	unsafe extern "C" fn initialize(_: VADisplay, _: *mut c_int, _: *mut c_int) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn success(_: VADisplay) -> VAStatus {
		0
	}
	unsafe extern "C" fn error_str(_: VAStatus) -> *const c_char {
		c"fake error".as_ptr()
	}
	unsafe extern "C" fn vendor(_: VADisplay) -> *const c_char {
		unreachable!()
	}
	unsafe extern "C" fn max_num(_: VADisplay) -> c_int {
		unreachable!()
	}
	unsafe extern "C" fn entrypoints(_: VADisplay, _: VAProfile, _: *mut VAEntrypoint, _: *mut c_int) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn attributes(
		_: VADisplay,
		_: VAProfile,
		_: VAEntrypoint,
		_: *mut VAConfigAttrib,
		_: c_int,
	) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn create_config(
		_: VADisplay,
		_: VAProfile,
		_: VAEntrypoint,
		_: *mut VAConfigAttrib,
		_: c_int,
		_: *mut VAConfigID,
	) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn destroy_id(_: VADisplay, _: VAGenericID) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn create_surfaces(
		_: VADisplay,
		_: c_uint,
		_: c_uint,
		_: c_uint,
		_: *mut VASurfaceID,
		_: c_uint,
		_: *mut VASurfaceAttrib,
		_: c_uint,
	) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn destroy_surfaces(_: VADisplay, _: *mut VASurfaceID, _: c_int) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn create_context(
		_: VADisplay,
		_: VAConfigID,
		_: c_int,
		_: c_int,
		_: c_int,
		_: *mut VASurfaceID,
		_: c_int,
		_: *mut VAContextID,
	) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn create_buffer(
		_: VADisplay,
		_: VAContextID,
		_: VABufferType,
		_: c_uint,
		_: c_uint,
		_: *mut c_void,
		_: *mut VABufferID,
	) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn map_buffer(_: VADisplay, _: VABufferID, mapped: *mut *mut c_void) -> VAStatus {
		let segment = Box::leak(Box::new(VACodedBufferSegment {
			size: 3,
			// Only read through the segment.
			buf: std::ptr::addr_of_mut!(PAYLOAD).cast(),
			..Default::default()
		}));
		// SAFETY: libva's out parameter.
		unsafe { *mapped = (segment as *mut VACodedBufferSegment).cast() };
		0
	}
	unsafe extern "C" fn unmap_buffer(_: VADisplay, _: VABufferID) -> VAStatus {
		0
	}
	unsafe extern "C" fn begin_picture(_: VADisplay, _: VAContextID, _: VASurfaceID) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn render_picture(_: VADisplay, _: VAContextID, _: *mut VABufferID, _: c_int) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn end_picture(_: VADisplay, _: VAContextID) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn sync_surface(_: VADisplay, _: VASurfaceID) -> VAStatus {
		SYNC_SURFACE.load(std::sync::atomic::Ordering::SeqCst)
	}
	unsafe extern "C" fn sync_buffer(_: VADisplay, _: VABufferID, _: u64) -> VAStatus {
		SYNC_BUFFER.load(std::sync::atomic::Ordering::SeqCst)
	}
	unsafe extern "C" fn export(_: VADisplay, _: VASurfaceID, _: u32, _: u32, _: *mut c_void) -> VAStatus {
		unreachable!()
	}
	unsafe extern "C" fn set_callback(_: VADisplay, _: VAMessageCallback, _: *mut c_void) -> VAMessageCallback {
		unreachable!()
	}

	pub(super) fn shared() -> Arc<Shared> {
		let va: &'static Va = Box::leak(Box::new(Va {
			_va: libloading::os::unix::Library::this().into(),
			_drm: libloading::os::unix::Library::this().into(),
			get_display_drm: get_display,
			initialize,
			terminate: success,
			error_str,
			query_vendor_string: vendor,
			max_num_entrypoints: max_num,
			query_config_entrypoints: entrypoints,
			get_config_attributes: attributes,
			create_config,
			destroy_config: destroy_id,
			create_surfaces,
			destroy_surfaces,
			create_context,
			destroy_context: destroy_id,
			create_buffer,
			destroy_buffer: destroy_id,
			map_buffer,
			unmap_buffer,
			begin_picture,
			render_picture,
			end_picture,
			sync_surface,
			sync_buffer: Some(sync_buffer),
			export_surface_handle: export,
			set_error_callback: set_callback,
			set_info_callback: set_callback,
		}));
		Arc::new(Shared {
			display: Arc::new(Display {
				va,
				raw: std::ptr::null_mut(),
				_node: std::fs::File::open("/dev/null").unwrap(),
				vendor: "fake".into(),
			}),
			lock: Mutex::new(()),
			busy: Mutex::new(vec![true; INPUT_SURFACES]),
			freed: Condvar::new(),
			lost: AtomicBool::new(false),
		})
	}
}

#[test]
fn readback_frees_a_slot_only_after_its_completion_is_established() {
	use std::sync::atomic::Ordering::SeqCst;
	const FAILED: VAStatus = VA_STATUS_ERROR_OPERATION_FAILED as VAStatus;
	let run = |sync_buffer: VAStatus, sync_surface: VAStatus, read: bool| {
		fake::SYNC_BUFFER.store(sync_buffer, SeqCst);
		fake::SYNC_SURFACE.store(sync_surface, SeqCst);
		let shared = fake::shared();
		let (jobs, receiver) = std::sync::mpsc::channel();
		let (reply, packet) = tokio::sync::oneshot::channel();
		jobs.send(Job {
			slot: 1,
			surface: 7,
			coded: 8,
			key_frame: true,
			prefix: &[],
			reply: read.then_some(reply),
		})
		.unwrap();
		drop(jobs);
		readback(shared.clone(), receiver);
		let busy = shared.busy.lock().unwrap()[1];
		(
			busy,
			shared.lost.load(SeqCst),
			packet
				.blocking_recv()
				.ok()
				.map(|r| r.map(|p| p.data).map_err(|e| e.to_string())),
		)
	};
	// A frame read back normally.
	let (busy, lost, packet) = run(0, 0, true);
	assert!(!busy && !lost);
	assert_eq!(packet, Some(Ok(vec![1, 2, 3])));
	// A failed submission the driver may have accepted: drained by syncing.
	assert_eq!(run(0, 0, false), (false, false, None));
	// Its surface cannot be synced: the slot stays with the driver.
	assert_eq!(run(0, FAILED, false), (true, true, None));
	// A failed encode whose surface is idle frees the slot with an error.
	let (busy, lost, packet) = run(FAILED, 0, true);
	assert!(!busy && !lost);
	assert!(packet.unwrap().is_err());
	// Neither the coded buffer nor the surface synced: fail closed.
	let (busy, lost, packet) = run(FAILED, FAILED, true);
	assert!(busy && lost);
	assert!(packet.unwrap().is_err());
}
