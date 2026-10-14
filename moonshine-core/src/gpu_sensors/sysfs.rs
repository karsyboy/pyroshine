//! Device sensors from sysfs: `hwmon` for every driver, plus amdgpu's busy
//! and VRAM counters and Intel's GPU frequency.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::DrmDevice;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Readings {
	pub busy_percent: Option<f64>,
	pub encoder_busy_percent: Option<f64>,
	pub core_clock_mhz: Option<f64>,
	pub memory_clock_mhz: Option<f64>,
	pub temperature_c: Option<f64>,
	pub hotspot_c: Option<f64>,
	pub power_w: Option<f64>,
	pub power_limit_w: Option<f64>,
	pub vram_used_bytes: Option<u64>,
	pub vram_total_bytes: Option<u64>,
}

pub(super) struct Sensors {
	device: PathBuf,
	hwmon: Vec<PathBuf>,
	/// Intel's actual-frequency file (i915 `gt_act_freq_mhz`, xe `act_freq`)
	/// and requested-frequency file (`gt_cur_freq_mhz`, `cur_freq`). The
	/// actual frequency is a point sample that reads 0 whenever the GT is
	/// power-gated (RC6), even under load; the requested one is used then.
	intel_frequency: Option<(PathBuf, PathBuf)>,
	/// Previous energy counter (µJ) and when it was read, for drivers that
	/// report energy instead of power (Intel discrete GPUs).
	energy: Option<(u64, Instant)>,
}

impl Sensors {
	pub(super) fn new(device: &DrmDevice) -> Self {
		let hwmon = std::fs::read_dir(device.sysfs.join("hwmon"))
			.map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()).collect())
			.unwrap_or_default();
		let intel_frequency = match device.driver.as_deref() {
			Some("i915") => first_existing(
				std::fs::read_dir(device.sysfs.join("drm"))
					.into_iter()
					.flatten()
					.filter_map(Result::ok)
					.map(|entry| entry.path().join("gt_act_freq_mhz")),
			)
			.map(|actual| (actual.clone(), actual.with_file_name("gt_cur_freq_mhz"))),
			Some("xe") => first_existing([device.sysfs.join("tile0/gt0/freq0/act_freq")])
				.map(|actual| (actual.clone(), actual.with_file_name("cur_freq"))),
			_ => None,
		};
		Self {
			device: device.sysfs.clone(),
			hwmon,
			intel_frequency,
			energy: None,
		}
	}

	pub(super) fn read(&mut self, now: Instant) -> Readings {
		let mut readings = Readings {
			busy_percent: number(&self.device.join("gpu_busy_percent")),
			encoder_busy_percent: number(&self.device.join("vcn_busy_percent")),
			vram_used_bytes: integer(&self.device.join("mem_info_vram_used")),
			vram_total_bytes: integer(&self.device.join("mem_info_vram_total")),
			core_clock_mhz: self
				.intel_frequency
				.as_ref()
				.and_then(|(actual, requested)| number(actual).filter(|mhz| *mhz > 0.0).or_else(|| number(requested))),
			..Default::default()
		};
		for hwmon in &self.hwmon {
			read_hwmon(hwmon, &mut readings);
		}
		if readings.power_w.is_none() {
			readings.power_w = self.power_from_energy(now);
		}
		readings
	}

	/// Average power since the previous read, from the first `energy*_input`.
	fn power_from_energy(&mut self, now: Instant) -> Option<f64> {
		let energy = self
			.hwmon
			.iter()
			.find_map(|hwmon| integer(&hwmon.join("energy1_input")))?;
		let previous = self.energy.replace((energy, now));
		let (last, at) = previous?;
		let seconds = now.saturating_duration_since(at).as_secs_f64();
		(seconds > 0.0 && energy >= last).then(|| (energy - last) as f64 / 1e6 / seconds)
	}
}

fn first_existing(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
	paths.into_iter().find(|path| path.exists())
}

fn read_hwmon(hwmon: &Path, readings: &mut Readings) {
	let label = |prefix: &str| {
		std::fs::read_to_string(hwmon.join(format!("{prefix}_label")))
			.map(|label| label.trim().to_ascii_lowercase())
			.ok()
	};
	for index in 1..=8 {
		let prefix = format!("temp{index}");
		let Some(millidegrees) = number(&hwmon.join(format!("{prefix}_input"))) else {
			continue;
		};
		let celsius = millidegrees / 1000.0;
		match label(&prefix).as_deref() {
			Some("junction" | "hotspot") => readings.hotspot_c = readings.hotspot_c.or(Some(celsius)),
			Some("edge" | "gpu" | "pkg") | None => readings.temperature_c = readings.temperature_c.or(Some(celsius)),
			_ => {},
		}
	}
	// amdgpu reports `power1_average` (or `power1_input` on newer parts), in µW.
	let power = number(&hwmon.join("power1_average")).or_else(|| number(&hwmon.join("power1_input")));
	readings.power_w = readings.power_w.or(power.map(|microwatts| microwatts / 1e6));
	let cap = number(&hwmon.join("power1_cap")).or_else(|| number(&hwmon.join("power1_max")));
	readings.power_limit_w = readings
		.power_limit_w
		.or(cap.filter(|cap| *cap > 0.0).map(|microwatts| microwatts / 1e6));
	for index in 1..=4 {
		let prefix = format!("freq{index}");
		let Some(hertz) = number(&hwmon.join(format!("{prefix}_input"))) else {
			continue;
		};
		let mhz = Some(hertz / 1e6);
		match label(&prefix).as_deref() {
			Some("sclk") => readings.core_clock_mhz = readings.core_clock_mhz.or(mhz),
			Some("mclk") => readings.memory_clock_mhz = readings.memory_clock_mhz.or(mhz),
			_ => {},
		}
	}
}

fn number(path: &Path) -> Option<f64> {
	std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn integer(path: &Path) -> Option<u64> {
	std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn write(dir: &Path, name: &str, value: &str) {
		let path = dir.join(name);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, format!("{value}\n")).unwrap();
	}

	fn device(root: &Path, driver: &str) -> DrmDevice {
		DrmDevice {
			sysfs: root.to_path_buf(),
			pci_address: Some("0000:03:00.0".into()),
			driver: Some(driver.into()),
		}
	}

	/// The layout of an RX 9070 XT on Linux 7.2 (amdgpu).
	#[test]
	fn reads_amdgpu_sensors() {
		let root = tempfile::tempdir().unwrap();
		let dir = root.path();
		write(dir, "gpu_busy_percent", "97");
		write(dir, "vcn_busy_percent", "41");
		write(dir, "mem_info_vram_used", "3099394048");
		write(dir, "mem_info_vram_total", "17095983104");
		let hwmon = dir.join("hwmon/hwmon3");
		for (name, value) in [
			("freq1_label", "sclk"),
			("freq1_input", "2950000000"),
			("freq2_label", "mclk"),
			("freq2_input", "1258000000"),
			("temp1_label", "edge"),
			("temp1_input", "53000"),
			("temp2_label", "junction"),
			("temp2_input", "71000"),
			("temp3_label", "mem"),
			("temp3_input", "56000"),
			("power1_label", "PPT"),
			("power1_average", "245000000"),
			("power1_cap", "340000000"),
		] {
			write(&hwmon, name, value);
		}
		let readings = Sensors::new(&device(dir, "amdgpu")).read(Instant::now());
		assert_eq!(
			readings,
			Readings {
				busy_percent: Some(97.0),
				encoder_busy_percent: Some(41.0),
				core_clock_mhz: Some(2950.0),
				memory_clock_mhz: Some(1258.0),
				temperature_c: Some(53.0),
				hotspot_c: Some(71.0),
				power_w: Some(245.0),
				power_limit_w: Some(340.0),
				vram_used_bytes: Some(3_099_394_048),
				vram_total_bytes: Some(17_095_983_104),
			}
		);
	}

	/// Intel discrete GPUs report energy, not power, and their frequency
	/// outside hwmon; integrated ones have neither sensor.
	#[test]
	fn derives_power_from_energy_and_reads_the_intel_frequency() {
		let root = tempfile::tempdir().unwrap();
		let dir = root.path();
		write(dir, "drm/card1/gt_act_freq_mhz", "1650");
		write(dir, "drm/card1/gt_cur_freq_mhz", "700");
		write(dir, "hwmon/hwmon5/energy1_input", "1000000");
		let mut sensors = Sensors::new(&device(dir, "i915"));
		let start = Instant::now();
		let first = sensors.read(start);
		assert_eq!(first.core_clock_mhz, Some(1650.0));
		assert_eq!(first.power_w, None, "a rate needs two readings");
		assert_eq!(first.busy_percent, None);
		write(dir, "hwmon/hwmon5/energy1_input", "31000000");
		// Power-gated at the instant of reading: the requested frequency.
		write(dir, "drm/card1/gt_act_freq_mhz", "0");
		let second = sensors.read(start + Duration::from_secs(2));
		assert_eq!(second.power_w, Some(15.0));
		assert_eq!(second.core_clock_mhz, Some(700.0));
	}
}
