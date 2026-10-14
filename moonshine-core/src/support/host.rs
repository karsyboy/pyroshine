//! Host facts for support reports: operating system, CPU, memory, GPUs and
//! the systemd unit Pyroshine runs in.

use std::path::Path;

use moonshine_management::dto::HostInfo;

use crate::gpu_sensors;

/// A systemd unit and the instance that runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceUnit {
	pub name: String,
	/// The user's systemd instance rather than the system's.
	pub user: bool,
}

impl ServiceUnit {
	/// The unit owning the cgroup `path`, when it is a service.
	pub(crate) fn from_cgroup(path: &str) -> Option<Self> {
		let name = path
			.rsplit('/')
			.next()
			.filter(|segment| segment.ends_with(".service"))?;
		Some(Self {
			name: name.to_string(),
			user: path.contains("/user@"),
		})
	}

	/// `systemctl`/`journalctl` arguments that select this unit.
	pub(crate) fn args(&self, command: &[&str]) -> Vec<String> {
		let mut args: Vec<String> = Vec::new();
		if self.user {
			args.push("--user".into());
		}
		args.extend(command.iter().map(|arg| arg.to_string()));
		args
	}
}

pub(crate) fn host_info(capture: Option<&Path>, service: Option<&ServiceUnit>) -> HostInfo {
	HostInfo {
		kernel: kernel(),
		os: os_release(),
		cpu: std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|cpuinfo| {
			cpuinfo.lines().find_map(|line| {
				line.strip_prefix("model name")?
					.split_once(':')
					.map(|(_, model)| model.trim().to_string())
			})
		}),
		cpu_threads: std::thread::available_parallelism().map_or(0, |threads| threads.get() as u32),
		memory_total_bytes: std::fs::read_to_string("/proc/meminfo").ok().and_then(|meminfo| {
			let kib: u64 = meminfo
				.lines()
				.find_map(|line| line.strip_prefix("MemTotal:"))?
				.split_whitespace()
				.next()?
				.parse()
				.ok()?;
			Some(kib * 1024)
		}),
		service_unit: service.map(|service| service.name.clone()),
		service_scope: service.map(|service| if service.user { "user" } else { "system" }.into()),
		gpus: gpu_sensors::devices(capture),
	}
}

fn kernel() -> String {
	// SAFETY: uname fills the zeroed struct; its fields are NUL-terminated.
	unsafe {
		let mut name: libc::utsname = std::mem::zeroed();
		if libc::uname(&mut name) != 0 {
			return String::new();
		}
		let field = |field: &[libc::c_char]| std::ffi::CStr::from_ptr(field.as_ptr()).to_string_lossy().into_owned();
		format!("{} {}", field(&name.release), field(&name.machine))
	}
}

fn os_release() -> Option<String> {
	let text = std::fs::read_to_string("/etc/os-release")
		.or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
		.ok()?;
	text.lines()
		.find_map(|line| line.strip_prefix("PRETTY_NAME="))
		.map(|value| value.trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn units_are_read_from_cgroup_paths() {
		assert_eq!(
			ServiceUnit::from_cgroup("/system.slice/system-pyroshine.slice/pyroshine@alice.service"),
			Some(ServiceUnit {
				name: "pyroshine@alice.service".into(),
				user: false,
			})
		);
		let user =
			ServiceUnit::from_cgroup("/user.slice/user-1000.slice/user@1000.service/app.slice/pyroshine.service")
				.unwrap();
		assert_eq!(user.name, "pyroshine.service");
		assert!(user.user);
		assert_eq!(user.args(&["status"]), ["--user", "status"]);
		assert_eq!(
			ServiceUnit::from_cgroup("/user.slice/user-1000.slice/session-2.scope"),
			None
		);
		// A terminal inside the user manager is not the manager's own unit.
		assert_eq!(
			ServiceUnit::from_cgroup("/user.slice/user-1000.slice/user@1000.service/app.slice/app-konsole-1.scope"),
			None
		);
	}

	#[test]
	fn describes_this_host() {
		let info = host_info(None, None);
		assert!(!info.kernel.is_empty());
		assert!(info.cpu_threads > 0);
		assert!(info.memory_total_bytes.is_some());
	}
}
