//! NVIDIA readings through NVML (`libnvidia-ml.so.1`), loaded at runtime.
//!
//! The library ships with the NVIDIA driver; nothing links against it, so
//! hosts without it are unaffected. Only the stable, versioned entry points
//! below are used; their signatures follow `nvml.h`.
//!
//! NVML is loaded and initialized once per process and never shut down:
//! repeated `nvmlInit`/`nvmlShutdown` and unloading the library next to the
//! driver's Vulkan state in the same process is avoidable risk, and a
//! session reconnect would otherwise repeat it.
//!
//! The proprietary driver publishes no DRM fdinfo counters, so process
//! attribution comes from NVML: the running-process lists name every process
//! with a GPU context and its memory, and the utilization samples add busy
//! shares for processes that were busy in a recent sampling period. Light
//! work often has no sample, so a group can be present with no engine shares.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_uint, c_ulonglong, c_void};
use std::sync::OnceLock;

use libloading::Library;
use moonshine_management::dto::{GpuProcessStats, GpuStats};

use super::{ProcessGroup, process_stats};

type Return = c_int;
type DeviceHandle = *mut c_void;

const SUCCESS: Return = 0;
const ERROR_NOT_FOUND: Return = 6;
const ERROR_INSUFFICIENT_SIZE: Return = 7;
const TEMPERATURE_GPU: c_uint = 0;
const CLOCK_GRAPHICS: c_uint = 0;
const CLOCK_MEM: c_uint = 2;
/// `NVML_VALUE_NOT_AVAILABLE` in a process's memory.
const NOT_AVAILABLE: c_ulonglong = c_ulonglong::MAX;

#[repr(C)]
#[derive(Default)]
struct Utilization {
	gpu: c_uint,
	memory: c_uint,
}

#[repr(C)]
#[derive(Default)]
struct Memory {
	total: c_ulonglong,
	free: c_ulonglong,
	used: c_ulonglong,
}

/// `nvmlProcessUtilizationSample_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ProcessSample {
	pid: c_uint,
	timestamp: c_ulonglong,
	sm: c_uint,
	memory: c_uint,
	encoder: c_uint,
	decoder: c_uint,
}

/// `nvmlProcessInfo_t`, as filled by the `_v3` running-process queries.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ProcessInfo {
	pid: c_uint,
	used_memory: c_ulonglong,
	gpu_instance: c_uint,
	compute_instance: c_uint,
}

type ProcessList = unsafe extern "C" fn(DeviceHandle, *mut c_uint, *mut ProcessInfo) -> Return;

struct Api {
	device_by_pci: unsafe extern "C" fn(*const c_char, *mut DeviceHandle) -> Return,
	utilization: unsafe extern "C" fn(DeviceHandle, *mut Utilization) -> Return,
	encoder_utilization: unsafe extern "C" fn(DeviceHandle, *mut c_uint, *mut c_uint) -> Return,
	temperature: unsafe extern "C" fn(DeviceHandle, c_uint, *mut c_uint) -> Return,
	power: unsafe extern "C" fn(DeviceHandle, *mut c_uint) -> Return,
	power_limit: unsafe extern "C" fn(DeviceHandle, *mut c_uint) -> Return,
	clock: unsafe extern "C" fn(DeviceHandle, c_uint, *mut c_uint) -> Return,
	memory: unsafe extern "C" fn(DeviceHandle, *mut Memory) -> Return,
	process_utilization: unsafe extern "C" fn(DeviceHandle, *mut ProcessSample, *mut c_uint, c_ulonglong) -> Return,
	graphics_processes: ProcessList,
	compute_processes: ProcessList,
	// Keeps the symbols above valid. The API lives in a static and is never
	// dropped, so the library stays loaded.
	_library: Library,
}

impl Api {
	/// The process-wide NVML, loaded and initialized on first use.
	fn get() -> Result<&'static Self, String> {
		static API: OnceLock<Result<Api, String>> = OnceLock::new();
		API.get_or_init(Self::load).as_ref().map_err(Clone::clone)
	}

	fn load() -> Result<Self, String> {
		// SAFETY: the driver's own NVML library; every symbol type below
		// mirrors its declaration in nvml.h.
		unsafe {
			let library = Library::new("libnvidia-ml.so.1").map_err(|e| e.to_string())?;
			macro_rules! symbol {
				($name:literal) => {
					*library
						.get(concat!($name, "\0").as_bytes())
						.map_err(|e| format!("{}: {e}", $name))?
				};
			}
			let init: unsafe extern "C" fn() -> Return = symbol!("nvmlInit_v2");
			let api = Self {
				device_by_pci: symbol!("nvmlDeviceGetHandleByPciBusId_v2"),
				utilization: symbol!("nvmlDeviceGetUtilizationRates"),
				encoder_utilization: symbol!("nvmlDeviceGetEncoderUtilization"),
				temperature: symbol!("nvmlDeviceGetTemperature"),
				power: symbol!("nvmlDeviceGetPowerUsage"),
				power_limit: symbol!("nvmlDeviceGetEnforcedPowerLimit"),
				clock: symbol!("nvmlDeviceGetClockInfo"),
				memory: symbol!("nvmlDeviceGetMemoryInfo"),
				process_utilization: symbol!("nvmlDeviceGetProcessUtilization"),
				graphics_processes: symbol!("nvmlDeviceGetGraphicsRunningProcesses_v3"),
				compute_processes: symbol!("nvmlDeviceGetComputeRunningProcesses_v3"),
				_library: library,
			};
			match init() {
				SUCCESS => Ok(api),
				code => Err(format!("nvmlInit failed ({code})")),
			}
		}
	}
}

/// An NVML handle for one GPU. NVML handles are thread-safe; the sampler
/// moves between blocking threads but is used by one at a time.
pub(super) struct Device {
	api: &'static Api,
	handle: DeviceHandle,
	/// NVML timestamp (µs) of the newest process sample seen.
	last_seen: c_ulonglong,
}

// SAFETY: NVML is thread-safe and handles stay valid while NVML is
// initialized, which is for the rest of the process.
unsafe impl Send for Device {}

impl Device {
	/// The GPU at `pci_address` (`0000:01:00.0`).
	pub(super) fn open(pci_address: &str) -> Result<Self, String> {
		let api = Api::get()?;
		let address = CString::new(pci_address).map_err(|e| e.to_string())?;
		let mut handle = std::ptr::null_mut();
		// SAFETY: a NUL-terminated bus ID and a valid out-pointer.
		match unsafe { (api.device_by_pci)(address.as_ptr(), &mut handle) } {
			SUCCESS => Ok(Self {
				api,
				handle,
				last_seen: 0,
			}),
			code => Err(format!("no NVML device at {pci_address} ({code})")),
		}
	}

	pub(super) fn read(&mut self, groups: &[ProcessGroup]) -> GpuStats {
		let api = self.api;
		let handle = self.handle;
		let value = |read: unsafe extern "C" fn(DeviceHandle, *mut c_uint) -> Return| {
			let mut value = 0;
			// SAFETY: a valid handle and out-pointer.
			(unsafe { read(handle, &mut value) } == SUCCESS).then_some(value)
		};
		let with = |read: unsafe extern "C" fn(DeviceHandle, c_uint, *mut c_uint) -> Return, kind| {
			let mut value = 0;
			// SAFETY: a valid handle, a sensor/clock constant from nvml.h and an out-pointer.
			(unsafe { read(handle, kind, &mut value) } == SUCCESS).then_some(value)
		};
		let mut utilization = Utilization::default();
		// SAFETY: a valid handle and out-pointer.
		let busy = (unsafe { (api.utilization)(handle, &mut utilization) } == SUCCESS).then_some(utilization.gpu);
		let mut encoder = 0;
		let mut period = 0;
		// SAFETY: as above.
		let encoder =
			(unsafe { (api.encoder_utilization)(handle, &mut encoder, &mut period) } == SUCCESS).then_some(encoder);
		let mut memory = Memory::default();
		// SAFETY: as above.
		let memory = (unsafe { (api.memory)(handle, &mut memory) } == SUCCESS).then_some(memory);
		let samples = self.samples();
		let running = self.running();
		GpuStats {
			driver: String::new(),
			source: "nvml".into(),
			busy_percent: busy.map(f64::from),
			encoder_busy_percent: encoder.map(f64::from),
			core_clock_mhz: with(api.clock, CLOCK_GRAPHICS).map(f64::from),
			memory_clock_mhz: with(api.clock, CLOCK_MEM).map(f64::from),
			temperature_c: with(api.temperature, TEMPERATURE_GPU).map(f64::from),
			hotspot_c: None,
			power_w: value(api.power).map(|milliwatts| f64::from(milliwatts) / 1000.0),
			power_limit_w: value(api.power_limit).map(|milliwatts| f64::from(milliwatts) / 1000.0),
			vram_used_bytes: memory.as_ref().map(|memory| memory.used),
			vram_total_bytes: memory.as_ref().map(|memory| memory.total),
			processes: group_stats(groups, &samples, &running),
		}
	}

	/// Utilization samples newer than the previous call.
	fn samples(&mut self) -> Vec<ProcessSample> {
		let samples = query(|buffer: *mut ProcessSample, count| {
			// SAFETY: `buffer` holds `count` writable elements (or is null with a zero count).
			unsafe { (self.api.process_utilization)(self.handle, buffer, count, self.last_seen) }
		});
		if let Some(newest) = samples.iter().map(|sample| sample.timestamp).max() {
			self.last_seen = newest;
		}
		samples
	}

	/// Processes with a graphics or compute context on the GPU, and their memory.
	fn running(&self) -> HashMap<u32, Option<u64>> {
		let mut running = HashMap::new();
		for list in [self.api.graphics_processes, self.api.compute_processes] {
			let infos = query(|buffer: *mut ProcessInfo, count| {
				// SAFETY: as in `samples`.
				unsafe { list(self.handle, count, buffer) }
			});
			for info in infos {
				let memory = (info.used_memory != NOT_AVAILABLE).then_some(info.used_memory);
				// A process with both contexts reports the same memory twice.
				running.entry(info.pid).or_insert(memory);
			}
		}
		running
	}
}

/// Call an NVML list query, growing the buffer when NVML asks for more.
/// `NOT_FOUND` (no entries) and errors yield an empty list.
fn query<T: Default + Clone>(mut call: impl FnMut(*mut T, *mut c_uint) -> Return) -> Vec<T> {
	let mut buffer = vec![T::default(); 64];
	for _ in 0..3 {
		let mut count = buffer.len() as c_uint;
		match call(buffer.as_mut_ptr(), &mut count) {
			SUCCESS => {
				buffer.truncate((count as usize).min(buffer.len()));
				return buffer;
			},
			ERROR_INSUFFICIENT_SIZE => buffer.resize((count as usize).max(buffer.len() * 2), T::default()),
			ERROR_NOT_FOUND => return Vec::new(),
			_ => return Vec::new(),
		}
	}
	Vec::new()
}

/// Per-group statistics: groups with a running process, their memory, and
/// their processes' average utilization over the samples in the window.
fn group_stats(
	groups: &[ProcessGroup],
	samples: &[ProcessSample],
	running: &HashMap<u32, Option<u64>>,
) -> Vec<GpuProcessStats> {
	let mut averages: HashMap<u32, ([f64; 3], u32)> = HashMap::new();
	for sample in samples {
		let (sums, count) = averages.entry(sample.pid).or_default();
		sums[0] += f64::from(sample.sm);
		sums[1] += f64::from(sample.encoder);
		sums[2] += f64::from(sample.decoder);
		*count += 1;
	}
	groups
		.iter()
		.filter_map(|group| {
			let mut shares = [0.0; 3];
			let mut present = false;
			let mut memory: Option<u64> = None;
			for pid in &group.pids {
				if let Some(used) = running.get(pid) {
					present = true;
					if let Some(used) = used {
						memory = Some(memory.unwrap_or(0) + used);
					}
				}
				if let Some((sums, count)) = averages.get(pid) {
					present = true;
					for (share, sum) in shares.iter_mut().zip(sums) {
						*share += sum / f64::from(*count);
					}
				}
			}
			present.then(|| {
				let shares = ["sm", "enc", "dec"]
					.into_iter()
					.zip(shares)
					.map(|(engine, share)| (engine.to_string(), share))
					.collect();
				process_stats(group, shares, memory)
			})
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The layouts NVML writes: samples are 32 bytes and process infos 24,
	/// both with the 64-bit field aligned.
	#[test]
	fn structures_match_the_nvml_layout() {
		assert_eq!(std::mem::size_of::<ProcessSample>(), 32);
		assert_eq!(std::mem::offset_of!(ProcessSample, timestamp), 8);
		assert_eq!(std::mem::size_of::<ProcessInfo>(), 24);
		assert_eq!(std::mem::offset_of!(ProcessInfo, used_memory), 8);
		assert_eq!(std::mem::size_of::<Memory>(), 24);
	}

	fn sample(pid: u32, sm: u32, encoder: u32) -> ProcessSample {
		ProcessSample {
			pid,
			sm,
			encoder,
			..Default::default()
		}
	}

	#[test]
	fn groups_are_present_by_context_and_averaged_over_their_samples() {
		let groups = [
			ProcessGroup {
				role: "application",
				label: "Game".into(),
				pids: vec![10, 11, 12],
			},
			ProcessGroup {
				role: "pyroshine",
				label: "Pyroshine".into(),
				pids: vec![20],
			},
			ProcessGroup {
				role: "application",
				label: "Unrelated".into(),
				pids: vec![30],
			},
		];
		// The game's renderer was sampled twice, its launcher once; Pyroshine
		// has a context but no sample (light work).
		let samples = [sample(10, 60, 0), sample(10, 80, 0), sample(11, 5, 0)];
		let running = HashMap::from([(10, Some(4 << 30)), (12, None), (20, Some(300 << 20))]);
		let stats = group_stats(&groups, &samples, &running);
		assert_eq!(stats.len(), 2, "a group without a context or sample is left out");
		assert_eq!(stats[0].label, "Game");
		let engines: Vec<_> = stats[0]
			.engines
			.iter()
			.map(|e| (e.engine.as_str(), e.busy_percent))
			.collect();
		assert_eq!(engines, [("sm", 75.0)]);
		assert_eq!(stats[0].vram_bytes, Some(4 << 30), "unavailable memory is not counted");
		assert_eq!(stats[1].label, "Pyroshine");
		assert!(stats[1].engines.is_empty());
		assert_eq!(stats[1].vram_bytes, Some(300 << 20));
	}

	#[test]
	fn list_queries_grow_their_buffer() {
		let mut calls = 0;
		let items = query(|buffer: *mut ProcessInfo, count| {
			calls += 1;
			// SAFETY: the test writes within the reported capacity.
			unsafe {
				if *count < 100 {
					*count = 100;
					return ERROR_INSUFFICIENT_SIZE;
				}
				*buffer = ProcessInfo {
					pid: 7,
					..Default::default()
				};
				*count = 1;
			}
			SUCCESS
		});
		assert_eq!((calls, items.len(), items[0].pid), (2, 1, 7));
		assert!(query(|_: *mut ProcessInfo, _| ERROR_NOT_FOUND).is_empty());
	}

	/// Reads a real NVIDIA GPU: `MOONSHINE_TEST_NVML=0000:01:00.0`. Prints
	/// every process with a context, to compare with `nvidia-smi`.
	#[test]
	#[ignore = "needs an NVIDIA GPU"]
	fn reads_an_nvidia_gpu() {
		let address = std::env::var("MOONSHINE_TEST_NVML").expect("MOONSHINE_TEST_NVML=<pci address>");
		let mut device = Device::open(&address).unwrap();
		let stats = device.read(&[]);
		println!("{stats:#?}");
		let mut running: Vec<_> = device.running().into_iter().collect();
		running.sort();
		for (pid, memory) in running {
			let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
			println!("{pid} {} {:?} MiB", name.trim(), memory.map(|bytes| bytes >> 20));
		}
		assert!(stats.vram_total_bytes.unwrap() > 0);
		assert!(stats.temperature_c.is_some());
		assert!(Device::open(&address).is_ok(), "NVML is initialized once and reused");
	}
}
