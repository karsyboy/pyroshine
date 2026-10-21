//! Desktop Mode capture probe (`moonshine-desktop-probe`).
//!
//! Runs the real Desktop Mode backend without encoding: authorizes a portal
//! session, receives frames for a while and reports their delivery (rate,
//! intervals, the age of each frame when the consumer received it) and the
//! process CPU time spent. Optionally injects a short scripted input sequence
//! through the authorized devices, for checking remote input on a desktop.
//!
//! Frames are released as soon as they arrive (no GPU work reads them), so
//! the figures are the capture path's own cost and latency.

use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use serde::Serialize;

use super::{DesktopConfig, PortalCapture, PortalCaptureConfig};
use crate::session::capture::input::CaptureInputEvent;
use crate::session::failure::StopCauseSlot;
use crate::session::manager::SessionShutdownReason;

/// What to probe.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
	/// How long to receive frames.
	pub duration: Duration,
	/// Frame rate asked of the compositor.
	pub refresh_rate: u32,
	/// Collect bounded capture timings; disabled for ordinary sessions.
	pub timings: bool,
	/// Diagnose the implicit path even when explicit sync is supported.
	pub implicit_sync: bool,
	pub resolution: (u32, u32),
	/// Ask for remote input and run the scripted input sequence after the first frame.
	pub input: bool,
	/// Restore-token file; `None` asks the desktop every time.
	pub restore_token: Option<std::path::PathBuf>,
}

/// Percentiles of a sample set, in microseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Percentiles {
	pub p50_us: u64,
	pub p95_us: u64,
	pub p99_us: u64,
	pub max_us: u64,
}

impl Percentiles {
	fn of(mut samples: Vec<u64>) -> Self {
		if samples.is_empty() {
			return Self::default();
		}
		samples.sort_unstable();
		let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
		Self {
			p50_us: at(0.5),
			p95_us: at(0.95),
			p99_us: at(0.99),
			max_us: *samples.last().expect("not empty"),
		}
	}
}

/// The probe's findings.
#[derive(Clone, Debug, Serialize)]
pub struct ProbeReport {
	pub capture: super::metrics::CaptureReport,
	pub desktop: String,
	pub current_desktop: String,
	pub render_node: String,
	pub width: u32,
	pub height: u32,
	pub fourcc: String,
	pub hdr: bool,
	pub input: String,
	pub consent: String,
	pub frames: u64,
	pub seconds: f64,
	pub fps: f64,
	/// Content time (the compositor's presentation timestamp) to receipt by
	/// the consumer, which includes the producer-fence wait.
	pub frame_age: Percentiles,
	/// Export completion to receipt: the admission channel alone.
	pub channel: Percentiles,
	pub interval: Percentiles,
	/// Process CPU time over the capture window, in percent of one core.
	pub cpu_percent: f64,
	pub input_events: usize,
}

fn cpu_time() -> Duration {
	let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
	// SAFETY: valid out pointer.
	unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
	// SAFETY: getrusage initialized it.
	let usage = unsafe { usage.assume_init() };
	let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
	tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// Linux key codes of "pyroshine" followed by Enter.
const TYPED: [u32; 10] = [25, 21, 19, 24, 31, 35, 23, 49, 18, 28];

/// The scripted input: pointer to the center, a left click, typing
/// "pyroshine⏎", a wheel notch down, then a reset as a disconnect sends.
pub(crate) fn input_script(width: u32, height: u32) -> Vec<CaptureInputEvent> {
	let (w, h) = (width.min(i16::MAX as u32) as i16, height.min(i16::MAX as u32) as i16);
	let mut events = vec![
		CaptureInputEvent::MouseMoveAbsolute {
			x: w / 2,
			y: h / 2,
			screen_width: w,
			screen_height: h,
		},
		CaptureInputEvent::MouseButtonDown { button: 0x110 },
		CaptureInputEvent::MouseButtonUp { button: 0x110 },
		CaptureInputEvent::MouseMoveRelative { dx: 40, dy: 20 },
	];
	for keycode in TYPED {
		events.push(CaptureInputEvent::KeyDown { keycode });
		events.push(CaptureInputEvent::KeyUp { keycode });
	}
	events.push(CaptureInputEvent::ScrollVertical { amount: -120 });
	// A key left pressed: the reset must release it.
	events.push(CaptureInputEvent::KeyDown { keycode: 42 });
	events
}

/// Run the probe. Blocks for the portal consent and `options.duration`.
pub fn run(options: ProbeOptions) -> Result<ProbeReport, String> {
	crate::desktop::Mode {
		width: options.resolution.0,
		height: options.resolution.1,
		refresh_millihertz: options.refresh_rate.saturating_mul(1000),
	}
	.validate()?;
	if options.duration.is_zero() {
		return Err("Probe duration must be positive".into());
	}
	let stop = ShutdownManager::<SessionShutdownReason>::new();
	let cause = StopCauseSlot::default();
	let (capture, endpoints) = PortalCapture::new(
		PortalCaptureConfig {
			desktop: DesktopConfig {
				enabled: true,
				input: options.input,
				..DesktopConfig::default()
			},
			gpu: None,
			client_identity: None,
			restore_token: options.restore_token.clone(),
			refresh_rate: options.refresh_rate,
			resolution: options.resolution,
			diagnostics: options.timings,
			implicit_sync: options.implicit_sync,
		},
		stop.clone(),
		cause.clone(),
	);
	let launched = capture.launch().map_err(|failure| failure.client_message())?;
	let ready = launched.ready().clone();
	let frames = endpoints.frames;
	let mut ages = Vec::new();
	let mut channel = Vec::new();
	let mut intervals = Vec::new();
	let mut input_events = 0;
	let mut last: Option<Instant> = None;
	let started = Instant::now();
	let cpu_started = cpu_time();
	while started.elapsed() < options.duration {
		let Ok(mut frame) = frames.recv_timeout_if(Duration::from_millis(100), true) else {
			if stop.is_shutdown_triggered() {
				break;
			}
			continue;
		};
		let now = Instant::now();
		frame.consumed.store(true, std::sync::atomic::Ordering::Release);
		drop(frame.capture_credit.take());
		ages.push(now.duration_since(frame.source_time).as_micros() as u64);
		channel.push(now.duration_since(frame.created_at).as_micros() as u64);
		if let Some(last) = last {
			intervals.push(now.duration_since(last).as_micros() as u64);
		}
		if last.is_none() && options.input {
			for event in input_script(ready.output.width, ready.output.height) {
				let _ = endpoints.input.send(event);
				input_events += 1;
			}
			let (done, waiting) = tokio::sync::oneshot::channel();
			let _ = endpoints.input.send(CaptureInputEvent::Reset {
				keys: Vec::new(),
				buttons: Vec::new(),
				ready: done,
			});
			input_events += 1;
			let _ = waiting.blocking_recv();
		}
		last = Some(now);
	}
	let seconds = started.elapsed().as_secs_f64();
	let cpu = cpu_time().saturating_sub(cpu_started).as_secs_f64();
	let interrupted = stop.is_shutdown_triggered();
	let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);
	drop(frames);
	// Wait for the capture thread: it closes the portal session.
	let deadline = Instant::now() + Duration::from_secs(10);
	while !stop.is_shutdown_completed() && Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(20));
	}
	if !stop.is_shutdown_completed() {
		return Err("Capture thread did not stop; desktop restoration is not confirmed".into());
	}
	check_completion(interrupted, &cause)?;
	let capture = launched
		.diagnostics()
		.ok_or("The capture worker returned no diagnostic observations")?;
	drop(launched);
	let frames_received = ages.len() as u64;
	if frames_received == 0 {
		return Err("The probe received no completed frames".into());
	}
	Ok(ProbeReport {
		capture,
		desktop: ready.desktop.kind.as_str().into(),
		current_desktop: ready.desktop.current_desktop.clone(),
		render_node: ready.capabilities.render_node.display().to_string(),
		width: ready.output.width,
		height: ready.output.height,
		fourcc: format!("{:?}", ready.capabilities.sdr_format),
		hdr: ready.capabilities.hdr(),
		input: ready.input.as_str().into(),
		consent: format!("{:?}", ready.consent),
		frames: frames_received,
		seconds,
		fps: frames_received as f64 / seconds,
		frame_age: Percentiles::of(ages),
		channel: Percentiles::of(channel),
		interval: Percentiles::of(intervals),
		cpu_percent: cpu / seconds * 100.0,
		input_events,
	})
}

fn check_completion(interrupted: bool, cause: &StopCauseSlot) -> Result<(), String> {
	if let Some(cause) = cause.get() {
		return Err(cause.detail);
	}
	if interrupted {
		return Err("Capture stopped before the requested probe duration completed".into());
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn early_worker_exit_cannot_report_a_successful_partial_measurement() {
		let cause = StopCauseSlot::default();
		assert!(check_completion(false, &cause).is_ok());
		assert!(check_completion(true, &cause).is_err());
		cause.record(crate::session::failure::StopCause {
			detail: "producer disappeared".into(),
			hint: None,
			unexpected: true,
			reported: false,
		});
		assert_eq!(check_completion(true, &cause).unwrap_err(), "producer disappeared");
		assert!(
			check_completion(false, &cause).is_err(),
			"failure during shutdown must also be reported"
		);
	}

	#[test]
	fn invalid_capture_requests_fail_before_connecting_to_the_desktop() {
		for (resolution, refresh_rate, duration) in [
			((0, 1080), 60, 1),
			((1920, 0), 60, 1),
			((1920, 1080), 0, 1),
			((1920, 1080), 1001, 1),
			((1920, 1080), 60, 0),
		] {
			assert!(
				run(ProbeOptions {
					duration: Duration::from_secs(duration),
					refresh_rate,
					timings: false,
					implicit_sync: false,
					resolution,
					input: false,
					restore_token: None
				})
				.is_err()
			);
		}
	}
}
