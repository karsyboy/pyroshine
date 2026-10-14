//! GPU activity readings for diagnostics.
//!
//! A [`GpuSampler`] reads the capture GPU's sensors and the GPU time of
//! process groups (the launched application and Pyroshine itself) through
//! the kernel's supported interfaces:
//!
//! - **sysfs**: the device's `hwmon` sensors (temperature, power, clocks)
//!   for every driver, plus amdgpu's busy and VRAM counters and Intel's
//!   actual frequency.
//! - **DRM fdinfo** (`/proc/<pid>/fdinfo`): per-client engine time and
//!   resident VRAM, as documented in the kernel's `drm-usage-stats`. This is
//!   what attributes GPU time to the application or to Pyroshine.
//! - **NVML** for the NVIDIA driver, loaded at runtime only when the capture
//!   GPU uses it, for device and per-process utilization.
//!
//! Sampling is blocking file I/O (an amdgpu sensor read can take a
//! millisecond). Callers run it on a blocking thread about once per second
//! while a client streams; it is never on a media path.

mod fdinfo;
mod nvml;
mod sysfs;

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use moonshine_management::dto::{GpuDevice, GpuEngineStats, GpuProcessStats, GpuStats};

/// The processes whose GPU time is reported together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessGroup {
	/// `application` or `pyroshine`.
	pub role: &'static str,
	pub label: String,
	pub pids: Vec<u32>,
}

/// A DRM device as found in sysfs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DrmDevice {
	/// `/sys/devices/.../<pci address>`.
	pub sysfs: PathBuf,
	pub pci_address: Option<String>,
	pub driver: Option<String>,
}

impl DrmDevice {
	/// The device behind a DRM node (render or primary), by device number.
	pub(crate) fn from_node(node: &Path) -> Option<Self> {
		let metadata = std::fs::metadata(node).ok()?;
		if !metadata.file_type().is_char_device() {
			return None;
		}
		let (major, minor) = (libc::major(metadata.rdev()), libc::minor(metadata.rdev()));
		let sysfs = std::fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device")).ok()?;
		Some(Self::from_sysfs(sysfs))
	}

	fn from_sysfs(sysfs: PathBuf) -> Self {
		let pci_address = sysfs
			.file_name()
			.and_then(|name| name.to_str())
			.filter(|name| name.matches(':').count() == 2 && name.contains('.'))
			.map(str::to_string);
		let driver = std::fs::read_link(sysfs.join("driver"))
			.ok()
			.and_then(|link| link.file_name()?.to_str().map(str::to_string));
		Self {
			sysfs,
			pci_address,
			driver,
		}
	}
}

/// Every render node and its device, marking the one at `capture`.
pub(crate) fn devices(capture: Option<&Path>) -> Vec<GpuDevice> {
	let capture = capture.and_then(|node| std::fs::canonicalize(node).ok());
	let Ok(entries) = std::fs::read_dir("/dev/dri") else {
		return Vec::new();
	};
	let mut nodes: Vec<PathBuf> = entries
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.starts_with("renderD"))
		})
		.collect();
	nodes.sort();
	nodes
		.into_iter()
		.map(|node| {
			let device = DrmDevice::from_node(&node);
			let read = |name: &str| {
				let device = device.as_ref()?;
				let value = std::fs::read_to_string(device.sysfs.join(name)).ok()?;
				Some(value.trim().to_string()).filter(|value| !value.is_empty())
			};
			let driver = device.as_ref().and_then(|device| device.driver.clone());
			let driver_version = driver
				.as_deref()
				.and_then(|driver| std::fs::read_to_string(format!("/sys/module/{driver}/version")).ok())
				.map(|version| version.trim().to_string());
			GpuDevice {
				render_node: node.display().to_string(),
				pci_address: device.as_ref().and_then(|device| device.pci_address.clone()),
				vendor_id: read("vendor"),
				device_id: read("device"),
				driver,
				driver_version,
				vbios: read("vbios_version"),
				capture: std::fs::canonicalize(&node).ok() == capture,
			}
		})
		.collect()
}

enum Backend {
	Sysfs(sysfs::Sensors),
	Nvml(nvml::Device),
}

/// Reads one GPU's activity. Engine busy shares are differences between
/// consecutive samples, so the first sample reports sensors only.
pub(crate) struct GpuSampler {
	device: DrmDevice,
	backend: Backend,
	clients: fdinfo::Tracker,
	last: Option<Instant>,
}

impl GpuSampler {
	/// A sampler for the GPU behind `render_node`, or `None` when its device
	/// cannot be identified.
	pub(crate) fn open(render_node: &Path) -> Option<Self> {
		let device = DrmDevice::from_node(render_node)?;
		let backend = match device.driver.as_deref() {
			Some("nvidia") => match device.pci_address.as_deref().map(nvml::Device::open) {
				Some(Ok(device)) => Backend::Nvml(device),
				Some(Err(error)) => {
					tracing::info!("NVML is unavailable ({error}); NVIDIA GPU readings are limited to sysfs");
					Backend::Sysfs(sysfs::Sensors::new(&device))
				},
				None => Backend::Sysfs(sysfs::Sensors::new(&device)),
			},
			_ => Backend::Sysfs(sysfs::Sensors::new(&device)),
		};
		Some(Self {
			clients: fdinfo::Tracker::new(device.pci_address.clone()),
			device,
			backend,
			last: None,
		})
	}

	/// Where readings come from: `nvml` or `sysfs`.
	pub(crate) fn source(&self) -> &'static str {
		match self.backend {
			Backend::Sysfs(_) => "sysfs",
			Backend::Nvml(_) => "nvml",
		}
	}

	/// Read the sensors and the GPU time of `groups` since the previous sample.
	pub(crate) fn sample(&mut self, groups: &[ProcessGroup]) -> GpuStats {
		let now = Instant::now();
		let elapsed = self.last.map(|last| now.saturating_duration_since(last));
		self.last = Some(now);
		let driver = self.device.driver.clone().unwrap_or_else(|| "unknown".into());
		match &mut self.backend {
			Backend::Sysfs(sensors) => {
				let readings = sensors.read(now);
				let processes = self.clients.sample(groups, elapsed);
				GpuStats {
					driver,
					source: "sysfs".into(),
					busy_percent: readings.busy_percent,
					encoder_busy_percent: readings.encoder_busy_percent,
					core_clock_mhz: readings.core_clock_mhz,
					memory_clock_mhz: readings.memory_clock_mhz,
					temperature_c: readings.temperature_c,
					hotspot_c: readings.hotspot_c,
					power_w: readings.power_w,
					power_limit_w: readings.power_limit_w,
					vram_used_bytes: readings.vram_used_bytes,
					vram_total_bytes: readings.vram_total_bytes,
					processes,
				}
			},
			Backend::Nvml(device) => {
				let mut stats = device.read(groups);
				stats.driver = driver;
				stats
			},
		}
	}
}

/// Engine class of a driver's engine name: amdgpu (`gfx`, `compute`, `dma`,
/// `dec`, `enc`, `enc_1`, `jpeg`, `vpe`), i915 (`render`, `copy`, `video`,
/// `video-enhance`, `compute`), xe (`rcs`, `bcs`, `vcs`, `vecs`, `ccs`) and
/// NVML's process counters (`sm`, `enc`, `dec`).
pub(crate) fn engine_class(engine: &str) -> &'static str {
	match engine {
		"gfx" | "render" | "rcs" | "sm" => "graphics",
		"compute" | "ccs" => "compute",
		"dma" | "copy" | "bcs" => "copy",
		"dec" | "jpeg" => "video_decode",
		"video" | "vcs" => "video",
		_ if engine.starts_with("enc") => "video_encode",
		_ => "other",
	}
}

/// Busy shares per engine, busiest first, dropping idle engines.
fn engines(mut shares: Vec<(String, f64)>) -> Vec<GpuEngineStats> {
	shares.retain(|(_, busy)| *busy >= 0.05);
	shares.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
	shares
		.into_iter()
		.map(|(engine, busy_percent)| GpuEngineStats {
			class: engine_class(&engine).into(),
			engine,
			busy_percent: (busy_percent * 10.0).round() / 10.0,
		})
		.collect()
}

fn process_stats(group: &ProcessGroup, shares: Vec<(String, f64)>, vram_bytes: Option<u64>) -> GpuProcessStats {
	GpuProcessStats {
		role: group.role.into(),
		label: group.label.clone(),
		engines: engines(shares),
		vram_bytes,
	}
}

/// Every process in the cgroup `path` (relative to the cgroup2 root) and its
/// descendants.
pub(crate) fn cgroup_pids(path: &str) -> Vec<u32> {
	let mut pids = Vec::new();
	let mut pending = vec![Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))];
	while let Some(directory) = pending.pop() {
		if let Ok(procs) = std::fs::read_to_string(directory.join("cgroup.procs")) {
			pids.extend(procs.lines().filter_map(|line| line.trim().parse::<u32>().ok()));
		}
		if let Ok(entries) = std::fs::read_dir(&directory) {
			pending.extend(
				entries
					.filter_map(Result::ok)
					.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
					.map(|entry| entry.path()),
			);
		}
	}
	pids
}

/// The cgroup2 path of a process, from `/proc/<pid>/cgroup`.
pub(crate) fn process_cgroup(pid: &str) -> Option<String> {
	std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
		.ok()?
		.lines()
		.find_map(|line| line.strip_prefix("0::").map(str::to_string))
}

/// Pyroshine's own processes: its service's cgroup when it runs as a
/// dedicated systemd service (XWayland and helpers included), otherwise the
/// process itself and its children.
pub(crate) fn own_processes() -> Vec<u32> {
	if let Some(cgroup) = process_cgroup("self").filter(|cgroup| cgroup.ends_with(".service")) {
		let pids = cgroup_pids(&cgroup);
		if !pids.is_empty() {
			return pids;
		}
	}
	let mut pids = vec![std::process::id()];
	if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
		for task in tasks.filter_map(Result::ok) {
			if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
				pids.extend(children.split_whitespace().filter_map(|pid| pid.parse::<u32>().ok()));
			}
		}
	}
	pids.sort_unstable();
	pids.dedup();
	pids
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn engine_names_map_to_classes_across_drivers() {
		for (engine, class) in [
			("gfx", "graphics"),
			("render", "graphics"),
			("rcs", "graphics"),
			("compute", "compute"),
			("ccs", "compute"),
			("enc", "video_encode"),
			("enc_1", "video_encode"),
			("dec", "video_decode"),
			("jpeg", "video_decode"),
			("video", "video"),
			("vcs", "video"),
			("dma", "copy"),
			("bcs", "copy"),
			("video-enhance", "other"),
			("vpe", "other"),
		] {
			assert_eq!(engine_class(engine), class, "{engine}");
		}
	}

	#[test]
	fn idle_engines_are_dropped_and_the_rest_sorted() {
		let stats = engines(vec![
			("dma".into(), 0.01),
			("gfx".into(), 42.04),
			("enc".into(), 63.26),
			("compute".into(), 42.04),
		]);
		let names: Vec<_> = stats
			.iter()
			.map(|stats| (stats.engine.as_str(), stats.busy_percent))
			.collect();
		assert_eq!(names, [("enc", 63.3), ("compute", 42.0), ("gfx", 42.0)]);
	}

	#[test]
	fn a_pci_device_is_named_by_its_address() {
		let device = DrmDevice::from_sysfs(PathBuf::from("/sys/devices/pci0000:00/0000:00:01.1/0000:03:00.0"));
		assert_eq!(device.pci_address.as_deref(), Some("0000:03:00.0"));
		let platform = DrmDevice::from_sysfs(PathBuf::from("/sys/devices/platform/soc/fd4c0000.gpu"));
		assert_eq!(platform.pci_address, None);
	}

	/// Reads every GPU of this host twice, one second apart, attributing the
	/// GPU time of all readable processes, or of the cgroup in
	/// `MOONSHINE_TEST_GPU_CGROUP` (for example a `systemd-run --user` unit's):
	/// `cargo test -p moonshine-core reads_this_hosts_gpus -- --ignored --nocapture`.
	#[test]
	#[ignore = "reads real GPUs"]
	fn reads_this_hosts_gpus() {
		let (label, pids) = match std::env::var("MOONSHINE_TEST_GPU_CGROUP") {
			Ok(cgroup) => (cgroup.clone(), cgroup_pids(&cgroup)),
			Err(_) => (
				"every process".to_string(),
				std::fs::read_dir("/proc")
					.unwrap()
					.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
					.collect(),
			),
		};
		let groups = [ProcessGroup {
			role: "application",
			label,
			pids,
		}];
		let gpus = devices(None);
		assert!(!gpus.is_empty(), "no render node");
		for gpu in gpus {
			println!("{gpu:#?}");
			let mut sampler = GpuSampler::open(Path::new(&gpu.render_node)).expect("the device is identified");
			sampler.sample(&groups);
			std::thread::sleep(std::time::Duration::from_secs(1));
			let stats = sampler.sample(&groups);
			println!("{stats:#?}");
			assert!(
				stats.temperature_c.is_some() || stats.core_clock_mhz.is_some() || stats.busy_percent.is_some(),
				"{} reports no sensor",
				gpu.render_node
			);
		}
	}
}
