//! The frame contract between a capture backend and the video pipeline.
//!
//! `ExportedFrame` is the single frame-exchange type: every backend produces
//! it and every encoder (conventional and PyroWave) consumes it the same way.
//!
//! | Property | Carried as |
//! | --- | --- |
//! | GPU identity | Implicit: the frame is on the capture GPU, whose verified Vulkan context the channel carries (`CaptureReceiver::context`) |
//! | Frame source | DMA-BUF planes (`planes()`), owned by a [`SourceLease`] |
//! | Pixel format, bit depth | DRM fourcc (`format`): packed RGB(A), 8-bit, 10-bit or FP16 |
//! | Modifier | `modifier`; always a pair in `gpu::ImportableModifiers` |
//! | Dimensions, crop | `width` × `height` is the visible image; frames are never cropped |
//! | Scaling | `scale_to_stream`: the whole frame fills the stream extent (Desktop Mode); otherwise it is sampled 1:1 |
//! | Primaries, transfer, matrix | [`FrameColorSpace`], from the source's color declaration |
//! | Range, chroma | Full-range RGB, so 4:4:4; the encoder converts to the negotiated YUV |
//! | HDR metadata | `hdr_metadata`, static HDR10 metadata of the source |
//! | Timestamps | `source_time` (content time, drives RTP), `created_at` (export completion) |
//! | Damage | None: every frame is encoded whole |
//! | Producer sync | Complete before the frame is sent (see below) |
//! | Consumer sync | `consumed` (content reuse) and the capture credit (demand), separately |
//!
//! A source that cannot meet a row (YUV or cropped buffers, an unsampleable
//! format, buffers on another GPU) must compose or convert it itself, or fail
//! to start; the pipeline never receives what it cannot read correctly.
//!
//! # Producer synchronization
//!
//! A frame is sent only once every producer write to its buffer has
//! completed: the backend waited on the producer's fence, or latched a client
//! commit only after its DMA-BUF finished rendering. Consumers therefore read
//! without waiting for the producer. A backend whose source signals
//! completion differently (for example a DRM syncobj timeline point) waits
//! for it before sending; carrying an unsignaled fence to the consumer would
//! need an extension of this type, not a different reading of it.
//!
//! # Source-buffer ownership
//!
//! Every frame owns a [`SourceLease`]: a strong reference to the Smithay
//! `Dmabuf` it was exported from (for the isolated compositor, a pool slot or
//! a client's direct-scanout buffer). The lease owns the plane descriptors, so
//! they stay open — and their numbers cannot be reused for another object —
//! for as long as the frame (or a clone of its lease) exists, regardless of
//! when the backend drops its pool, retires a resolution's pool or exits.
//! Plane fds are only handed out as `BorrowedFd`s tied to the frame's lifetime.
//!
//! The lease guarantees descriptor and memory validity. *Content* reuse is a
//! separate contract: `consumed` tells the backend it may render into, or
//! release to its client, that buffer again, and is set only once no GPU work
//! can still read it (after a completed read, or a failure that provably never
//! submitted one). The pipeline sets it after its read; the channel sets it
//! for frames it discards (stale epoch, full, disconnected). A failure whose
//! GPU access is unknown never sets it, so that buffer stays held rather than
//! risk reuse under a pending read. The capture credit is a third, independent
//! signal that bounds capture demand through transport.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use smithay::backend::allocator::Buffer;
use smithay::backend::allocator::dmabuf::Dmabuf;

/// Color encoding of a captured frame, as its source declared it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FrameColorSpace {
	/// sRGB (BT.709 primaries, sRGB EOTF, BT.709 matrix).
	#[default]
	Srgb,
	/// HDR10 (BT.2020 primaries, PQ EOTF, BT.2020 NCL matrix).
	///
	/// The pixel data is already BT.2020+PQ encoded; the encoder converts it
	/// straight to YUV (passthrough).
	Bt2020Pq,
	/// scRGB (BT.709 primaries, linear light, typically FP16 with values > 1.0
	/// for HDR highlights).
	///
	/// Used by DXGI HDR games whose swapchain negotiates
	/// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`. The encoder must apply a
	/// BT.709→BT.2020 gamut mapping and PQ OETF before YUV conversion (it is
	/// *not* a passthrough), so it is tracked separately from `Bt2020Pq`.
	ScrgbLinear,
}

/// Static HDR metadata (HDR10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HdrMetadata {
	/// Mastering display color primaries (CIE 1931 xy, in 0.00002 units).
	pub display_primaries: [(u16, u16); 3],
	/// White point (CIE 1931 xy, in 0.00002 units).
	pub white_point: (u16, u16),
	/// Maximum luminance in 0.0001 cd/m² units.
	pub max_luminance: u32,
	/// Minimum luminance in 0.0001 cd/m² units.
	pub min_luminance: u32,
	/// Maximum content light level in cd/m² (nits).
	pub max_cll: u16,
	/// Maximum frame-average light level in cd/m² (nits).
	pub max_fall: u16,
}

/// HDR mode state sent from the video pipeline to the control stream.
///
/// Combines the `enabled` flag (whether the client should be in HDR mode)
/// with optional HDR metadata. The `enabled` flag toggles based on actual
/// frame content — SDR frames set it to false, HDR frames set it to true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HdrModeState {
	/// Whether the client's display should be in HDR mode.
	pub enabled: bool,
	/// HDR10 static metadata from the composited content.
	pub metadata: Option<HdrMetadata>,
}

impl HdrModeState {
	/// Create an initial HDR mode state with the given enabled flag.
	/// Metadata is `None` until the video pipeline provides it.
	pub fn new(enabled: bool) -> Self {
		Self {
			enabled,
			metadata: None,
		}
	}
}

impl HdrMetadata {
	/// Reasonable fallback metadata for HDR10 when applications don't provide
	/// their own. Uses BT.2020 primaries, D65 white point, and a conservative
	/// 1000 nit peak luminance.
	pub fn fallback() -> Self {
		Self {
			// BT.2020 display primaries in 0.00002 units.
			display_primaries: [
				(35400, 14600), // Red:   0.708, 0.292
				(8500, 39850),  // Green: 0.170, 0.797
				(6550, 2300),   // Blue:  0.131, 0.046
			],
			// D65 white point in 0.00002 units.
			white_point: (15635, 16450), // 0.3127, 0.3290
			// 1000 nits max luminance in 0.0001 cd/m².
			max_luminance: 10_000_000,
			// 0.001 nits min luminance in 0.0001 cd/m².
			min_luminance: 10,
			// Unknown content light levels.
			max_cll: 0,
			max_fall: 0,
		}
	}
}

/// Byte order of an overlay image's 8-bit premultiplied-alpha texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayFormat {
	/// R, G, B, A bytes (xcursor images).
	Rgba8,
	/// B, G, R, A bytes (little-endian `wl_shm` ARGB8888/XRGB8888).
	Bgra8,
}

/// Immutable overlay pixels, shared by every frame until the content changes.
#[derive(Debug)]
pub(crate) struct OverlayImage {
	/// Changes whenever the pixels change; encoders upload once per generation.
	pub generation: u64,
	pub width: u32,
	pub height: u32,
	pub format: OverlayFormat,
	/// Tightly packed premultiplied texels; opaque formats carry alpha 255.
	pub pixels: Box<[u8]>,
}

/// Texels of one late-composition layer.
#[derive(Debug, Clone)]
pub(crate) enum OverlayContent {
	/// CPU texels shared until they change (cursors, `wl_shm` surfaces).
	Pixels(Arc<OverlayImage>),
	/// A single-plane 8-bit client DMA-BUF read in place by the encoder. The
	/// frame's source hold keeps the client from reusing it until `consumed`.
	Dmabuf {
		dmabuf: Dmabuf,
		/// Ignore the alpha channel (X formats).
		opaque: bool,
	},
}

/// A small element the encoder composites over a directly exported frame
/// ("late composition"), instead of the compositor rendering the whole scene.
///
/// Encoded output must match what GLES composition would produce: texels are
/// premultiplied, scaled by `opacity` (the GLES element alpha) and blended
/// 1:1 source-over at `(x, y)` in output pixels, in the frame's own encoding,
/// before any color conversion.
#[derive(Debug, Clone)]
pub(crate) struct FrameOverlay {
	pub content: OverlayContent,
	pub x: i32,
	pub y: i32,
	pub opacity: f32,
}

impl FrameOverlay {
	pub(crate) fn width(&self) -> u32 {
		match &self.content {
			OverlayContent::Pixels(image) => image.width,
			OverlayContent::Dmabuf { dmabuf, .. } => dmabuf.width(),
		}
	}
	pub(crate) fn height(&self) -> u32 {
		match &self.content {
			OverlayContent::Pixels(image) => image.height,
			OverlayContent::Dmabuf { dmabuf, .. } => dmabuf.height(),
		}
	}
}

/// Late-composition layers per frame: a Steam notification, then the cursor.
pub(crate) const MAX_OVERLAYS: usize = 2;

/// Layers bottom to top; `None` entries are skipped.
pub(crate) type FrameOverlays = [Option<FrameOverlay>; MAX_OVERLAYS];

/// Where a `source`-sized image lands when scaled onto a `frame`: the largest
/// centered rectangle with the source's aspect ratio (`x, y, width, height`
/// in frame pixels), the rest of the frame black. Equal aspect ratios fill it.
pub(crate) fn fit_viewport(source: (u32, u32), frame: (u32, u32)) -> (u32, u32, u32, u32) {
	let (sw, sh) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
	let (fw, fh) = (u64::from(frame.0), u64::from(frame.1));
	// Width-limited when source is wider than the frame (sw/sh >= fw/fh).
	let (w, h) = if sw * fh >= sh * fw {
		(fw, ((sh * fw + sw / 2) / sw).clamp(1, fh))
	} else {
		(((sw * fh + sh / 2) / sh).clamp(1, fw), fh)
	};
	(((fw - w) / 2) as u32, ((fh - h) / 2) as u32, w as u32, h as u32)
}

/// Strong ownership of a frame's source DMA-BUF and its plane descriptors.
#[derive(Clone, Debug)]
pub(crate) struct SourceLease(
	// Held, not read: dropping the last lease closes the plane descriptors.
	#[allow(dead_code)] Option<Dmabuf>,
);

impl SourceLease {
	#[cfg(test)]
	pub(crate) fn dmabuf(&self) -> Option<&Dmabuf> {
		self.0.as_ref()
	}
}

/// How a captured frame was produced. Diagnostics only: the encoders handle
/// every path the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CapturePath {
	/// The application's own DMA-BUF, exported without composition.
	Direct,
	/// Direct export with the cursor or a Steam notification drawn by the
	/// encoder's conversion (late composition).
	DirectLate,
	/// The scene composited with GLES into a capture buffer.
	#[default]
	Composited,
}

/// A frame's capture path and, when it was composited, the first condition
/// that ruled out direct export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureOrigin {
	pub path: CapturePath,
	/// Isolated-compositor diagnostics; other backends report `None`.
	pub composition_reason: Option<crate::session::compositor::DirectReject>,
}

/// A captured frame handed to the video pipeline for encoding.
#[derive(Debug)]
pub(crate) struct ExportedFrame {
	/// Owns the plane descriptors below; see the module documentation.
	#[cfg_attr(not(test), allow(dead_code))]
	source: SourceLease,
	/// Per-plane DMA-BUF metadata. Descriptors are owned by `source`.
	planes: Vec<ExportedPlane>,
	/// Admission remains occupied until downstream is ready for another frame.
	pub capture_credit: Option<super::admission::CaptureCredit>,
	/// DRM format (e.g. Argb8888, Abgr2101010).
	pub format: u32,
	/// DRM modifier (e.g. Linear, tiled).
	pub modifier: u64,
	/// Frame width in pixels.
	pub width: u32,
	/// Frame height in pixels.
	pub height: u32,
	/// When the backend finished producing the frame (render completion for
	/// composed frames, export for direct ones).
	pub created_at: Instant,
	/// When the frame's content became current: the sampled refresh deadline
	/// under fixed pacing, or the latch time of the newest content under VRR
	/// pacing. The RTP timestamp is derived from it, so a client can recover
	/// the source cadence; it is never later than `created_at`.
	pub source_time: Instant,
	/// Captured with VRR (presentation-driven) pacing; diagnostics only.
	pub vrr_capture: bool,
	/// GLES preparation/submission start, retained across the fence wait.
	/// Direct exports have no compositor render and use `created_at` for pacing.
	pub composition_started_at: Option<Instant>,
	/// The backend's index of the source buffer (pool slot or client
	/// buffer); diagnostics only.
	pub buffer_index: usize,
	/// Shared flag set to `true` once no GPU work can still read the source,
	/// signalling the backend that this buffer's content may be reused.
	pub consumed: Arc<AtomicBool>,
	/// Color space of the rendered frame.
	pub color_space: FrameColorSpace,
	/// Optional HDR metadata from the composited content.
	pub hdr_metadata: Option<HdrMetadata>,
	/// Layers to composite at encode time, bottom to top; all `None` when the
	/// frame is complete.
	pub overlays: FrameOverlays,
	/// How the frame was produced; diagnostics only.
	pub origin: CaptureOrigin,
	/// Scale the whole frame onto the negotiated stream extent when the two
	/// differ (a desktop output streamed at another resolution). Otherwise a
	/// frame is sampled 1:1 and one that raced an extent change is
	/// edge-extended. Encoders with their own scaler (PyroWave) always scale.
	pub scale_to_stream: bool,
}

/// Layout of a single DMA-BUF plane; its descriptor is owned by the frame.
#[derive(Debug, Clone, Copy)]
struct ExportedPlane {
	fd: RawFd,
	offset: u32,
	stride: u32,
}

/// A plane of a live frame. The descriptor is borrowed from the frame's lease.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourcePlane<'frame> {
	pub fd: BorrowedFd<'frame>,
	/// Byte offset into the DMA-BUF for this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
}

impl ExportedFrame {
	/// Export `dmabuf` for the encoder, taking a strong reference to it.
	pub(crate) fn from_dmabuf(
		dmabuf: &Dmabuf,
		buffer_index: usize,
		consumed: Arc<AtomicBool>,
		color_space: FrameColorSpace,
		hdr_metadata: Option<HdrMetadata>,
	) -> Self {
		let planes = dmabuf
			.handles()
			.zip(dmabuf.offsets())
			.zip(dmabuf.strides())
			.map(|((handle, offset), stride)| ExportedPlane {
				fd: handle.as_fd().as_raw_fd(),
				offset,
				stride,
			})
			.collect();
		let created_at = Instant::now();
		Self {
			source: SourceLease(Some(dmabuf.clone())),
			planes,
			capture_credit: None,
			format: dmabuf.format().code as u32,
			modifier: Into::<u64>::into(dmabuf.format().modifier),
			width: dmabuf.width(),
			height: dmabuf.height(),
			created_at,
			source_time: created_at,
			vrr_capture: false,
			composition_started_at: None,
			buffer_index,
			consumed,
			color_space,
			hdr_metadata,
			overlays: Default::default(),
			origin: CaptureOrigin::default(),
			scale_to_stream: false,
		}
	}

	/// Layers to composite, bottom to top.
	pub(crate) fn overlays(&self) -> impl Iterator<Item = &FrameOverlay> + '_ {
		self.overlays.iter().flatten()
	}

	pub(crate) fn has_overlays(&self) -> bool {
		self.overlays.iter().any(Option::is_some)
	}

	/// The frame's planes, with descriptors borrowed from its source lease.
	pub(crate) fn planes(&self) -> impl ExactSizeIterator<Item = SourcePlane<'_>> + '_ {
		self.planes.iter().map(|plane| SourcePlane {
			// SAFETY: `plane.fd` is a descriptor of `self.source`, which owns it
			// (Arc<OwnedFd>) and is neither mutated nor dropped while `self` is
			// borrowed, so it stays open for the returned lifetime.
			fd: unsafe { BorrowedFd::borrow_raw(plane.fd) },
			offset: plane.offset,
			stride: plane.stride,
		})
	}

	/// An additional owner of the source buffer. Production consumers do not
	/// need one: imports own their own references (duplicated descriptors and
	/// imported Vulkan memory) before the frame is dropped.
	#[cfg(test)]
	pub(crate) fn source_lease(&self) -> SourceLease {
		self.source.clone()
	}

	/// Charge composition to the same frame pacing window as encoding.
	/// Keep created_at's existing completion-based latency semantics separate.
	pub(crate) fn pacing_origin(&self) -> Instant {
		self.composition_started_at.unwrap_or(self.created_at)
	}

	/// A frame without a source buffer, for admission/pipeline tests.
	#[cfg(test)]
	pub(crate) fn for_test() -> Self {
		let created_at = Instant::now();
		Self {
			source: SourceLease(None),
			planes: Vec::new(),
			capture_credit: None,
			format: 0,
			modifier: 0,
			width: 1920,
			height: 1080,
			created_at,
			source_time: created_at,
			vrr_capture: false,
			composition_started_at: None,
			buffer_index: 0,
			consumed: Arc::new(AtomicBool::new(false)),
			color_space: FrameColorSpace::Srgb,
			hdr_metadata: None,
			overlays: Default::default(),
			origin: CaptureOrigin::default(),
			scale_to_stream: false,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::fd::OwnedFd;
	use std::sync::atomic::Ordering;

	use smithay::backend::allocator::dmabuf::DmabufFlags;
	use smithay::backend::allocator::{Fourcc, Modifier};

	/// Identity of the file behind `fd`, independent of its descriptor number.
	fn identity(fd: BorrowedFd<'_>) -> (u64, u64) {
		let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
		// SAFETY: fd is open for the borrow and `stat` is a valid out pointer.
		assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) }, 0);
		// SAFETY: fstat succeeded and initialized the structure.
		let stat = unsafe { stat.assume_init() };
		(stat.st_dev, stat.st_ino)
	}

	/// A single-plane Smithay DMA-BUF backed by an anonymous file. Ownership
	/// semantics do not depend on the exporter, so no GPU is needed.
	fn source(name: &str) -> Dmabuf {
		let file: OwnedFd = tempfile::Builder::new()
			.prefix(name)
			.tempfile()
			.unwrap()
			.into_file()
			.into();
		let mut builder = Dmabuf::builder((64, 64), Fourcc::Xrgb8888, Modifier::Linear, DmabufFlags::empty());
		assert!(builder.add_plane(file, 0, 256));
		builder.build().unwrap()
	}

	#[test]
	fn high_precision_buffers_without_a_color_declaration_remain_sdr() {
		for format in [Fourcc::Abgr2101010, Fourcc::Abgr16161616f] {
			let fd: OwnedFd = tempfile::tempfile().unwrap().into();
			let mut builder = Dmabuf::builder((64, 64), format, Modifier::Linear, DmabufFlags::empty());
			assert!(builder.add_plane(fd, 0, 512));
			let dmabuf = builder.build().unwrap();
			let frame = ExportedFrame::from_dmabuf(
				&dmabuf,
				0,
				Arc::new(AtomicBool::new(false)),
				FrameColorSpace::default(),
				None,
			);
			assert_eq!(frame.color_space, FrameColorSpace::Srgb);
			assert!(frame.hdr_metadata.is_none());
		}
	}

	/// STAB-004: the compositor dropping its pool (shutdown, resolution
	/// retirement or a released client buffer) before a frame is imported does
	/// not close the frame's descriptors, and their numbers are not reused for
	/// other objects while the frame exists, even under descriptor churn.
	#[test]
	fn frame_owns_its_source_descriptors_through_owner_teardown_and_fd_reuse() {
		let pool_buffer = source("pool");
		let expected = identity(pool_buffer.handles().next().unwrap());
		let frame = ExportedFrame::from_dmabuf(
			&pool_buffer,
			0,
			Arc::new(AtomicBool::new(false)),
			FrameColorSpace::Srgb,
			None,
		);
		let raw = frame.planes().next().unwrap().fd.as_raw_fd();
		// The compositor's last reference goes away (thread exit / pool retire).
		drop(pool_buffer);
		// Churn descriptors: a closed number would be handed out again here.
		let churn: Vec<_> = (0..256).map(|i| source(&format!("churn{i}"))).collect();
		assert!(
			churn
				.iter()
				.all(|buffer| buffer.handles().all(|fd| fd.as_raw_fd() != raw)),
			"a live frame's descriptor number was reused"
		);
		let plane = frame.planes().next().unwrap();
		assert_eq!(plane.fd.as_raw_fd(), raw);
		assert_eq!(
			identity(plane.fd),
			expected,
			"frame must still reference its own buffer"
		);
		assert_eq!((plane.offset, plane.stride), (0, 256));
		// Ownership is not content release: the compositor slot stays busy.
		assert!(!frame.consumed.load(Ordering::Acquire));
		// A consumer that outlives the frame value keeps the source alive too.
		let lease = frame.source_lease();
		drop(frame);
		let lease_fd = lease.dmabuf().unwrap().handles().next().unwrap();
		assert_eq!(identity(lease_fd), expected);
	}

	/// STAB-004: a frame queued for the encoder survives the compositor side
	/// of the capture channel being torn down before import.
	#[test]
	fn queued_frame_survives_compositor_exit_before_import() {
		let (tx, rx) = super::super::admission::capture_channel();
		let buffer = source("queued");
		let expected = identity(buffer.handles().next().unwrap());
		// Grant demand as the encoder would, then publish one frame.
		rx.request_for_test();
		let credit = tx.try_acquire().unwrap();
		tx.try_send(
			ExportedFrame::from_dmabuf(
				&buffer,
				0,
				Arc::new(AtomicBool::new(false)),
				FrameColorSpace::Srgb,
				None,
			),
			credit,
		)
		.unwrap();
		drop(tx);
		drop(buffer);
		let frame = rx.recv_timeout(std::time::Duration::ZERO).unwrap();
		assert_eq!(identity(frame.planes().next().unwrap().fd), expected);
	}

	#[test]
	fn scaled_sources_keep_their_aspect_ratio() {
		assert_eq!(fit_viewport((3840, 2160), (1920, 1080)), (0, 0, 1920, 1080));
		// Three side-by-side monitors in a 16:9 stream: letterboxed.
		assert_eq!(fit_viewport((8560, 1440), (2560, 1440)), (0, 504, 2560, 431));
		// 16:10 in 16:9: pillarboxed.
		assert_eq!(fit_viewport((2560, 1600), (1920, 1080)), (96, 0, 1728, 1080));
		assert_eq!(fit_viewport((640, 160), (320, 180)), (0, 50, 320, 80));
	}

	#[test]
	fn fallback_mastering_gamut_is_bt2020() {
		let m = HdrMetadata::fallback();
		assert_eq!(m.display_primaries, [(35400, 14600), (8500, 39850), (6550, 2300)]);
		assert_eq!(m.white_point, (15635, 16450));
		assert_eq!(m.max_luminance, 1000 * 10000);
	}
}
