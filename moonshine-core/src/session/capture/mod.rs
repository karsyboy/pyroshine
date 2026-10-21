//! Capture sources and their coordination with the session.
//!
//! A *capture backend* produces the frames a session streams. The session
//! layer selects one, starts it, waits for its readiness, wires its endpoints
//! into the streams, applies negotiated output modes and stops it through the
//! session's stop. Everything else about a source stays inside its backend.
//!
//! | Responsibility | Owner |
//! | --- | --- |
//! | Selection | [`CaptureBackend`], chosen by the session when it initializes |
//! | Initialization and readiness | The backend, started through [`start_capture_thread`] |
//! | Frame production | The backend, through [`admission::CaptureSender`] |
//! | Frame contract and buffer lifetime | [`frame::ExportedFrame`] (shared) |
//! | Capability reporting | [`CaptureCapabilities`], reported once at readiness |
//! | Output mode and capture pacing | [`LiveCapture::reconfigure`] with [`OutputMode`] |
//! | Shutdown | The session stop; the backend's worker guard holds completion |
//! | Failure reporting | Launch errors, then the worker's exit stopping the session |
//! | Resource ownership | The backend's thread; the capture GPU outlives it |
//!
//! Two backends exist:
//!
//! - [`CaptureBackend::Isolated`] (Game Mode, the default) runs the session's
//!   own headless Smithay compositor (`session/compositor/`), which keeps its
//!   Wayland/XWayland environment, application surfaces, output, scene, focus
//!   and input to itself.
//! - [`CaptureBackend::Desktop`] (Desktop Mode) streams the user's running
//!   Wayland desktop through a guard-owned native virtual output: direct
//!   PipeWire frames, input through separately authorized portal devices.
//!
//! A further backend adds a variant to [`CaptureBackend`], [`CaptureSource`]
//! and [`LiveCapture`]; the compiler then lists every operation it must
//! provide. See `docs/CAPTURE_BACKENDS.md` for the contract a backend meets.

pub(crate) mod admission;
pub(crate) mod frame;
pub(crate) mod input;
pub(crate) mod portal;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use smithay::backend::allocator::Fourcc;

use crate::session::SessionContext;
use crate::session::capture::input::CaptureInputEvent;
use crate::session::compositor::{Compositor, CompositorConfig, LaunchedCompositor};
use crate::session::failure::{SessionFailure, StopCauseSlot};
use crate::session::lifecycle::WorkerGuard;
use crate::session::manager::SessionShutdownReason;

use self::admission::{CaptureContextHold, CaptureReceiver};

/// The capture backend serving a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CaptureBackend {
	/// Isolated Game Mode: a headless Smithay compositor and XWayland owned by
	/// the session, running only the launched application.
	#[default]
	Isolated,
	/// Desktop Mode: a native virtual output on the user's running Wayland
	/// desktop, with portal-authorized remote input.
	Desktop,
}

impl CaptureBackend {
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Isolated => "isolated",
			Self::Desktop => "desktop",
		}
	}
}

/// How capture opportunities and frame callbacks are scheduled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CapturePacing {
	/// The refresh tick at the negotiated rate is the only capture clock.
	#[default]
	Fixed,
	/// Presentation-driven capture for a client that paces playback itself
	/// (it requested VRR presentation). Each latched commit opens a capture
	/// opportunity at once, limited to the negotiated rate on average; frame
	/// callbacks follow those "flips" like a VRR panel whose maximum refresh
	/// is the stream rate. Frames are stamped with the time their newest
	/// content was latched, so the RTP timeline carries the source cadence.
	Vrr,
}

impl CapturePacing {
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Fixed => "fixed",
			Self::Vrr => "vrr",
		}
	}
}

/// Properties of the live capture output. Only these require changing a
/// running backend; codec, bitrate, chroma, bit depth and transport settings
/// belong to the video pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
	/// Capture pacing the current client asked for; the backend may restrict
	/// it (see [`CaptureCapabilities::presentation_pacing`]).
	pub capture_pacing: CapturePacing,
}

/// What a running capture backend delivers, reported once with its readiness.
///
/// Every frame is a DMA-BUF on the capture GPU in a format and modifier the
/// encoders import there (`gpu::ImportableModifiers`); a source that cannot
/// provide that fails to start instead of streaming through a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaptureCapabilities {
	pub backend: CaptureBackend,
	/// DRM render node of the capture GPU. Its verified Vulkan context travels
	/// with the frame channel ([`CaptureReceiver::context`]) and is the only
	/// device frames are imported on: there is no cross-device path.
	pub render_node: PathBuf,
	/// Format of frames the backend composes itself in SDR. Directly exported
	/// client buffers keep their own (importable) format.
	pub sdr_format: Fourcc,
	/// Format of composed HDR frames; `None` when the backend cannot produce
	/// HDR without restarting.
	pub hdr_format: Option<Fourcc>,
	/// Presentation-driven capture is available to clients that request it.
	/// Otherwise every client is captured on the fixed refresh clock.
	pub presentation_pacing: bool,
}

impl CaptureCapabilities {
	/// Whether the backend can switch to HDR output while running.
	pub(crate) fn hdr(&self) -> bool {
		self.hdr_format.is_some()
	}
}

/// Where the launched application connects to present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApplicationDisplay {
	/// X11 display number (e.g. `:1` → `1`).
	pub xdisplay: u32,
	/// Wayland socket name in `XDG_RUNTIME_DIR`.
	pub wayland_display: String,
}

/// Endpoints of a capture backend that the session wires into its streams.
pub(crate) struct CaptureEndpoints {
	/// Frames for the video pipeline, with the capture GPU context.
	pub frames: CaptureReceiver,
	/// Keyboard, pointer, touch and pen input for the captured scene.
	pub input: calloop::channel::Sender<CaptureInputEvent>,
}

/// Configuration of every backend; the session's backend uses its part.
#[derive(Clone, Debug, Default)]
pub(crate) struct CaptureSettings {
	pub compositor: CompositorConfig,
	pub desktop: portal::DesktopConfig,
}

/// A session's capture source before it runs.
pub(crate) enum CaptureSource {
	Isolated(Compositor),
	Desktop(portal::PortalCapture),
}

impl CaptureSource {
	/// Create the selected backend's channels. Nothing runs until [`Self::launch`].
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn new(
		backend: CaptureBackend,
		settings: CaptureSettings,
		context: &SessionContext,
		log_stats: bool,
		stop: ShutdownManager<SessionShutdownReason>,
		cause: StopCauseSlot,
		foreground_tx: tokio::sync::watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	) -> (Self, CaptureEndpoints) {
		match backend {
			CaptureBackend::Isolated => {
				let (compositor, endpoints) = Compositor::new(
					settings.compositor,
					crate::session::compositor::CompositorContext::from_session(context, log_stats),
					stop,
					foreground_tx,
				);
				(Self::Isolated(compositor), endpoints)
			},
			CaptureBackend::Desktop => {
				let config = portal::PortalCaptureConfig {
					desktop: settings.desktop,
					gpu: None,
					restore_token: portal::token::default_path(),
					refresh_rate: context.refresh_rate,
					resolution: context.resolution,
				};
				let (capture, endpoints) = portal::PortalCapture::new(config, stop, cause);
				(Self::Desktop(capture), endpoints)
			},
		}
	}

	/// Start the backend and wait for its readiness. Blocks; the failure says
	/// why it could not start, for the operator and the client.
	pub(crate) fn launch(self) -> Result<LiveCapture, SessionFailure> {
		match self {
			Self::Isolated(compositor) => compositor.launch().map(LiveCapture::Isolated).map_err(|error| {
				SessionFailure::new(
					"compositor_start_failed",
					format!("The streaming compositor could not start: {error}."),
				)
				.with_hint(
					"Check the GPU selection (gpu in the configuration) and the startup checks on the \
					 Diagnostics page or with pyroshine healthcheck.",
				)
			}),
			Self::Desktop(capture) => capture.launch().map(LiveCapture::Desktop),
		}
	}
}

/// A running capture backend.
pub(crate) enum LiveCapture {
	Isolated(LaunchedCompositor),
	Desktop(portal::LaunchedPortal),
}

impl LiveCapture {
	pub(crate) fn desktop_lease(&self) -> Option<String> {
		match self {
			Self::Desktop(d) => Some(d.ready().lease.clone()),
			Self::Isolated(_) => None,
		}
	}
	pub(crate) fn capabilities(&self) -> &CaptureCapabilities {
		match self {
			Self::Isolated(compositor) => &compositor.ready().capabilities,
			Self::Desktop(desktop) => &desktop.ready().capabilities,
		}
	}

	/// What the management interface shows about this capture.
	pub(crate) fn details(&self) -> moonshine_management::dto::CaptureDetails {
		let capabilities = self.capabilities();
		let mut details = moonshine_management::dto::CaptureDetails {
			mode: "game".into(),
			mode_label: "Game Mode".into(),
			backend: capabilities.backend.as_str().into(),
			render_node: capabilities.render_node.display().to_string(),
			dynamic_range: if capabilities.hdr() { "HDR" } else { "SDR" }.into(),
			output: None,
			desktop: None,
			current_desktop: None,
			input: None,
			consent: None,
		};
		if let Self::Desktop(desktop) = self {
			let ready = desktop.ready();
			details.mode = "desktop".into();
			details.mode_label = "Desktop Mode".into();
			details.output = Some(moonshine_management::dto::CapturedOutputDetails {
				width: ready.output.width,
				height: ready.output.height,
				refresh_millihertz: ready.output.refresh_millihertz,
			});
			details.desktop = Some(ready.desktop.kind.as_str().into());
			details.current_desktop = Some(ready.desktop.current_desktop.clone());
			details.input = Some(ready.input.as_str().into());
			details.consent = Some(
				match ready.consent {
					portal::Consent::Restored => "restored",
					portal::Consent::Requested => "requested",
				}
				.into(),
			);
		}
		details
	}

	/// Where a launched application presents: the isolated backend's own
	/// displays. Desktop Mode launches no application.
	pub(crate) fn application_display(&self) -> Option<&ApplicationDisplay> {
		match self {
			Self::Isolated(compositor) => Some(&compositor.ready().display),
			Self::Desktop(_) => None,
		}
	}

	/// Whether the stream's encoder keeps a scaled source's aspect ratio
	/// (conventional codecs) or stretches it (PyroWave's scaler). Managed native
	/// outputs match the stream size; this retains viewport/input bookkeeping.
	pub(crate) fn set_scaling_fit(&mut self, fit: bool) {
		if let Self::Desktop(desktop) = self {
			desktop.set_scaling_fit(fit);
		}
	}

	/// Apply `mode` to the live output and return the effective HDR state.
	/// The caller refuses the stream when that differs from the negotiated one.
	pub(crate) async fn reconfigure(&mut self, mode: OutputMode) -> Result<bool, ()> {
		match self {
			Self::Isolated(compositor) => compositor.reconfigure(mode).await,
			Self::Desktop(desktop) => desktop.reconfigure(mode).await,
		}
	}
}

/// Run a capture backend's producer on a thread of its own and wait for it to
/// report readiness through the sender it is given.
///
/// Every backend inherits this ownership contract:
///
/// - The worker is registered with the session before the thread exists, so
///   session completion implies the thread has exited and dropped everything
///   it owns (buffers, client sockets, GPU state).
/// - `context` keeps the capture GPU's Vulkan device alive until the thread
///   has *fully* exited, thread-local destructors included. GPU drivers
///   release a thread's EGL state there; on NVIDIA, destroying the Vulkan
///   device while they run deadlocks inside the driver. Completion is
///   released only after the device hold.
/// - The thread ending for any reason (return, error or panic) stops the
///   session with `reason`, immediately and again through the worker guard.
/// - Readiness returns the value `body` sent. Otherwise this returns the
///   error `body` returned, or why the thread ended, within `ready_timeout`.
///   A backend that is still starting when the session stops must return
///   rather than report readiness; the session's teardown then waits for it.
pub(crate) fn start_capture_thread<R: Send + 'static>(
	name: &'static str,
	stop: &ShutdownManager<SessionShutdownReason>,
	reason: SessionShutdownReason,
	context: CaptureContextHold,
	ready_timeout: Duration,
	body: impl FnOnce(mpsc::SyncSender<R>) -> Result<(), String> + Send + 'static,
) -> Result<R, String> {
	let worker =
		WorkerGuard::register(stop, reason).map_err(|()| format!("the session stopped before the {name} started"))?;
	let (ready_tx, ready_rx) = mpsc::sync_channel(1);
	// Why the thread ended before readiness. The body drops its ready sender
	// before this is sent, so a disconnected ready channel waits briefly for it.
	let (failure_tx, failure_rx) = mpsc::sync_channel::<String>(1);
	let stopped = stop.trigger_shutdown_token(reason);
	let producer = std::thread::Builder::new()
		.name(name.to_string())
		.spawn(move || {
			let _stopped = stopped;
			if let Err(error) = body(ready_tx) {
				let _ = failure_tx.try_send(error);
			}
		})
		.map_err(|e| {
			tracing::error!("Failed to spawn {name} thread: {e}");
			format!("the {name} thread could not be created: {e}")
		})?;
	std::thread::Builder::new()
		.name(format!("{name}-exit"))
		.spawn(move || {
			let _ = producer.join();
			drop(context);
			drop(worker);
		})
		.map_err(|e| {
			tracing::error!("Failed to spawn {name} exit thread: {e}");
			format!("the {name} exit thread could not be created: {e}")
		})?;

	match ready_rx.recv_timeout(ready_timeout) {
		Ok(ready) => Ok(ready),
		Err(mpsc::RecvTimeoutError::Disconnected) => Err(failure_rx
			.recv_timeout(Duration::from_secs(1))
			.unwrap_or_else(|_| format!("the {name} stopped before it was ready"))),
		Err(mpsc::RecvTimeoutError::Timeout) => {
			tracing::warn!("Timed out waiting for {name} ready");
			Err(failure_rx.try_recv().unwrap_or_else(|_| {
				format!(
					"the {name} did not become ready within {} seconds",
					ready_timeout.as_secs()
				)
			}))
		},
	}
}

#[cfg(test)]
mod tests;
