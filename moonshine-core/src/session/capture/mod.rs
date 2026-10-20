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
//! Only isolated Game Mode exists: [`CaptureBackend::Isolated`] runs the
//! session's own headless Smithay compositor (`session/compositor/`), which
//! keeps its Wayland/XWayland environment, application surfaces, output,
//! scene, focus and input to itself. A second backend adds a variant to
//! [`CaptureBackend`], [`CaptureSource`] and [`LiveCapture`]; the compiler
//! then lists every operation it must provide. See `docs/ARCHITECTURE.md`
//! ("Capture backends") for the contract a new backend must meet.

pub(crate) mod admission;
pub(crate) mod frame;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use smithay::backend::allocator::Fourcc;

use crate::session::SessionContext;
use crate::session::compositor::input::CompositorInputEvent;
use crate::session::compositor::{Compositor, CompositorConfig, LaunchedCompositor};
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
}

impl CaptureBackend {
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Isolated => "isolated",
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
	pub input: calloop::channel::Sender<CompositorInputEvent>,
}

/// A session's capture source before it runs.
pub(crate) enum CaptureSource {
	Isolated(Compositor),
}

impl CaptureSource {
	/// Create the selected backend's channels. Nothing runs until [`Self::launch`].
	pub(crate) fn new(
		backend: CaptureBackend,
		config: CompositorConfig,
		context: &SessionContext,
		log_stats: bool,
		stop: ShutdownManager<SessionShutdownReason>,
		foreground_tx: tokio::sync::watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	) -> (Self, CaptureEndpoints) {
		match backend {
			CaptureBackend::Isolated => {
				let (compositor, endpoints) = Compositor::new(
					config,
					crate::session::compositor::CompositorContext::from_session(context, log_stats),
					stop,
					foreground_tx,
				);
				(Self::Isolated(compositor), endpoints)
			},
		}
	}

	/// Start the backend and wait for its readiness. Blocks; the error says
	/// why it could not start, for the operator.
	pub(crate) fn launch(self) -> Result<LiveCapture, String> {
		match self {
			Self::Isolated(compositor) => compositor.launch().map(LiveCapture::Isolated),
		}
	}
}

/// A running capture backend.
pub(crate) enum LiveCapture {
	Isolated(LaunchedCompositor),
}

impl LiveCapture {
	pub(crate) fn capabilities(&self) -> &CaptureCapabilities {
		match self {
			Self::Isolated(compositor) => &compositor.ready().capabilities,
		}
	}

	pub(crate) fn application_display(&self) -> &ApplicationDisplay {
		match self {
			Self::Isolated(compositor) => &compositor.ready().display,
		}
	}

	/// Apply `mode` to the live output and return the effective HDR state.
	/// The caller refuses the stream when that differs from the negotiated one.
	pub(crate) async fn reconfigure(&mut self, mode: OutputMode) -> Result<bool, ()> {
		match self {
			Self::Isolated(compositor) => compositor.reconfigure(mode).await,
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
