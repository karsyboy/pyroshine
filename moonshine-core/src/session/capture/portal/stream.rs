//! PipeWire consumer of the portal's screen-cast stream.
//!
//! The stream negotiates DMA-BUFs only, in the formats and modifiers the
//! capture GPU imports (`gpu::ImportableModifiers`); a compositor that can
//! share only memory copies fails the negotiation, which is reported instead
//! of streaming through a CPU copy.
//!
//! Buffers follow the frame contract (`capture/frame.rs`):
//!
//! - **Newest only.** Every `process` callback drains the queue and keeps the
//!   newest buffer as the pending frame; older ones go straight back to the
//!   compositor. A capture stall therefore never builds a backlog.
//! - **Demand driven.** The pending frame is sent only with a capture credit;
//!   without demand it waits, replaced by any newer frame.
//! - **Held until consumed.** A sent buffer is returned to the compositor
//!   only after the pipeline set its `consumed` flag, so the compositor never
//!   renders into a buffer the encoder may still read. The frame's lease owns
//!   duplicated plane descriptors, independent of the PipeWire buffer.
//! - **Producer complete.** Before sending, the buffer's implicit write fence
//!   is waited for (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`), so consumers read
//!   without waiting for the producer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::sys as spa_sys;
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use crate::session::capture::admission::{CaptureSendError, CaptureSender};
use crate::session::capture::frame::{ExportedFrame, FrameColorSpace};

/// SPA video formats the stream offers, best first, with their DRM fourcc.
/// Little-endian SPA names list bytes in memory order; DRM names list the
/// channels of a little-endian word from its most significant bits.
const FORMATS: [(u32, u32); 8] = [
	(spa_sys::SPA_VIDEO_FORMAT_BGRx, 0x3432_5258),       // XRGB8888
	(spa_sys::SPA_VIDEO_FORMAT_RGBx, 0x3432_4258),       // XBGR8888
	(spa_sys::SPA_VIDEO_FORMAT_BGRA, 0x3432_5241),       // ARGB8888
	(spa_sys::SPA_VIDEO_FORMAT_RGBA, 0x3432_4241),       // ABGR8888
	(spa_sys::SPA_VIDEO_FORMAT_xRGB_210LE, 0x3033_5258), // XRGB2101010
	(spa_sys::SPA_VIDEO_FORMAT_xBGR_210LE, 0x3033_4258), // XBGR2101010
	(spa_sys::SPA_VIDEO_FORMAT_ARGB_210LE, 0x3033_5241), // ARGB2101010
	(spa_sys::SPA_VIDEO_FORMAT_ABGR_210LE, 0x3033_4241), // ABGR2101010
];

/// `SPA_POD_PROP_FLAG_DONT_FIXATE` (PipeWire 0.3.33). The flag constant is
/// gated behind a libspa version feature; its value is protocol.
const DONT_FIXATE: PropertyFlags = PropertyFlags::from_bits_retain(spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE);

/// How long a buffer's producer fence may stay unsignaled before the frame
/// is skipped. Compositors queue buffers after rendering, so this is
/// normally already signaled.
const PRODUCER_FENCE_TIMEOUT: Duration = Duration::from_millis(50);

/// A format the stream offers: SPA format, DRM fourcc and importable modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OfferedFormat {
	pub spa_format: u32,
	pub fourcc: u32,
	pub modifiers: Vec<u64>,
}

/// The formats to offer for a GPU importing `importable` (fourcc, modifier)
/// pairs; formats without an importable modifier are left out.
pub(crate) fn offered_formats(importable: impl Fn(u32) -> Vec<u64>) -> Vec<OfferedFormat> {
	FORMATS
		.iter()
		.filter_map(|&(spa_format, fourcc)| {
			let modifiers = importable(fourcc);
			(!modifiers.is_empty()).then_some(OfferedFormat {
				spa_format,
				fourcc,
				modifiers,
			})
		})
		.collect()
}

/// Color declaration of the negotiated format. Unset fields mean sRGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeclaredColor {
	pub transfer: u32,
	pub primaries: u32,
}

impl DeclaredColor {
	/// The frame color space this declaration describes, or `None` when the
	/// source declares an encoding the pipeline cannot read correctly.
	pub(crate) fn color_space(self) -> Option<FrameColorSpace> {
		match (self.transfer, self.primaries) {
			(
				0 | spa_sys::SPA_VIDEO_TRANSFER_SRGB | spa_sys::SPA_VIDEO_TRANSFER_BT709,
				0 | spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT709,
			) => Some(FrameColorSpace::Srgb),
			(spa_sys::SPA_VIDEO_TRANSFER_SMPTE2084, spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020) => {
				Some(FrameColorSpace::Bt2020Pq)
			},
			_ => None,
		}
	}
}

/// The negotiated stream format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamFormat {
	pub fourcc: u32,
	pub modifier: u64,
	pub width: u32,
	pub height: u32,
	/// Frame rate the producer declared, `None` when variable (0/1).
	pub framerate: Option<Fraction>,
	pub color: DeclaredColor,
}

/// What the stream reports to its owner, polled after each loop iteration.
#[derive(Debug, Default)]
pub(crate) struct StreamStatus {
	pub format: Option<StreamFormat>,
	/// A fatal stream failure, for the operator.
	pub failure: Option<String>,
	/// Frames sent to the pipeline and frames dropped for a reason other than
	/// a newer frame replacing them.
	pub sent: u64,
	pub skipped: u64,
	/// When the newest frame arrived.
	pub last_frame: Option<Instant>,
	/// `process` callbacks and buffers received, for diagnostics.
	pub processed: u64,
	pub received: u64,
	/// Buffers currently held: pending and sent but not consumed.
	pub held: usize,
}

/// A PipeWire buffer the stream owns until it is queued back.
struct Held {
	buffer: *mut pw::sys::pw_buffer,
	/// Set by the pipeline once no GPU work can read the buffer.
	consumed: Arc<AtomicBool>,
}

/// The buffer's DMA-BUF, built once and rebuilt only when its layout changes.
struct CachedDmabuf {
	layout: (i64, u32, i32),
	dmabuf: Dmabuf,
}

struct Inner {
	sender: CaptureSender,
	offered: Vec<OfferedFormat>,
	/// Frame rate requested from the producer.
	rate: u32,
	status: StreamStatus,
	/// Newest received frame not sent yet.
	pending: Option<(*mut pw::sys::pw_buffer, Option<i64>)>,
	in_flight: Vec<Held>,
	dmabufs: HashMap<usize, CachedDmabuf>,
	next_index: usize,
	crop_warned: bool,
}

/// A connected screen-cast stream. Lives on the capture thread with its
/// PipeWire loop; dropping it disconnects the stream and returns every buffer.
pub(crate) struct CaptureStream {
	inner: Rc<RefCell<Inner>>,
	_listener: pw::stream::StreamListener<()>,
	stream: pw::stream::StreamRc,
}

fn serialize(value: Value) -> Vec<u8> {
	pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
		.expect("serializing a well-formed pod")
		.0
		.into_inner()
}

/// One `EnumFormat` object: `format` with `modifiers` (a choice the producer
/// fixates, or a single fixated value) at any size and frame rate.
fn format_pod(format: &OfferedFormat, modifiers: &[u64], fixated: bool, rate: u32) -> Vec<u8> {
	let modifier_value = if fixated || modifiers.len() == 1 {
		Value::Long(modifiers[0] as i64)
	} else {
		Value::Choice(ChoiceValue::Long(Choice(
			ChoiceFlags::empty(),
			ChoiceEnum::Enum {
				default: modifiers[0] as i64,
				alternatives: modifiers.iter().map(|&m| m as i64).collect(),
			},
		)))
	};
	let modifier_flags = if fixated {
		PropertyFlags::MANDATORY
	} else {
		PropertyFlags::MANDATORY.union(DONT_FIXATE)
	};
	serialize(Value::Object(Object {
		type_: spa_sys::SPA_TYPE_OBJECT_Format,
		id: spa_sys::SPA_PARAM_EnumFormat,
		properties: vec![
			Property::new(
				spa_sys::SPA_FORMAT_mediaType,
				Value::Id(Id(spa_sys::SPA_MEDIA_TYPE_video)),
			),
			Property::new(
				spa_sys::SPA_FORMAT_mediaSubtype,
				Value::Id(Id(spa_sys::SPA_MEDIA_SUBTYPE_raw)),
			),
			Property::new(spa_sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(format.spa_format))),
			Property {
				key: spa_sys::SPA_FORMAT_VIDEO_modifier,
				flags: modifier_flags,
				value: modifier_value,
			},
			Property::new(
				spa_sys::SPA_FORMAT_VIDEO_size,
				Value::Choice(ChoiceValue::Rectangle(Choice(
					ChoiceFlags::empty(),
					ChoiceEnum::Range {
						default: Rectangle {
							width: 1920,
							height: 1080,
						},
						min: Rectangle { width: 1, height: 1 },
						max: Rectangle {
							width: 16384,
							height: 16384,
						},
					},
				))),
			),
			// Producers deliver frames as content changes, at most at the
			// (maximum) frame rate; 0/1 means variable. Compositors throttle to
			// the default, so it is the stream's rate.
			Property::new(
				spa_sys::SPA_FORMAT_VIDEO_framerate,
				Value::Choice(ChoiceValue::Fraction(Choice(
					ChoiceFlags::empty(),
					ChoiceEnum::Range {
						default: Fraction { num: rate, denom: 1 },
						min: Fraction { num: 0, denom: 1 },
						max: Fraction { num: 1000, denom: 1 },
					},
				))),
			),
			Property::new(
				spa_sys::SPA_FORMAT_VIDEO_maxFramerate,
				Value::Choice(ChoiceValue::Fraction(Choice(
					ChoiceFlags::empty(),
					ChoiceEnum::Range {
						default: Fraction { num: rate, denom: 1 },
						min: Fraction { num: 1, denom: 1 },
						max: Fraction { num: 1000, denom: 1 },
					},
				))),
			),
		],
	}))
}

/// Buffer and metadata requirements once a format is fixed: DMA-BUF only,
/// with the presentation header and a crop region (to refuse cropped frames).
fn buffer_params() -> Vec<Vec<u8>> {
	let meta = |kind: u32, size: usize| {
		serialize(Value::Object(Object {
			type_: spa_sys::SPA_TYPE_OBJECT_ParamMeta,
			id: spa_sys::SPA_PARAM_Meta,
			properties: vec![
				Property::new(spa_sys::SPA_PARAM_META_type, Value::Id(Id(kind))),
				Property::new(spa_sys::SPA_PARAM_META_size, Value::Int(size as i32)),
			],
		}))
	};
	vec![
		serialize(Value::Object(Object {
			type_: spa_sys::SPA_TYPE_OBJECT_ParamBuffers,
			id: spa_sys::SPA_PARAM_Buffers,
			properties: vec![
				Property::new(
					spa_sys::SPA_PARAM_BUFFERS_buffers,
					Value::Choice(ChoiceValue::Int(Choice(
						ChoiceFlags::empty(),
						ChoiceEnum::Range {
							default: 4,
							min: 3,
							max: 16,
						},
					))),
				),
				Property::new(
					spa_sys::SPA_PARAM_BUFFERS_dataType,
					Value::Int(1 << spa_sys::SPA_DATA_DmaBuf),
				),
				// One plane and no sync-object blocks: implicit synchronization.
				// Producers that offer explicit sync (KWin's SyncTimeline layout,
				// two extra syncobj blocks) then wait for the consumer to signal
				// each buffer's release point, which this stream does not do.
				Property::new(spa_sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
			],
		})),
		meta(spa_sys::SPA_META_Header, size_of::<spa_sys::spa_meta_header>()),
		meta(spa_sys::SPA_META_VideoCrop, size_of::<spa_sys::spa_meta_region>()),
	]
}

/// The modifier property of a format pod: its value(s) and whether the
/// producer left it for the consumer to fixate.
fn modifier_choice(param: &Pod) -> Option<(Vec<u64>, bool)> {
	let (_, value) = pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?;
	let Value::Object(object) = value else {
		return None;
	};
	let property = object
		.properties
		.into_iter()
		.find(|property| property.key == spa_sys::SPA_FORMAT_VIDEO_modifier)?;
	match property.value {
		Value::Long(modifier) => Some((vec![modifier as u64], false)),
		Value::Choice(ChoiceValue::Long(Choice(_, ChoiceEnum::Enum { default, alternatives }))) => {
			let mut modifiers = vec![default as u64];
			modifiers.extend(
				alternatives
					.into_iter()
					.map(|m| m as u64)
					.filter(|m| *m != default as u64),
			);
			Some((modifiers, property.flags.contains(DONT_FIXATE)))
		},
		Value::Choice(ChoiceValue::Long(Choice(_, ChoiceEnum::None(modifier)))) => Some((vec![modifier as u64], false)),
		_ => None,
	}
}

/// Duplicate a plane descriptor for the frame's own lease.
fn duplicate(fd: i64) -> Result<OwnedFd, String> {
	let fd = i32::try_from(fd).map_err(|_| format!("invalid DMA-BUF descriptor {fd}"))?;
	// SAFETY: fcntl only reads the descriptor number; the result is owned.
	let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
	if duplicated < 0 {
		return Err(format!(
			"cannot duplicate a DMA-BUF descriptor: {}",
			std::io::Error::last_os_error()
		));
	}
	// SAFETY: a fresh descriptor this process owns.
	Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

/// Wait until every producer write to `fd` completed. `Ok(false)` on timeout.
/// Kernels without `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (before 6.0) cannot
/// report it; the compositor's queueing order is then relied on.
fn wait_producer(fd: BorrowedFd<'_>, timeout: Duration) -> Result<bool, String> {
	#[repr(C)]
	struct ExportSyncFile {
		flags: u32,
		fd: i32,
	}
	// _IOWR('b', 2, struct dma_buf_export_sync_file)
	const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
	const DMA_BUF_SYNC_READ: u32 = 1;
	let mut request = ExportSyncFile {
		flags: DMA_BUF_SYNC_READ,
		fd: -1,
	};
	// SAFETY: the request structure matches the kernel ABI and outlives the call.
	if unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut request) } < 0 {
		let error = std::io::Error::last_os_error();
		return match error.raw_os_error() {
			Some(libc::ENOTTY | libc::EINVAL) => Ok(true),
			_ => Err(format!("cannot read the DMA-BUF fence: {error}")),
		};
	}
	// SAFETY: the kernel returned a new sync_file descriptor we own.
	let sync_file = unsafe { OwnedFd::from_raw_fd(request.fd) };
	let mut poll = libc::pollfd {
		fd: sync_file.as_raw_fd(),
		events: libc::POLLIN,
		revents: 0,
	};
	// SAFETY: one valid pollfd.
	let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
	Ok(ready > 0)
}

/// Content time of a buffer presented at `pts` (CLOCK_MONOTONIC ns), never
/// later than `now`.
pub(crate) fn source_time(pts: Option<i64>, now: Instant) -> Instant {
	let Some(pts) = pts.filter(|pts| *pts > 0) else {
		return now;
	};
	let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
	// SAFETY: valid out pointer.
	unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
	let monotonic = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
	let age = monotonic.saturating_sub(pts);
	// A timestamp from another clock or the future is not usable.
	if !(0..1_000_000_000).contains(&age) {
		return now;
	}
	now.checked_sub(Duration::from_nanos(age as u64)).unwrap_or(now)
}

impl Inner {
	/// Queue back every sent buffer the pipeline has finished with.
	fn sweep(&mut self, stream: &pw::stream::Stream) {
		self.in_flight.retain(|held| {
			if held.consumed.load(Ordering::Acquire) {
				// SAFETY: the buffer was dequeued from this stream and not queued since.
				unsafe { stream.queue_raw_buffer(held.buffer) };
				false
			} else {
				true
			}
		});
	}

	fn requeue(stream: &pw::stream::Stream, buffer: *mut pw::sys::pw_buffer) {
		// SAFETY: as in `sweep`.
		unsafe { stream.queue_raw_buffer(buffer) };
	}

	/// A usable frame's presentation time, or why the buffer is skipped.
	/// `Ok(None)` for buffers that carry no new picture.
	fn inspect(&mut self, buffer: *mut pw::sys::pw_buffer) -> Result<Option<Option<i64>>, String> {
		// SAFETY: a dequeued buffer stays valid until it is queued back.
		let spa_buffer = unsafe { &*(*buffer).buffer };
		if spa_buffer.n_datas == 0 {
			return Ok(None);
		}
		// SAFETY: n_datas entries.
		let data = unsafe { &*spa_buffer.datas };
		if data.type_ != spa_sys::SPA_DATA_DmaBuf {
			return Err(format!(
				"the compositor shared a non-DMA-BUF buffer (type {})",
				data.type_
			));
		}
		// SAFETY: a negotiated buffer has a chunk per data.
		let chunk = unsafe { &*data.chunk };
		if chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
			return Ok(None);
		}
		let mut pts = None;
		for index in 0..spa_buffer.n_metas as usize {
			// SAFETY: n_metas entries, each with `size` bytes of data.
			let meta = unsafe { &*spa_buffer.metas.add(index) };
			if meta.data.is_null() {
				continue;
			}
			match meta.type_ {
				spa_sys::SPA_META_Header if meta.size as usize >= size_of::<spa_sys::spa_meta_header>() => {
					// SAFETY: size checked.
					let header = unsafe { &*(meta.data as *const spa_sys::spa_meta_header) };
					if header.flags & spa_sys::SPA_META_HEADER_FLAG_CORRUPTED != 0 {
						return Ok(None);
					}
					pts = Some(header.pts);
				},
				spa_sys::SPA_META_VideoCrop if meta.size as usize >= size_of::<spa_sys::spa_meta_region>() => {
					// SAFETY: size checked.
					let crop = unsafe { &*(meta.data as *const spa_sys::spa_meta_region) };
					let format = self.status.format.expect("buffers follow a format");
					let region = crop.region;
					let full = region.size.width == 0
						|| (region.position.x == 0
							&& region.position.y == 0
							&& region.size.width == format.width
							&& region.size.height == format.height);
					if !full {
						if !self.crop_warned {
							tracing::warn!(?region.position.x, region.size.width, region.size.height, "The compositor sent a cropped frame; Desktop Mode streams whole outputs only");
							self.crop_warned = true;
						}
						return Err("cropped frame".into());
					}
				},
				_ => {},
			}
		}
		Ok(Some(pts))
	}

	/// The DMA-BUF of `buffer` for the negotiated format.
	fn dmabuf(&mut self, buffer: *mut pw::sys::pw_buffer) -> Result<(usize, Dmabuf), String> {
		let format = self.status.format.ok_or("no negotiated format")?;
		// SAFETY: as in `inspect`.
		let spa_buffer = unsafe { &*(*buffer).buffer };
		// SAFETY: as in `inspect`; single-plane modifiers only are negotiated.
		let data = unsafe { &*spa_buffer.datas };
		let chunk = unsafe { &*data.chunk };
		let layout = (data.fd, chunk.offset, chunk.stride);
		let key = buffer as usize;
		if let Some(cached) = self.dmabufs.get(&key).filter(|cached| cached.layout == layout) {
			return Ok((key, cached.dmabuf.clone()));
		}
		let stride = u32::try_from(chunk.stride).map_err(|_| format!("invalid DMA-BUF stride {}", chunk.stride))?;
		let fourcc = Fourcc::try_from(format.fourcc).map_err(|_| format!("unknown fourcc {:#x}", format.fourcc))?;
		let mut builder = Dmabuf::builder(
			(format.width as i32, format.height as i32),
			fourcc,
			Modifier::from(format.modifier),
			DmabufFlags::empty(),
		);
		builder.add_plane(duplicate(data.fd)?, chunk.offset, stride);
		let dmabuf = builder.build().ok_or("incomplete DMA-BUF")?;
		self.dmabufs.insert(
			key,
			CachedDmabuf {
				layout,
				dmabuf: dmabuf.clone(),
			},
		);
		Ok((key, dmabuf))
	}

	/// Send the pending frame if the pipeline wants one.
	fn try_send(&mut self, stream: &pw::stream::Stream) {
		self.sweep(stream);
		if self.pending.is_none() {
			return;
		}
		let Some(credit) = self.sender.try_acquire() else {
			return;
		};
		let (buffer, pts) = self.pending.take().expect("checked above");
		let frame = match self.export(buffer, pts) {
			Ok(frame) => frame,
			Err(error) => {
				tracing::debug!(%error, "Skipping a desktop frame");
				self.status.skipped += 1;
				Self::requeue(stream, buffer);
				return;
			},
		};
		let consumed = frame.consumed.clone();
		match self.sender.try_send(frame, credit) {
			Ok(()) => {
				self.status.sent += 1;
				self.in_flight.push(Held { buffer, consumed });
			},
			Err(error) => {
				if error == CaptureSendError::Disconnected {
					self.status
						.failure
						.get_or_insert_with(|| "the video pipeline stopped".into());
				}
				Self::requeue(stream, buffer);
			},
		}
	}

	fn export(&mut self, buffer: *mut pw::sys::pw_buffer, pts: Option<i64>) -> Result<ExportedFrame, String> {
		let format = self.status.format.ok_or("no negotiated format")?;
		let color_space = format.color.color_space().ok_or("undeclared color encoding")?;
		let (key, dmabuf) = self.dmabuf(buffer)?;
		use smithay::backend::allocator::Buffer as _;
		let _ = dmabuf.size();
		let plane = dmabuf.handles().next().ok_or("DMA-BUF without planes")?;
		if !wait_producer(plane, PRODUCER_FENCE_TIMEOUT)? {
			return Err("the compositor's rendering did not complete in time".into());
		}
		let index = self
			.dmabufs
			.keys()
			.position(|candidate| *candidate == key)
			.unwrap_or(self.next_index);
		let now = Instant::now();
		let mut frame = ExportedFrame::from_dmabuf(&dmabuf, index, Arc::new(AtomicBool::new(false)), color_space, None);
		frame.source_time = source_time(pts, now);
		// The desktop's output keeps its own size; encoders scale it.
		frame.scale_to_stream = true;
		Ok(frame)
	}

	fn on_format(&mut self, stream: &pw::stream::Stream, param: &Pod) {
		let mut info = spa::param::video::VideoInfoRaw::new();
		if info.parse(param).is_err() {
			self.status.failure = Some("the compositor announced an unreadable video format".into());
			return;
		}
		let spa_format = info.format().as_raw();
		let Some(offered) = self
			.offered
			.iter()
			.find(|offered| offered.spa_format == spa_format)
			.cloned()
		else {
			self.status.failure = Some(format!(
				"the compositor chose an unsupported video format ({spa_format})"
			));
			return;
		};
		let Some((modifiers, needs_fixation)) = modifier_choice(param) else {
			self.status.failure = Some(
				"the compositor offered only shared-memory buffers; Desktop Mode needs DMA-BUF screen capture".into(),
			);
			return;
		};
		if needs_fixation {
			// The producer left the choice to us: fixate the best importable
			// modifier and wait for the renegotiated format.
			let Some(&modifier) = modifiers.iter().find(|m| offered.modifiers.contains(m)) else {
				self.status.failure = Some("no common DMA-BUF modifier with the compositor".into());
				return;
			};
			let pod = format_pod(&offered, &[modifier], true, self.rate);
			let mut params = [Pod::from_bytes(&pod).expect("serialized pod")];
			if let Err(error) = stream.update_params(&mut params) {
				self.status.failure = Some(format!("cannot fixate the DMA-BUF modifier: {error}"));
			}
			return;
		}
		let modifier = modifiers[0];
		if !offered.modifiers.contains(&modifier) {
			self.status.failure = Some(format!(
				"the compositor chose DMA-BUF modifier {modifier:#x}, which the capture GPU cannot import"
			));
			return;
		}
		let size = info.size();
		let framerate = info.framerate();
		let format = StreamFormat {
			fourcc: offered.fourcc,
			modifier,
			width: size.width,
			height: size.height,
			framerate: (framerate.num > 0).then_some(framerate),
			color: DeclaredColor {
				transfer: info.transfer_function(),
				primaries: info.color_primaries(),
			},
		};
		if format.color.color_space().is_none() {
			self.status.failure = Some(format!(
				"the compositor declared a color encoding Desktop Mode cannot stream (transfer {}, primaries {})",
				format.color.transfer, format.color.primaries
			));
			return;
		}
		tracing::info!(
			fourcc = format!("{:?}", Fourcc::try_from(format.fourcc).ok()),
			modifier = format!("{modifier:#x}"),
			width = format.width,
			height = format.height,
			framerate = ?format.framerate.map(|f| f.num as f64 / f.denom.max(1) as f64),
			max_framerate = format!("{}/{}", info.max_framerate().num, info.max_framerate().denom),
			"Desktop capture format negotiated"
		);
		// A renegotiation (output mode change) invalidates cached layouts.
		self.dmabufs.clear();
		self.status.format = Some(format);
		let params = buffer_params();
		let mut pods: Vec<&Pod> = params
			.iter()
			.map(|bytes| Pod::from_bytes(bytes).expect("serialized pod"))
			.collect();
		if let Err(error) = stream.update_params(&mut pods) {
			self.status.failure = Some(format!("cannot request DMA-BUF buffers: {error}"));
		}
	}

	fn on_process(&mut self, stream: &pw::stream::Stream) {
		self.status.processed += 1;
		let mut newest = None;
		loop {
			// SAFETY: plain dequeue; the buffer is owned until queued back.
			let buffer = unsafe { stream.dequeue_raw_buffer() };
			if buffer.is_null() {
				break;
			}
			self.status.received += 1;
			if let Some(older) = newest.replace(buffer) {
				Self::requeue(stream, older);
			}
		}
		let Some(buffer) = newest else {
			return;
		};
		match self.inspect(buffer) {
			Ok(Some(pts)) => {
				self.status.last_frame = Some(Instant::now());
				if let Some((older, _)) = self.pending.replace((buffer, pts)) {
					Self::requeue(stream, older);
				}
				self.try_send(stream);
			},
			Ok(None) => Self::requeue(stream, buffer),
			Err(error) => {
				self.status.skipped += 1;
				tracing::debug!(%error, "Skipping a desktop buffer");
				Self::requeue(stream, buffer);
			},
		}
	}
}

impl CaptureStream {
	/// Connect to `node_id` on `core` (the portal's PipeWire remote).
	pub(crate) fn connect(
		core: &pw::core::CoreRc,
		node_id: u32,
		offered: Vec<OfferedFormat>,
		rate: u32,
		sender: CaptureSender,
	) -> Result<Self, String> {
		if offered.is_empty() {
			return Err("the capture GPU imports no DMA-BUF format a screen cast can use".into());
		}
		let stream = pw::stream::StreamRc::new(
			core.clone(),
			"pyroshine-desktop",
			pw::properties::properties! {
				*pw::keys::MEDIA_TYPE => "Video",
				*pw::keys::MEDIA_CATEGORY => "Capture",
				*pw::keys::MEDIA_ROLE => "Screen",
			},
		)
		.map_err(|e| format!("cannot create the PipeWire stream: {e}"))?;
		let inner = Rc::new(RefCell::new(Inner {
			sender,
			offered: offered.clone(),
			rate,
			status: StreamStatus::default(),
			pending: None,
			in_flight: Vec::new(),
			dmabufs: HashMap::new(),
			next_index: 0,
			crop_warned: false,
		}));
		let on_param = inner.clone();
		let on_process = inner.clone();
		let on_state = inner.clone();
		let on_remove = inner.clone();
		let listener = stream
			.add_local_listener::<()>()
			.state_changed(move |_, _, old, new| {
				tracing::debug!(?old, ?new, "Desktop capture stream state");
				if let pw::stream::StreamState::Error(error) = new {
					on_state
						.borrow_mut()
						.status
						.failure
						.get_or_insert_with(|| format!("the screen-cast stream failed: {error}"));
				} else if matches!(new, pw::stream::StreamState::Unconnected)
					&& !matches!(old, pw::stream::StreamState::Unconnected)
				{
					on_state
						.borrow_mut()
						.status
						.failure
						.get_or_insert_with(|| "the compositor ended the screen cast".into());
				}
			})
			.param_changed(move |stream, _, id, param| {
				if id == spa_sys::SPA_PARAM_Format
					&& let Some(param) = param
				{
					on_param.borrow_mut().on_format(stream, param);
				}
			})
			.remove_buffer(move |_, _, buffer| {
				let mut inner = on_remove.borrow_mut();
				inner.dmabufs.remove(&(buffer as usize));
				// PipeWire reclaims removed buffers itself.
				inner.in_flight.retain(|held| held.buffer != buffer);
				if inner.pending.is_some_and(|(pending, _)| pending == buffer) {
					inner.pending = None;
				}
			})
			.process(move |stream, _| on_process.borrow_mut().on_process(stream))
			.register()
			.map_err(|e| format!("cannot listen to the PipeWire stream: {e}"))?;

		let pods: Vec<Vec<u8>> = offered
			.iter()
			.map(|format| format_pod(format, &format.modifiers, false, rate))
			.collect();
		let mut params: Vec<&Pod> = pods
			.iter()
			.map(|bytes| Pod::from_bytes(bytes).expect("serialized pod"))
			.collect();
		stream
			.connect(
				spa::utils::Direction::Input,
				Some(node_id),
				pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::DONT_RECONNECT,
				&mut params,
			)
			.map_err(|e| format!("cannot connect to the screen cast: {e}"))?;
		Ok(Self {
			inner,
			_listener: listener,
			stream,
		})
	}

	/// Send the pending frame if the pipeline asked for one, and return the
	/// buffers it finished with. Called on capture demand and periodically.
	pub(crate) fn service(&self) {
		self.inner.borrow_mut().try_send(&self.stream);
	}

	pub(crate) fn with_status<R>(&self, read: impl FnOnce(&StreamStatus) -> R) -> R {
		let mut inner = self.inner.borrow_mut();
		inner.status.held = inner.in_flight.len() + usize::from(inner.pending.is_some());
		read(&inner.status)
	}
}

impl Drop for CaptureStream {
	fn drop(&mut self) {
		// Buffers still held are reclaimed by the disconnect; their frames own
		// duplicated descriptors and stay valid.
		let mut inner = self.inner.borrow_mut();
		inner.pending = None;
		inner.in_flight.clear();
		drop(inner);
		let _ = self.stream.disconnect();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn offered_formats_follow_the_gpu_and_keep_preference_order() {
		let offered = offered_formats(|fourcc| match fourcc {
			0x3432_4258 => vec![0, 0x0200_0000_0000_0001],
			0x3432_5258 => vec![0],
			_ => vec![],
		});
		assert_eq!(
			offered.iter().map(|f| f.fourcc).collect::<Vec<_>>(),
			[0x3432_5258, 0x3432_4258]
		);
		assert!(offered_formats(|_| vec![]).is_empty());
	}

	#[test]
	fn only_srgb_and_hdr10_declarations_are_streamed() {
		let declared = |transfer, primaries| DeclaredColor { transfer, primaries }.color_space();
		assert_eq!(declared(0, 0), Some(FrameColorSpace::Srgb));
		assert_eq!(
			declared(
				spa_sys::SPA_VIDEO_TRANSFER_SRGB,
				spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT709
			),
			Some(FrameColorSpace::Srgb)
		);
		assert_eq!(
			declared(
				spa_sys::SPA_VIDEO_TRANSFER_SMPTE2084,
				spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020
			),
			Some(FrameColorSpace::Bt2020Pq)
		);
		// PQ with BT.709 primaries, or BT.2020 with sRGB transfer, would be
		// streamed with the wrong colors.
		assert_eq!(
			declared(
				spa_sys::SPA_VIDEO_TRANSFER_SMPTE2084,
				spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT709
			),
			None
		);
		assert_eq!(
			declared(
				spa_sys::SPA_VIDEO_TRANSFER_SRGB,
				spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020
			),
			None
		);
	}

	#[test]
	fn modifier_choices_are_read_from_offered_pods() {
		let format = OfferedFormat {
			spa_format: spa_sys::SPA_VIDEO_FORMAT_BGRx,
			fourcc: 0x3432_5258,
			modifiers: vec![7, 0],
		};
		let open = format_pod(&format, &format.modifiers, false, 60);
		assert_eq!(
			modifier_choice(Pod::from_bytes(&open).unwrap()),
			Some((vec![7, 0], true))
		);
		let fixed = format_pod(&format, &[0], true, 60);
		assert_eq!(
			modifier_choice(Pod::from_bytes(&fixed).unwrap()),
			Some((vec![0], false))
		);
	}

	#[test]
	fn presentation_times_never_lie_ahead_of_receipt() {
		let now = Instant::now();
		assert_eq!(source_time(None, now), now);
		assert_eq!(source_time(Some(i64::MAX), now), now);
		let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
		unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
		let pts = ts.tv_sec * 1_000_000_000 + ts.tv_nsec - 5_000_000;
		let time = source_time(Some(pts), now);
		assert!(time <= now && now.duration_since(time) >= Duration::from_millis(5));
	}
}
