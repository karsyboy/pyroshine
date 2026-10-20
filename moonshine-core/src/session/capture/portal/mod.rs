//! Desktop Mode: stream the user's running Wayland desktop through the XDG
//! Desktop Portal.
//!
//! The backend owns nothing of the desktop. It asks the portal for a
//! RemoteDesktop session with one monitor (or a view-only ScreenCast session
//! where RemoteDesktop is missing or input is disabled), receives the
//! monitor's frames from PipeWire as DMA-BUFs and forwards client input to
//! the devices the portal authorized. Stopping closes the portal session;
//! the desktop, its applications, outputs and layout are never changed.
//!
//! | Concern | Where |
//! | --- | --- |
//! | Desktop session discovery and per-desktop quirks | [`desktop`] |
//! | Portal negotiation, consent, restore tokens | [`dbus`], [`token`] |
//! | Compositor GPU identification | [`device`] |
//! | PipeWire frames, buffer lifetime, producer completion | [`stream`] |
//! | Keyboard, pointer and touch through libei or portal methods | [`input`] |
//!
//! Everything runs on the `desktop-capture` thread started through
//! `start_capture_thread`: one calloop loop serves the PipeWire loop, libei,
//! client input and capture demand. A `desktop-portal` helper thread waits
//! for the portal closing the session and for the desktop locking, and is
//! joined before the capture thread exits.
//!
//! The portal works the same on every desktop that implements it (KDE Plasma,
//! GNOME, Ubuntu's GNOME session and others); see `docs/DESKTOP_MODE.md` for
//! the tested desktops and their differences.

pub(crate) mod dbus;
pub(crate) mod desktop;
pub(crate) mod device;
pub(crate) mod input;
pub mod probe;
pub(crate) mod stream;
pub(crate) mod token;

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use calloop::{EventLoop, Interest, Mode, PostAction};
use pipewire as pw;
use serde::{Deserialize, Serialize};
use smithay::backend::allocator::Fourcc;

use self::dbus::{CursorMode, PortalError, PortalSession, SessionRequest};
use self::desktop::{DesktopKind, DesktopSession};
use self::input::{DesktopInput, InputTransport, OutputGeometry};
use self::stream::{CaptureStream, StreamFormat};
use super::admission::{CaptureReceiver, CaptureSender, capture_channel};
use super::input::CaptureInputEvent;
use super::{CaptureBackend, CaptureCapabilities, CaptureEndpoints, OutputMode};
use crate::session::failure::{SessionFailure, StopCause, StopCauseSlot};
use crate::session::manager::SessionShutdownReason;

/// Desktop entry the portal associates Pyroshine with (`pyroshine.desktop`).
const APP_ID: &str = "pyroshine";

/// How long the desktop's consent dialog may stay unanswered. Bounded by the
/// launch request (`APP_LAUNCH_HTTP_TIMEOUT_SECS`).
const CONSENT_TIMEOUT: Duration = Duration::from_secs(45);

/// How long the compositor may take to negotiate the stream format.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Readiness deadline of the capture thread: consent, then the stream.
const READY_TIMEOUT: Duration = Duration::from_secs(55);

/// Desktop Mode settings (`[desktop]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
	/// Offer Desktop Mode to clients as its own application entry.
	pub enabled: bool,
	/// Title of that entry in Moonlight.
	pub title: String,
	/// Let the client control the desktop's keyboard and pointer (the desktop
	/// asks for consent). Off: view-only.
	pub input: bool,
	/// Stream the desktop's audio output.
	pub audio: bool,
}

impl Default for DesktopConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			title: "Desktop Mode".to_string(),
			input: true,
			audio: true,
		}
	}
}

/// What Desktop Mode needs from the session configuration.
#[derive(Clone, Debug)]
pub(crate) struct PortalCaptureConfig {
	pub desktop: DesktopConfig,
	/// `compositor.gpu`: the GPU to import on when the compositor does not
	/// advertise its own.
	pub gpu: Option<String>,
	/// Where the restore token is kept; `None` disables restoring.
	pub restore_token: Option<PathBuf>,
	/// Frame rate asked of the compositor: the launching client's.
	pub refresh_rate: u32,
}

/// The captured output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
	pub width: u32,
	pub height: u32,
	/// Declared frame rate in millihertz, when the compositor declared one.
	pub refresh_millihertz: Option<u32>,
	pub logical_position: Option<(i32, i32)>,
	pub logical_size: Option<(i32, i32)>,
}

/// How the desktop's consent was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Consent {
	/// The portal accepted a stored restore token (or a desktop-level grant)
	/// without asking; the stored token was offered.
	Restored,
	/// The portal asked the user (or granted it by policy) for this session.
	Requested,
}

/// Desktop Mode readiness, reported once.
#[derive(Clone, Debug)]
pub(crate) struct DesktopReady {
	pub capabilities: CaptureCapabilities,
	pub desktop: DesktopSession,
	pub output: CapturedOutput,
	pub input: InputTransport,
	pub consent: Consent,
}

/// State the capture thread shares with the session for reconfiguration.
#[derive(Debug)]
struct Live {
	/// Current captured size; follows output mode changes.
	size: Mutex<(u32, u32)>,
}

/// Desktop Mode before it runs.
pub(crate) struct PortalCapture {
	config: PortalCaptureConfig,
	stop: ShutdownManager<SessionShutdownReason>,
	cause: StopCauseSlot,
	frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CaptureInputEvent>,
}

/// A running Desktop Mode capture.
pub(crate) struct LaunchedPortal {
	ready: DesktopReady,
	live: Arc<Live>,
}

/// A thread failure with its operator-facing classification.
type FailureSlot = Arc<OnceLock<SessionFailure>>;

fn portal_failure(error: &PortalError, desktop: Option<DesktopKind>) -> SessionFailure {
	match error {
		PortalError::Unavailable(reason) => SessionFailure::new(
			"desktop_portal_unavailable",
			format!("The desktop does not offer screen capture to Pyroshine ({reason})."),
		)
		.with_hint(
			"Install the desktop's portal backend (xdg-desktop-portal-kde on Plasma, \
			 xdg-desktop-portal-gnome on GNOME) and log in again.",
		),
		PortalError::Denied => SessionFailure::new(
			"desktop_capture_denied",
			"Screen sharing was declined on the host desktop.",
		)
		.with_hint(consent_hint(desktop)),
		PortalError::TimedOut => SessionFailure::new(
			"desktop_consent_timeout",
			"Nobody answered the host desktop's screen-sharing request.",
		)
		.with_hint(consent_hint(desktop)),
		PortalError::Failed(reason) => SessionFailure::new(
			"desktop_capture_failed",
			format!("The desktop's screen capture failed: {reason}."),
		)
		.with_hint(crate::session::failure::SEE_SERVICE_LOG),
	}
}

fn consent_hint(desktop: Option<DesktopKind>) -> &'static str {
	match desktop {
		Some(DesktopKind::Kde) => {
			"Accept the request on the host once and keep \"Allow restoring\" checked, or allow \
			 Pyroshine permanently: flatpak permission-set kde-authorized remote-desktop pyroshine yes"
		},
		Some(DesktopKind::Gnome) => {
			"Accept the request on the host once and keep \"Remember this selection\" checked; \
			 later sessions then start without asking."
		},
		_ => "Accept the screen-sharing request on the host once; later sessions reuse that consent.",
	}
}

fn capture_failure(code: &'static str, message: impl Into<String>) -> SessionFailure {
	SessionFailure::new(code, message).with_hint(crate::session::failure::SEE_SERVICE_LOG)
}

impl PortalCapture {
	pub(crate) fn new(
		config: PortalCaptureConfig,
		stop: ShutdownManager<SessionShutdownReason>,
		cause: StopCauseSlot,
	) -> (Self, CaptureEndpoints) {
		let (frame_tx, frames): (CaptureSender, CaptureReceiver) = capture_channel();
		let (input, input_rx) = calloop::channel::channel();
		(
			Self {
				config,
				stop,
				cause,
				frame_tx,
				input_rx,
			},
			CaptureEndpoints { frames, input },
		)
	}

	/// Start the portal session and the stream; blocks until frames flow or
	/// the start failed (including an unanswered consent dialog).
	pub(crate) fn launch(self) -> Result<LaunchedPortal, SessionFailure> {
		let Self {
			config,
			stop,
			cause,
			frame_tx,
			input_rx,
		} = self;
		let failure: FailureSlot = Arc::default();
		let live = Arc::new(Live {
			size: Mutex::new((0, 0)),
		});
		let thread_failure = failure.clone();
		let thread_live = live.clone();
		let thread_stop = stop.clone();
		let hold = frame_tx.context_hold();
		let ready = super::start_capture_thread(
			"desktop-capture",
			&stop,
			SessionShutdownReason::DesktopCaptureStopped,
			hold,
			READY_TIMEOUT,
			move |ready_tx| {
				run(config, frame_tx, input_rx, ready_tx, thread_live, thread_stop, cause).map_err(|failure| {
					tracing::error!(code = failure.code, "Desktop capture failed: {}", failure.message);
					let message = failure.message.clone();
					let _ = thread_failure.set(failure);
					message
				})
			},
		)
		.map_err(|message| {
			failure.get().cloned().unwrap_or_else(|| {
				capture_failure(
					"desktop_capture_failed",
					format!("Desktop capture could not start: {message}."),
				)
			})
		})?;
		Ok(LaunchedPortal { ready, live })
	}
}

impl LaunchedPortal {
	pub(crate) fn ready(&self) -> &DesktopReady {
		&self.ready
	}

	/// Desktop Mode never changes the desktop's outputs: any stream size is
	/// served by scaling the captured output in the encoder, and the HDR
	/// state is the captured content's. The caller refuses a stream whose
	/// HDR state differs.
	pub(crate) fn reconfigure(&mut self, mode: OutputMode) -> Result<bool, ()> {
		let (width, height) = *self.live.size.lock().unwrap_or_else(|e| e.into_inner());
		if (mode.width, mode.height) != (width, height) {
			tracing::info!(
				stream_width = mode.width,
				stream_height = mode.height,
				desktop_width = width,
				desktop_height = height,
				"Scaling the desktop output to the stream resolution"
			);
		}
		let hdr = self.ready.capabilities.hdr();
		if mode.hdr != hdr {
			tracing::warn!(
				requested_hdr = mode.hdr,
				desktop_hdr = hdr,
				"The client's dynamic range differs from the captured desktop's"
			);
		}
		Ok(hdr)
	}
}

/// Everything the portal handed over for one session.
struct PortalSetup {
	desktop: DesktopSession,
	session: PortalSession,
	pipewire: OwnedFd,
	eis: Option<OwnedFd>,
	consent: Consent,
}

async fn setup(
	config: &PortalCaptureConfig,
	desktop_kind: &OnceLock<DesktopKind>,
) -> Result<PortalSetup, SessionFailure> {
	let connection = zbus::Connection::session().await.map_err(|error| {
		SessionFailure::new(
			"desktop_session_unavailable",
			format!("Pyroshine cannot reach the desktop's session bus: {error}."),
		)
		.with_hint("Desktop Mode streams the desktop of the user Pyroshine runs as; log in to it first.")
	})?;
	let desktop = desktop::discover(&connection).await.map_err(|unavailable| {
		SessionFailure::new(
			"desktop_session_unavailable",
			format!("There is no desktop to stream: {unavailable}."),
		)
		.with_hint("Log in to a Wayland desktop session (Plasma, GNOME) as the user Pyroshine runs as.")
	})?;
	let _ = desktop_kind.set(desktop.kind);
	tracing::info!(
		desktop = desktop.kind.as_str(),
		current_desktop = desktop.current_desktop,
		socket = %desktop.wayland_socket.display(),
		"Starting Desktop Mode"
	);
	dbus::register(&connection, APP_ID).await;
	let restore_token = config.restore_token.as_deref().and_then(token::load);
	let offered_token = restore_token.is_some();
	let started = PortalSession::start(
		&connection,
		SessionRequest {
			cursor: CursorMode::Embedded,
			input: config.desktop.input,
			restore_token,
		},
	)
	.await;
	let session = match started {
		Ok(session) => session,
		Err(error) => {
			if let (Some(path), PortalError::Denied | PortalError::Failed(_)) = (&config.restore_token, &error) {
				// A revoked or stale grant must not be offered again.
				token::forget(path);
			}
			return Err(portal_failure(&error, Some(desktop.kind)));
		},
	};
	if let (Some(path), Some(token)) = (&config.restore_token, &session.restore_token) {
		token::store(path, token);
	}
	tracing::info!(
		streams = session.streams.len(),
		remote_desktop = session.remote_desktop,
		devices = session.devices,
		screen_cast_version = session.versions.screen_cast,
		remote_desktop_version = session.versions.remote_desktop,
		restored = offered_token,
		"The desktop authorized the portal session"
	);
	let pipewire = session
		.open_pipewire_remote()
		.await
		.map_err(|error| portal_failure(&error, Some(desktop.kind)))?;
	let input_allowed = session.devices & (dbus::DEVICE_KEYBOARD | dbus::DEVICE_POINTER) != 0;
	let eis = if session.remote_desktop && input_allowed && session.versions.eis() {
		match session.connect_to_eis().await {
			Ok(fd) => Some(fd),
			Err(error) => {
				tracing::warn!(%error, "libei input is unavailable; using the portal's input methods");
				None
			},
		}
	} else {
		None
	};
	Ok(PortalSetup {
		desktop,
		session,
		pipewire,
		eis,
		consent: if offered_token {
			Consent::Restored
		} else {
			Consent::Requested
		},
	})
}

/// The render node to import frames on: the compositor's main device, which
/// `compositor.gpu` must match when set.
fn capture_render_node(config: &PortalCaptureConfig, desktop: &DesktopSession) -> Result<PathBuf, SessionFailure> {
	let main = device::compositor_main_device(&desktop.wayland_socket).map_err(|error| {
		capture_failure(
			"desktop_capture_failed",
			format!("Cannot identify the desktop's GPU: {error}."),
		)
	})?;
	let compositor_node = main.and_then(device::render_node);
	match (compositor_node, &config.gpu) {
		(Some(node), Some(_)) => {
			let configured = crate::healthcheck::find_render_node(&config.gpu)
				.map_err(|error| capture_failure("desktop_gpu_mismatch", error))?;
			if device::device_number(&configured) != device::device_number(&node) {
				return Err(SessionFailure::new(
					"desktop_gpu_mismatch",
					format!(
						"The desktop renders on {} but compositor.gpu selects {}.",
						node.display(),
						configured.display()
					),
				)
				.with_hint(
					"Desktop Mode encodes on the GPU the desktop renders on; remove compositor.gpu or set it to that GPU.",
				));
			}
			Ok(node)
		},
		(Some(node), None) => Ok(node),
		(None, gpu) => {
			tracing::info!("The desktop compositor advertises no main DMA-BUF device; using the configured GPU");
			crate::healthcheck::find_render_node(gpu).map_err(|error| capture_failure("desktop_capture_failed", error))
		},
	}
}

/// Events from the `desktop-portal` helper thread.
#[derive(Debug)]
enum WatchEvent {
	/// The portal closed the session (sharing stopped on the desktop, logout).
	Closed,
	/// The desktop's lock state changed.
	Locked(bool),
}

/// Watch the portal session and the desktop's logind session until `done`.
fn spawn_watcher(
	connection: zbus::Connection,
	handle: zbus::zvariant::OwnedObjectPath,
	session_id: Option<String>,
	events: calloop::channel::Sender<WatchEvent>,
	done: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
	std::thread::Builder::new()
		.name("desktop-portal".into())
		.spawn(move || {
			let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
				return;
			};
			runtime.block_on(async move {
				let closed = async {
					match dbus::closed(&connection, handle.as_ref()).await {
						Ok(()) => {
							let _ = events.send(WatchEvent::Closed);
						},
						Err(error) => tracing::debug!(%error, "Cannot watch the portal session"),
					}
				};
				// The lock watch may end at once (no logind session); it must not
				// end the watcher, which only `done` or the portal closing ends.
				let locked = async {
					watch_lock(session_id, events.clone()).await;
					std::future::pending::<()>().await
				};
				tokio::select! {
					_ = closed => {},
					_ = locked => {},
					_ = done => {},
				}
			});
		})
}

/// Report the desktop's logind `LockedHint` changes.
async fn watch_lock(session_id: Option<String>, events: calloop::channel::Sender<WatchEvent>) {
	use futures_util::StreamExt;
	let Some(session_id) = session_id else {
		return;
	};
	let Ok(system) = zbus::Connection::system().await else {
		return;
	};
	let Ok(path) = system
		.call_method(
			Some("org.freedesktop.login1"),
			"/org/freedesktop/login1",
			Some("org.freedesktop.login1.Manager"),
			"GetSession",
			&(session_id.as_str(),),
		)
		.await
		.and_then(|reply| reply.body().deserialize::<zbus::zvariant::OwnedObjectPath>())
	else {
		return;
	};
	let Ok(proxy) = zbus::Proxy::new(
		&system,
		"org.freedesktop.login1",
		path,
		"org.freedesktop.login1.Session",
	)
	.await
	else {
		return;
	};
	let mut changes = proxy.receive_property_changed::<bool>("LockedHint").await;
	while let Some(change) = changes.next().await {
		if let Ok(locked) = change.get().await {
			let _ = events.send(WatchEvent::Locked(locked));
		}
	}
}

/// Loop state of the capture thread.
struct State {
	mainloop: pw::main_loop::MainLoopRc,
	stream: CaptureStream,
	input: DesktopInput,
	live: Arc<Live>,
	format: StreamFormat,
	/// Why the capture ended; the loop exits once set.
	ended: Option<SessionFailure>,
	locked: bool,
	stats_at: Instant,
	stats_sent: u64,
}

impl State {
	/// Run the PipeWire loop without blocking and check the stream.
	fn pump(&mut self) {
		self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
		self.check_stream();
	}

	fn check_stream(&mut self) {
		let (format, failure) = self
			.stream
			.with_status(|status| (status.format, status.failure.clone()));
		if let Some(failure) = failure {
			self.ended.get_or_insert_with(|| {
				capture_failure("desktop_capture_lost", format!("The desktop capture ended: {failure}."))
			});
			return;
		}
		if let Some(format) = format
			&& format != self.format
		{
			tracing::info!(
				old_width = self.format.width,
				old_height = self.format.height,
				width = format.width,
				height = format.height,
				"The captured desktop output changed"
			);
			*self.live.size.lock().unwrap_or_else(|e| e.into_inner()) = (format.width, format.height);
			// The encoders keep the negotiated size and scale the new output.
			self.input.set_output(OutputGeometry {
				width: format.width,
				height: format.height,
				..self.input.output().clone()
			});
			self.format = format;
		}
	}

	fn log_stats(&mut self) {
		let elapsed = self.stats_at.elapsed();
		if elapsed < Duration::from_secs(10) {
			return;
		}
		let (sent, skipped, last, processed, received, held) = self.stream.with_status(|status| {
			(
				status.sent,
				status.skipped,
				status.last_frame,
				status.processed,
				status.received,
				status.held,
			)
		});
		tracing::debug!(
			fps = (sent - self.stats_sent) as f64 / elapsed.as_secs_f64(),
			skipped,
			processed,
			received,
			held,
			idle_ms = last.map(|last| last.elapsed().as_millis() as u64),
			locked = self.locked,
			"Desktop capture statistics"
		);
		self.stats_at = Instant::now();
		self.stats_sent = sent;
	}
}

fn insert_error<T>(error: calloop::InsertError<T>) -> SessionFailure {
	capture_failure(
		"desktop_capture_failed",
		format!("Cannot watch an event source: {}.", error.error),
	)
}

fn dup(fd: std::os::fd::BorrowedFd<'_>) -> Result<OwnedFd, SessionFailure> {
	fd.try_clone_to_owned()
		.map_err(|e| capture_failure("desktop_capture_failed", format!("Cannot duplicate a descriptor: {e}.")))
}

fn run(
	config: PortalCaptureConfig,
	frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CaptureInputEvent>,
	ready_tx: mpsc::SyncSender<DesktopReady>,
	live: Arc<Live>,
	stop: ShutdownManager<SessionShutdownReason>,
	cause: StopCauseSlot,
) -> Result<(), SessionFailure> {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|e| {
			capture_failure(
				"desktop_capture_failed",
				format!("Cannot start the portal client: {e}."),
			)
		})?;
	let desktop_kind = OnceLock::new();
	let setup = runtime.block_on(async {
		stop.wrap_cancel(tokio::time::timeout(CONSENT_TIMEOUT, setup(&config, &desktop_kind)))
			.await
	});
	let PortalSetup {
		desktop,
		session,
		pipewire,
		eis,
		consent,
	} = match setup {
		// The session stopped while the portal was asking: nothing was
		// handed over yet; dropping the futures closed the connection.
		Err(_) => return Ok(()),
		Ok(Err(_)) => return Err(portal_failure(&PortalError::TimedOut, desktop_kind.get().copied())),
		Ok(Ok(setup)) => setup?,
	};
	// From here on the portal session is open: every exit closes it below.
	let result = capture(
		&config, frame_tx, input_rx, ready_tx, &live, &stop, &cause, &desktop, &session, pipewire, eis, consent,
	);
	runtime.block_on(async {
		let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
	});
	tracing::info!("Desktop Mode capture stopped; the desktop session is unchanged");
	result
}

#[allow(clippy::too_many_arguments)]
fn capture(
	config: &PortalCaptureConfig,
	mut frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CaptureInputEvent>,
	ready_tx: mpsc::SyncSender<DesktopReady>,
	live: &Arc<Live>,
	stop: &ShutdownManager<SessionShutdownReason>,
	cause: &StopCauseSlot,
	desktop: &DesktopSession,
	session: &PortalSession,
	pipewire: OwnedFd,
	eis: Option<OwnedFd>,
	consent: Consent,
) -> Result<(), SessionFailure> {
	let source = session.streams[0].clone();
	if session.streams.len() > 1 {
		tracing::info!(
			streams = session.streams.len(),
			"The portal shared several sources; streaming the first"
		);
	}

	// The GPU the frames live on, verified like the isolated backend's.
	let render_node = capture_render_node(config, desktop)?;
	let render_file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(&render_node)
		.map_err(|e| {
			capture_failure(
				"desktop_capture_failed",
				format!("Cannot open the desktop's GPU {}: {e}.", render_node.display()),
			)
		})?;
	let capture_context =
		crate::gpu::capture_context(&render_file).map_err(|error| capture_failure("desktop_capture_failed", error))?;
	let importable = crate::gpu::ImportableModifiers::query(&capture_context);
	frame_tx.set_context(capture_context);
	let offered = stream::offered_formats(|fourcc| importable.modifiers(fourcc));
	let demand = frame_tx.take_demand_source();

	pw::init();
	let mainloop = pw::main_loop::MainLoopRc::new(None)
		.map_err(|e| capture_failure("desktop_capture_failed", format!("Cannot start PipeWire: {e}.")))?;
	let context = pw::context::ContextRc::new(&mainloop, None)
		.map_err(|e| capture_failure("desktop_capture_failed", format!("Cannot start PipeWire: {e}.")))?;
	let core = context.connect_fd_rc(pipewire, None).map_err(|e| {
		capture_failure(
			"desktop_capture_failed",
			format!("Cannot connect to the desktop's PipeWire: {e}."),
		)
	})?;
	let core_error: std::rc::Rc<std::cell::RefCell<Option<String>>> = Default::default();
	let core_error_slot = core_error.clone();
	let _core_listener = core
		.add_listener_local()
		.error(move |id, _, result, message| {
			if id == pw::core::PW_ID_CORE {
				core_error_slot
					.borrow_mut()
					.get_or_insert_with(|| format!("PipeWire error {result}: {message}"));
			}
		})
		.register();
	let stream = CaptureStream::connect(&core, source.node_id, offered, config.refresh_rate.max(1), frame_tx)
		.map_err(|error| capture_failure("desktop_capture_failed", error))?;

	// Wait for the compositor to negotiate a format.
	let deadline = Instant::now() + FORMAT_TIMEOUT;
	let format = loop {
		if stop.is_shutdown_triggered() {
			return Ok(());
		}
		mainloop
			.loop_()
			.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
		let (format, failure) = stream.with_status(|status| (status.format, status.failure.clone()));
		if let Some(failure) = failure.or_else(|| core_error.borrow().clone()) {
			return Err(capture_failure(
				"desktop_capture_unsupported",
				format!("The desktop's screen cast cannot be streamed: {failure}."),
			));
		}
		if let Some(format) = format {
			break format;
		}
		if Instant::now() > deadline {
			return Err(capture_failure(
				"desktop_capture_failed",
				"The desktop's screen cast did not negotiate a format within 5 seconds.",
			));
		}
	};
	*live.size.lock().unwrap_or_else(|e| e.into_inner()) = (format.width, format.height);

	let geometry = OutputGeometry {
		width: format.width,
		height: format.height,
		logical_position: source.position,
		logical_size: source.size,
		mapping_id: source.mapping_id.clone(),
		node_id: source.node_id,
	};
	let input = match eis {
		Some(fd) => DesktopInput::eis(fd, geometry.clone()).map_err(|e| {
			capture_failure(
				"desktop_capture_failed",
				format!("Cannot use the input connection: {e}."),
			)
		})?,
		None if session.remote_desktop && session.devices != 0 => {
			DesktopInput::portal(session.connection().clone(), session.handle(), geometry.clone())
		},
		None => DesktopInput::none(geometry.clone()),
	};
	let color = format.color.color_space().expect("checked at negotiation");
	let hdr = color != crate::session::capture::frame::FrameColorSpace::Srgb;
	let fourcc = Fourcc::try_from(format.fourcc).unwrap_or(Fourcc::Xrgb8888);
	let ready = DesktopReady {
		capabilities: CaptureCapabilities {
			backend: CaptureBackend::Desktop,
			render_node: render_node.clone(),
			sdr_format: fourcc,
			hdr_format: hdr.then_some(fourcc),
			presentation_pacing: false,
		},
		desktop: desktop.clone(),
		output: CapturedOutput {
			width: format.width,
			height: format.height,
			refresh_millihertz: format
				.framerate
				.map(|rate| (u64::from(rate.num) * 1000 / u64::from(rate.denom.max(1))) as u32),
			logical_position: source.position,
			logical_size: source.size,
		},
		input: input.transport(),
		consent,
	};

	// Watch the portal session and the desktop lock from a helper thread.
	let (watch_tx, watch_rx) = calloop::channel::channel();
	let (watch_done, watch_done_rx) = tokio::sync::oneshot::channel();
	let watcher = spawn_watcher(
		session.connection().clone(),
		session.handle().into(),
		desktop.session_id.clone(),
		watch_tx,
		watch_done_rx,
	)
	.map_err(|e| {
		capture_failure(
			"desktop_capture_failed",
			format!("Cannot start the portal watcher: {e}."),
		)
	})?;

	let result = (|| {
		let mut event_loop: EventLoop<State> = EventLoop::try_new()
			.map_err(|e| capture_failure("desktop_capture_failed", format!("Cannot create the event loop: {e}.")))?;
		let handle = event_loop.handle();
		let pipewire_fd = dup(mainloop.loop_().fd())?;
		handle
			.insert_source(
				calloop::generic::Generic::new(pipewire_fd, Interest::READ, Mode::Level),
				|_, _, state: &mut State| {
					state.pump();
					Ok(PostAction::Continue)
				},
			)
			.map_err(insert_error)?;
		if let Some(fd) = input.eis_fd() {
			let fd = dup(fd)?;
			handle
				.insert_source(
					calloop::generic::Generic::new(fd, Interest::READ, Mode::Level),
					|_, _, state: &mut State| match state.input.dispatch() {
						Ok(()) => Ok(PostAction::Continue),
						Err(error) => {
							tracing::warn!(%error, "Remote input ended; the desktop stays view-only");
							Ok(PostAction::Remove)
						},
					},
				)
				.map_err(insert_error)?;
		}
		handle
			.insert_source(input_rx, |event, _, state: &mut State| {
				if let calloop::channel::Event::Msg(event) = event {
					state.input.handle(event);
				}
			})
			.map_err(insert_error)?;
		if let Some(demand) = demand {
			handle
				.insert_source(demand, |_, _, state: &mut State| state.stream.service())
				.map_err(insert_error)?;
		}
		handle
			.insert_source(watch_rx, |event, _, state: &mut State| match event {
				calloop::channel::Event::Msg(WatchEvent::Closed) => {
					state.ended.get_or_insert_with(|| {
						SessionFailure::new(
							"desktop_capture_stopped",
							"Screen sharing was stopped on the host desktop.",
						)
					});
				},
				calloop::channel::Event::Msg(WatchEvent::Locked(locked)) => {
					state.locked = locked;
					tracing::info!(
						locked,
						"The desktop's lock state changed; streaming continues as the desktop shows it"
					);
				},
				calloop::channel::Event::Closed => {},
			})
			.map_err(insert_error)?;
		handle
			.insert_source(
				calloop::timer::Timer::from_duration(Duration::from_millis(100)),
				|_, _, state: &mut State| {
					// Return consumed buffers even without new demand.
					state.stream.service();
					state.check_stream();
					state.log_stats();
					calloop::timer::TimeoutAction::ToDuration(Duration::from_millis(100))
				},
			)
			.map_err(insert_error)?;

		let mut state = State {
			mainloop: mainloop.clone(),
			stream,
			input,
			live: live.clone(),
			format,
			ended: None,
			locked: false,
			stats_at: Instant::now(),
			stats_sent: 0,
		};
		tracing::info!(
			desktop = desktop.kind.as_str(),
			width = format.width,
			height = format.height,
			render_node = %render_node.display(),
			input = state.input.transport().as_str(),
			hdr,
			"Desktop capture ready"
		);
		if ready_tx.send(ready).is_err() {
			return Ok(());
		}
		while !stop.is_shutdown_triggered() && state.ended.is_none() {
			event_loop
				.dispatch(Some(Duration::from_millis(100)), &mut state)
				.map_err(|e| capture_failure("desktop_capture_failed", format!("Event loop failed: {e}.")))?;
		}
		// Release everything held before the devices and the stream go away.
		state.input.shutdown();
		state.pump();
		match state.ended.take() {
			Some(ended) if !stop.is_shutdown_triggered() => {
				// Recorded before this thread's exit stops the session.
				cause.record(StopCause {
					detail: ended.message.clone(),
					hint: ended.hint.clone(),
					unexpected: ended.code != "desktop_capture_stopped",
					reported: false,
				});
				Err(ended)
			},
			_ => Ok(()),
		}
	})();
	let _ = watch_done.send(());
	let _ = watcher.join();
	result
}
