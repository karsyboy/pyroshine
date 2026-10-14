//! NVIDIA readings through NVML (`libnvidia-ml.so.1`), loaded at runtime.
//!
//! The library ships with the NVIDIA driver; nothing links against it, so
//! hosts without it are unaffected. Only the stable, versioned entry points
//! below are used; their signatures follow `nvml.h`.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_uint, c_ulonglong, c_void};

use libloading::Library;
use moonshine_management::dto::GpuStats;

use super::{ProcessGroup, process_stats};

type Return = c_int;
type DeviceHandle = *mut c_void;

const SUCCESS: Return = 0;
const ERROR_NOT_FOUND: Return = 6;
const ERROR_INSUFFICIENT_SIZE: Return = 7;
const TEMPERATURE_GPU: c_uint = 0;
const CLOCK_GRAPHICS: c_uint = 0;
const CLOCK_MEM: c_uint = 2;

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessSample {
	pid: c_uint,
	timestamp: c_ulonglong,
	sm: c_uint,
	memory: c_uint,
	encoder: c_uint,
	decoder: c_uint,
}

struct Api {
	shutdown: unsafe extern "C" fn() -> Return,
	device_by_pci: unsafe extern "C" fn(*const c_char, *mut DeviceHandle) -> Return,
	utilization: unsafe extern "C" fn(DeviceHandle, *mut Utilization) -> Return,
	encoder_utilization: unsafe extern "C" fn(DeviceHandle, *mut c_uint, *mut c_uint) -> Return,
	temperature: unsafe extern "C" fn(DeviceHandle, c_uint, *mut c_uint) -> Return,
	power: unsafe extern "C" fn(DeviceHandle, *mut c_uint) -> Return,
	power_limit: unsafe extern "C" fn(DeviceHandle, *mut c_uint) -> Return,
	clock: unsafe extern "C" fn(DeviceHandle, c_uint, *mut c_uint) -> Return,
	memory: unsafe extern "C" fn(DeviceHandle, *mut Memory) -> Return,
	process_utilization: unsafe extern "C" fn(DeviceHandle, *mut ProcessSample, *mut c_uint, c_ulonglong) -> Return,
	// Keeps the symbols above valid; dropped last.
	_library: Library,
}

impl Api {
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
				shutdown: symbol!("nvmlShutdown"),
				device_by_pci: symbol!("nvmlDeviceGetHandleByPciBusId_v2"),
				utilization: symbol!("nvmlDeviceGetUtilizationRates"),
				encoder_utilization: symbol!("nvmlDeviceGetEncoderUtilization"),
				temperature: symbol!("nvmlDeviceGetTemperature"),
				power: symbol!("nvmlDeviceGetPowerUsage"),
				power_limit: symbol!("nvmlDeviceGetEnforcedPowerLimit"),
				clock: symbol!("nvmlDeviceGetClockInfo"),
				memory: symbol!("nvmlDeviceGetMemoryInfo"),
				process_utilization: symbol!("nvmlDeviceGetProcessUtilization"),
				_library: library,
			};
			match init() {
				SUCCESS => Ok(api),
				code => Err(format!("nvmlInit failed ({code})")),
			}
		}
	}
}

impl Drop for Api {
	fn drop(&mut self) {
		// SAFETY: balances the successful nvmlInit in `load`.
		unsafe {
			(self.shutdown)();
		}
	}
}

/// An NVML handle for one GPU. NVML handles are thread-safe; the sampler
/// moves between blocking threads but is used by one at a time.
pub(super) struct Device {
	api: Api,
	handle: DeviceHandle,
	/// NVML timestamp (µs) of the newest process sample seen.
	last_seen: c_ulonglong,
}

// SAFETY: NVML is thread-safe and the handle stays valid until nvmlShutdown,
// which only `Api::drop` calls.
unsafe impl Send for Device {}

impl Device {
	/// The GPU at `pci_address` (`0000:01:00.0`).
	pub(super) fn open(pci_address: &str) -> Result<Self, String> {
		let api = Api::load()?;
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
		let api = &self.api;
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
		let processes = self.processes(groups);
		GpuStats {
			driver: String::new(),
			source: "nvml".into(),
			busy_percent: busy.map(f64::from),
			encoder_busy_percent: encoder.map(f64::from),
			core_clock_mhz: with(self.api.clock, CLOCK_GRAPHICS).map(f64::from),
			memory_clock_mhz: with(self.api.clock, CLOCK_MEM).map(f64::from),
			temperature_c: with(self.api.temperature, TEMPERATURE_GPU).map(f64::from),
			hotspot_c: None,
			power_w: value(self.api.power).map(|milliwatts| f64::from(milliwatts) / 1000.0),
			power_limit_w: value(self.api.power_limit).map(|milliwatts| f64::from(milliwatts) / 1000.0),
			vram_used_bytes: memory.as_ref().map(|memory| memory.used),
			vram_total_bytes: memory.as_ref().map(|memory| memory.total),
			processes,
		}
	}

	/// Per-process utilization since the previous call, summed per group.
	/// NVML reports one sample per process that used the GPU in its last
	/// sampling period, as percentages of that period.
	fn processes(&mut self, groups: &[ProcessGroup]) -> Vec<moonshine_management::dto::GpuProcessStats> {
		let mut samples = vec![ProcessSample::default(); 64];
		let mut count = samples.len() as c_uint;
		// SAFETY: `samples` holds `count` writable elements.
		let mut result =
			unsafe { (self.api.process_utilization)(self.handle, samples.as_mut_ptr(), &mut count, self.last_seen) };
		if result == ERROR_INSUFFICIENT_SIZE {
			samples.resize(count as usize, ProcessSample::default());
			// SAFETY: as above, after growing to the reported count.
			result = unsafe {
				(self.api.process_utilization)(self.handle, samples.as_mut_ptr(), &mut count, self.last_seen)
			};
		}
		// NOT_FOUND: no process used the GPU since `last_seen`.
		if result != SUCCESS && result != ERROR_NOT_FOUND {
			return Vec::new();
		}
		let samples = if result == SUCCESS {
			&samples[..(count as usize).min(samples.len())]
		} else {
			&[]
		};
		if let Some(newest) = samples.iter().map(|sample| sample.timestamp).max() {
			self.last_seen = newest;
		}
		let mut latest: HashMap<u32, ProcessSample> = HashMap::new();
		for sample in samples {
			let entry = latest.entry(sample.pid).or_insert(*sample);
			if sample.timestamp > entry.timestamp {
				*entry = *sample;
			}
		}
		groups
			.iter()
			.filter_map(|group| {
				let mut totals = [0u32; 3];
				let mut found = false;
				for pid in &group.pids {
					if let Some(sample) = latest.get(pid) {
						found = true;
						totals[0] += sample.sm;
						totals[1] += sample.encoder;
						totals[2] += sample.decoder;
					}
				}
				found.then(|| {
					let shares = ["sm", "enc", "dec"]
						.into_iter()
						.zip(totals)
						.map(|(engine, total)| (engine.to_string(), f64::from(total)))
						.collect();
					process_stats(group, shares, None)
				})
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The sample layout NVML writes: 32 bytes with the timestamp aligned.
	#[test]
	fn process_samples_match_the_nvml_layout() {
		assert_eq!(std::mem::size_of::<ProcessSample>(), 32);
		assert_eq!(std::mem::offset_of!(ProcessSample, timestamp), 8);
		assert_eq!(std::mem::size_of::<Memory>(), 24);
	}

	/// Reads a real NVIDIA GPU: `MOONSHINE_TEST_NVML=0000:01:00.0`.
	#[test]
	#[ignore = "needs an NVIDIA GPU"]
	fn reads_an_nvidia_gpu() {
		let address = std::env::var("MOONSHINE_TEST_NVML").expect("MOONSHINE_TEST_NVML=<pci address>");
		let mut device = Device::open(&address).unwrap();
		let stats = device.read(&[]);
		println!("{stats:#?}");
		assert!(stats.vram_total_bytes.unwrap() > 0);
		assert!(stats.temperature_c.is_some());
	}
}
