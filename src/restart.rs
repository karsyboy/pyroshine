//! Starting the server again after an operator restart
//! (`ShutdownReason::Restart`, from the web interface or the session bus).
//!
//! After the normal shutdown the process replaces itself with a new start of
//! the same command. It works the same under systemd, Nix or a terminal, and
//! the process (and its service's main PID) stays. The executable's path is
//! recorded at startup rather than taken from `/proc/self/exe`, so a restart
//! after a package upgrade starts the upgraded version, like `systemctl
//! restart`.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;

/// How this process was started.
pub struct Invocation {
	executable: PathBuf,
	arguments: Vec<OsString>,
}

impl Invocation {
	pub fn current() -> Option<Self> {
		let executable = std::env::current_exe().ok()?;
		let arguments: Vec<OsString> = std::env::args_os().collect();
		(!arguments.is_empty()).then_some(Self { executable, arguments })
	}

	/// Replace this process with a new start. Returns only on failure.
	pub fn exec(&self) -> std::io::Error {
		close_on_exec();
		std::process::Command::new(&self.executable)
			.arg0(&self.arguments[0])
			.args(&self.arguments[1..])
			.exec()
	}
}

/// Mark every descriptor above standard error close-on-exec. Rust opens its
/// own descriptors that way, but one a native library opened without the
/// flag would survive into the new start (a listening socket would keep its
/// port).
fn close_on_exec() {
	// SAFETY: close_range with CLOSE_RANGE_CLOEXEC only sets the flag on open
	// descriptors; nothing is closed or reused.
	let result = unsafe { libc::close_range(3, libc::c_uint::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) };
	if result == 0 {
		return;
	}
	// Kernels before 5.11: one descriptor at a time.
	let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
		return;
	};
	let descriptors: Vec<libc::c_int> = entries
		.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
		.filter(|descriptor| *descriptor > 2)
		.collect();
	for descriptor in descriptors {
		// SAFETY: F_SETFD only changes the descriptor's flags (FD_CLOEXEC is
		// the only one); a descriptor that closed meanwhile fails with EBADF.
		unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn descriptors_are_marked_close_on_exec() {
		let file = std::fs::File::open("/proc/self/status").unwrap();
		let descriptor = std::os::fd::AsRawFd::as_raw_fd(&file);
		// SAFETY: clears the flag on a descriptor this test owns.
		unsafe { libc::fcntl(descriptor, libc::F_SETFD, 0) };
		close_on_exec();
		// SAFETY: reads the flags of a descriptor this test owns.
		let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
		assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
	}
}
