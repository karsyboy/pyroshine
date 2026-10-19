use std::ffi::{CStr, CString};
use std::io::IsTerminal;
use std::io::Write;
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ash::vk::TaggedStructure;
use pixelforge::{
	Codec, EncodeBitDepth, EncodeConfig, Encoder, EncoderCapabilities, PixelFormat, RateControlMode, VideoContext,
};

use crate::config::Config;
use crate::session::stream::video::pyrowave::{PyroWaveEncoder, SOURCE_REVISION, SOURCE_URL};
use crate::session::stream::video::{
	BitDepth, ChromaFormat, ConversionQueueMode, NegotiatedVideoFormat, VideoCodec, convert,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
	Passed,
	Warning,
	Failed,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
	pub name: &'static str,
	pub outcome: CheckOutcome,
	pub message: String,
	pub duration_ms: u64,
}

#[derive(Clone, Debug)]
pub struct HealthReport {
	pub checks: Vec<CheckResult>,
	pub all_fatal_passed: bool,
	pub supported_codecs: u32,
	pub hdr_supported: bool,
	pub dma_buf_supported: bool,
	pub gpu_name: String,
	pub gpu: GpuDetails,
	pub duration: Duration,
}

/// The probed capture GPU in detail: what the advertised capabilities were
/// derived from, and why each profile that is not advertised failed.
#[derive(Clone, Debug, Default)]
pub struct GpuDetails {
	/// Vulkan driver name and version (`driverName`/`driverInfo`).
	pub driver: String,
	/// Vulkan Video encode support of the capture device (see `gpu::encode_status`).
	pub vulkan_video_encode: String,
	/// Every profile in [`CODEC_PROFILES`] order that was probed.
	pub profiles: Vec<ProfileProbe>,
}

/// The probe result of one codec profile (a [`CODEC_PROFILES`] bit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileProbe {
	pub bit: u32,
	/// Why the profile is not advertised, or `None` when it is.
	pub unavailable: Option<String>,
}

/// GPU capability results produced by [`probe_capabilities`].
///
/// Unlike [`run_healthcheck`], this only runs the GPU/codec/HDR/DMA-BUF probes
/// and does not gate startup on the full set of checks, so it can be invoked
/// even when the full health check is skipped. DMA-BUF support is still
/// captured because the video pipeline cannot encode without it.
pub struct Capabilities {
	pub supported_codecs: u32,
	pub hdr_supported: bool,
	pub dma_buf_supported: bool,
	pub gpu_name: String,
	pub gpu: GpuDetails,
}

// Codec mode support bitmask values — single source of truth, also used by the
// webserver to build the advertised `ServerCodecModeSupport` bitmask from the
// probed `report.supported_codecs`.
pub const CODEC_H264: u32 = 0x00000001;
pub const CODEC_HEVC: u32 = 0x00000100;
pub const CODEC_HEVC_MAIN10: u32 = 0x00000200;
pub const CODEC_AV1_MAIN8: u32 = 0x00010000;
pub const CODEC_AV1_MAIN10: u32 = 0x00020000;
pub const CODEC_H264_HIGH_8444: u32 = 0x00040000;
pub const CODEC_HEVC_REXT_8444: u32 = 0x00080000;
pub const CODEC_HEVC_REXT_10444: u32 = 0x00100000;
pub const CODEC_AV1_HIGH_8444: u32 = 0x00200000;
pub const CODEC_AV1_HIGH_10444: u32 = 0x00400000;
/// Private Sunshine/Moonlight wire-v1 extension bits. Chroma and HDR are
/// orthogonal capabilities; SDR uses the 8-bit intermediate and HDR10 uses 10-bit.
pub const CODEC_PYROWAVE: u32 = 0x00800000;
pub const CODEC_PYROWAVE_444: u32 = 0x01000000;
pub const CODEC_PYROWAVE_HDR: u32 = 0x02000000;
pub const CODEC_PYROWAVE_MASK: u32 = CODEC_PYROWAVE | CODEC_PYROWAVE_444 | CODEC_PYROWAVE_HDR;

/// Test a negotiated mode against the exact combinations successfully probed
/// at startup.  This intentionally has no codec-name-only shortcut.
/// Codec profiles in advertisement order: capability bit, stable id, label.
pub const CODEC_PROFILES: &[(u32, &str, &str)] = &[
	(CODEC_H264, "h264", "H.264"),
	(CODEC_H264_HIGH_8444, "h264_444", "H.264 4:4:4"),
	(CODEC_HEVC, "hevc", "HEVC"),
	(CODEC_HEVC_MAIN10, "hevc_main10", "HEVC Main10"),
	(CODEC_HEVC_REXT_8444, "hevc_444", "HEVC 4:4:4"),
	(CODEC_HEVC_REXT_10444, "hevc_444_10", "HEVC 4:4:4 10-bit"),
	(CODEC_AV1_MAIN8, "av1", "AV1"),
	(CODEC_AV1_MAIN10, "av1_main10", "AV1 10-bit"),
	(CODEC_AV1_HIGH_8444, "av1_444", "AV1 4:4:4"),
	(CODEC_AV1_HIGH_10444, "av1_444_10", "AV1 4:4:4 10-bit"),
	(CODEC_PYROWAVE, "pyrowave", "PyroWave"),
	(CODEC_PYROWAVE_444, "pyrowave_444", "PyroWave 4:4:4"),
	(CODEC_PYROWAVE_HDR, "pyrowave_hdr", "PyroWave HDR10"),
];

/// Labels of the profiles in `capabilities`, in advertisement order.
pub fn codec_profile_labels(capabilities: u32) -> Vec<&'static str> {
	CODEC_PROFILES
		.iter()
		.filter(|(bit, _, _)| capabilities & bit != 0)
		.map(|(_, _, label)| *label)
		.collect()
}

pub fn supports_video_format(capabilities: u32, format: NegotiatedVideoFormat) -> bool {
	if format.codec == VideoCodec::PyroWave {
		if format.range != crate::session::stream::video::ColorRange::Full {
			return false;
		}
		let chroma_supported = match format.chroma {
			ChromaFormat::Yuv420 => capabilities & CODEC_PYROWAVE != 0,
			ChromaFormat::Yuv444 => capabilities & CODEC_PYROWAVE_444 != 0,
		};
		let dynamic_range_supported = match (format.bit_depth, format.hdr) {
			(BitDepth::Eight, false) => true,
			(BitDepth::Ten, true) => capabilities & CODEC_PYROWAVE_HDR != 0,
			_ => false,
		};
		return chroma_supported && dynamic_range_supported;
	}

	let bit = match (format.codec, format.chroma, format.bit_depth) {
		(VideoCodec::H264, ChromaFormat::Yuv420, BitDepth::Eight) => CODEC_H264,
		(VideoCodec::H264, ChromaFormat::Yuv444, BitDepth::Eight) => CODEC_H264_HIGH_8444,
		(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Eight) => CODEC_HEVC,
		(VideoCodec::Hevc, ChromaFormat::Yuv420, BitDepth::Ten) => CODEC_HEVC_MAIN10,
		(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Eight) => CODEC_HEVC_REXT_8444,
		(VideoCodec::Hevc, ChromaFormat::Yuv444, BitDepth::Ten) => CODEC_HEVC_REXT_10444,
		(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Eight) => CODEC_AV1_MAIN8,
		(VideoCodec::Av1, ChromaFormat::Yuv420, BitDepth::Ten) => CODEC_AV1_MAIN10,
		(VideoCodec::Av1, ChromaFormat::Yuv444, BitDepth::Eight) => CODEC_AV1_HIGH_8444,
		(VideoCodec::Av1, ChromaFormat::Yuv444, BitDepth::Ten) => CODEC_AV1_HIGH_10444,
		_ => return false,
	};
	capabilities & bit != 0
}

impl HealthReport {
	fn empty() -> Self {
		Self {
			checks: Vec::new(),
			all_fatal_passed: true,
			supported_codecs: 0,
			hdr_supported: false,
			dma_buf_supported: false,
			gpu_name: String::new(),
			gpu: GpuDetails::default(),
			duration: Duration::ZERO,
		}
	}

	/// Record a profile's probe result; a later result for the same profile
	/// (a withdrawal) replaces the earlier one.
	fn record_profile(&mut self, bit: u32, unavailable: Option<String>) {
		match self.gpu.profiles.iter_mut().find(|probe| probe.bit == bit) {
			Some(probe) => probe.unavailable = unavailable,
			None => self.gpu.profiles.push(ProfileProbe { bit, unavailable }),
		}
	}

	fn add(&mut self, name: &'static str, outcome: CheckOutcome, message: String, duration_ms: u64) {
		if outcome == CheckOutcome::Failed {
			self.all_fatal_passed = false;
		}
		self.checks.push(CheckResult {
			name,
			outcome,
			message,
			duration_ms,
		});
	}

	fn add_passed(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Passed, message, duration_ms);
	}

	fn add_failed(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Failed, message, duration_ms);
	}

	fn add_warn(&mut self, name: &'static str, message: String, duration_ms: u64) {
		self.add(name, CheckOutcome::Warning, message, duration_ms);
	}
}

/// Return `(all_fatal_passed, fatal_count, warning_count)`.
pub fn health_summary(report: &HealthReport) -> (bool, usize, usize) {
	let fatal = report
		.checks
		.iter()
		.filter(|c| c.outcome == CheckOutcome::Failed)
		.count();
	let warn = report
		.checks
		.iter()
		.filter(|c| c.outcome == CheckOutcome::Warning)
		.count();
	(report.all_fatal_passed, fatal, warn)
}

pub fn log_health_report(report: &HealthReport) {
	iter_checks(report, |outcome, name, msg| match outcome {
		CheckOutcome::Passed => {
			tracing::debug!("{:>15}  OK   {msg}", name);
		},
		CheckOutcome::Failed => {
			tracing::error!("{:>15}  FAIL\n{msg}", name);
		},
		CheckOutcome::Warning => {
			tracing::warn!("{:>15}  WARN\n{msg}", name);
		},
	});

	let (passed, fatal, warn) = health_summary(report);
	if passed {
		if warn > 0 {
			tracing::debug!(
				"Health checks passed in {}ms ({} warnings).",
				report.duration.as_millis(),
				warn
			);
		} else {
			tracing::debug!("Health checks passed in {}ms.", report.duration.as_millis());
		}
	} else {
		tracing::error!(
			"Health checks FAILED in {}ms ({} errors, {} warnings). Fix issues above or use --no-health-check.",
			report.duration.as_millis(),
			fatal,
			warn,
		);
	}
}

/// Invoke `f` for every check with its outcome, name and message.
fn iter_checks(report: &HealthReport, mut f: impl FnMut(CheckOutcome, &str, &str)) {
	for check in &report.checks {
		f(check.outcome, check.name, &check.message);
	}
}

/// Run the health check and log the results to the console.
pub fn print_health_report(report: &HealthReport) {
	let tty = std::io::stdout().is_terminal();
	let (red, green, yellow, reset) = if tty {
		("\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[m")
	} else {
		("", "", "", "")
	};

	iter_checks(report, |outcome, name, msg| match outcome {
		CheckOutcome::Passed => {
			println!("  {green}OK{reset}    {:>15}  {msg}", name);
		},
		CheckOutcome::Failed => {
			println!("  {red}FAIL{reset}  {:>15}", name);
			for line in msg.lines() {
				println!("        {line}");
			}
		},
		CheckOutcome::Warning => {
			println!("  {yellow}WARN{reset}  {:>15}", name);
			for line in msg.lines() {
				println!("        {line}");
			}
		},
	});

	let (passed, fatal, warn) = health_summary(report);
	println!();
	if passed {
		if warn > 0 {
			println!(
				"{green}Health checks passed{reset} in {}ms ({} warnings).",
				report.duration.as_millis(),
				warn
			);
		} else {
			println!(
				"{green}All health checks passed{reset} in {}ms.",
				report.duration.as_millis()
			);
		}
	} else {
		println!(
			"{red}Health checks FAILED{reset} in {}ms ({} errors, {} warnings).",
			report.duration.as_millis(),
			fatal,
			warn,
		);
	}
}

/// Probe GPU capabilities and HDR support without the full health check.
///
/// Runs the render-node, EGL, Vulkan, codec and DMA-BUF probes and records
/// their results. This is the value-producing part of [`run_healthcheck`] and
/// is invoked unconditionally so the server always advertises real capabilities.
pub fn probe_capabilities(config: Option<&Config>) -> Capabilities {
	let gpu_config = config.map(|c| c.compositor.gpu.clone()).unwrap_or(None);
	let conversion_queue = conversion_queue(config);

	let mut report = HealthReport::empty();

	run_gpu_checks(&mut report, &gpu_config, conversion_queue);
	// --no-health-check discards the report, but identity/selection failures
	// and profiles withdrawn for wrong colors must still reach the
	// administrator before the generic startup gate.
	for check in report.checks.iter().filter(|check| {
		(check.outcome == CheckOutcome::Failed
			&& matches!(check.name, "GPU config" | "Render access" | "EGL/GLES" | "Vulkan"))
			|| (check.outcome != CheckOutcome::Passed && check.name == COLOR_CONVERSION)
	}) {
		tracing::error!(check = check.name, error = %check.message, "GPU capability probe failed");
	}

	Capabilities {
		supported_codecs: report.supported_codecs,
		hdr_supported: report.hdr_supported,
		dma_buf_supported: report.dma_buf_supported,
		gpu_name: report.gpu_name,
		gpu: report.gpu,
	}
}

fn conversion_queue(config: Option<&Config>) -> ConversionQueueMode {
	config
		.map(|config| config.stream.video.conversion_queue)
		.unwrap_or_default()
}

/// Run the GPU/codec/HDR probes and record their results into `report`.
fn run_gpu_checks(report: &mut HealthReport, gpu_config: &Option<String>, conversion_queue: ConversionQueueMode) {
	check_render_nodes(report, gpu_config);
	let render_node_open = check_render_node_open(report, gpu_config);
	// Prefer the configured/accessible node over the first discovered one, so
	// the EGL probe (HDR detection) runs on the GPU the compositor will use.
	let egl_result = check_egl(report, render_node_open.as_ref());
	let vk_context = check_vulkan(report, render_node_open.as_ref().filter(|_| egl_result.is_some()));
	check_codecs(report, vk_context.as_ref());
	check_color_conversion(report, vk_context.as_ref(), conversion_queue);
	report.dma_buf_supported = check_dmabuf(report, vk_context.as_ref());

	if let Some(ref ctx) = vk_context {
		let props = ctx.device_properties();
		let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
			.to_string_lossy()
			.to_string();
		report.gpu_name = name;
	} else if let Some((ref name, _, _)) = egl_result {
		report.gpu_name = name.clone();
	}

	if let Some((_, hdr, _)) = &egl_result {
		report.hdr_supported = *hdr && vk_context.is_some() && report.dma_buf_supported;
	}
}

/// Checks whose probes create GPU encoders or contexts. A re-run inside the
/// server repeats them only while no session runs.
pub const GPU_CHECKS: &[&str] = &[
	"Render nodes",
	"GPU config",
	"Render access",
	"EGL/GLES",
	"Vulkan",
	"Codecs",
	COLOR_CONVERSION,
	"PyroWave",
	"DMA-BUF",
];

/// Run the checks again inside the running server (the web interface's
/// "Run checks"). Unlike [`run_healthcheck`] it never binds the server's own
/// listeners, and while a session runs it reports the GPU probes of
/// `previous` instead of creating encoders next to the live stream.
pub fn recheck_in_server(config: &Config, previous: Option<&HealthReport>, session_active: bool) -> HealthReport {
	let started = Instant::now();
	let mut report = HealthReport::empty();
	match previous.filter(|_| session_active) {
		None if session_active => report.add_warn(
			"GPU",
			"  GPU probes were not run: a session is streaming.\n  Run the checks again when no session runs.".into(),
			0,
		),
		Some(previous) => {
			for check in previous.checks.iter().filter(|check| GPU_CHECKS.contains(&check.name)) {
				let message = format!(
					"{}\n  Result of the previous check: GPU probes do not run while a session streams.",
					check.message.trim_end()
				);
				report.add(check.name, check.outcome, message, check.duration_ms);
			}
			report.supported_codecs = previous.supported_codecs;
			report.hdr_supported = previous.hdr_supported;
			report.dma_buf_supported = previous.dma_buf_supported;
			report.gpu_name = previous.gpu_name.clone();
			report.gpu = previous.gpu.clone();
		},
		None => run_gpu_checks(&mut report, &config.compositor.gpu, conversion_queue(Some(config))),
	}
	check_xwayland(&mut report);
	check_xdg_runtime(&mut report);
	check_inhibit(&mut report);
	check_kcmp(&mut report);
	check_ports_in_server(&mut report, config, session_active);
	check_uinput(&mut report);
	check_uhid(&mut report);
	check_input_group(&mut report);
	check_dirs(&mut report, config);
	check_frame_limit(&mut report, config);
	report.duration = started.elapsed();
	report
}

pub fn run_healthcheck(config: Option<&Config>) -> HealthReport {
	let started = Instant::now();

	let gpu_config = config.map(|c| c.compositor.gpu.clone()).unwrap_or(None);

	let mut report = HealthReport::empty();

	// --- GPU checks ---

	run_gpu_checks(&mut report, &gpu_config, conversion_queue(config));

	// --- External dependencies ---

	check_xwayland(&mut report);
	check_xdg_runtime(&mut report);
	check_inhibit(&mut report);
	check_kcmp(&mut report);

	// --- Ports (requires config) ---

	if let Some(config) = config {
		check_ports(&mut report, config);
	}

	check_uinput(&mut report);
	check_uhid(&mut report);
	check_input_group(&mut report);

	if let Some(config) = config {
		check_dirs(&mut report, config);
		check_frame_limit(&mut report, config);
	}

	report.duration = started.elapsed();
	report
}

// ---------------------------------------------------------------------------
// GPU helpers
// ---------------------------------------------------------------------------

fn group_name_of(path: &Path) -> String {
	let Ok(meta) = std::fs::metadata(path) else {
		return "unknown".into();
	};
	let gid = meta.st_gid();
	let grp = unsafe { libc::getgrgid(gid) };
	if grp.is_null() {
		format!("gid {gid}")
	} else {
		unsafe { CStr::from_ptr((*grp).gr_name) }.to_string_lossy().into_owned()
	}
}

/// GID of a group by name, or `None` if it doesn't exist.
fn group_gid(name: &str) -> Option<libc::gid_t> {
	let cname = CString::new(name).ok()?;
	let grp = unsafe { libc::getgrnam(cname.as_ptr()) };
	if grp.is_null() {
		None
	} else {
		Some(unsafe { (*grp).gr_gid })
	}
}

/// Whether the current process is a member of the given group (primary or supplementary).
fn is_member_of_group(name: &str) -> bool {
	let Some(gid) = group_gid(name) else {
		return false;
	};
	if unsafe { libc::getgid() } == gid {
		return true;
	}
	let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
	if n <= 0 {
		return false;
	}
	let mut groups = vec![0 as libc::gid_t; n as usize];
	let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
	if n < 0 {
		return false;
	}
	groups.iter().take(n as usize).any(|&g| g == gid)
}

fn check_render_nodes(report: &mut HealthReport, _gpu_config: &Option<String>) -> Option<PathBuf> {
	let start = Instant::now();
	let dri = Path::new("/dev/dri");
	if !dri.exists() {
		report.add_failed(
			"Render nodes",
			"  No /dev/dri directory found.\n  Ensure the GPU kernel DRM driver is loaded and /dev/dri exists.\n  Ubuntu: `sudo ubuntu-drivers autoinstall` (or install matching kernel/firmware)\n  Fedora: `sudo dnf install mesa-dri-drivers`\n  Arch: `sudo pacman -S mesa`".into(),
			start.elapsed().as_millis() as u64,
		);
		return None;
	}

	let mut entries: Vec<_> = match std::fs::read_dir(dri) {
		Ok(e) => e
			.filter_map(|e| e.ok())
			.filter(|e| {
				e.file_name()
					.to_str()
					.map(|n| n.starts_with("renderD"))
					.unwrap_or(false)
			})
			.collect(),
		Err(e) => {
			report.add_failed(
				"Render nodes",
				format!("  Cannot read /dev/dri: {e}\n  Check directory permissions."),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	entries.sort_by_key(|e| e.file_name());

	if entries.is_empty() {
		report.add_failed(
			"Render nodes",
			"  No render nodes found in /dev/dri.\n  Install GPU drivers with render node support.\n  Ubuntu: `sudo ubuntu-drivers autoinstall`\n  Fedora: `sudo dnf install mesa-dri-drivers`\n  Arch: `sudo pacman -S mesa`".into(),
			start.elapsed().as_millis() as u64,
		);
		return None;
	}

	let names: Vec<String> = entries
		.iter()
		.map(|e| e.file_name().to_string_lossy().into_owned())
		.collect();
	report.add_passed("Render nodes", names.join(", "), start.elapsed().as_millis() as u64);

	Some(entries[0].path())
}

fn check_render_node_open(report: &mut HealthReport, gpu_config: &Option<String>) -> Option<PathBuf> {
	let start = Instant::now();
	let node = match find_render_node(gpu_config) {
		Ok(n) => n,
		Err(e) => {
			// No diagnostic fallback: even --no-health-check must not advertise
			// an unrelated GPU when the runtime selection cannot be resolved.
			report.add_failed(
				"GPU config",
				format!("  Cannot resolve capture render node: {e}"),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	match std::fs::OpenOptions::new().read(true).write(true).open(&node) {
		Ok(_fd) => {
			let group = group_name_of(&node);
			report.add_passed(
				"Render access",
				format!("{} (owned by group '{group}')", node.display()),
				start.elapsed().as_millis() as u64,
			);
			Some(node)
		},
		Err(e) => {
			let group = group_name_of(&node);
			report.add_failed(
				"Render access",
				format!(
					"  Cannot open {}: {e}\n  The node is owned by group '{group}'.\n  Add your user to this group: `sudo usermod -aG {group} $USER`\n  Then log out and back in, or use systemd service with SupplementaryGroups={group}.",
					node.display(),
				),
				start.elapsed().as_millis() as u64,
			);
			None
		},
	}
}

fn check_egl(report: &mut HealthReport, node: Option<&PathBuf>) -> Option<(String, bool, Vec<String>)> {
	let start = Instant::now();
	let node = node?;

	let render_fd = match std::fs::OpenOptions::new().read(true).write(true).open(node) {
		Ok(fd) => fd,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!("  Cannot open render node for EGL probe: {e}"),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let gbm_device = match smithay::backend::allocator::gbm::GbmDevice::new(render_fd) {
		Ok(d) => d,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!(
					"  Failed to create GBM device: {e}\n  libgbm is required for GPU rendering.\n  Ubuntu: `sudo apt install libgbm1`\n  Fedora: `sudo dnf install mesa-libgbm`\n  Arch: `sudo pacman -S mesa`"
				),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let egl_display = match unsafe { smithay::backend::egl::EGLDisplay::new(gbm_device) } {
		Ok(d) => d,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!(
					"  Failed to create EGL display: {e}\n  GPU drivers may be missing or incomplete.\n  Ubuntu: `sudo ubuntu-drivers autoinstall`\n  Fedora: `sudo dnf install mesa-libEGL`\n  Arch: `sudo pacman -S mesa`"
				),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let egl_context = match smithay::backend::egl::EGLContext::new(&egl_display) {
		Ok(c) => c,
		Err(e) => {
			report.add_failed(
				"EGL/GLES",
				format!("  Failed to create EGL context: {e}\n  GPU does not support the required OpenGL ES features."),
				start.elapsed().as_millis() as u64,
			);
			return None;
		},
	};

	let render_formats = egl_context.dmabuf_render_formats();
	let mut format_names: Vec<String> = render_formats.iter().map(|f| format!("{:?}", f.code)).collect();
	format_names.sort();
	format_names.dedup();
	let hdr_supported = render_formats.iter().any(|f| {
		matches!(
			f.code,
			smithay::backend::allocator::Fourcc::Abgr2101010 | smithay::backend::allocator::Fourcc::Abgr16161616f
		)
	});

	// Try to derive a GPU name from the render node's uevent DRIVER= field,
	// falling back to the render node path when unavailable.
	let gpu_name = node
		.file_name()
		.and_then(|n| n.to_str())
		.map(|name| Path::new("/sys/class/drm").join(name).join("device/uevent"))
		.and_then(|uevent| std::fs::read_to_string(&uevent).ok())
		.and_then(|uevent| {
			uevent
				.lines()
				.find_map(|line| line.strip_prefix("DRIVER=").map(|d| d.to_string()))
		})
		.unwrap_or_else(|| node.display().to_string());

	let msg = if hdr_supported {
		format!("{gpu_name} (HDR-capable, formats: {})", format_names.join(", "))
	} else {
		format!("{gpu_name} (SDR-only, formats: {})", format_names.join(", "))
	};

	report.add_passed("EGL/GLES", msg, start.elapsed().as_millis() as u64);
	Some((gpu_name, hdr_supported, format_names))
}

fn check_vulkan(report: &mut HealthReport, node: Option<&PathBuf>) -> Option<VideoContext> {
	let start = Instant::now();
	let context = node
		.ok_or_else(|| "No usable configured capture render node/EGL path".to_string())
		.and_then(|node| {
			let file = std::fs::OpenOptions::new()
				.read(true)
				.write(true)
				.open(node)
				.map_err(|e| format!("Cannot open {}: {e}", node.display()))?;
			crate::gpu::capture_context(&file)
		});
	match context {
		Ok(ctx) => {
			let props = ctx.device_properties();
			let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
				.to_string_lossy()
				.to_string();
			let api_version = props.api_version;
			let driver = vulkan_driver(&ctx);
			let encode = crate::gpu::encode_status(&ctx);
			let msg = format!(
				"{device_name}, {driver}, Vulkan {}.{}.{}\n  Vulkan Video encode: {encode}",
				vk_version_major(api_version),
				vk_version_minor(api_version),
				vk_version_patch(api_version),
			);
			report.gpu.driver = driver;
			report.gpu.vulkan_video_encode = encode;
			report.add_passed("Vulkan", msg, start.elapsed().as_millis() as u64);
			Some(ctx)
		},
		Err(e) => {
			let msg = format!(
				"  Failed to initialize Vulkan: {e}\n  Ensure the Vulkan driver for this GPU is installed (Mesa RADV or ANV, or NVIDIA's).\n  Ubuntu: `sudo apt install mesa-vulkan-drivers` (or the NVIDIA driver)\n  Fedora: `sudo dnf install vulkan-loader mesa-vulkan-drivers`\n  Arch: `sudo pacman -S vulkan-icd-loader vulkan-radeon` (AMD) or `vulkan-intel` (Intel)"
			);
			report.add_failed("Vulkan", msg, start.elapsed().as_millis() as u64);
			None
		},
	}
}

fn check_codecs(report: &mut HealthReport, context: Option<&VideoContext>) {
	let start = Instant::now();
	let ctx = match context {
		Some(c) => c,
		None => {
			report.add_failed(
				"Codecs",
				"  Skipped (Vulkan check failed).".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let mut supported = 0u32;
	let mut names = Vec::new();

	let candidates = [
		(
			Codec::H264,
			PixelFormat::Yuv420,
			EncodeBitDepth::Eight,
			CODEC_H264,
			"H.264 4:2:0 8-bit",
		),
		(
			Codec::H264,
			PixelFormat::Yuv444,
			EncodeBitDepth::Eight,
			CODEC_H264_HIGH_8444,
			"H.264 4:4:4 8-bit",
		),
		(
			Codec::H265,
			PixelFormat::Yuv420,
			EncodeBitDepth::Eight,
			CODEC_HEVC,
			"HEVC 4:2:0 8-bit",
		),
		(
			Codec::H265,
			PixelFormat::Yuv420,
			EncodeBitDepth::Ten,
			CODEC_HEVC_MAIN10,
			"HEVC 4:2:0 10-bit",
		),
		(
			Codec::H265,
			PixelFormat::Yuv444,
			EncodeBitDepth::Eight,
			CODEC_HEVC_REXT_8444,
			"HEVC 4:4:4 8-bit",
		),
		(
			Codec::H265,
			PixelFormat::Yuv444,
			EncodeBitDepth::Ten,
			CODEC_HEVC_REXT_10444,
			"HEVC 4:4:4 10-bit",
		),
		(
			Codec::AV1,
			PixelFormat::Yuv420,
			EncodeBitDepth::Eight,
			CODEC_AV1_MAIN8,
			"AV1 4:2:0 8-bit",
		),
		(
			Codec::AV1,
			PixelFormat::Yuv420,
			EncodeBitDepth::Ten,
			CODEC_AV1_MAIN10,
			"AV1 4:2:0 10-bit",
		),
		(
			Codec::AV1,
			PixelFormat::Yuv444,
			EncodeBitDepth::Eight,
			CODEC_AV1_HIGH_8444,
			"AV1 4:4:4 8-bit",
		),
		(
			Codec::AV1,
			PixelFormat::Yuv444,
			EncodeBitDepth::Ten,
			CODEC_AV1_HIGH_10444,
			"AV1 4:4:4 10-bit",
		),
	];

	for (codec, chroma, depth, bit, label) in candidates {
		match probe_profile(ctx, codec, chroma, depth) {
			Ok(()) => {
				supported |= bit;
				names.push(label);
				report.record_profile(bit, None);
			},
			Err(error) => {
				tracing::debug!(label, %error, "Video profile is not supported");
				report.record_profile(bit, Some(error));
			},
		}
	}

	// PyroWave is compute-based, not a Vulkan Video profile. Probe its exact
	// library ABI, same-adapter creation, external-memory interop, and both
	// chroma encoder configurations independently.
	let mut pyrowave_errors = Vec::new();
	let mut pyrowave_420_8 = false;
	let mut pyrowave_420_10 = false;
	let mut pyrowave_444_8 = false;
	let mut pyrowave_444_10 = false;
	for (chroma, depth, label) in [
		(ChromaFormat::Yuv420, BitDepth::Eight, "PyroWave 4:2:0 8-bit"),
		(ChromaFormat::Yuv420, BitDepth::Ten, "PyroWave 4:2:0 10-bit"),
		(ChromaFormat::Yuv444, BitDepth::Eight, "PyroWave 4:4:4 8-bit"),
		(ChromaFormat::Yuv444, BitDepth::Ten, "PyroWave 4:4:4 10-bit"),
	] {
		match PyroWaveEncoder::is_available(ctx, chroma, depth) {
			Ok(()) => {
				match (chroma, depth) {
					(ChromaFormat::Yuv420, BitDepth::Eight) => pyrowave_420_8 = true,
					(ChromaFormat::Yuv420, BitDepth::Ten) => pyrowave_420_10 = true,
					(ChromaFormat::Yuv444, BitDepth::Eight) => pyrowave_444_8 = true,
					(ChromaFormat::Yuv444, BitDepth::Ten) => pyrowave_444_10 = true,
				}
				names.push(label);
			},
			Err(error) => {
				tracing::debug!(%error, %chroma, bit_depth = depth.bits(), "PyroWave profile is unavailable");
				pyrowave_errors.push(format!("{chroma} {}-bit: {error}", depth.bits()));
			},
		}
	}
	let pyrowave_reason = |chroma: ChromaFormat, depth: BitDepth| {
		pyrowave_errors
			.iter()
			.find(|error| error.starts_with(&format!("{chroma} {}-bit: ", depth.bits())))
			.cloned()
			.unwrap_or_else(|| "PyroWave is unavailable".into())
	};
	let mut record_pyrowave = |bit: u32, available: bool, reason: String| {
		if available {
			supported |= bit;
		}
		report.record_profile(bit, (!available).then_some(reason));
	};
	record_pyrowave(
		CODEC_PYROWAVE,
		pyrowave_420_8,
		pyrowave_reason(ChromaFormat::Yuv420, BitDepth::Eight),
	);
	record_pyrowave(
		CODEC_PYROWAVE_444,
		pyrowave_444_8,
		pyrowave_reason(ChromaFormat::Yuv444, BitDepth::Eight),
	);
	// The HDR flag is shared by every advertised chroma mode. Only advertise
	// it when each SDR-capable mode also passed its 10-bit probe.
	let hdr = (pyrowave_420_8 || pyrowave_444_8)
		&& (!pyrowave_420_8 || pyrowave_420_10)
		&& (!pyrowave_444_8 || pyrowave_444_10);
	record_pyrowave(
		CODEC_PYROWAVE_HDR,
		hdr,
		if pyrowave_420_8 && !pyrowave_420_10 {
			pyrowave_reason(ChromaFormat::Yuv420, BitDepth::Ten)
		} else if pyrowave_444_8 && !pyrowave_444_10 {
			pyrowave_reason(ChromaFormat::Yuv444, BitDepth::Ten)
		} else {
			"no SDR PyroWave profile is available".into()
		},
	);
	if pyrowave_errors.is_empty() {
		report.add_passed(
			"PyroWave",
			format!("{SOURCE_URL} @ {SOURCE_REVISION}"),
			start.elapsed().as_millis() as u64,
		);
	} else {
		report.add_warn(
			"PyroWave",
			format!(
				"  Optional codec unavailable from {SOURCE_URL} @ {SOURCE_REVISION}:\n  {}",
				pyrowave_errors.join("\n  ")
			),
			start.elapsed().as_millis() as u64,
		);
	}

	report.supported_codecs = supported;

	let elapsed = start.elapsed().as_millis() as u64;
	let not_offered = unavailable_summary(&report.gpu.profiles, CODEC_CONVENTIONAL_MASK);
	if names.is_empty() {
		report.add_failed(
			"Codecs",
			format!(
				"  No usable video encode profile: no Vulkan Video encoder and no PyroWave.\n  Vulkan Video encode: {}\n  Not offered:\n{not_offered}",
				report.gpu.vulkan_video_encode
			),
			elapsed,
		);
	} else if supported & CODEC_CONVENTIONAL_MASK == 0 {
		// Startup proceeds: PyroWave clients can stream. Moonlight clients
		// without PyroWave are told which profiles exist when they connect.
		report.add_warn(
			"Codecs",
			format!(
				"  Only PyroWave can stream from this GPU: {}\n  No H.264, HEVC or AV1 for Moonlight clients without PyroWave.\n  Vulkan Video encode: {}",
				names.join(", "),
				report.gpu.vulkan_video_encode
			),
			elapsed,
		);
	} else if not_offered.is_empty() {
		report.add_passed("Codecs", names.join(", "), elapsed);
	} else {
		report.add_passed(
			"Codecs",
			format!("{}\n  Not offered:\n{not_offered}", names.join(", ")),
			elapsed,
		);
	}
}

/// Every conventional profile bit (Vulkan Video codecs).
const CODEC_CONVENTIONAL_MASK: u32 = CODEC_H264
	| CODEC_H264_HIGH_8444
	| CODEC_HEVC
	| CODEC_HEVC_MAIN10
	| CODEC_HEVC_REXT_8444
	| CODEC_HEVC_REXT_10444
	| CODEC_AV1_MAIN8
	| CODEC_AV1_MAIN10
	| CODEC_AV1_HIGH_8444
	| CODEC_AV1_HIGH_10444;

/// Probe a conventional profile exactly as a stream would use it: the codec
/// on this device, the profile's capabilities, constant-bitrate rate control
/// and an encoder instance. Returns the first reason it cannot be used.
fn probe_profile(ctx: &VideoContext, codec: Codec, chroma: PixelFormat, depth: EncodeBitDepth) -> Result<(), String> {
	/// Well below every encoder's maximum: probes test support, not limits.
	const PROBE_BITRATE: u32 = 10_000_000;
	if let Some(reason) = ctx.encode_unsupported_reason(codec) {
		return Err(reason.to_string());
	}
	let base = match codec {
		Codec::H264 => EncodeConfig::h264(128, 128),
		Codec::H265 => EncodeConfig::h265(128, 128),
		Codec::AV1 => EncodeConfig::av1(128, 128),
	};
	let config = base
		.with_pixel_format(chroma)
		.with_bit_depth(depth)
		.with_rate_control(RateControlMode::Cbr)
		.with_target_bitrate(PROBE_BITRATE);
	let caps = EncoderCapabilities::query(ctx, &config).map_err(|error| error.to_string())?;
	crate::session::stream::video::encode_caps::cbr_target(&caps, codec, u64::from(PROBE_BITRATE))?;
	Encoder::new(ctx.clone(), config)
		.map(drop)
		.map_err(|error| error.to_string())
}

/// Indented lines naming the unavailable profiles within `mask`, one line per
/// distinct reason.
fn unavailable_summary(profiles: &[ProfileProbe], mask: u32) -> String {
	let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
	for probe in profiles.iter().filter(|probe| probe.bit & mask != 0) {
		let Some(reason) = probe.unavailable.as_deref() else {
			continue;
		};
		let label = CODEC_PROFILES
			.iter()
			.find(|(bit, _, _)| *bit == probe.bit)
			.map_or("?", |(_, _, label)| *label);
		match groups.iter_mut().find(|(r, _)| *r == reason) {
			Some((_, labels)) => labels.push(label),
			None => groups.push((reason, vec![label])),
		}
	}
	groups
		.iter()
		.map(|(reason, labels)| format!("    {}: {reason}", labels.join(", ")))
		.collect::<Vec<_>>()
		.join("\n")
}

/// `driverName driverInfo` of the context's device, e.g. `radv Mesa 26.2.4`.
fn vulkan_driver(ctx: &VideoContext) -> String {
	let mut driver = ash::vk::PhysicalDeviceDriverProperties::default();
	let mut properties = ash::vk::PhysicalDeviceProperties2::default().push(&mut driver);
	// SAFETY: core Vulkan 1.2 query on the context's live physical device.
	unsafe {
		ctx.instance()
			.get_physical_device_properties2(ctx.physical_device(), &mut properties);
	}
	let name = driver
		.driver_name_as_c_str()
		.map(CStr::to_string_lossy)
		.unwrap_or_default();
	let info = driver
		.driver_info_as_c_str()
		.map(CStr::to_string_lossy)
		.unwrap_or_default();
	format!("{name} {info}").trim().to_string()
}

const COLOR_CONVERSION: &str = "Color conversion";

/// The converter output format each conventional profile encodes from.
const CONVERSION_PROFILES: [(pixelforge::OutputFormat, u32); 4] = [
	(
		pixelforge::OutputFormat::NV12,
		CODEC_H264 | CODEC_HEVC | CODEC_AV1_MAIN8,
	),
	(pixelforge::OutputFormat::P010, CODEC_HEVC_MAIN10 | CODEC_AV1_MAIN10),
	(
		pixelforge::OutputFormat::YUV444,
		CODEC_H264_HIGH_8444 | CODEC_HEVC_REXT_8444 | CODEC_AV1_HIGH_8444,
	),
	(
		pixelforge::OutputFormat::YUV444P10,
		CODEC_HEVC_REXT_10444 | CODEC_AV1_HIGH_10444,
	),
];

/// Run the stream converter's known-answer self-check for every output format
/// a verified profile needs, on the configured conversion queue.
///
/// Encoder probes only show that a profile can be created; a GPU or driver
/// that miscompiles the conversion shader still produces wrong colors in every
/// frame (issue #11: green, striped video on an RTX 5070 Ti). Stream setup runs
/// the same check and refuses such a converter; running it here reports the
/// defect before any client connects, and withdraws the affected profiles so
/// clients are offered only profiles that stream correctly.
fn check_color_conversion(report: &mut HealthReport, context: Option<&VideoContext>, mode: ConversionQueueMode) {
	// Without Vulkan, the Vulkan and Codecs checks already explain the failure.
	let Some(context) = context else {
		return;
	};
	record_color_conversion(report, |format| {
		convert::self_check(context, mode, format).map_err(|failure| failure.detail)
	});
}

/// [`check_color_conversion`] with the self-check supplied by the caller.
fn record_color_conversion(
	report: &mut HealthReport,
	mut self_check: impl FnMut(pixelforge::OutputFormat) -> Result<(), String>,
) {
	let start = Instant::now();
	let mut passed = Vec::new();
	let mut failures = Vec::new();
	let mut withdrawn = 0;
	for (format, profiles) in CONVERSION_PROFILES {
		if report.supported_codecs & profiles == 0 {
			continue;
		}
		match self_check(format) {
			Ok(()) => passed.push(convert::format_label(format)),
			Err(detail) => {
				failures.push(format!("{}: {detail}", convert::format_label(format)));
				withdrawn |= report.supported_codecs & profiles;
			},
		}
	}
	let elapsed = start.elapsed().as_millis() as u64;
	if failures.is_empty() {
		if !passed.is_empty() {
			report.add_passed(
				COLOR_CONVERSION,
				format!("Output matches the reference for {}", passed.join(", ")),
				elapsed,
			);
		}
		return;
	}
	report.supported_codecs &= !withdrawn;
	for (bit, _, _) in CODEC_PROFILES.iter().filter(|(bit, _, _)| withdrawn & bit != 0) {
		report.record_profile(*bit, Some("wrong colors from this GPU's color conversion".into()));
	}
	let remaining = codec_profile_labels(report.supported_codecs);
	let mut message = format!(
		"  Wrong colors from this GPU's conversion to {}\n  \
		 Not offered to clients: {}.\n  \
		 This is a Pyroshine or GPU driver bug, not a setting. Report it with the Diagnostics page's \
		 Copy diagnostics (or this output) at {}.\n",
		failures.join("; "),
		codec_profile_labels(withdrawn).join(", "),
		crate::session::failure::ISSUES_URL,
	);
	if remaining.is_empty() {
		message.push_str("  No codec profile remains: this host cannot stream until the conversion is fixed.");
		report.add_failed(COLOR_CONVERSION, message, elapsed);
	} else {
		message.push_str(&format!("  Clients can still use: {}.", remaining.join(", ")));
		report.add_warn(COLOR_CONVERSION, message, elapsed);
	}
}

fn check_dmabuf(report: &mut HealthReport, context: Option<&VideoContext>) -> bool {
	let start = Instant::now();
	let ctx = match context {
		Some(c) => c,
		None => {
			report.add_failed(
				"DMA-BUF",
				"  Skipped (Vulkan check failed).".into(),
				start.elapsed().as_millis() as u64,
			);
			return false;
		},
	};

	let instance = ctx.instance();
	let physical_device = ctx.physical_device();

	let extensions = match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
		Ok(exts) => exts,
		Err(e) => {
			report.add_failed(
				"DMA-BUF",
				format!("  Failed to enumerate Vulkan device extensions: {e}"),
				start.elapsed().as_millis() as u64,
			);
			return false;
		},
	};

	let has_ext = |name: &CStr| -> bool {
		extensions.iter().any(|ext| {
			let ext_name = unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) };
			ext_name == name
		})
	};

	let has_ext_mem_fd = has_ext(ash::khr::external_memory_fd::NAME);
	let has_drm_mod = has_ext(ash::ext::image_drm_format_modifier::NAME);

	if has_ext_mem_fd && has_drm_mod {
		report.add_passed(
			"DMA-BUF",
			"VK_KHR_external_memory_fd, VK_EXT_image_drm_format_modifier".into(),
			start.elapsed().as_millis() as u64,
		);
		true
	} else {
		let mut missing = Vec::new();
		if !has_ext_mem_fd {
			missing.push("VK_KHR_external_memory_fd");
		}
		if !has_drm_mod {
			missing.push("VK_EXT_image_drm_format_modifier");
		}
		report.add_failed(
			"DMA-BUF",
			format!(
				"  Missing extensions: {}\n  DMA-BUF import is required for zero-copy video encoding.\n  Update GPU drivers to the latest version.",
				missing.join(", ")
			),
			start.elapsed().as_millis() as u64,
		);
		false
	}
}

fn vk_version_major(v: u32) -> u32 {
	v >> 22
}

fn vk_version_minor(v: u32) -> u32 {
	(v >> 12) & 0x3ff
}

fn vk_version_patch(v: u32) -> u32 {
	v & 0xfff
}

// ---------------------------------------------------------------------------
// External dependencies
// ---------------------------------------------------------------------------

fn check_xwayland(report: &mut HealthReport) {
	let start = Instant::now();
	match which::which("Xwayland") {
		Ok(path) => {
			report.add_passed(
				"Xwayland",
				path.display().to_string(),
				start.elapsed().as_millis() as u64,
			);
		},
		Err(_) => {
			report.add_warn(
				"Xwayland",
				"  Xwayland not found in PATH.\n  Ubuntu: `sudo apt install xwayland`\n  Fedora: `sudo dnf install xorg-x11-server-Xwayland`\n  Arch: `sudo pacman -S xorg-x11-server`".into(),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_xdg_runtime(report: &mut HealthReport) {
	let start = Instant::now();
	match std::env::var("XDG_RUNTIME_DIR") {
		Ok(dir) => {
			let path = Path::new(&dir);
			if !path.is_dir() {
				report.add_failed(
					"Runtime dir",
					format!(
						"  XDG_RUNTIME_DIR ({dir}) is not a directory.\n  Ensure XDG_RUNTIME_DIR points to a valid runtime directory:\n  export XDG_RUNTIME_DIR=/run/user/$(id -u)"
					),
					start.elapsed().as_millis() as u64,
				);
			} else {
				// Quick writability test: try to create a temp file.
				let test = path.join(".moonshine-healthcheck-test");
				match std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
					Ok(_) => {
						let _ = std::fs::remove_file(&test);
						report.add_passed("Runtime dir", dir, start.elapsed().as_millis() as u64);
					},
					Err(e) => {
						report.add_failed(
							"Runtime dir",
							format!(
								"  Cannot write to XDG_RUNTIME_DIR ({dir}): {e}\n  Check permissions on the runtime directory."
							),
							start.elapsed().as_millis() as u64,
						);
					},
				}
			}
		},
		Err(_) => {
			report.add_failed(
				"Runtime dir",
				"  XDG_RUNTIME_DIR is not set.\n  Set: export XDG_RUNTIME_DIR=/run/user/$(id -u)".into(),
				start.elapsed().as_millis() as u64,
			);
		},
	}
}

fn check_inhibit(report: &mut HealthReport) {
	let start = Instant::now();

	let conn = match zbus::blocking::Connection::system() {
		Ok(conn) => conn,
		Err(e) => {
			report.add_warn(
				"Sleep inhibit",
				format!("  Cannot connect to system D-Bus: {e}\n  systemd-logind may not be running."),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let msg = match conn.call_method(
		Some("org.freedesktop.login1"),
		"/org/freedesktop/login1",
		Some("org.freedesktop.login1.Manager"),
		"Inhibit",
		&("sleep", "Moonshine", "healthcheck", "block"),
	) {
		Ok(msg) => msg,
		Err(e) => {
			report.add_warn(
				"Sleep inhibit",
				format!(
					"  Cannot acquire sleep inhibit: {e}\n  \
					 The packaged service joins the 'pyroshine' group, which polkit allows to block sleep\n  \
					 (NixOS: 'moonshine'). A server started from a terminal needs your user in that group:\n  \
					 `sudo usermod -aG pyroshine $USER`, then log out and back in."
				),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let raw = match msg.data().fds().first().map(|fd| fd.as_raw_fd()) {
		Some(fd) => fd,
		None => {
			report.add_warn(
				"Sleep inhibit",
				"  logind returned no file descriptor in inhibit response.".into(),
				start.elapsed().as_millis() as u64,
			);
			return;
		},
	};

	let duped = unsafe { libc::dup(raw) };
	if duped < 0 {
		report.add_warn(
			"Sleep inhibit",
			"  Failed to duplicate inhibit file descriptor.".into(),
			start.elapsed().as_millis() as u64,
		);
		return;
	}
	let _fd = unsafe { OwnedFd::from_raw_fd(duped) };

	report.add_passed(
		"Sleep inhibit",
		"Can acquire logind sleep inhibit (polkit rule and group membership ok)".into(),
		start.elapsed().as_millis() as u64,
	);
}

fn check_kcmp(report: &mut HealthReport) {
	let start = Instant::now();

	/// `KCMP_FILE` — not exposed by `libc` or glibc.
	const KCMP_FILE: libc::c_int = 0;

	// SAFETY: uses /dev/null fds; syscall with scalar arguments.
	let result = unsafe {
		let a = std::fs::File::open("/dev/null");
		let b = std::fs::File::open("/dev/null");
		match (a, b) {
			(Ok(a), Ok(b)) => libc::syscall(
				libc::SYS_kcmp,
				libc::getpid(),
				libc::getpid(),
				KCMP_FILE,
				a.as_raw_fd() as libc::c_ulong,
				b.as_raw_fd() as libc::c_ulong,
			),
			(Err(e), _) | (_, Err(e)) => {
				report.add_failed(
					"kcmp(2)",
					format!("  Cannot open /dev/null for kcmp(2) probe: {e}"),
					start.elapsed().as_millis() as u64,
				);
				return;
			},
		}
	};

	if result < 0 {
		let error = std::io::Error::last_os_error();
		let hint = match error.raw_os_error() {
			Some(libc::ENOSYS) => {
				"  Kernel was compiled without CONFIG_CHECKPOINT_RESTORE.\n  \
				kcmp(2) is required for DMA-BUF import cache validation.\n  \
				Rebuild your kernel with CONFIG_CHECKPOINT_RESTORE=y."
			},
			Some(libc::EPERM) => {
				"  kcmp(2) is blocked by a seccomp filter (e.g. Flatpak/container).\n  \
				kcmp(2) is required for DMA-BUF import cache validation.\n  \
				Run outside the sandbox or adjust the seccomp profile."
			},
			_ => "  kcmp(2) is required for DMA-BUF import cache validation.",
		};
		report.add_failed(
			"kcmp(2)",
			format!("  kcmp(2) not available: {error}\n{hint}"),
			start.elapsed().as_millis() as u64,
		);
		return;
	}

	report.add_passed("kcmp(2)", "Available".into(), start.elapsed().as_millis() as u64);
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

fn check_ports(report: &mut HealthReport, config: &Config) {
	let start = Instant::now();

	let ports: &[(u16, &str, bool)] = &[
		(config.webserver.port, "HTTP", true),
		(config.webserver.port_https, "HTTPS", true),
		(config.stream.port, "RTSP", true),
		(config.stream.video.port, "Video UDP", false),
		(config.stream.audio.port, "Audio UDP", false),
		(config.stream.control.port, "Control UDP", false),
	];

	let mut failed = Vec::new();
	let mut ok_list = Vec::new();

	for &(port, name, is_tcp) in ports {
		let result = if is_tcp {
			std::net::TcpListener::bind((config.address.as_str(), port)).map(|_| ())
		} else {
			std::net::UdpSocket::bind((config.address.as_str(), port)).map(|_| ())
		};

		match result {
			Ok(_) => ok_list.push(format!("{name}({port})")),
			Err(e) => failed.push(format!("{name} port {port}: {e}")),
		}
	}

	report_ports(report, start, &failed, &ok_list);
}

fn report_ports(report: &mut HealthReport, start: Instant, failed: &[String], ok_list: &[String]) {
	if failed.is_empty() {
		report.add_passed("Ports", ok_list.join(", "), start.elapsed().as_millis() as u64);
	} else {
		let mut msg = format!("  Port conflicts detected: {}\n", failed.join("; "));
		msg.push_str(
			"  Another program (often another streaming server such as Sunshine) holds them.\n  \
			 Stop it, or change the ports in the configuration.\n",
		);
		msg.push_str(&format!("  Available ports: {}", ok_list.join(", ")));
		report.add_failed("Ports", msg, start.elapsed().as_millis() as u64);
	}
}

/// [`check_ports`] from inside the running server: its TCP listeners are
/// held by the server itself, and the UDP stream ports by a live session.
fn check_ports_in_server(report: &mut HealthReport, config: &Config, session_active: bool) {
	let start = Instant::now();
	let mut failed = Vec::new();
	let mut ok_list = vec![format!(
		"HTTP({}), HTTPS({}), RTSP({}) served by Pyroshine",
		config.webserver.port, config.webserver.port_https, config.stream.port
	)];
	let udp = [
		(config.stream.video.port, "Video UDP"),
		(config.stream.audio.port, "Audio UDP"),
		(config.stream.control.port, "Control UDP"),
	];
	if session_active {
		ok_list.push("UDP stream ports in use by the session".into());
	} else {
		for (port, name) in udp {
			match std::net::UdpSocket::bind((config.address.as_str(), port)) {
				Ok(_) => ok_list.push(format!("{name}({port})")),
				Err(e) => failed.push(format!("{name} port {port}: {e}")),
			}
		}
	}
	report_ports(report, start, &failed, &ok_list);
}

// ---------------------------------------------------------------------------
// Warning checks
// ---------------------------------------------------------------------------

/// Whether a kernel module is loaded (also true for modules built into the kernel).
fn kernel_module_loaded(name: &str) -> bool {
	Path::new("/sys/module").join(name).exists()
}

fn check_uinput(report: &mut HealthReport) {
	let start = Instant::now();
	match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
		Ok(_fd) => {
			report.add_passed("uinput", "Writable".into(), start.elapsed().as_millis() as u64);
		},
		Err(e) => {
			let msg = if kernel_module_loaded("uinput") {
				format!(
					"  Cannot access /dev/uinput: {e}\n  \
					 The uinput module is loaded, so this is a permissions problem.\n  \
					 Ensure the udev rules are installed: /usr/lib/udev/rules.d/60-moonshine.rules\n  \
					 And that your user is in the 'input' group: `sudo usermod -aG input $USER`\n  \
					 Then log out and back in (or reboot)."
				)
			} else {
				format!(
					"  Cannot access /dev/uinput: {e}\n  \
					 The uinput kernel module is not loaded.\n  \
					 Load it now: `sudo modprobe uinput`\n  \
					 Persistent: the moonshine package ships a modules-load.d drop-in\n  \
					 (moonshine.conf) that loads it at boot."
				)
			};
			report.add_warn("uinput", msg, start.elapsed().as_millis() as u64);
		},
	}
}

fn check_uhid(report: &mut HealthReport) {
	let start = Instant::now();
	match std::fs::OpenOptions::new().write(true).open("/dev/uhid") {
		Ok(_fd) => {
			report.add_passed("uhid", "Writable".into(), start.elapsed().as_millis() as u64);
		},
		Err(e) => {
			let msg = if kernel_module_loaded("uhid") {
				format!(
					"  Cannot access /dev/uhid: {e}\n  \
					 The uhid module is loaded, so this is a permissions problem.\n  \
					 Ensure the udev rules are installed: /usr/lib/udev/rules.d/60-moonshine.rules\n  \
					 And that your user is in the 'input' group: `sudo usermod -aG input $USER`\n  \
					 Then log out and back in (or reboot)."
				)
			} else {
				format!(
					"  Cannot access /dev/uhid: {e}\n  \
					 The uhid kernel module is not loaded.\n  \
					 Load it now: `sudo modprobe uhid`\n  \
					 Persistent: the moonshine package ships a modules-load.d drop-in\n  \
					 (moonshine.conf) that loads it at boot."
				)
			};
			report.add_warn("uhid", msg, start.elapsed().as_millis() as u64);
		},
	}
}

fn check_input_group(report: &mut HealthReport) {
	let start = Instant::now();
	if is_member_of_group("input") {
		report.add_passed(
			"input group",
			"User is in the 'input' group; streamed games can read virtual gamepads when headless.".into(),
			start.elapsed().as_millis() as u64,
		);
	} else {
		report.add_warn(
			"input group",
			"  The user is not a member of the 'input' group.\n  \
			 When streaming headless (no active desktop session), streamed games need this group\n  \
			 to read the virtual gamepads that moonshine creates.\n  \
			 Add yourself to the group and log out and back in (or reboot):\n  \
			 `sudo usermod -aG input $USER`\n  \
			 If you always stream while a desktop session is active, this is not required: the\n  \
			 active seat user is granted access to input devices via ACLs (uaccess)."
				.into(),
			start.elapsed().as_millis() as u64,
		);
	}
}

fn check_dirs(report: &mut HealthReport, config: &Config) {
	let start = Instant::now();

	let mut failed = Vec::new();

	// Pure read-only check: verify each required directory either exists and is
	// writable, or its parent is writable so the daemon can create it at startup.
	// We intentionally do NOT create directories here (a healthcheck should be
	// side-effect free and must not mask permission issues).
	let check_dir = |dir: &Path, label: &str, failed: &mut Vec<String>| {
		if dir.exists() {
			let test = dir.join(".moonshine-healthcheck-test");
			if let Err(e) = std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
				failed.push(format!("{label} {}: {e}", dir.display()));
			} else {
				let _ = std::fs::remove_file(&test);
			}
		} else if let Some(parent) = dir.parent() {
			let test = parent.join(".moonshine-healthcheck-test");
			if let Err(e) = std::fs::File::create(&test).and_then(|mut f| f.write_all(b"x")) {
				failed.push(format!(
					"{label} {} does not exist and cannot be created (parent not writable): {e}",
					dir.display()
				));
			} else {
				let _ = std::fs::remove_file(&test);
			}
		} else {
			failed.push(format!("{label} {} has no parent directory", dir.display()));
		}
	};

	// Check state directory
	let state_dir = dirs::data_dir()
		.map(|d| d.join("moonshine"))
		.unwrap_or_else(|| PathBuf::from("/var/lib/moonshine"));
	check_dir(&state_dir, "State dir", &mut failed);

	// Check cert/key directories
	let cert_dir = config.webserver.certificate.parent();
	let key_dir = config.webserver.private_key.parent();
	for dir in [cert_dir, key_dir].iter().flatten() {
		check_dir(dir, "Config dir", &mut failed);
	}

	if failed.is_empty() {
		report.add_passed(
			"Config dirs",
			format!(
				"{}, {}",
				state_dir.display(),
				cert_dir.map(|d| d.display().to_string()).unwrap_or_default()
			),
			start.elapsed().as_millis() as u64,
		);
	} else {
		report.add_warn(
			"Config dirs",
			format!(
				"  Directory issues:\n    {}\n  Check permissions.",
				failed.join("\n    ")
			),
			start.elapsed().as_millis() as u64,
		);
	}
}

// ---------------------------------------------------------------------------
// Shared utility: find the appropriate DRM render node
// ---------------------------------------------------------------------------

/// Find the appropriate DRM render node, respecting GPU config and env override.
pub(crate) fn find_render_node(gpu_config: &Option<String>) -> Result<PathBuf, String> {
	if let Ok(node) = std::env::var("MOONSHINE_RENDER_NODE") {
		return Ok(PathBuf::from(node));
	}

	let dri_path = Path::new("/dev/dri");
	if !dri_path.exists() {
		return Err("No /dev/dri directory found".to_string());
	}

	let mut entries: Vec<_> = std::fs::read_dir(dri_path)
		.map_err(|e| format!("Failed to read /dev/dri: {e}"))?
		.filter_map(|entry| entry.ok())
		.filter(|entry| {
			entry
				.file_name()
				.to_str()
				.map(|name| name.starts_with("renderD"))
				.unwrap_or(false)
		})
		.collect();

	entries.sort_by_key(|e| e.file_name());

	if entries.is_empty() {
		return Err("No render node found in /dev/dri".to_string());
	}

	let get_device_info = |entry: &std::fs::DirEntry| -> Option<(String, String)> {
		let file_name = entry.file_name();
		let name = file_name.to_str()?;
		let device_path = Path::new("/sys/class/drm").join(name).join("device/uevent");
		let content = std::fs::read_to_string(device_path).ok()?;
		Some((name.to_string(), content))
	};

	if let Some(config_str) = gpu_config {
		let path = Path::new(config_str);
		if path.is_absolute() {
			if path.exists() {
				return Ok(path.to_path_buf());
			}
			return Err(format!("Configured GPU path does not exist: {config_str}"));
		}

		for entry in &entries {
			if let Some((name, uevent)) = get_device_info(entry) {
				if name == *config_str {
					return Ok(entry.path());
				}
				if uevent.to_lowercase().contains(&config_str.to_lowercase()) {
					return Ok(entry.path());
				}
			}
		}

		return Err(format!("No GPU found matching configuration: {config_str}"));
	}

	let mut best_node = None;
	let mut best_score: i32 = -1;

	for entry in &entries {
		let score = if let Some((_, uevent)) = get_device_info(entry) {
			if uevent.contains("PCI_ID=10DE") {
				100
			} else if uevent.contains("PCI_ID=1002") {
				50
			} else {
				0
			}
		} else {
			0
		};

		if score > best_score {
			best_score = score;
			best_node = Some(entry.path());
		}
	}

	Ok(best_node.unwrap_or_else(|| entries[0].path()))
}

/// Frame limiting status: what the configured limiter covers, and whether a
/// configured MangoHud limiter is available. Never fatal.
fn check_frame_limit(report: &mut HealthReport, config: &Config) {
	use crate::session::frame_limit::{FrameLimitMethod, mangohud_layer_installed, support_summary};
	let start = Instant::now();
	let wants_mangohud = config.frame_limit.method == FrameLimitMethod::Mangohud
		|| config
			.applications
			.iter()
			.any(|a| a.frame_limit.frame_limit_method == Some(FrameLimitMethod::Mangohud))
		|| config
			.application_scanners
			.iter()
			.any(|s| s.frame_limit().frame_limit_method == Some(FrameLimitMethod::Mangohud));
	let mangohud = mangohud_layer_installed();
	let (supported, message) = support_summary(&config.frame_limit, mangohud);
	let elapsed = start.elapsed().as_millis() as u64;
	if supported && !(wants_mangohud && !mangohud) {
		report.add_passed("Frame limiter", message, elapsed);
	} else if supported {
		report.add_warn(
			"Frame limiter",
			format!(
				"  {message}\n  An application or scanner selects the MangoHud limiter, which is not installed; \
				 those launches use the Proton limiters."
			),
			elapsed,
		);
	} else {
		report.add_warn("Frame limiter", format!("  {message}"), elapsed);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::video::ColorRange;

	#[test]
	fn unavailable_capture_path_advertises_no_video_capabilities() {
		let mut report = HealthReport::empty();
		let context = check_vulkan(&mut report, None);
		assert!(context.is_none());
		check_codecs(&mut report, context.as_ref());
		assert!(!check_dmabuf(&mut report, context.as_ref()));
		assert_eq!(report.supported_codecs, 0);
		assert!(!report.hdr_supported);
		assert!(!report.all_fatal_passed);
		assert!(
			report
				.checks
				.iter()
				.any(|check| check.message.contains("capture render node/EGL"))
		);
	}

	#[test]
	fn profile_capabilities_are_not_codec_wide() {
		let caps = CODEC_HEVC | CODEC_HEVC_REXT_10444;
		assert!(supports_video_format(
			caps,
			NegotiatedVideoFormat::sdr(
				VideoCodec::Hevc,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Limited,
			)
		));
		assert!(supports_video_format(
			caps,
			NegotiatedVideoFormat::sdr(
				VideoCodec::Hevc,
				ChromaFormat::Yuv444,
				BitDepth::Ten,
				ColorRange::Limited,
			)
		));
		assert!(!supports_video_format(
			caps,
			NegotiatedVideoFormat::sdr(
				VideoCodec::Hevc,
				ChromaFormat::Yuv444,
				BitDepth::Eight,
				ColorRange::Limited,
			)
		));
	}

	#[test]
	fn pyrowave_profiles_require_chroma_hdr_bits_and_full_range() {
		assert_eq!(CODEC_PYROWAVE, 0x00800000);
		assert_eq!(CODEC_PYROWAVE_444, 0x01000000);
		assert_eq!(CODEC_PYROWAVE_HDR, 0x02000000);
		let caps = CODEC_PYROWAVE_444 | CODEC_PYROWAVE_HDR;
		assert!(supports_video_format(
			caps,
			NegotiatedVideoFormat::hdr10(VideoCodec::PyroWave, ChromaFormat::Yuv444, ColorRange::Full,)
		));
		assert!(!supports_video_format(
			caps,
			NegotiatedVideoFormat::hdr10(VideoCodec::PyroWave, ChromaFormat::Yuv444, ColorRange::Limited,)
		));
		assert!(!supports_video_format(
			CODEC_PYROWAVE_444,
			NegotiatedVideoFormat::hdr10(VideoCodec::PyroWave, ChromaFormat::Yuv444, ColorRange::Full)
		));
		assert!(supports_video_format(
			CODEC_PYROWAVE,
			NegotiatedVideoFormat::sdr(
				VideoCodec::PyroWave,
				ChromaFormat::Yuv420,
				BitDepth::Eight,
				ColorRange::Full,
			)
		));
		assert!(!supports_video_format(
			CODEC_PYROWAVE | CODEC_PYROWAVE_HDR,
			NegotiatedVideoFormat::sdr(
				VideoCodec::PyroWave,
				ChromaFormat::Yuv420,
				BitDepth::Ten,
				ColorRange::Full,
			)
		));
	}

	fn conversion_report(supported_codecs: u32) -> HealthReport {
		HealthReport {
			supported_codecs,
			dma_buf_supported: true,
			..HealthReport::empty()
		}
	}

	/// Issue #11: a GPU whose conversion loses chroma samples encodes every
	/// frame "successfully" with wrong colors. The check withdraws exactly the
	/// profiles that convert through the failing format and keeps the rest.
	#[test]
	fn wrong_colors_withdraw_the_affected_profiles() {
		use pixelforge::OutputFormat;
		let all = CODEC_H264 | CODEC_HEVC | CODEC_HEVC_MAIN10 | CODEC_HEVC_REXT_8444 | CODEC_AV1_MAIN8 | CODEC_PYROWAVE;
		let mut report = conversion_report(all);
		let mut checked = Vec::new();
		record_color_conversion(&mut report, |format| {
			checked.push(format);
			match format {
				OutputFormat::NV12 => Err("Cb 0/4 by column within a 4x2 block".into()),
				_ => Ok(()),
			}
		});
		// AV1/HEVC 4:4:4 10-bit are not supported: their format is not checked.
		assert_eq!(checked, [OutputFormat::NV12, OutputFormat::P010, OutputFormat::YUV444]);
		assert_eq!(
			report.supported_codecs,
			CODEC_HEVC_MAIN10 | CODEC_HEVC_REXT_8444 | CODEC_PYROWAVE
		);
		let check = &report.checks[0];
		assert_eq!((check.name, check.outcome), (COLOR_CONVERSION, CheckOutcome::Warning));
		assert!(report.all_fatal_passed, "other profiles still stream");
		let mut lines = check.message.lines().map(str::trim);
		assert_eq!(
			lines.next(),
			Some("Wrong colors from this GPU's conversion to 4:2:0 8-bit (NV12): Cb 0/4 by column within a 4x2 block")
		);
		assert_eq!(lines.next(), Some("Not offered to clients: H.264, HEVC, AV1."));
		assert!(
			check
				.message
				.contains("Clients can still use: HEVC Main10, HEVC 4:4:4, PyroWave.")
		);
		assert!(check.message.contains(crate::session::failure::ISSUES_URL));
		// The withdrawn profiles' probe results now carry the reason.
		let reason = |bit| {
			report
				.gpu
				.profiles
				.iter()
				.find(|probe| probe.bit == bit)
				.and_then(|probe| probe.unavailable.clone())
		};
		assert!(reason(CODEC_H264).unwrap().contains("wrong colors"));
		assert_eq!(reason(CODEC_HEVC_MAIN10), None);
	}

	#[test]
	fn unavailable_profiles_are_listed_once_per_reason() {
		let mut report = HealthReport::empty();
		report.record_profile(CODEC_H264, None);
		report.record_profile(CODEC_HEVC_REXT_8444, Some("no 4:4:4".into()));
		report.record_profile(CODEC_AV1_HIGH_8444, Some("no 4:4:4".into()));
		report.record_profile(CODEC_AV1_MAIN8, Some("no AV1 encoder".into()));
		report.record_profile(CODEC_PYROWAVE, Some("library missing".into()));
		// A later result replaces the earlier one.
		report.record_profile(CODEC_H264, Some("withdrawn".into()));
		assert_eq!(report.gpu.profiles.len(), 5);
		assert_eq!(
			unavailable_summary(&report.gpu.profiles, CODEC_CONVENTIONAL_MASK),
			"    H.264: withdrawn\n    HEVC 4:4:4, AV1 4:4:4: no 4:4:4\n    AV1: no AV1 encoder"
		);
		assert_eq!(unavailable_summary(&[], CODEC_CONVENTIONAL_MASK), "");
	}

	#[test]
	fn wrong_colors_for_every_remaining_profile_fail_the_check() {
		let mut report = conversion_report(CODEC_H264 | CODEC_HEVC);
		record_color_conversion(&mut report, |_| Err("Y 3/4".into()));
		assert_eq!(report.supported_codecs, 0);
		assert_eq!(report.checks[0].outcome, CheckOutcome::Failed);
		assert!(!report.all_fatal_passed);
		assert!(report.checks[0].message.contains("No codec profile remains"));

		// A correct conversion passes and keeps every profile.
		let mut report = conversion_report(CODEC_H264 | CODEC_AV1_HIGH_10444);
		record_color_conversion(&mut report, |_| Ok(()));
		assert_eq!(report.supported_codecs, CODEC_H264 | CODEC_AV1_HIGH_10444);
		assert_eq!(report.checks[0].outcome, CheckOutcome::Passed);
		assert_eq!(
			report.checks[0].message,
			"Output matches the reference for 4:2:0 8-bit (NV12), 4:4:4 10-bit"
		);
		// PyroWave alone needs no conversion check.
		let mut report = conversion_report(CODEC_PYROWAVE);
		record_color_conversion(&mut report, |_| unreachable!());
		assert!(report.checks.is_empty());
	}
}
