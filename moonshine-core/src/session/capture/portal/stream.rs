//! PipeWire consumer of the guard-owned virtual output.
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
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
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

use super::ledger::{BufferId, Ledger};
use super::metrics::{CaptureReport, Timings};
use super::readiness::{Completion, Readiness, producer_fence};
use super::sync::{self, Release, SyncBuffer, SyncDevice, TimelineMeta};
use crate::session::capture::admission::{CaptureCredit, CaptureSendError, CaptureSender};
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

/// How long a buffer's producer fence may stay unsignaled before capture
/// fails. Compositors queue buffers after rendering, so this is
/// normally already signaled.
const PRODUCER_FENCE_TIMEOUT: Duration = Duration::from_millis(50);

fn refresh_fraction(millihertz: u32) -> Fraction {
	let (mut numerator, mut denominator) = (millihertz, 1000);
	while denominator != 0 {
		(numerator, denominator) = (denominator, numerator % denominator);
	}
	Fraction {
		num: millihertz / numerator,
		denom: 1000 / numerator,
	}
}

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
	///
	/// Desktops declare SDR differently: KWin leaves both fields unset,
	/// Mutter declares gamma 2.2 with BT.709 primaries. Every gamma-encoded
	/// SDR transfer with BT.709 (or unstated) primaries is the encoding the
	/// pipeline streams as SDR; only PQ with BT.2020 primaries is HDR10.
	pub(crate) fn color_space(self) -> Option<FrameColorSpace> {
		const SDR_TRANSFERS: [u32; 5] = [
			spa_sys::SPA_VIDEO_TRANSFER_UNKNOWN,
			spa_sys::SPA_VIDEO_TRANSFER_GAMMA22,
			spa_sys::SPA_VIDEO_TRANSFER_BT709,
			spa_sys::SPA_VIDEO_TRANSFER_SRGB,
			spa_sys::SPA_VIDEO_TRANSFER_BT601,
		];
		let bt709 = matches!(
			self.primaries,
			spa_sys::SPA_VIDEO_COLOR_PRIMARIES_UNKNOWN | spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT709
		);
		if SDR_TRANSFERS.contains(&self.transfer) && bt709 {
			Some(FrameColorSpace::Srgb)
		} else if self.transfer == spa_sys::SPA_VIDEO_TRANSFER_SMPTE2084
			&& self.primaries == spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020
		{
			Some(FrameColorSpace::Bt2020Pq)
		} else {
			None
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
	/// Maximum frame rate the producer agreed to: the lower of the output's
	/// refresh and the requested rate (KWin, Mutter).
	pub max_framerate: Option<Fraction>,
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
	/// Actual announced pool, distinct from the requested buffer count.
	pub pool: usize,
	pub pending: usize,
	pub no_credit: u64,
	pub first_completed: Option<Instant>,
}

/// A PipeWire buffer the stream owns until it is queued back.
struct Held {
	id: BufferId,
	published: Instant,
	/// Set by the pipeline once no GPU work can read the buffer.
	consumed: Arc<AtomicBool>,
}

struct Pending {
	id: BufferId,
	pts: Option<i64>,
	dmabuf: Dmabuf,
	completion: Option<Completion>,
	arrived: Instant,
	credit: Option<CaptureCredit>,
}

/// The buffer's DMA-BUF, built once and rebuilt only when its layout changes.
struct CachedDmabuf {
	index: usize,
	layout: (i64, u32, i32),
	dmabuf: Dmabuf,
}

struct Inner {
	sender: CaptureSender,
	offered: Vec<OfferedFormat>,
	/// Frame rate requested from the producer.
	rate: Fraction,
	size: Option<(u32, u32)>,
	active: bool,
	pacing_allowed: bool,
	status: StreamStatus,
	/// Newest received frame not sent yet.
	pending: Option<Pending>,
	in_flight: Vec<Held>,
	ledger: Ledger,
	readiness: Rc<Readiness>,
	sync_device: Option<Rc<SyncDevice>>,
	sync_buffers: HashMap<BufferId, SyncBuffer>,
	releases: HashMap<BufferId, Release>,
	acquire_points: HashMap<BufferId, u64>,
	explicit_seen: bool,
	dmabufs: HashMap<usize, CachedDmabuf>,
	next_index: usize,
	crop_warned: bool,
	started: Instant,
	mode: crate::desktop::Mode,
	observed_mode: Option<crate::desktop::Mode>,
	timings: Timings,
}

/// A connected screen-cast stream. Lives on the capture thread with its
/// PipeWire loop; dropping it disconnects the stream and returns every buffer.
pub(crate) struct CaptureStream {
	inner: Rc<RefCell<Inner>>,
	_listener: pw::stream::StreamListener<()>,
	_request_listener: Option<super::pacer::Listener>,
	pacer: Option<Rc<RefCell<super::pacer::Pacer>>>,
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
fn format_pod(
	format: &OfferedFormat,
	modifiers: &[u64],
	fixated: bool,
	rate: Fraction,
	size: Option<(u32, u32)>,
) -> Vec<u8> {
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
				size.map(|(width, height)| Value::Rectangle(Rectangle { width, height }))
					.unwrap_or_else(|| {
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
						)))
					}),
			),
			// Producers deliver frames as content changes, at most at the
			// (maximum) frame rate; 0/1 means variable. Compositors throttle to
			// the default, so it is the stream's rate.
			Property::new(
				spa_sys::SPA_FORMAT_VIDEO_framerate,
				Value::Choice(ChoiceValue::Fraction(Choice(
					ChoiceFlags::empty(),
					ChoiceEnum::Range {
						default: rate,
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
						default: rate,
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
fn buffer_params(explicit: bool) -> Vec<Vec<u8>> {
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
	let mut params = vec![
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
	];
	if explicit {
		let (_, Value::Object(mut buffers)) =
			pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(&params[0])
				.expect("serialized buffer requirements")
		else {
			unreachable!()
		};
		buffers
			.properties
			.retain(|p| p.key != spa_sys::SPA_PARAM_BUFFERS_blocks);
		buffers
			.properties
			.push(Property::new(spa_sys::SPA_PARAM_BUFFERS_blocks, Value::Int(3)));
		buffers.properties.push(Property {
			key: sync::BUFFERS_META_TYPE,
			flags: PropertyFlags::MANDATORY,
			value: Value::Int(1 << sync::META_TIMELINE),
		});
		params.insert(0, serialize(Value::Object(buffers)));
		params.push(meta(sync::META_TIMELINE, size_of::<TimelineMeta>()));
	}
	params
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
	/// Removed holds still participate in drain: their duplicated DMA-BUFs
	/// remain valid, but only the current announcement may be queued again.
	fn sweep(&mut self, stream: &pw::stream::Stream) {
		let mut index = 0;
		while index < self.in_flight.len() {
			if !self.in_flight[index].consumed.load(Ordering::Acquire) {
				index += 1;
				continue;
			}
			let held = self.in_flight.swap_remove(index);
			if self.timings.enabled {
				self.timings.hold.record(held.published.elapsed());
			}
			self.requeue(stream, held.id);
		}
	}

	fn signal_release(&mut self, id: BufferId) -> bool {
		self.acquire_points.remove(&id);
		if let Some(release) = self.releases.remove(&id)
			&& let Err(error) = release.signal()
		{
			self.status.failure = Some(error);
			return false;
		}
		true
	}

	fn requeue(&mut self, stream: &pw::stream::Stream, id: BufferId) {
		if self.signal_release(id) && self.ledger.release(id) {
			// SAFETY: release accepted this dequeued, still-announced buffer,
			// and explicit GPU consumption has been signalled when negotiated.
			unsafe { stream.queue_raw_buffer(id.address as *mut pw::sys::pw_buffer) };
		}
	}

	fn discard_pending(&mut self, stream: &pw::stream::Stream) {
		if let Some(pending) = self.pending.take() {
			let id = pending.id;
			// Disarm the fence before requeue; no GPU reader was submitted.
			drop(pending);
			self.requeue(stream, id);
		}
	}

	/// Claim exactly the release point delivered with this announcement. Never
	/// read metadata again after publication: the producer can remove its pool.
	fn claim_sync(&mut self, id: BufferId) -> Result<(), String> {
		// SAFETY: the ledger just accepted a dequeued, live buffer.
		let buffer = unsafe { &*(*(id.address as *mut pw::sys::pw_buffer)).buffer };
		if buffer.n_metas > 0 && buffer.metas.is_null() {
			return Err("missing buffer metadata array".into());
		}
		let mut timeline = None;
		for index in 0..buffer.n_metas as usize {
			let meta = unsafe { &*buffer.metas.add(index) };
			if meta.type_ == sync::META_TIMELINE {
				if timeline.is_some() || meta.data.is_null() || (meta.size as usize) < size_of::<TimelineMeta>() {
					return Err("malformed PipeWire timeline metadata".into());
				}
				timeline = Some(meta.data.cast::<TimelineMeta>());
			}
		}
		if !matches!(buffer.n_datas, 1 | 3) || buffer.datas.is_null() {
			return Err("invalid capture data block count".into());
		}
		// SAFETY: bounded to one/three descriptors in this live SPA buffer.
		let datas = unsafe { std::slice::from_raw_parts(buffer.datas, buffer.n_datas as usize) };
		let mut types = [0u32; 3];
		for (index, data) in datas.iter().enumerate() {
			types[index] = data.type_;
		}
		sync::validate_layout(timeline.is_some(), &types[..datas.len()])?;
		let Some(meta) = timeline else { return Ok(()) };
		let device = self.sync_device.as_ref().ok_or("explicit sync was not negotiated")?;
		if let std::collections::hash_map::Entry::Vacant(entry) = self.sync_buffers.entry(id) {
			let acquire = duplicate(datas[1].fd)?;
			let release = duplicate(datas[2].fd)?;
			entry.insert(SyncBuffer::new(device, acquire.as_fd(), release.as_fd())?);
		}
		// SAFETY: the metadata size and pointer were checked above; the
		// producer may supply an address without Rust alignment guarantees.
		let value = unsafe { meta.read_unaligned() };
		let release = self
			.sync_buffers
			.get_mut(&id)
			.expect("imported timelines")
			.claim(value)?;
		self.releases.insert(id, release);
		self.acquire_points.insert(id, value.acquire_point);
		// SAFETY: owned dequeued metadata, validated size; promising a release
		// only after the owned action exists. No timeout-based unscheduled sweep.
		unsafe {
			meta.write_unaligned(TimelineMeta {
				flags: value.flags & !sync::UNSCHEDULED_RELEASE,
				..value
			})
		};
		self.explicit_seen = true;
		Ok(())
	}

	/// A usable frame's presentation time, or why the buffer is skipped.
	/// `Ok(None)` for buffers that carry no new picture.
	fn inspect(&mut self, buffer: *mut pw::sys::pw_buffer) -> Result<Option<Option<i64>>, String> {
		// SAFETY: a dequeued buffer stays valid until it is queued back.
		let spa_buffer = unsafe { &*(*buffer).buffer };
		if spa_buffer.n_datas == 0 {
			return Ok(None);
		}
		if !matches!(spa_buffer.n_datas, 1 | 3) || spa_buffer.datas.is_null() {
			return Err("the producer did not supply the negotiated single DMA-BUF plane".into());
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
		if data.chunk.is_null() {
			return Err("the producer supplied no DMA-BUF chunk".into());
		}
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
			return Ok((cached.index, cached.dmabuf.clone()));
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
		let index = self.next_index;
		self.next_index += 1;
		self.dmabufs.insert(
			key,
			CachedDmabuf {
				index,
				layout,
				dmabuf: dmabuf.clone(),
			},
		);
		Ok((index, dmabuf))
	}

	/// Readiness is serviced outside PipeWire's process callback. Timeout is
	/// a controlled capture failure, never permission to publish incomplete pixels.
	fn complete_pending(&mut self) -> Result<bool, String> {
		let Some(pending) = &mut self.pending else {
			return Ok(false);
		};
		if let Some(completion) = &pending.completion {
			if !completion.ready()? {
				if pending.arrived.elapsed() >= PRODUCER_FENCE_TIMEOUT {
					return Err("the compositor's rendering did not complete in time".into());
				}
				return Ok(false);
			}
			pending.completion = None;
			if !self.ledger.complete(pending.id) {
				return Err("producer completion belongs to a removed buffer".into());
			}
			if self.timings.enabled {
				self.timings.producer_ready.record(pending.arrived.elapsed());
			}
			self.status.first_completed.get_or_insert_with(Instant::now);
		}
		if self.ledger.pool_size() < 3 {
			return Err(format!(
				"the compositor supplied only {} buffers; Desktop Mode requires at least 3 for producer, pending and GPU ownership",
				self.ledger.pool_size()
			));
		}
		Ok(true)
	}

	fn try_send(&mut self, stream: &pw::stream::Stream) {
		self.sweep(stream);
		if self.status.failure.is_some() {
			return;
		}
		match self.complete_pending() {
			Ok(true) => {},
			Ok(false) => return,
			Err(error) => {
				self.status.failure = Some(error);
				self.discard_pending(stream);
				return;
			},
		}
		if !self.active {
			return;
		}
		let pending = self.pending.as_mut().expect("completed pending buffer");
		if pending.credit.is_none() {
			pending.credit = self.sender.try_acquire();
		}
		let Some(credit) = pending.credit.take() else { return };
		let pending = self.pending.take().expect("checked above");
		let id = pending.id;
		let frame = self.export(&pending);
		let consumed = frame.consumed.clone();
		if !self.ledger.publish(id) {
			self.status.failure = Some("a removed buffer reached publication".into());
			return;
		}
		match self.sender.try_send(frame, credit) {
			Ok(()) => {
				self.status.sent += 1;
				if self.timings.enabled {
					self.timings.acquire_to_publish.record(pending.arrived.elapsed());
				}
				self.in_flight.push(Held {
					id,
					consumed,
					published: Instant::now(),
				});
			},
			Err(error) => {
				if error == CaptureSendError::Disconnected {
					self.status
						.failure
						.get_or_insert_with(|| "the video pipeline stopped".into());
				}
				self.requeue(stream, id);
			},
		}
	}

	fn export(&self, pending: &Pending) -> ExportedFrame {
		let format = self.status.format.expect("a pending frame has a format");
		let color = format.color.color_space().expect("validated at negotiation");
		let index = self
			.dmabufs
			.get(&pending.id.address)
			.expect("cached pending DMA-BUF")
			.index;
		let mut frame =
			ExportedFrame::from_dmabuf(&pending.dmabuf, index, Arc::new(AtomicBool::new(false)), color, None);
		frame.source_time = source_time(pending.pts, pending.arrived);
		frame.scale_to_stream = self.size.is_none();
		frame
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
			let pod = format_pod(&offered, &[modifier], true, self.rate, self.size);
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
			framerate: (framerate.num > 0 && framerate.denom > 0).then_some(framerate),
			max_framerate: Some(info.max_framerate()).filter(|rate| rate.num > 0 && rate.denom > 0),
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
		// Pending pixels belong to the old format. Published frames retain
		// their leases and ledger holds until consumers finish.
		self.discard_pending(stream);
		self.dmabufs.clear();
		self.status.format = Some(format);
		let params = buffer_params(self.sync_device.is_some());
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
		if self.status.failure.is_some() {
			return;
		}
		let mut newest = None;
		loop {
			// SAFETY: plain dequeue; the buffer is owned until queued back.
			let buffer = unsafe { stream.dequeue_raw_buffer() };
			if buffer.is_null() {
				break;
			}
			let id = match self.ledger.dequeue(buffer as usize) {
				Ok(id) => id,
				Err(error) => {
					// Do not queue the resent pointer: an encoder may still read it.
					self.status.failure = Some(error.into());
					if let Some(older) = newest {
						self.requeue(stream, older);
					}
					return;
				},
			};
			if let Err(error) = self.claim_sync(id) {
				self.status.failure = Some(error);
				if let Some(older) = newest {
					self.requeue(stream, older);
				}
				return;
			}
			self.status.received += 1;
			if let Some(older) = newest.replace(id) {
				self.requeue(stream, older);
			}
		}
		let Some(id) = newest else { return };
		let buffer = id.address as *mut pw::sys::pw_buffer;
		// Transfer claimed demand to the newer pixels. Dropping it here would
		// leave a receiver still waiting with idle (unrequested) admission,
		// starving capture until its receive timeout requests another credit.
		let carried_credit = self.pending.as_mut().and_then(|p| p.credit.take());
		self.discard_pending(stream);
		let credit = if self.active {
			let Some(credit) = carried_credit.or_else(|| self.sender.try_acquire()) else {
				self.status.no_credit += 1;
				self.requeue(stream, id);
				return;
			};
			Some(credit)
		} else {
			None
		}; // Pre-commit first-frame validation, never publication.
		let arrival = (|| {
			let Some(pts) = self.inspect(buffer)? else {
				return Ok(None);
			};
			let (_, dmabuf) = self.dmabuf(buffer)?;
			let plane = dmabuf.handles().next().ok_or("DMA-BUF without planes")?;
			let fence = match self.sync_buffers.get(&id) {
				Some(sync) => {
					// The acquire point was snapshotted by claim_sync; retain it
					// separately from mutable producer metadata.
					let point = self
						.acquire_points
						.remove(&id)
						.ok_or("missing explicit acquire point")?;
					sync.acquire.completion(point)?
				},
				None => producer_fence(plane)?,
			};
			let completion = self.readiness.watch(fence)?;
			Ok::<_, String>(Some(Pending {
				id,
				pts,
				dmabuf,
				completion: Some(completion),
				arrived: Instant::now(),
				credit,
			}))
		})();
		match arrival {
			Ok(Some(pending)) => {
				self.status.last_frame = Some(pending.arrived);
				assert!(self.ledger.pending(id));
				self.pending = Some(pending);
			},
			Ok(None) => self.requeue(stream, id),
			Err(error) => {
				self.status.skipped += 1;
				self.status.failure = Some(error);
				self.requeue(stream, id);
			},
		}
	}
}

impl CaptureStream {
	/// Connect to `node_id` on `core` (the portal's PipeWire remote).
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn connect(
		core: &pw::core::CoreRc,
		target: &crate::desktop::Target,
		offered: Vec<OfferedFormat>,
		sender: CaptureSender,
		readiness: Rc<Readiness>,
		diagnostics: bool,
		sync_device: Option<Rc<SyncDevice>>,
		request_driven: bool,
	) -> Result<Self, String> {
		let rate = refresh_fraction(target.mode.refresh_millihertz);
		let size = Some((target.mode.width, target.mode.height));
		if offered.is_empty() {
			return Err("the capture GPU imports no DMA-BUF format a screen cast can use".into());
		}
		let pacer = request_driven.then(|| {
			Rc::new(RefCell::new(super::pacer::Pacer::new(Duration::from_secs_f64(
				f64::from(rate.denom) / f64::from(rate.num),
			))))
		});
		let mut props = pw::properties::properties! {
			*pw::keys::MEDIA_TYPE => "Video", *pw::keys::MEDIA_CATEGORY => "Capture", *pw::keys::MEDIA_ROLE => "Screen",
			"target.object" => target.serial.map(|s| s.to_string()).unwrap_or_else(|| target.node_id.to_string()),
		};
		if request_driven {
			props.insert("node.supports-lazy", "2");
		}
		let stream = pw::stream::StreamRc::new(core.clone(), "pyroshine-desktop", props)
			.map_err(|e| format!("cannot create the PipeWire stream: {e}"))?;
		let inner = Rc::new(RefCell::new(Inner {
			sender,
			offered: offered.clone(),
			rate,
			size,
			active: false,
			pacing_allowed: true,
			status: StreamStatus::default(),
			pending: None,
			in_flight: Vec::new(),
			ledger: Ledger::default(),
			readiness,
			sync_device,
			sync_buffers: HashMap::new(),
			releases: HashMap::new(),
			acquire_points: HashMap::new(),
			explicit_seen: false,
			dmabufs: HashMap::new(),
			next_index: 0,
			crop_warned: false,
			started: Instant::now(),
			mode: target.mode,
			observed_mode: target.observed_mode,
			timings: Timings {
				enabled: diagnostics,
				..Default::default()
			},
		}));
		let on_paint = pacer.clone();
		let on_param = inner.clone();
		let on_process = inner.clone();
		let on_state = inner.clone();
		let on_remove = inner.clone();
		let on_add = inner.clone();
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
			.add_buffer(move |_, _, buffer| {
				let mut inner = on_add.borrow_mut();
				if let Err(error) = inner.ledger.announce(buffer as usize) {
					inner.status.failure = Some(error.into());
				}
			})
			.remove_buffer(move |_, _, buffer| {
				let mut inner = on_remove.borrow_mut();
				let removed = inner.ledger.remove(buffer as usize);
				if let Some(id) = removed {
					inner.sync_buffers.remove(&id);
				}
				inner.dmabufs.remove(&(buffer as usize));
				// Removed in-flight holds remain until GPU completion. Their
				// leases own the descriptors; their old pointers are never queued.
				if inner.pending.as_ref().is_some_and(|p| Some(p.id) == removed) {
					let id = inner.pending.take().expect("pending removed buffer").id;
					inner.signal_release(id);
				}
			})
			.process(move |stream, _| {
				let mut inner = on_process.borrow_mut();
				let started = inner.timings.enabled.then(Instant::now);
				let received = inner.status.received;
				inner.on_process(stream);
				if inner.status.received != received
					&& let Some(pacer) = &on_paint
				{
					pacer.borrow_mut().paint(Instant::now());
				}
				if let Some(started) = started {
					inner.timings.callback.record(started.elapsed());
				}
			})
			.register()
			.map_err(|e| format!("cannot listen to the PipeWire stream: {e}"))?;

		let mut pods: Vec<Vec<u8>> = offered
			.iter()
			.map(|format| format_pod(format, &format.modifiers, false, rate, size))
			.collect();
		pods.extend(buffer_params(inner.borrow().sync_device.is_some()));
		let mut params: Vec<&Pod> = pods
			.iter()
			.map(|bytes| Pod::from_bytes(bytes).expect("serialized pod"))
			.collect();
		let request_listener = pacer
			.as_ref()
			.map(|pacer| super::pacer::Listener::attach(&stream, pacer.clone()));
		let driver = if request_driven {
			pw::stream::StreamFlags::DRIVER
		} else {
			pw::stream::StreamFlags::empty()
		};
		stream
			.connect(
				spa::utils::Direction::Input,
				Some(target.node_id),
				pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::DONT_RECONNECT | driver,
				&mut params,
			)
			.map_err(|e| format!("cannot connect to the screen cast: {e}"))?;
		Ok(Self {
			inner,
			_listener: listener,
			_request_listener: request_listener,
			pacer,
			stream,
		})
	}

	/// Send the pending frame if the pipeline asked for one, and return the
	/// buffers it finished with. Called on capture demand and periodically.
	pub(crate) fn replacement_sender(&self) -> CaptureSender {
		self.inner.borrow().sender.replacement()
	}
	pub(crate) fn observe_output(&self, mode: Option<crate::desktop::Mode>) {
		self.inner.borrow_mut().observed_mode = mode;
	}
	pub(crate) fn sync_device(&self) -> Option<Rc<SyncDevice>> {
		self.inner.borrow().sync_device.clone()
	}
	pub(crate) fn readiness(&self) -> Rc<Readiness> {
		self.inner.borrow().readiness.clone()
	}
	pub(crate) fn set_active(&self, active: bool) {
		let mut inner = self.inner.borrow_mut();
		inner.active = active;
		inner.pacing_allowed = active;
		// Pre-commit validation pixels must not become the first stream frame.
		inner.discard_pending(&self.stream);
	}
	pub(crate) fn drained(&self) -> bool {
		self.inner.borrow_mut().sweep(&self.stream);
		self.inner.borrow().in_flight.is_empty()
	}
	pub(crate) fn disconnect_drained(&self) -> Result<(), String> {
		self.set_active(false);
		if !self.drained() {
			return Err("cannot disconnect capture while GPU reads remain".into());
		}
		self.stream.disconnect().map_err(|e| e.to_string())
	}
	pub(crate) fn validate_frame(&self) -> Result<bool, String> {
		self.inner.borrow_mut().complete_pending()
	}
	pub(crate) fn request_driven(&self) -> bool {
		self.pacer.is_some()
	}
	pub(crate) fn schedule_after(&self) -> Duration {
		let blocked = {
			let inner = self.inner.borrow();
			!inner.pacing_allowed || inner.active && !inner.sender.requested() || inner.pending.is_some()
		};
		if blocked {
			return Duration::from_millis(250);
		}
		self.pacer.as_ref().map_or(Duration::from_millis(250), |p| {
			p.borrow()
				.due(Instant::now())
				.clamp(Duration::from_millis(1), Duration::from_millis(250))
		})
	}
	pub(crate) fn service(&self) {
		self.inner.borrow_mut().try_send(&self.stream);
		if let Some(pacer) = &self.pacer {
			let credit = {
				let inner = self.inner.borrow();
				inner.pacing_allowed
					&& inner.status.failure.is_none()
					&& inner.pending.is_none()
					&& (!inner.active || inner.sender.requested())
			};
			let trigger = {
				let mut p = pacer.borrow_mut();
				p.live(matches!(self.stream.state(), pw::stream::StreamState::Streaming) && self.stream.is_driving());
				p.trigger(Instant::now(), credit)
			};
			if trigger && self.stream.trigger_process().is_err() {
				pacer.borrow_mut().failed();
			}
		}
	}

	pub(crate) fn report(&self) -> CaptureReport {
		let inner = self.inner.borrow();
		let format = inner.status.format;
		CaptureReport {
			request_driven: self.pacer.is_some(),
			paint_requests: self.pacer.as_ref().map_or(0, |p| p.borrow().requests),
			paint_triggers: self.pacer.as_ref().map_or(0, |p| p.borrow().triggers),
			requested_mode: inner.mode,
			output_mode: inner.observed_mode,
			pipewire_size: format.map(|f| (f.width, f.height)).unwrap_or_default(),
			pipewire_framerate: format.and_then(|f| f.framerate).map(|f| (f.num, f.denom)),
			pipewire_max_framerate: format.and_then(|f| f.max_framerate).map(|f| (f.num, f.denom)),
			fourcc: format.map(|f| f.fourcc).unwrap_or_default(),
			modifier: format.map(|f| f.modifier).unwrap_or_default(),
			planes: usize::from(!inner.dmabufs.is_empty()),
			sync_mode: if inner.explicit_seen {
				"explicit-timeline"
			} else {
				"implicit-dmabuf"
			}
			.into(),
			actual_pool: inner.ledger.pool_size(),
			held: inner.in_flight.len(),
			pending: usize::from(inner.pending.is_some()),
			no_credit: inner.status.no_credit,
			sent: inner.status.sent,
			skipped: inner.status.skipped,
			first_completed_us: inner
				.status
				.first_completed
				.map(|t| t.duration_since(inner.started).as_micros() as u64),
			timings_enabled: inner.timings.enabled,
			callback: inner.timings.callback.report(),
			producer_ready: inner.timings.producer_ready.report(),
			acquire_to_publish: inner.timings.acquire_to_publish.report(),
			hold: inner.timings.hold.report(),
		}
	}

	pub(crate) fn with_status<R>(&self, read: impl FnOnce(&StreamStatus) -> R) -> R {
		let mut inner = self.inner.borrow_mut();
		inner.status.pending = usize::from(inner.pending.is_some());
		inner.status.held = inner.in_flight.len();
		inner.status.pool = inner.ledger.pool_size();
		read(&inner.status)
	}
}

impl Drop for CaptureStream {
	fn drop(&mut self) {
		// The owning loop drains GPU readers first. Pending pixels have never
		// been published and can release their producer ownership immediately.
		let mut inner = self.inner.borrow_mut();
		inner.discard_pending(&self.stream);
		// Active streams are drained by their capture owner before drop.
		debug_assert!(inner.in_flight.is_empty());
		drop(inner);
		let _ = self.stream.disconnect();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn refresh_offers_preserve_rational_precision_with_canonical_whole_rates() {
		for (mhz, num, denom) in [(60000, 60, 1), (59940, 2997, 50), (119880, 2997, 25), (144000, 144, 1)] {
			let rate = refresh_fraction(mhz);
			assert_eq!((rate.num, rate.denom), (num, denom));
			assert_eq!(u64::from(rate.num) * 1000, u64::from(mhz) * u64::from(rate.denom));
		}
	}

	#[test]
	fn explicit_buffer_offer_requires_metadata_and_keeps_an_implicit_alternative() {
		let objects = |explicit| {
			buffer_params(explicit)
				.iter()
				.filter_map(|bytes| {
					let (_, Value::Object(object)) =
						pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(bytes).unwrap()
					else {
						return None;
					};
					(object.id == spa_sys::SPA_PARAM_Buffers).then_some(object)
				})
				.collect::<Vec<_>>()
		};
		let plain = objects(false);
		assert_eq!(plain.len(), 1);
		let offered = objects(true);
		assert_eq!(offered.len(), 2);
		let timeline = offered[0]
			.properties
			.iter()
			.find(|p| p.key == sync::BUFFERS_META_TYPE)
			.unwrap();
		assert!(timeline.flags.contains(PropertyFlags::MANDATORY));
		assert_eq!(timeline.value, Value::Int(1 << sync::META_TIMELINE));
		assert_eq!(offered[1], plain[0]);
		assert!(offered[1].properties.iter().all(|p| p.key != sync::BUFFERS_META_TYPE));
		for (object, count) in offered.iter().zip([3, 1]) {
			assert!(
				object
					.properties
					.iter()
					.any(|p| p.key == spa_sys::SPA_PARAM_BUFFERS_blocks && p.value == Value::Int(count))
			);
		}
	}

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
		let open = format_pod(
			&format,
			&format.modifiers,
			false,
			Fraction {
				num: 60000,
				denom: 1000,
			},
			None,
		);
		assert_eq!(
			modifier_choice(Pod::from_bytes(&open).unwrap()),
			Some((vec![7, 0], true))
		);
		let fixed = format_pod(
			&format,
			&[0],
			true,
			Fraction {
				num: 60000,
				denom: 1000,
			},
			None,
		);
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
