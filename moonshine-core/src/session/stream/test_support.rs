//! Test-only helpers shared by the media stream tests.

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

/// A socket identified by its kernel inode.
///
/// Release tests must prove a stopped handler closed its socket. Re-binding
/// the freed ephemeral port cannot prove that: a concurrently running test
/// may take the port first. A UDP port is free exactly when the last
/// descriptor of its socket closes, so this checks the descriptors instead.
pub(crate) struct SocketId(u64);

impl SocketId {
	pub(crate) fn of(socket: &impl AsRawFd) -> Self {
		let metadata = std::fs::metadata(format!("/proc/self/fd/{}", socket.as_raw_fd()))
			.expect("socket descriptor is visible in /proc/self/fd");
		Self(metadata.ino())
	}

	/// Whether any descriptor in this process still refers to the socket.
	pub(crate) fn is_open(&self) -> bool {
		let target = format!("socket:[{}]", self.0);
		std::fs::read_dir("/proc/self/fd")
			.expect("/proc/self/fd is readable")
			.filter_map(Result::ok)
			.filter_map(|entry| std::fs::read_link(entry.path()).ok())
			.any(|link| link.as_os_str() == target.as_str())
	}
}

/// A free UDP port below the kernel's ephemeral range, for tests that release
/// and rebind one fixed port across sessions. Every call returns a different
/// port, so such tests running in parallel never share one.
///
/// Such a port must not come from `bind(…:0)`: while the test has released
/// it, any concurrent `bind(…:0)` (tests run in parallel) can be handed exactly
/// that port. The kernel never assigns ports outside the ephemeral range to
/// `bind(…:0)`, so only an explicit bind of the same number can take it.
pub(crate) fn fixed_udp_port() -> u16 {
	static NEXT_BELOW: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
	let ephemeral_start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
		.ok()
		.and_then(|range| range.split_whitespace().next()?.parse::<u16>().ok())
		.unwrap_or(32768);
	let lowest = ephemeral_start.saturating_sub(2000).max(1024);
	loop {
		// Hand out each candidate once, counting down from the range's start.
		let offset = NEXT_BELOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let port = ephemeral_start
			.checked_sub(1 + offset)
			.filter(|port| *port >= lowest)
			.expect("a free UDP port below the ephemeral range");
		if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
			return port;
		}
	}
}

/// Whether a UDP socket bound to local `port` is open in this process.
///
/// Unlike re-binding the port, this cannot be confused by a child process
/// that a parallel test forked: until it executes, such a child shares every
/// descriptor of this process, so a socket closed here can stay bound briefly.
pub(crate) fn udp_port_open_here(port: u16) -> bool {
	let suffix = format!(":{port:04X}");
	let inodes: Vec<String> = ["/proc/net/udp", "/proc/net/udp6"]
		.iter()
		.filter_map(|table| std::fs::read_to_string(table).ok())
		.flat_map(|table| {
			table
				.lines()
				.skip(1)
				.filter_map(|line| {
					let fields: Vec<_> = line.split_whitespace().collect();
					fields[1].ends_with(&suffix).then(|| format!("socket:[{}]", fields[9]))
				})
				.collect::<Vec<_>>()
		})
		.collect();
	!inodes.is_empty()
		&& std::fs::read_dir("/proc/self/fd")
			.expect("/proc/self/fd is readable")
			.filter_map(Result::ok)
			.filter_map(|entry| std::fs::read_link(entry.path()).ok())
			.any(|link| inodes.iter().any(|inode| link.as_os_str() == inode.as_str()))
}

/// Construct a stream on fixed `port`, retrying while only another process
/// holds the port (see [`udp_port_open_here`]). A socket still open in this
/// process is a leak and fails at once; a port held elsewhere for 5 s fails too.
pub(crate) async fn construct_on_fixed_port<T>(
	port: u16,
	mut construct: impl AsyncFnMut() -> Result<T, ()>,
) -> Result<T, String> {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
	loop {
		if let Ok(stream) = construct().await {
			return Ok(stream);
		}
		if udp_port_open_here(port) {
			return Err(format!("port {port} is still open in this process"));
		}
		if std::time::Instant::now() >= deadline {
			return Err(format!("construction on port {port} kept failing"));
		}
		tokio::time::sleep(std::time::Duration::from_millis(2)).await;
	}
}

#[cfg(test)]
mod tests {
	use super::{SocketId, fixed_udp_port, udp_port_open_here};

	#[test]
	fn fixed_ports_are_distinct_and_never_ephemeral() {
		let ephemeral_start: u16 = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
			.map(|range| range.split_whitespace().next().unwrap().parse().unwrap())
			.unwrap_or(32768);
		let (first, second) = (fixed_udp_port(), fixed_udp_port());
		assert_ne!(first, second);
		assert!(first < ephemeral_start && second < ephemeral_start);
	}

	#[test]
	fn open_ports_are_attributed_to_this_process() {
		let port = fixed_udp_port();
		assert!(!udp_port_open_here(port));
		let socket = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
		assert!(udp_port_open_here(port));
		drop(socket);
		assert!(!udp_port_open_here(port));
	}

	#[test]
	fn socket_stays_open_until_its_last_descriptor_closes() {
		let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
		let id = SocketId::of(&socket);
		assert!(id.is_open());
		// A duplicated descriptor keeps the port bound after the original drops.
		let duplicate = socket.try_clone().unwrap();
		drop(socket);
		assert!(id.is_open());
		drop(duplicate);
		assert!(!id.is_open());
	}
}
