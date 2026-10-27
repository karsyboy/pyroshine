//! Managed Desktop Mode capture. The desktop guard owns native outputs,
//! topology and audio routing; this worker receives their PipeWire DMA-BUFs
//! directly and routes input through a separately authorized portal session.

pub(crate) mod dbus;
pub(crate) mod desktop;
pub(crate) mod device;
pub(crate) mod input;
mod ledger;
pub mod metrics;
mod pacer;
pub mod probe;
pub(crate) mod readiness;
pub(crate) mod stream;
pub(crate) mod sync;
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

use self::dbus::{PortalError, PortalSession, SessionRequest};
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
	/// Time to retain takeover after the controlling client disconnects.
	pub disconnect_grace_secs: u64,
}

impl Default for DesktopConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			title: "Desktop Mode".to_string(),
			input: true,
			audio: true,
			disconnect_grace_secs: 30,
		}
	}
}

/// What Desktop Mode needs from the session configuration.
#[derive(Clone, Debug)]
pub(crate) struct PortalCaptureConfig {
	pub desktop: DesktopConfig,
	pub client_identity: Option<String>,
	/// `compositor.gpu`: the GPU to import on when the compositor does not
	/// advertise its own.
	pub gpu: Option<String>,
	/// Where the restore token is kept; `None` disables restoring.
	pub restore_token: Option<PathBuf>,
	/// Frame rate asked of the compositor: the launching client's.
	pub refresh_rate: u32,
	pub resolution: (u32, u32),
	pub diagnostics: bool,
	pub implicit_sync: bool,
}

/// The captured output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
	pub width: u32,
	pub height: u32,
	/// Frame rate in millihertz: the declared rate, else the maximum rate the
	/// compositor agreed to (its output refresh, or the client's rate if lower).
	pub refresh_millihertz: Option<u32>,
	pub logical_position: Option<(i32, i32)>,
	pub logical_size: Option<(i32, i32)>,
}

/// How the desktop's consent was asked for. The portal alone decides whether
/// to show its dialog: a saved permission that no longer covers the request
/// (another source, input added) makes it ask again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Consent {
	/// A saved permission (restore token) of an earlier session was offered.
	Restored,
	/// No saved permission: the desktop asked, or granted it by its policy.
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
	pub lease: String,
}

/// State the capture thread shares with the session for reconfiguration.
#[derive(Debug)]
struct Live {
	/// Current captured size; follows output mode changes.
	size: Mutex<(u32, u32)>,
	/// How the stream presents the output, for input mapping.
	view: Arc<input::StreamView>,
	mode: Mutex<crate::desktop::Mode>,
	changes: calloop::channel::Sender<ModeChange>,
	diagnostics: Mutex<Option<metrics::CaptureReport>>,
}

#[derive(Debug)]
struct ModeChange {
	mode: crate::desktop::Mode,
	done: tokio::sync::oneshot::Sender<Result<bool, ()>>,
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
	/// The encoder letterboxes a scaled output (conventional codecs); PyroWave
	/// stretches it.
	fit: bool,
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
		_ => "Authorize remote keyboard and pointer access on the host; saved permissions may be reused.",
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
		let (changes, mode_rx) = calloop::channel::channel();
		let live = Arc::new(Live {
			size: Mutex::new((0, 0)),
			view: Arc::default(),
			mode: Mutex::new(crate::desktop::Mode {
				width: config.resolution.0,
				height: config.resolution.1,
				refresh_millihertz: config.refresh_rate.saturating_mul(1000),
			}),
			changes,
			diagnostics: Mutex::new(None),
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
				run(
					config,
					frame_tx,
					input_rx,
					ready_tx,
					thread_live,
					thread_stop,
					cause,
					mode_rx,
				)
				.map_err(|failure| {
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
		Ok(LaunchedPortal { ready, live, fit: true })
	}
}

impl LaunchedPortal {
	pub(crate) fn diagnostics(&self) -> Option<metrics::CaptureReport> {
		self.live.diagnostics.lock().unwrap_or_else(|e| e.into_inner()).clone()
	}
	pub(crate) fn ready(&self) -> &DesktopReady {
		&self.ready
	}

	/// Whether the stream's encoder keeps the output's aspect ratio when it
	/// scales it; set before [`Self::reconfigure`] for each negotiated stream.
	pub(crate) fn set_scaling_fit(&mut self, fit: bool) {
		self.fit = fit;
		self.live.view.set_fit(fit);
	}

	/// Reconfigure the owned native output, never simulate a mode with scaling.
	/// The managed output is always SDR, so a request for HDR yields SDR (the
	/// returned effective state) rather than failing the stream.
	pub(crate) async fn reconfigure(&mut self, mode: OutputMode) -> Result<bool, ()> {
		let mode = crate::desktop::Mode {
			width: mode.width,
			height: mode.height,
			refresh_millihertz: mode.refresh_rate.saturating_mul(1000),
		};
		if *self.live.mode.lock().unwrap_or_else(|e| e.into_inner()) == mode {
			return Ok(false);
		}
		let (done, waiting) = tokio::sync::oneshot::channel();
		self.live.changes.send(ModeChange { mode, done }).map_err(|_| ())?;
		tokio::time::timeout(Duration::from_secs(20), waiting)
			.await
			.map_err(|_| ())?
			.map_err(|_| ())?
	}
}

/// Everything the portal handed over for one session.
struct PortalSetup {
	desktop: DesktopSession,
	session: Option<PortalSession>,
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
	// A capability probe never changes outputs. Consent is obtained before the
	// guard begins the durable display transaction.
	let support = crate::desktop::status().map_err(|e| capture_failure("desktop_guard_unavailable", e))?;
	if !support.supported {
		return Err(capture_failure(
			"desktop_capture_unsupported",
			support.reason.unwrap_or_else(|| "Desktop Mode is unsupported".into()),
		));
	}
	if !config.desktop.input {
		return Ok(PortalSetup {
			desktop,
			session: None,
			eis: None,
			consent: Consent::Requested,
		});
	}
	dbus::register(&connection, APP_ID).await;
	let restore_token = config.restore_token.as_deref().and_then(token::load);
	let offered = restore_token.is_some();
	let session = PortalSession::start(&connection, SessionRequest { restore_token })
		.await
		.map_err(|e| {
			if matches!(e, PortalError::Denied | PortalError::Failed(_))
				&& let Some(path) = &config.restore_token
			{
				token::forget(path);
			}
			portal_failure(&e, Some(desktop.kind))
		})?;
	let required = dbus::DEVICE_KEYBOARD | dbus::DEVICE_POINTER;
	if session.devices & required != required {
		return Err(capture_failure(
			"desktop_input_denied",
			"Desktop Mode requires keyboard and pointer authorization",
		));
	}
	let eis = session
		.connect_to_eis()
		.await
		.map_err(|e| portal_failure(&e, Some(desktop.kind)))?;
	if let (Some(path), Some(token)) = (&config.restore_token, &session.restore_token) {
		token::store(path, token);
	}
	Ok(PortalSetup {
		desktop,
		session: Some(session),
		eis: Some(eis),
		consent: if offered { Consent::Restored } else { Consent::Requested },
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
	portal: Option<(zbus::Connection, zbus::zvariant::OwnedObjectPath)>,
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
					let Some((connection, handle)) = portal else {
						std::future::pending::<()>().await;
						return;
					};
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
	if let Ok(locked) = proxy.get_property::<bool>("LockedHint").await {
		let _ = events.send(WatchEvent::Locked(locked));
	}
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
	input_readiness: input::Readiness,
	live: Arc<Live>,
	format: StreamFormat,
	/// Why the capture ended; the loop exits once set.
	ended: Option<SessionFailure>,
	locked: bool,
	stats_at: Instant,
	stats_sent: u64,
	core: pw::core::CoreRc,
	core_error: std::rc::Rc<std::cell::RefCell<Option<String>>>,
	offered: Vec<stream::OfferedFormat>,
	guard: crate::desktop::Client,
	target: crate::desktop::Target,
	retired: Vec<(CaptureStream, String)>,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl State {
	/// Run the PipeWire loop without blocking and check the stream.
	fn pump(&mut self) {
		self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
		self.stream.service();
		self.check_stream();
		for (stream, _) in &self.retired {
			stream.service();
		}
		let mut index = 0;
		while index < self.retired.len() {
			if self.retired[index].0.drained() {
				let (_, id) = self.retired.remove(index);
				if let Err(error) = self.guard.retire(&id) {
					self.ended = Some(capture_failure("desktop_retire_failed", error));
				}
			} else {
				index += 1;
			}
		}
	}

	fn check_input(&mut self) {
		let ready = self.input.ready();
		self.input.set_suspended(!ready);
		if self.input_readiness.expired(ready, Instant::now()) {
			self.ended.get_or_insert_with(|| {
				capture_failure(
					"desktop_input_lost",
					"Remote input devices did not become ready for the virtual output within 3 seconds",
				)
			});
		}
	}

	fn check_stream(&mut self) {
		if let Some(error) = self.core_error.borrow().clone() {
			self.ended = Some(capture_failure("desktop_capture_lost", error));
			return;
		}
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
			self.ended = Some(capture_failure(
				"desktop_capture_changed",
				"The compositor changed the managed capture format unexpectedly; restoring displays",
			));
		}
	}

	fn reconfigure(&mut self, mode: crate::desktop::Mode) -> Result<bool, String> {
		if self.stop.is_shutdown_triggered() {
			return Err("Session is stopping".into());
		}
		let target = self.guard.replacement(mode)?;
		let (replacement, _) = open_stream(
			&self.core,
			&target,
			self.offered.clone(),
			self.stream.replacement_sender(),
			StreamOptions {
				mainloop: self.mainloop.clone(),
				readiness: self.stream.readiness(),
				sync_device: self.stream.sync_device(),
				diagnostics: self
					.live
					.diagnostics
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.as_ref()
					.is_some_and(|d| d.timings_enabled),
			},
			self.stop.clone(),
		)?;
		self.stream.set_active(false);
		let deadline = Instant::now() + FORMAT_TIMEOUT;
		while !self.stream.drained() || self.retired.iter().any(|(s, _)| !s.drained()) {
			self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
			self.stream.service();
			for (stream, _) in &self.retired {
				stream.service();
			}
			if self.stop.is_shutdown_triggered() || Instant::now() >= deadline {
				return Err("Old desktop capture buffers did not drain before replacement".into());
			}
			std::thread::sleep(Duration::from_millis(2));
		}
		// KWin closes old producer nodes during commit. Disconnect the drained
		// consumer first, and flush that destruction before the native owner is
		// retired, so late node/port updates cannot address a removed resource.
		self.stream.disconnect_drained()?;
		pipewire_roundtrip(&self.mainloop, &self.core, &self.stop)?;
		let target = self.guard.commit(&target.id)?;
		self.input.set_output(geometry(&target));
		wait_input(&self.mainloop, &mut self.input, self.stop.clone())?;
		let format = replacement
			.with_status(|s| s.format)
			.ok_or("Replacement stream has no format")?;
		replacement.observe_output(target.observed_mode);
		let previous = std::mem::replace(&mut self.stream, replacement);
		self.retired.push((previous, self.target.id.clone()));
		self.target = target;
		self.format = format;
		self.stats_sent = 0;
		self.stats_at = Instant::now();
		*self.live.size.lock().unwrap_or_else(|e| e.into_inner()) = (mode.width, mode.height);
		*self.live.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
		self.live.view.set(mode.width, mode.height, true);
		self.stream.set_active(true);
		Ok(false)
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

#[allow(clippy::too_many_arguments)]
fn run(
	config: PortalCaptureConfig,
	frame_tx: CaptureSender,
	input_rx: calloop::channel::Channel<CaptureInputEvent>,
	ready_tx: mpsc::SyncSender<DesktopReady>,
	live: Arc<Live>,
	stop: ShutdownManager<SessionShutdownReason>,
	cause: StopCauseSlot,
	mode_rx: calloop::channel::Channel<ModeChange>,
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
		eis,
		consent,
	} = match setup {
		// The session stopped while the portal was asking: nothing was
		// handed over yet; dropping the futures closed the connection.
		Err(_) => return Ok(()),
		Ok(Err(_)) => return Err(portal_failure(&PortalError::TimedOut, desktop_kind.get().copied())),
		Ok(Ok(setup)) => setup?,
	};
	let mode = crate::desktop::Mode {
		width: config.resolution.0,
		height: config.resolution.1,
		refresh_millihertz: config.refresh_rate.saturating_mul(1000),
	};
	let (guard, target) = crate::desktop::Client::prepare(mode, config.desktop.audio, config.client_identity.clone())
		.map_err(|e| capture_failure("desktop_prepare_failed", e))?;
	let result = capture(
		&config,
		frame_tx,
		input_rx,
		ready_tx,
		&live,
		&stop,
		&cause,
		&desktop,
		session.as_ref(),
		eis,
		consent,
		guard.clone(),
		target,
		mode_rx,
	);
	let restored = guard.restore();
	runtime.block_on(async {
		if let Some(session) = session {
			let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
		}
	});
	if let Err(error) = restored {
		return Err(capture_failure("desktop_recovery_required", error));
	}

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
	session: Option<&PortalSession>,
	eis: Option<OwnedFd>,
	consent: Consent,
	guard: crate::desktop::Client,
	target: crate::desktop::Target,
	mode_rx: calloop::channel::Channel<ModeChange>,
) -> Result<(), SessionFailure> {
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
	let core = context.connect_rc(None).map_err(|e| {
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
	let readiness = readiness::Readiness::new().map_err(|e| capture_failure("desktop_capture_failed", e))?;
	let (stream, format) = open_stream(
		&core,
		&target,
		offered.clone(),
		frame_tx,
		StreamOptions {
			mainloop: mainloop.clone(),
			readiness: readiness.clone(),
			diagnostics: config.diagnostics,
			sync_device: (!config.implicit_sync)
				.then(|| sync::SyncDevice::probe(std::os::fd::AsFd::as_fd(&render_file)))
				.flatten(),
		},
		stop.clone(),
	)
	.map_err(|e| capture_failure("desktop_capture_unsupported", e))?;
	let output_geometry = geometry(&target);

	let mut input = match eis {
		Some(fd) => DesktopInput::eis(fd, output_geometry.clone()).map_err(|e| {
			capture_failure(
				"desktop_capture_failed",
				format!("Cannot use the input connection: {e}."),
			)
		})?,
		None => DesktopInput::none(output_geometry.clone()),
	};
	wait_input_devices(&mainloop, &mut input, stop.clone())
		.map_err(|e| capture_failure("desktop_input_unavailable", e))?;
	if format.color.color_space().is_none_or(|color| color.is_hdr()) {
		return Err(capture_failure(
			"desktop_hdr_unsupported",
			"The prepared virtual output is not SDR",
		));
	}
	let target = guard
		.commit(&target.id)
		.map_err(|e| capture_failure("desktop_commit_failed", e))?;
	input.set_output(geometry(&target));
	*live.size.lock().unwrap_or_else(|e| e.into_inner()) = (format.width, format.height);
	wait_input(&mainloop, &mut input, stop.clone()).map_err(|e| capture_failure("desktop_input_unavailable", e))?;
	input.set_view(live.view.clone());
	let color = format.color.color_space().expect("checked at negotiation");
	let hdr = color.is_hdr();
	if hdr {
		return Err(capture_failure(
			"desktop_hdr_unsupported",
			"Managed Desktop Mode requires an SDR virtual output",
		));
	}
	stream.observe_output(target.observed_mode);
	stream.set_active(true);
	*live.diagnostics.lock().unwrap_or_else(|e| e.into_inner()) = Some(stream.report());
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
				.or(format.max_framerate)
				.map(|rate| (u64::from(rate.num) * 1000 / u64::from(rate.denom.max(1))) as u32),
			logical_position: Some((0, 0)),
			logical_size: Some((format.width as i32, format.height as i32)),
		},
		input: input.transport(),
		consent,
		lease: guard.lease.clone(),
	};

	// Watch the portal session and the desktop lock from a helper thread.
	let (watch_tx, watch_rx) = calloop::channel::channel();
	let (watch_done, watch_done_rx) = tokio::sync::oneshot::channel();
	let watcher = spawn_watcher(
		session.map(|s| (s.connection().clone(), s.handle().into())),
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
		handle
			.insert_source(
				calloop::generic::Generic::new(dup(readiness.fd())?, Interest::READ, Mode::Level),
				|_, _, state: &mut State| {
					state.stream.service();
					state.check_stream();
					Ok(PostAction::Continue)
				},
			)
			.map_err(insert_error)?;
		if stream.request_driven() {
			handle
				.insert_source(calloop::timer::Timer::immediate(), |_, _, state: &mut State| {
					state.stream.service();
					calloop::timer::TimeoutAction::ToDuration(state.stream.schedule_after())
				})
				.map_err(insert_error)?;
		}
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
						Ok(()) => {
							state.check_input();
							Ok(PostAction::Continue)
						},
						Err(error) => {
							state.ended = Some(capture_failure("desktop_input_lost", error));
							Ok(PostAction::Remove)
						},
					},
				)
				.map_err(insert_error)?;
		}
		handle
			.insert_source(mode_rx, |event, _, state: &mut State| {
				if let calloop::channel::Event::Msg(change) = event {
					match state.reconfigure(change.mode) {
						Ok(hdr) => {
							let _ = change.done.send(Ok(hdr));
						},
						Err(error) => {
							state.ended = Some(capture_failure("desktop_reconfigure_failed", error));
							let _ = change.done.send(Err(()));
						},
					}
				}
			})
			.map_err(insert_error)?;
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
					if locked {
						state.ended = Some(capture_failure(
							"desktop_locked",
							"The host desktop was locked; restoring displays",
						));
					}
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
					state.check_input();
					state.log_stats();
					let mut diagnostics = state.live.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
					if diagnostics.is_some() {
						*diagnostics = Some(state.stream.report());
					}
					calloop::timer::TimeoutAction::ToDuration(Duration::from_millis(100))
				},
			)
			.map_err(insert_error)?;

		let mut state = State {
			mainloop: mainloop.clone(),
			stream,
			input,
			input_readiness: input::Readiness::default(),
			live: live.clone(),
			format,
			ended: None,
			locked: false,
			stats_at: Instant::now(),
			stats_sent: 0,
			core: core.clone(),
			core_error: core_error.clone(),
			offered,
			guard: guard.clone(),
			target,
			retired: Vec::new(),
			stop: stop.clone(),
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
			if let Err(error) = event_loop.dispatch(Some(Duration::from_millis(100)), &mut state) {
				state.ended = Some(capture_failure(
					"desktop_capture_failed",
					format!("Event loop failed: {error}."),
				));
			}
		}
		// Release everything held before the devices and the stream go away.
		state.input.shutdown();
		state.stream.set_active(false);
		// Do not return a producer buffer while an encoder can still read it.
		while !state.stream.drained() || state.retired.iter().any(|(s, _)| !s.drained()) {
			state.pump();
			std::thread::sleep(Duration::from_millis(2));
		}
		state.pump();
		{
			let mut diagnostics = state.live.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
			if diagnostics.is_some() {
				*diagnostics = Some(state.stream.report());
			}
		}
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

fn geometry(target: &crate::desktop::Target) -> OutputGeometry {
	OutputGeometry {
		width: target.mode.width,
		height: target.mode.height,
		logical_position: Some((0, 0)),
		logical_size: Some((target.mode.width as i32, target.mode.height as i32)),
		mapping_id: None,
		follow_scale: true,
	}
}
struct StreamOptions {
	mainloop: pw::main_loop::MainLoopRc,
	readiness: std::rc::Rc<readiness::Readiness>,
	sync_device: Option<std::rc::Rc<sync::SyncDevice>>,
	diagnostics: bool,
}

fn pipewire_roundtrip(
	mainloop: &pw::main_loop::MainLoopRc,
	core: &pw::core::CoreRc,
	stop: &ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	let done = std::rc::Rc::new(std::cell::Cell::new(false));
	let sequence = core.sync(0).map_err(|e| e.to_string())?;
	let received = done.clone();
	let _listener = core
		.add_listener_local()
		.done(move |id, seq| {
			if id == pw::core::PW_ID_CORE && seq == sequence {
				received.set(true);
			}
		})
		.register();
	let deadline = Instant::now() + FORMAT_TIMEOUT;
	while !done.get() {
		if stop.is_shutdown_triggered() || Instant::now() >= deadline {
			return Err("PipeWire did not acknowledge capture disconnection before replacement".into());
		}
		mainloop
			.loop_()
			.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(5)));
	}
	Ok(())
}

fn open_stream(
	core: &pw::core::CoreRc,
	target: &crate::desktop::Target,
	offered: Vec<stream::OfferedFormat>,
	sender: CaptureSender,
	options: StreamOptions,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(CaptureStream, StreamFormat), String> {
	let request_driven = pacer::probe(core, &options.mainloop, target.node_id, &stop);
	let candidate = CaptureStream::connect(
		core,
		target,
		offered.clone(),
		sender,
		options.readiness.clone(),
		options.diagnostics,
		options.sync_device.clone(),
		request_driven,
	)?;
	match wait_format(&options.mainloop, &candidate, target, stop.clone()) {
		Ok(format) => Ok((candidate, format)),
		Err(error) if options.sync_device.is_some() && !stop.is_shutdown_triggered() => {
			tracing::warn!(%error, "Reopening capture with implicit synchronization before desktop commit");
			let sender = candidate.replacement_sender();
			drop(candidate);
			let stream = CaptureStream::connect(
				core,
				target,
				offered,
				sender,
				options.readiness,
				options.diagnostics,
				None,
				request_driven,
			)?;
			let format = wait_format(&options.mainloop, &stream, target, stop)?;
			Ok((stream, format))
		},
		Err(error) => Err(error),
	}
}

pub(crate) fn wait_format(
	mainloop: &pw::main_loop::MainLoopRc,
	stream: &CaptureStream,
	target: &crate::desktop::Target,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<StreamFormat, String> {
	let deadline = Instant::now() + FORMAT_TIMEOUT;
	loop {
		if stop.is_shutdown_triggered() {
			return Err("Desktop setup was cancelled".into());
		}
		mainloop
			.loop_()
			.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
		stream.service();
		let (format, failure) = stream.with_status(|s| (s.format, s.failure.clone()));
		if let Some(error) = failure {
			return Err(error);
		}
		if let Some(format) = format {
			let rate = format
				.framerate
				.or(format.max_framerate)
				.map(|r| u64::from(r.num) * 1000 / u64::from(r.denom.max(1)))
				.unwrap_or(0);
			if (format.width, format.height) != (target.mode.width, target.mode.height)
				|| rate.abs_diff(u64::from(target.mode.refresh_millihertz)) > 100
			{
				// Native output acknowledgement and PipeWire renegotiation are
				// asynchronous. In particular, KWin first reports its initial
				// 60 Hz format even after the requested mode has been applied.
				// Keep dispatching until the matching format or the deadline.
				if Instant::now() >= deadline {
					return Err(format!(
						"The desktop negotiated {}x{} at {} mHz instead of {}x{} at {} mHz",
						format.width,
						format.height,
						rate,
						target.mode.width,
						target.mode.height,
						target.mode.refresh_millihertz
					));
				}
				continue;
			}
			if format.color.color_space().is_none_or(|color| color.is_hdr()) {
				return Err("Managed Desktop Mode requires an SDR virtual output".into());
			}
			if stream.validate_frame()? {
				return Ok(format);
			}
		}
		if Instant::now() >= deadline {
			return Err("The desktop did not produce an importable frame within 5 seconds".into());
		}
	}
}
fn wait_input(
	mainloop: &pw::main_loop::MainLoopRc,
	input: &mut DesktopInput,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	let deadline = Instant::now() + Duration::from_secs(3);
	loop {
		mainloop.loop_().iterate(pw::loop_::Timeout::None);
		input.dispatch()?;
		if input.ready() {
			return Ok(());
		}
		if stop.is_shutdown_triggered() || Instant::now() >= deadline {
			return Err("Remote input devices did not become ready for the virtual output".into());
		}
		std::thread::sleep(Duration::from_millis(5));
	}
}

fn wait_input_devices(
	mainloop: &pw::main_loop::MainLoopRc,
	input: &mut DesktopInput,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	let deadline = Instant::now() + Duration::from_secs(3);
	loop {
		mainloop.loop_().iterate(pw::loop_::Timeout::None);
		input.dispatch()?;
		if input.authorized_devices() {
			return Ok(());
		}
		if stop.is_shutdown_triggered() || Instant::now() >= deadline {
			return Err(
				"Authorized keyboard and pointer devices are unavailable; physical displays remain enabled".into(),
			);
		}
		std::thread::sleep(Duration::from_millis(5));
	}
}

#[cfg(test)]
mod takeover_tests {
	use super::*;

	/// Uses real portal input, native output takeover and PipeWire capture.
	/// Sends no keyboard, pointer or touch events.
	#[test]
	#[ignore = "requires explicit desktop takeover opt-in, a saved input grant and an unlocked KDE desktop"]
	fn managed_takeover_keeps_input_across_output_changes() {
		assert_eq!(std::env::var("MOONSHINE_TEST_TAKEOVER").as_deref(), Ok("1"));
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let path = token::default_path().expect("Input token directory");
		assert!(token::load(&path).is_some(), "Authorize remote input before this test");
		let stop = ShutdownManager::<SessionShutdownReason>::new();
		let cause = StopCauseSlot::default();
		let (capture, endpoints) = PortalCapture::new(
			PortalCaptureConfig {
				desktop: DesktopConfig {
					enabled: true,
					input: true,
					audio: false,
					..Default::default()
				},
				gpu: None,
				client_identity: None,
				restore_token: Some(path),
				resolution: (2880, 1920),
				diagnostics: true,
				implicit_sync: false,
				refresh_rate: 60,
			},
			stop.clone(),
			cause.clone(),
		);
		let mut launched = capture.launch();
		let result = match &mut launched {
			Ok(launched) => (|| -> Result<(), String> {
				let start = Instant::now();
				let mut frames = 0;
				while start.elapsed() < Duration::from_secs(5) && !stop.is_shutdown_triggered() {
					if let Ok(mut frame) = endpoints.frames.recv_timeout_if(Duration::from_millis(100), true) {
						frame.consumed.store(true, std::sync::atomic::Ordering::Release);
						drop(frame.capture_credit.take());
						frames += 1;
					}
				}
				eprintln!(
					"desktop-takeover: {frames} frames across {:.2}s",
					start.elapsed().as_secs_f64()
				);
				if stop.is_shutdown_triggered() {
					return Err(format!("Capture stopped during takeover: {:?}", cause.get()));
				}
				if frames == 0 {
					return Err("No capture frames arrived".into());
				}
				// Same-mode reconnect retains the producer but resets admission.
				// Stop requesting captures long enough for outstanding buffers to
				// drain, then require pixels without input or another output change.
				for cycle in 0..12 {
					std::thread::sleep(Duration::from_millis(250));
					let report = launched.diagnostics().ok_or("Capture diagnostics missing")?;
					if !report.request_driven && report.pending != 1 {
						return Err(format!("Paused producer lost its latest picture: {report:?}"));
					}
					endpoints.frames.reset();
					let resumed_at = Instant::now();
					let mut frame = endpoints
						.frames
						.recv_timeout_if(Duration::from_millis(500), true)
						.map_err(|e| format!("Same-mode reconnect {cycle} produced no frame: {e}"))?;
					eprintln!(
						"desktop-reconnect: cycle={cycle} first_frame_us={}",
						resumed_at.elapsed().as_micros()
					);
					frame.consumed.store(true, std::sync::atomic::Ordering::Release);
					drop(frame.capture_credit.take());
				}
				// Simulate a consumer holding a read across replacement. The old
				// stream must wait for consumption before its producer can retire.
				for (width, height, refresh_rate) in [(2880, 1920, 120), (1920, 1080, 60), (1920, 1080, 60)] {
					let mut held = endpoints
						.frames
						.recv_timeout_if(Duration::from_secs(2), true)
						.map_err(|e| e.to_string())?;
					let consumed = held.consumed.clone();
					drop(held.capture_credit.take());
					let release = std::thread::spawn(move || {
						std::thread::sleep(Duration::from_millis(100));
						consumed.store(true, std::sync::atomic::Ordering::Release);
					});
					let configured = runtime.block_on(launched.reconfigure(OutputMode {
						width,
						height,
						refresh_rate,
						hdr: false,
						capture_pacing: super::super::CapturePacing::Fixed,
					}));
					release.join().map_err(|_| "Reader release panicked")?;
					configured.map_err(|_| "Desktop replacement failed")?;
					drop(held);
					let mut frame = endpoints
						.frames
						.recv_timeout_if(Duration::from_secs(2), true)
						.map_err(|e| e.to_string())?;
					let dimensions = (frame.width, frame.height);
					frame.consumed.store(true, std::sync::atomic::Ordering::Release);
					drop(frame.capture_credit.take());
					if dimensions != (width, height) {
						return Err(format!("Replacement published stale geometry: {dimensions:?}"));
					}
				}
				Ok(())
			})(),
			Err(error) => Err(error.client_message()),
		};
		let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);
		drop(launched);
		drop(endpoints);
		let deadline = Instant::now() + Duration::from_secs(15);
		while !stop.is_shutdown_completed() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(20));
		}
		assert!(stop.is_shutdown_completed(), "Capture worker cleanup did not complete");
		assert_eq!(crate::desktop::cleanup_complete(), Ok(()));
		assert_eq!(
			result,
			Ok(()),
			"Capture failure: {:?}; stop: {:?}",
			cause.get(),
			stop.shutdown_reason()
		);
	}
}
