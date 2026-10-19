//! Machine-readable benchmark results (`--json`) and the measurement-side
//! samplers that feed them.
//!
//! Everything here runs in the benchmark process only: procfs/sysfs reads and
//! a loopback receiver's arrival times. Nothing is added to the streaming
//! path, and the server never runs this code.
//!
//! The document is versioned by [`SCHEMA`]. Fields are only ever added; a
//! change of meaning needs a new schema version so stored baselines remain
//! comparable. `scripts/bench_results.py` validates and compares documents.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use moonshine_core::healthcheck::{self, Capabilities};
use moonshine_core::session::stream::video::NegotiatedVideoFormat;
use serde_json::{Value, json};

use super::{LatencyDistribution, StatsSummary};

/// Version of the `--json` document.
pub(super) const SCHEMA: &str = "pyroshine-bench/1";

/// Environment variables that change what is measured; recorded verbatim.
const RECORDED_ENVIRONMENT: &[&str] = &[
	"MOONSHINE_PYROWAVE_LIBRARY",
	"MOONSHINE_CONVENTIONAL_FRAMES_IN_FLIGHT",
	"MOONSHINE_APPLICATION_UNIT",
	"MOONSHINE_LOG",
	"VK_DRIVER_FILES",
	"VK_ICD_FILENAMES",
	"VK_INSTANCE_LAYERS",
	"ANV_DEBUG",
	"RADV_DEBUG",
	"RADV_PERFTEST",
	"MESA_VK_DEVICE_SELECT",
];

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
	std::fs::read_to_string(path).ok().map(|text| text.trim().to_string())
}

/// Kernel, CPU, memory and every DRM render node with its driver and PCI ids.
/// The baseline harness adds toolchain, revisions and Vulkan driver versions.
pub(super) fn host() -> Value {
	let cpu_model = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|info| {
		info.lines()
			.find_map(|line| line.strip_prefix("model name"))
			.and_then(|rest| rest.split_once(':'))
			.map(|(_, model)| model.trim().to_string())
	});
	let memory_kib = std::fs::read_to_string("/proc/meminfo").ok().and_then(|info| {
		info.lines()
			.find_map(|line| line.strip_prefix("MemTotal:"))
			.and_then(|value| value.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
	});
	let mut nodes = Vec::new();
	if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
		let mut names: Vec<String> = entries
			.flatten()
			.filter_map(|entry| entry.file_name().into_string().ok())
			.filter(|name| name.starts_with("renderD"))
			.collect();
		names.sort();
		for name in names {
			let device = Path::new("/sys/class/drm").join(&name).join("device");
			let driver = std::fs::read_link(device.join("driver"))
				.ok()
				.and_then(|link| link.file_name()?.to_str().map(str::to_string));
			let pci_address = std::fs::canonicalize(&device)
				.ok()
				.and_then(|path| path.file_name()?.to_str().map(str::to_string));
			nodes.push(json!({
				"node": format!("/dev/dri/{name}"),
				"driver": driver,
				"pci_address": pci_address,
				"vendor_id": read_trimmed(device.join("vendor")),
				"device_id": read_trimmed(device.join("device")),
			}));
		}
	}
	json!({
		"kernel": read_trimmed("/proc/sys/kernel/osrelease"),
		"cpu_model": cpu_model,
		"logical_cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
		"memory_kib": memory_kib,
		"cpu_governor": read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
		"render_nodes": nodes,
	})
}

pub(super) fn environment() -> Value {
	RECORDED_ENVIRONMENT
		.iter()
		.filter_map(|name| Some((name.to_string(), Value::String(std::env::var(name).ok()?))))
		.collect::<serde_json::Map<_, _>>()
		.into()
}

/// The capture GPU's probed capabilities, as the server would advertise them.
pub(super) fn capabilities(capabilities: &Capabilities) -> Value {
	json!({
		"gpu_name": capabilities.gpu_name,
		"supported_codecs_mask": capabilities.supported_codecs,
		"profiles": healthcheck::CODEC_PROFILES
			.iter()
			.filter(|(bit, _, _)| capabilities.supported_codecs & bit != 0)
			.map(|(_, id, _)| *id)
			.collect::<Vec<_>>(),
		"hdr_supported": capabilities.hdr_supported,
		"dma_buf_supported": capabilities.dma_buf_supported,
		"driver": capabilities.gpu.driver,
		"vulkan_video_encode": capabilities.gpu.vulkan_video_encode,
		// Every probed profile, with the reason it is not advertised.
		"profile_probes": capabilities
			.gpu
			.profiles
			.iter()
			.map(|probe| {
				let id = healthcheck::CODEC_PROFILES
					.iter()
					.find(|(bit, _, _)| *bit == probe.bit)
					.map_or("unknown", |(_, id, _)| *id);
				json!({
					"profile": id,
					"available": probe.unavailable.is_none(),
					"reason": probe.unavailable,
				})
			})
			.collect::<Vec<_>>(),
	})
}

/// Why a workload cannot run on the probed GPU, or `None` when it can.
pub(super) fn unsupported_reason(capabilities: &Capabilities, format: NegotiatedVideoFormat) -> Option<String> {
	if !capabilities.dma_buf_supported {
		return Some("the capture GPU cannot import DMA-BUFs".to_string());
	}
	if format.hdr && !capabilities.hdr_supported {
		return Some("the capture GPU does not support HDR capture".to_string());
	}
	if !healthcheck::supports_video_format(capabilities.supported_codecs, format) {
		return Some(format!(
			"the capture GPU does not encode {} {} {}-bit {}",
			format.codec,
			format.chroma,
			format.bit_depth.bits(),
			if format.hdr { "HDR" } else { "SDR" }
		));
	}
	None
}

pub(super) fn distribution(distribution: &LatencyDistribution) -> Value {
	json!({
		"avg": round(distribution.avg_us),
		"min": distribution.min_us,
		"p50": distribution.p50_us,
		"p95": distribution.p95_us,
		"p99": distribution.p99_us,
		"max": distribution.max_us,
	})
}

fn round(value: f64) -> f64 {
	(value * 100.0).round() / 100.0
}

pub(super) fn summary(summary: &StatsSummary) -> Value {
	json!({
		"frames": summary.count,
		"elapsed_s": round(summary.elapsed_s),
		"fps": round(summary.fps),
		"encoded_mbps": round(summary.mbps),
		"submitted_udp_payload_mbps": round(summary.wire_mbps),
		"key_frames": summary.key_frames,
		"stale_frames_dropped": summary.stale_frames_dropped,
		"vrr_frames": summary.vrr_frames,
		"latency_us": {
			"total": distribution(&summary.total),
			"frame_age": distribution(&summary.age),
			"submit": distribution(&summary.submit),
			"encode_wait": distribution(&summary.encode_wait),
			"slot_wait": distribution(&summary.slot_wait),
			"source_interval": distribution(&summary.source_interval),
		},
		"breakdown_avg_us": {
			"channel_wait": round(summary.avg_channel_wait_us),
			"import": round(summary.avg_import_us),
			"convert": round(summary.avg_convert_us),
			"submit": round(summary.submit.avg_us),
			"encode_wait": round(summary.encode_wait.avg_us),
			"packetize": round(summary.avg_packetize_us),
			"enqueue": round(summary.avg_enqueue_us),
			"send": round(summary.avg_send_us),
			"consumer_queue": round(summary.avg_consumer_queue_us),
			"accounted": round(summary.avg_accounted_us()),
			"delta": round(summary.avg_delta_us()),
		},
		"outstanding_at_submit": {
			"1": summary.outstanding[0],
			"2": summary.outstanding[1],
			"3": summary.outstanding[2],
			"4+": summary.outstanding[3],
		},
		"frame_size_bytes": {
			"avg": round(summary.encoded_size.avg_bytes),
			"p50": summary.encoded_size.p50_bytes,
			"p95": summary.encoded_size.p95_bytes,
			"p99": summary.encoded_size.p99_bytes,
			"max": summary.encoded_size.max_bytes,
			"packets_per_frame": round(summary.avg_packets_per_frame),
		},
		"packets": {
			"attempted": summary.attempted_packets,
			"submitted": summary.submitted_packets,
			"failed": summary.failed_packets,
			"discarded": summary.discarded_packets,
		},
		"capture": {
			"direct": summary.capture_paths[0],
			"direct_late": summary.capture_paths[1],
			"composited": summary.capture_paths[2],
			"composition_reasons": summary.composition_reasons,
		},
	})
}

/// First-packet arrival of each video frame at the loopback receiver, after
/// warmup. Arrival intervals are the frame pacing a client on loopback sees;
/// they include the benchmark task's own scheduling delay.
#[derive(Default)]
pub(super) struct ArrivalTrace {
	pub intervals_us: Vec<u64>,
	pub frames: u64,
	pub last: Option<Instant>,
}

impl ArrivalTrace {
	pub(super) fn record(&mut self, at: Instant) {
		self.frames += 1;
		if let Some(last) = self.last.replace(at) {
			self.intervals_us.push(at.duration_since(last).as_micros() as u64);
		}
	}
}

/// Frame pacing judged against the target interval: intervals longer than
/// 1.5 target intervals are late, and each whole extra interval is a frame
/// slot without a delivered frame.
pub(super) fn pacing(trace: &ArrivalTrace, target_fps: u32, elapsed: Duration, delivered: u64) -> Value {
	let target_us = 1_000_000.0 / f64::from(target_fps.max(1));
	let samples = &trace.intervals_us;
	let late = samples
		.iter()
		.filter(|&&interval| interval as f64 > target_us * 1.5)
		.count();
	let missed_slots: u64 = samples
		.iter()
		.map(|&interval| ((interval as f64 / target_us).round() as u64).saturating_sub(1))
		.sum();
	let mean = if samples.is_empty() {
		0.0
	} else {
		samples.iter().sum::<u64>() as f64 / samples.len() as f64
	};
	let stddev = if samples.len() < 2 {
		0.0
	} else {
		(samples.iter().map(|&s| (s as f64 - mean).powi(2)).sum::<f64>() / (samples.len() - 1) as f64).sqrt()
	};
	let expected = (elapsed.as_secs_f64() * f64::from(target_fps)).round() as u64;
	json!({
		"target_interval_us": round(target_us),
		"expected_frames": expected,
		"delivered_frames": delivered,
		"delivery_ratio": if expected > 0 { round(delivered as f64 / expected as f64) } else { 0.0 },
		"arrival": {
			"frames": trace.frames,
			"intervals": samples.len(),
			"interval_us": distribution(&LatencyDistribution::new(samples.iter().map(|&s| u128::from(s)).sum(), samples)),
			"stddev_us": round(stddev),
			"late_intervals": late,
			"missed_frame_slots": missed_slots,
		},
	})
}

/// CPU seconds, per-engine GPU busy time of this process's DRM clients and
/// memory/handle counts.
#[derive(Clone)]
pub(super) struct ProcessSnapshot {
	at: Instant,
	cpu_s: f64,
	gpu_ns: BTreeMap<String, u64>,
}

impl ProcessSnapshot {
	pub(super) fn take() -> Self {
		let (cpu_s, gpu_ns) = super::process_usage();
		Self {
			at: Instant::now(),
			cpu_s,
			gpu_ns,
		}
	}
}

/// `VmRSS`/`VmHWM` in KiB, threads, open descriptors and how many of those
/// are DMA-BUFs.
pub(super) fn process_counts() -> Value {
	let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
	let field = |name: &str| {
		status
			.lines()
			.find_map(|line| line.strip_prefix(name))
			.and_then(|value| value.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
	};
	json!({
		"rss_kib": field("VmRSS:"),
		"peak_rss_kib": field("VmHWM:"),
		"threads": std::fs::read_dir("/proc/self/task").map(|dir| dir.count()).ok(),
		"fds": std::fs::read_dir("/proc/self/fd").map(|dir| dir.count()).ok(),
		"dmabuf_fds": dmabuf_fds(),
	})
}

/// Open descriptors of this process that refer to DMA-BUFs.
pub(super) fn dmabuf_fds() -> usize {
	let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
		return 0;
	};
	entries
		.flatten()
		.filter_map(|entry| std::fs::read_link(entry.path()).ok())
		.filter(|target| {
			let target = target.to_string_lossy();
			target.starts_with("/dmabuf:") || target.starts_with("anon_inode:dmabuf")
		})
		.count()
}

/// CPU (percent of one core) and GPU engine busy milliseconds per second of
/// this process between two snapshots. The application runs in its own unit
/// and is not included. Drivers without DRM fdinfo (NVIDIA's proprietary
/// driver) report no engines; the baseline harness samples `nvidia-smi`.
pub(super) fn process_usage(start: &ProcessSnapshot, end: &ProcessSnapshot) -> Value {
	let elapsed = end.at.duration_since(start.at).as_secs_f64().max(f64::EPSILON);
	let engines: BTreeMap<String, f64> = end
		.gpu_ns
		.iter()
		.map(|(engine, ns)| {
			let before = start.gpu_ns.get(engine).copied().unwrap_or(0);
			(engine.clone(), round(ns.saturating_sub(before) as f64 / 1e6 / elapsed))
		})
		.collect();
	json!({
		"cpu_percent_of_one_core": round((end.cpu_s - start.cpu_s) / elapsed * 100.0),
		"gpu_engine_busy_ms_per_s": engines,
	})
}

/// Device-wide busy percentage of every GPU that exposes it (amdgpu's
/// `gpu_busy_percent`), sampled every 250 ms on a thread of its own while a
/// workload is measured. It includes all GPU users, not only Pyroshine.
pub(super) struct DeviceBusySampler {
	stop: Arc<AtomicBool>,
	thread: std::thread::JoinHandle<BTreeMap<String, Vec<u64>>>,
}

impl DeviceBusySampler {
	pub(super) fn start() -> Option<Self> {
		let files: Vec<(String, std::path::PathBuf)> = std::fs::read_dir("/sys/class/drm")
			.ok()?
			.flatten()
			.filter_map(|entry| {
				let name = entry.file_name().into_string().ok()?;
				if !name.starts_with("renderD") {
					return None;
				}
				let file = entry.path().join("device/gpu_busy_percent");
				file.exists().then(|| (format!("/dev/dri/{name}"), file))
			})
			.collect();
		if files.is_empty() {
			return None;
		}
		let stop = Arc::new(AtomicBool::new(false));
		let thread = std::thread::Builder::new()
			.name("bench-gpu-busy".into())
			.spawn({
				let stop = stop.clone();
				move || {
					let mut samples: BTreeMap<String, Vec<u64>> = BTreeMap::new();
					while !stop.load(Ordering::Relaxed) {
						for (node, file) in &files {
							if let Some(busy) = read_trimmed(file).and_then(|value| value.parse().ok()) {
								samples.entry(node.clone()).or_default().push(busy);
							}
						}
						std::thread::sleep(Duration::from_millis(250));
					}
					samples
				}
			})
			.ok()?;
		Some(Self { stop, thread })
	}

	pub(super) fn finish(self) -> Value {
		self.stop.store(true, Ordering::Relaxed);
		let samples = self.thread.join().unwrap_or_default();
		samples
			.into_iter()
			.map(|(node, mut values)| {
				values.sort_unstable();
				let average = values.iter().sum::<u64>() as f64 / values.len().max(1) as f64;
				let (_, p95, _) = super::percentiles(&values);
				(
					node,
					json!({
						"samples": values.len(),
						"avg_percent": round(average),
						"p95_percent": p95,
						"max_percent": values.last().copied(),
					}),
				)
			})
			.collect::<serde_json::Map<_, _>>()
			.into()
	}
}

/// The whole `--json` document.
pub(super) fn document(capabilities: Option<&Capabilities>, runs: Vec<Value>, started_unix: u64) -> Value {
	json!({
		"schema": SCHEMA,
		"tool": {
			"name": "moonshine-bench",
			"version": env!("CARGO_PKG_VERSION"),
			"argv": std::env::args().collect::<Vec<_>>(),
		},
		"started_at_unix": started_unix,
		"host": host(),
		"environment": environment(),
		"capabilities": capabilities.map(self::capabilities),
		"runs": runs,
	})
}

pub(super) fn write(path: &Path, document: &Value) -> std::io::Result<()> {
	let mut text = serde_json::to_string_pretty(document).map_err(std::io::Error::other)?;
	text.push('\n');
	std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pacing_counts_late_intervals_and_missed_slots() {
		let start = Instant::now();
		let mut trace = ArrivalTrace::default();
		// 60 FPS: on time, on time, one dropped slot (33 ms), on time.
		for offset_ms in [0, 17, 33, 67, 83] {
			trace.record(start + Duration::from_millis(offset_ms));
		}
		let pacing = pacing(&trace, 60, Duration::from_secs(1), 60);
		assert_eq!(pacing["arrival"]["intervals"], 4);
		assert_eq!(pacing["arrival"]["late_intervals"], 1);
		assert_eq!(pacing["arrival"]["missed_frame_slots"], 1);
		assert_eq!(pacing["expected_frames"], 60);
		assert_eq!(pacing["delivery_ratio"], 1.0);
		assert_eq!(pacing["arrival"]["interval_us"]["max"], 34_000);
	}

	#[test]
	fn empty_pacing_is_well_formed() {
		let pacing = pacing(&ArrivalTrace::default(), 120, Duration::ZERO, 0);
		assert_eq!(pacing["arrival"]["intervals"], 0);
		assert_eq!(pacing["delivery_ratio"], 0.0);
		assert_eq!(pacing["arrival"]["interval_us"]["p99"], 0);
	}

	#[test]
	fn unsupported_formats_name_the_missing_capability() {
		use moonshine_core::session::stream::video::{BitDepth, ChromaFormat, ColorRange, VideoCodec};
		let capabilities = Capabilities {
			supported_codecs: healthcheck::CODEC_H264 | healthcheck::CODEC_HEVC | healthcheck::CODEC_HEVC_MAIN10,
			hdr_supported: false,
			dma_buf_supported: true,
			gpu_name: "test".into(),
			gpu: Default::default(),
		};
		let h264 = NegotiatedVideoFormat::sdr(
			VideoCodec::H264,
			ChromaFormat::Yuv420,
			BitDepth::Eight,
			ColorRange::Limited,
		);
		assert_eq!(unsupported_reason(&capabilities, h264), None);
		let av1 = NegotiatedVideoFormat::sdr(
			VideoCodec::Av1,
			ChromaFormat::Yuv420,
			BitDepth::Eight,
			ColorRange::Limited,
		);
		assert!(unsupported_reason(&capabilities, av1).unwrap().contains("AV1"));
		let hdr = NegotiatedVideoFormat::hdr10(VideoCodec::Hevc, ChromaFormat::Yuv420, ColorRange::Limited);
		assert!(unsupported_reason(&capabilities, hdr).unwrap().contains("HDR"));
		let no_dmabuf = Capabilities {
			dma_buf_supported: false,
			..capabilities
		};
		assert!(unsupported_reason(&no_dmabuf, h264).unwrap().contains("DMA-BUF"));
	}

	#[test]
	fn document_carries_schema_host_and_runs() {
		let document = document(None, vec![json!({"status": "ok"})], 1);
		assert_eq!(document["schema"], SCHEMA);
		assert!(document["host"]["render_nodes"].is_array());
		assert_eq!(document["runs"][0]["status"], "ok");
		assert!(document["capabilities"].is_null());
	}
}
