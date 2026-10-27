//! Nonblocking producer completion on the capture loop. The epoll descriptor
//! wakes calloop without a polling thread or another frame queue.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;

pub(crate) struct Readiness(OwnedFd);

impl Readiness {
	pub fn new() -> Result<Rc<Self>, String> {
		// SAFETY: no borrowed pointers; the returned descriptor is owned.
		let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
		if fd < 0 {
			return Err(format!(
				"cannot create producer readiness: {}",
				std::io::Error::last_os_error()
			));
		}
		Ok(Rc::new(Self(unsafe { OwnedFd::from_raw_fd(fd) })))
	}

	pub fn fd(&self) -> BorrowedFd<'_> {
		self.0.as_fd()
	}

	pub(super) fn watch(self: &Rc<Self>, fd: OwnedFd) -> Result<Completion, String> {
		let mut event = libc::epoll_event {
			events: libc::EPOLLIN as u32,
			u64: 0,
		};
		// SAFETY: the descriptor and event are live for this call. Completion
		// removes the registration before closing the watched descriptor.
		if unsafe { libc::epoll_ctl(self.0.as_raw_fd(), libc::EPOLL_CTL_ADD, fd.as_raw_fd(), &mut event) } < 0 {
			return Err(format!(
				"cannot watch producer completion: {}",
				std::io::Error::last_os_error()
			));
		}
		Ok(Completion {
			fd,
			readiness: self.clone(),
		})
	}
}

pub(super) struct Completion {
	fd: OwnedFd,
	readiness: Rc<Readiness>,
}

impl Completion {
	/// Error and hangup readiness never stand in for completed pixels.
	pub fn ready(&self) -> Result<bool, String> {
		let mut poll = libc::pollfd {
			fd: self.fd.as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		};
		// SAFETY: one initialized pollfd; timeout zero never waits.
		let result = unsafe { libc::poll(&mut poll, 1, 0) };
		if result < 0 {
			let error = std::io::Error::last_os_error();
			return if error.kind() == std::io::ErrorKind::Interrupted {
				Ok(false)
			} else {
				Err(format!("cannot poll producer completion: {error}"))
			};
		}
		if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
			return Err("the producer completion fence failed".into());
		}
		Ok(poll.revents & libc::POLLIN != 0)
	}
}

impl Drop for Completion {
	fn drop(&mut self) {
		// SAFETY: both descriptors remain live through this call. Removing
		// explicitly also works when another duplicated fd keeps the file alive.
		unsafe {
			libc::epoll_ctl(
				self.readiness.0.as_raw_fd(),
				libc::EPOLL_CTL_DEL,
				self.fd.as_raw_fd(),
				std::ptr::null_mut(),
			)
		};
	}
}

/// Snapshot the producer's implicit write fences at arrival. On kernels without
/// sync_file export, polling the DMA-BUF still waits for its writer; queueing
/// order alone is never proof of completion.
pub(super) fn producer_fence(fd: BorrowedFd<'_>) -> Result<OwnedFd, String> {
	#[repr(C)]
	struct ExportSyncFile {
		flags: u32,
		fd: i32,
	}
	const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
	let mut request = ExportSyncFile { flags: 1, fd: -1 };
	// SAFETY: the request matches linux/dma-buf.h and outlives ioctl.
	if unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut request) } < 0 {
		let error = std::io::Error::last_os_error();
		return match error.raw_os_error() {
			Some(libc::ENOTTY | libc::EINVAL) => fd.try_clone_to_owned().map_err(|e| e.to_string()),
			_ => Err(format!("cannot read the DMA-BUF fence: {error}")),
		};
	}
	if request.fd < 0 {
		return Err("the producer returned an invalid fence descriptor".into());
	}
	// SAFETY: ioctl returned a new owned sync_file descriptor.
	Ok(unsafe { OwnedFd::from_raw_fd(request.fd) })
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;
	fn pipe() -> (OwnedFd, std::fs::File) {
		let mut fds = [-1; 2];
		// SAFETY: two valid out slots; pipe2 creates owned descriptors.
		assert_eq!(
			unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
			0
		);
		unsafe { (OwnedFd::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
	}

	#[test]
	fn late_completion_wakes_the_loop_and_drop_disarms_it() {
		let readiness = Readiness::new().unwrap();
		let (read, mut write) = pipe();
		let completion = readiness.watch(read).unwrap();
		assert!(!completion.ready().unwrap());
		write.write_all(&[1]).unwrap();
		assert!(completion.ready().unwrap());
		let mut poll = libc::pollfd {
			fd: readiness.fd().as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		};
		assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
		drop(completion);
		assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 0);
	}

	#[test]
	fn stop_does_not_wait_for_an_unsignaled_fence_and_hangup_is_failure() {
		let readiness = Readiness::new().unwrap();
		let (read, write) = pipe();
		let completion = readiness.watch(read).unwrap();
		assert!(!completion.ready().unwrap());
		drop(write);
		assert!(completion.ready().is_err());
		drop(completion);
		let (read, _write) = pipe();
		let completion = readiness.watch(read).unwrap();
		assert!(!completion.ready().unwrap());
		drop(completion);
	}
}
