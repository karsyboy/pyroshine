//! PipeWire timeline synchronization, using the existing DRM crate's checked
//! ioctl bindings on the verified capture node. No device search or CPU wait.
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;

use drm::control::{Device as _, syncobj};
use smithay::reexports::drm;

// PipeWire 1.2 wire values, available even with older build-host SPA headers.
pub(super) const META_TIMELINE: u32 = 9;
pub(super) const DATA_SYNCOBJ: u32 = 5;
pub(super) const BUFFERS_META_TYPE: u32 = 7;
pub(super) const UNSCHEDULED_RELEASE: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct TimelineMeta {
	pub flags: u32,
	pub padding: u32,
	pub acquire_point: u64,
	pub release_point: u64,
}
const _: () = assert!(size_of::<TimelineMeta>() == 24);

pub(super) fn validate_layout(timeline: bool, types: &[u32]) -> Result<(), String> {
	let valid = if timeline {
		types == [pipewire::spa::sys::SPA_DATA_DmaBuf, DATA_SYNCOBJ, DATA_SYNCOBJ]
	} else {
		types == [pipewire::spa::sys::SPA_DATA_DmaBuf]
	};
	if !valid {
		return Err("capture buffer does not match its pixel-plane and timeline layout".into());
	}
	Ok(())
}

pub(crate) struct SyncDevice(OwnedFd);
impl AsFd for SyncDevice {
	fn as_fd(&self) -> BorrowedFd<'_> {
		self.0.as_fd()
	}
}
impl drm::Device for SyncDevice {}
impl drm::control::Device for SyncDevice {}

fn eventfd() -> Result<OwnedFd, String> {
	// SAFETY: returns a fresh descriptor, no borrowed memory.
	let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
	if fd < 0 {
		return Err(std::io::Error::last_os_error().to_string());
	}
	// SAFETY: fresh owned descriptor.
	Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl SyncDevice {
	/// Exercise create, export/import, asynchronous wait and signal on this
	/// exact device. Old kernels/drivers retain implicit DMA-BUF synchronization.
	pub fn probe(fd: BorrowedFd<'_>) -> Option<Rc<Self>> {
		let device = Rc::new(Self(fd.try_clone_to_owned().ok()?));
		let probe = (|| -> Result<(), String> {
			let original = Timeline {
				handle: device.create_syncobj(false).map_err(|e| e.to_string())?,
				device: device.clone(),
			};
			let exported = device
				.syncobj_to_fd(original.handle, false)
				.map_err(|e| e.to_string())?;
			let imported = device.import(exported.as_fd())?;
			let ready = imported.completion(1)?;
			let mut poll = libc::pollfd {
				fd: std::os::fd::AsRawFd::as_raw_fd(&ready),
				events: libc::POLLIN,
				revents: 0,
			};
			// SAFETY: one initialized pollfd; never waits for a producer.
			if unsafe { libc::poll(&mut poll, 1, 0) } != 0 {
				return Err("unsignaled timeline was reported ready".into());
			}
			original.signal(1)?;
			let mut value = 0u64;
			// SAFETY: initialized u64 and a live eventfd. Signal above is
			// synchronous; EAGAIN refuses explicit sync rather than waiting.
			let read = unsafe {
				libc::read(
					std::os::fd::AsRawFd::as_raw_fd(&ready),
					std::ptr::from_mut(&mut value).cast(),
					8,
				)
			};
			if read != 8 || value == 0 {
				return Err("timeline eventfd did not signal".into());
			}
			Ok(())
		})();
		match probe {
			Ok(()) => Some(device),
			Err(error) => {
				tracing::debug!(%error, "Capture GPU uses implicit DMA-BUF synchronization");
				None
			},
		}
	}

	pub(super) fn import(self: &Rc<Self>, fd: BorrowedFd<'_>) -> Result<Rc<Timeline>, String> {
		// Keep an owned duplicate through import. The DRM handle then retains
		// the timeline independently of the producer's descriptors and pool.
		let fd = fd.try_clone_to_owned().map_err(|e| e.to_string())?;
		let handle = self
			.fd_to_syncobj(fd.as_fd(), false)
			.map_err(|e| format!("cannot import capture timeline: {e}"))?;
		Ok(Rc::new(Timeline {
			device: self.clone(),
			handle,
		}))
	}
}

pub(super) struct Timeline {
	device: Rc<SyncDevice>,
	handle: syncobj::Handle,
}
impl Timeline {
	pub fn completion(&self, point: u64) -> Result<OwnedFd, String> {
		let fd = eventfd()?;
		self.device
			.syncobj_eventfd(self.handle, point, fd.as_fd(), false)
			.map_err(|e| format!("cannot register capture timeline completion: {e}"))?;
		Ok(fd)
	}
	fn signal(&self, point: u64) -> Result<(), String> {
		self.device
			.syncobj_timeline_signal(&[self.handle], &[point])
			.map_err(|e| format!("cannot signal capture release timeline: {e}"))
	}
}
impl Drop for Timeline {
	fn drop(&mut self) {
		let _ = self.device.destroy_syncobj(self.handle);
	}
}

pub(super) struct SyncBuffer {
	pub acquire: Rc<Timeline>,
	pub release: Rc<Timeline>,
	last_release: Option<u64>,
}
impl SyncBuffer {
	pub fn new(device: &Rc<SyncDevice>, acquire: BorrowedFd<'_>, release: BorrowedFd<'_>) -> Result<Self, String> {
		Ok(Self {
			acquire: device.import(acquire)?,
			release: device.import(release)?,
			last_release: None,
		})
	}
	pub fn claim(&mut self, meta: TimelineMeta) -> Result<Release, String> {
		validate(meta, self.last_release)?;
		self.last_release = Some(meta.release_point);
		Ok(Release {
			timeline: self.release.clone(),
			point: meta.release_point,
		})
	}
}

fn validate(meta: TimelineMeta, last_release: Option<u64>) -> Result<(), String> {
	if meta.flags & !UNSCHEDULED_RELEASE != 0
		|| meta.padding != 0
		|| meta.release_point == 0
		|| last_release.is_some_and(|last| meta.release_point <= last)
	{
		return Err("malformed or stale PipeWire timeline metadata".into());
	}
	Ok(())
}

/// Exactly one loop-owned release action per dequeued buffer, including
/// discarded/no-credit frames. Kept after removal until the final GPU read.
pub(super) struct Release {
	timeline: Rc<Timeline>,
	point: u64,
}
impl Release {
	pub fn signal(self) -> Result<(), String> {
		self.timeline.signal(self.point)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn sync_descriptors_are_never_reinterpreted_as_pixel_planes() {
		let pixel = pipewire::spa::sys::SPA_DATA_DmaBuf;
		assert!(validate_layout(false, &[pixel]).is_ok());
		assert!(validate_layout(true, &[pixel, DATA_SYNCOBJ, DATA_SYNCOBJ]).is_ok());
		for types in [
			vec![],
			vec![pixel, pixel],
			vec![pixel, DATA_SYNCOBJ],
			vec![pixel, pixel, pixel],
			vec![DATA_SYNCOBJ, pixel, DATA_SYNCOBJ],
		] {
			assert!(validate_layout(true, &types).is_err());
			assert!(validate_layout(false, &types).is_err());
		}
		assert!(validate_layout(false, &[pixel, DATA_SYNCOBJ, DATA_SYNCOBJ]).is_err());
		assert!(validate_layout(true, &[pixel]).is_err());
	}
	#[test]
	fn an_unsupported_capture_device_does_not_select_another_node() {
		let file = tempfile::tempfile().unwrap();
		assert!(SyncDevice::probe(file.as_fd()).is_none());
		assert!(
			file.metadata().is_ok(),
			"probing did not consume the caller's descriptor"
		);
	}

	#[test]
	fn timeline_points_are_not_guessed_or_released_twice() {
		let meta = TimelineMeta {
			flags: UNSCHEDULED_RELEASE,
			padding: 0,
			acquire_point: 90,
			release_point: 3,
		};
		// Independent timelines do not need equal/ordered acquire and release points.
		assert!(validate(meta, Some(2)).is_ok());
		assert!(validate(meta, Some(3)).is_err());
		assert!(validate(meta, Some(4)).is_err());
		assert!(
			validate(
				TimelineMeta {
					release_point: 0,
					..meta
				},
				None
			)
			.is_err()
		);
		assert!(validate(TimelineMeta { flags: 2, ..meta }, None).is_err());
		assert!(validate(TimelineMeta { padding: 1, ..meta }, None).is_err());
	}
}
