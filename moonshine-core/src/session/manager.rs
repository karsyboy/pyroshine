//! Session ownership: lifecycle, transitions, cancellation and teardown.
//!
//! The manager owns at most one session. Its lifecycle is explicit:
//!
//! - `Idle`: no session and no resources. Only this state accepts a new launch.
//! - `Live`: a session record plus either the owned state (`Initialized`,
//!   `Launched`, `Active`) or a transition that has checked the state out.
//!   Absence of state therefore always means "a transition owns it", never idle.
//! - `Stopping`: a single teardown task owns everything. Replacement waits for
//!   it, so new sessions never race the previous session's ports, Pulse socket,
//!   application unit or GPU objects.
//!
//! Slow work (systemd, compositor start, worker spawn, pause/reconfigure) runs
//! in manager-owned transition tasks outside the mutex. A transition holds a
//! completion token of its session, so a stop cancels it and waits for it to
//! hand any state back; a transition commits only if its session epoch and
//! transition id are still current and the session is not stopping. See
//! `docs/ARCHITECTURE.md` ("Session ownership and shutdown").

use std::future::Future;
use std::net::IpAddr;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_shutdown::{DelayShutdownToken, ShutdownManager};
use tokio::sync::{Mutex, MutexGuard, broadcast, watch};

use crate::ShutdownReason;
use crate::session::AuthorizationReceiver;
use crate::session::FrameStats;
use crate::session::ResumeRequest;
use crate::session::SessionContext;
use crate::session::SessionKeyData;
use crate::session::SessionKeys;
use crate::session::SessionKeysSender;
use crate::session::SystemSession;
use crate::session::application_unit_name;
use crate::session::authorization::StreamAuthorization;
use crate::session::compositor::CapturePacing;
use crate::session::compositor::CompositorConfig;
use crate::session::keys::KeyLedger;
use crate::session::lifecycle::StartLatch;
use crate::session::status::{
	ClientPresence, LifecycleStatus, LiveStatus, ManagerStatus, StopRecord, TransitionStatus,
};
use crate::session::stream::audio::AudioStreamConfig;
use crate::session::stream::audio::AudioStreamContext;
use crate::session::stream::control::ControlStreamConfig;
use crate::session::stream::video::VideoStreamConfig;
use crate::session::stream::video::VideoStreamContext;

/// Bound for stopping the application unit: the systemd stop policy (SIGTERM
/// allowance, post hooks, SIGKILL) plus the termination check, as derived in
/// `application.rs`.
const APPLICATION_STOP_TIMEOUT: Duration = crate::session::application::APPLICATION_STOP_DEADLINE;
/// Bound for every session worker to exit and release its resources.
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// End-to-end teardown deadline, from the stop request through application
/// cleanup and every worker's exit. Exceeding it, or failing to establish that
/// the application terminated, is a terminal failure: the session stays
/// `Stopping` (it may still own resources) and the service shuts down so its
/// supervisor can restart it.
pub(crate) const SESSION_TEARDOWN_DEADLINE: Duration =
	Duration::from_secs(APPLICATION_STOP_TIMEOUT.as_secs() + WORKER_SHUTDOWN_TIMEOUT.as_secs());

#[derive(Debug, PartialEq, Eq)]
enum ReconnectDecision {
	FastResume,
	Reconfigure {
		video_changed_fields: Vec<&'static str>,
		audio_changed: bool,
	},
	RejectShuttingDown,
}

fn reconnect_decision(
	active_video: &VideoStreamContext,
	requested_video: &VideoStreamContext,
	active_audio: &AudioStreamContext,
	requested_audio: &AudioStreamContext,
	shutting_down: bool,
) -> ReconnectDecision {
	if shutting_down {
		return ReconnectDecision::RejectShuttingDown;
	}
	let video_changed_fields = active_video.changed_fields(requested_video);
	let audio_changed = active_audio != requested_audio;
	if video_changed_fields.is_empty() && !audio_changed {
		ReconnectDecision::FastResume
	} else {
		ReconnectDecision::Reconfigure {
			video_changed_fields,
			audio_changed,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShutdownReason {
	/// Session manager is shutting down.
	ManagerShutdown,
	/// The session was stopped by the user.
	UserStopped,
	/// The launched application exited.
	ApplicationStopped,
	/// Video packet handler stopped unexpectedly.
	VideoPacketHandlerStopped,
	/// Video encoder stopped unexpectedly.
	VideoEncoderStopped,
	/// Audio packet handler stopped unexpectedly.
	AudioPacketHandlerStopped,
	/// PulseAudio server stopped unexpectedly.
	PulseServerStopped,
	/// Audio encoder stopped unexpectedly.
	AudioEncoderStopped,
	/// Control stream stopped unexpectedly.
	ControlStreamStopped,
	/// Input handler stopped unexpectedly.
	InputHandlerStopped,
	/// Compositor stopped unexpectedly.
	CompositorStopped,
	/// A launch, start or reconfiguration failed after changing resources.
	TransitionFailed,
}

/// Inputs for spawning the stream workers of a launched session.
///
/// `generation` is the authorization the PLAY was accepted under. Workers
/// activate their first media epoch for exactly this generation; delivery
/// stops as soon as a later `/resume` replaces it.
pub(crate) struct StartRequest {
	pub video: VideoStreamContext,
	pub audio: AudioStreamContext,
	/// Capture pacing requested by the client of `generation`.
	pub capture_pacing: CapturePacing,
	pub generation: u64,
	pub authorization_rx: AuthorizationReceiver,
	pub stop: ShutdownManager<SessionShutdownReason>,
}

/// Every reconnect resets both media epochs. `video: None` retains the costly
/// pipeline and resets counters + IDR; audio always commits its negotiated context.
///
/// The plan is an immutable snapshot taken under the manager lock when PLAY is
/// accepted: the authorization generation and the keys published for it. A
/// newer `/resume` during the reconfiguration cannot mix its keys or
/// generation into this epoch, and the epoch it activates cannot deliver
/// media once that generation is no longer current.
#[derive(Debug)]
pub(crate) struct ResumePlan {
	pub video: Option<VideoStreamContext>,
	pub audio: AudioStreamContext,
	/// Capture pacing requested by the resuming client. Applied on both
	/// paths: a pacing change alone never recreates the video pipeline.
	pub capture_pacing: CapturePacing,
	pub generation: u64,
	pub keys: crate::session::keys::ActiveKeys,
}

/// The resource-owning work behind each manager transition.
///
/// The manager decides *whether* and *when* a transition may run and owns its
/// result; implementations only create, change or release resources. Each
/// future may be cancelled at any await by a session stop: implementations
/// must leave anything they created either inside a returned value or owned by
/// a registered session worker. The application unit is the exception: the
/// manager records it before `launch` and always stops it during teardown.
pub(crate) trait SessionBackend: Send + Sync + 'static {
	type Initialized: Send + 'static;
	type Launched: Send + 'static;
	type Active: Send + 'static;

	fn initialize(
		&self,
		context: SessionContext,
		stop: ShutdownManager<SessionShutdownReason>,
		foreground_tx: watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	) -> impl Future<Output = Result<Self::Initialized, ()>> + Send;

	fn launch(&self, session: Self::Initialized) -> impl Future<Output = Result<Self::Launched, ()>> + Send;

	fn start(
		&self,
		session: Self::Launched,
		request: StartRequest,
	) -> impl Future<Output = Result<(Self::Active, Vec<StartLatch>), ()>> + Send;

	/// Barrier for a reconnect: stop delivering the current epoch. The future
	/// must not borrow the session so the manager can await it unlocked.
	fn pause(
		&self,
		session: &Self::Active,
		video: bool,
		audio: bool,
	) -> impl Future<Output = Result<(), ()>> + Send + 'static;

	fn resume(&self, session: &mut Self::Active, plan: ResumePlan) -> impl Future<Output = Result<(), ()>> + Send;

	/// Stop the application unit and establish that it terminated. Must be
	/// idempotent; `Ok` for an absent unit. `Err` keeps the session from being
	/// reported idle.
	fn stop_application(&self, unit_name: &str) -> impl Future<Output = Result<(), ()>> + Send;
}

enum SessionState<B: SessionBackend> {
	/// Session initialized; compositor and app not yet started.
	Initialized(B::Initialized),
	/// Compositor and app launched; waiting for RTSP PLAY.
	Launched(B::Launched),
	/// Streams active.
	Active(B::Active),
}

impl<B: SessionBackend> SessionState<B> {
	fn name(&self) -> &'static str {
		match self {
			Self::Initialized(_) => "initialized",
			Self::Launched(_) => "launched",
			Self::Active(_) => "active",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionKind {
	Initialize,
	Launch,
	Start,
	Announce,
	Resume,
}

impl From<TransitionKind> for TransitionStatus {
	fn from(kind: TransitionKind) -> Self {
		match kind {
			TransitionKind::Initialize => Self::Initialize,
			TransitionKind::Launch => Self::Launch,
			TransitionKind::Start => Self::Start,
			TransitionKind::Announce => Self::Announce,
			TransitionKind::Resume => Self::Resume,
		}
	}
}

#[derive(Clone, Copy, Debug)]
struct Transition {
	id: u64,
	kind: TransitionKind,
}

/// Everything the manager knows about the current session, independent of
/// which state object currently exists.
struct SessionRecord {
	/// Session lifetime number, distinct from authorization generations.
	epoch: u64,
	/// Cancellation and completion boundary of this session.
	stop: ShutdownManager<SessionShutdownReason>,
	/// Authoritative context reported to HTTP/RTSP.
	context: SessionContext,
	/// Compositor state belongs to this session lifetime, not a client generation.
	foreground: watch::Receiver<Option<moonshine_management::dto::ForegroundApplication>>,
	/// Contexts used by the live encoders, once streaming.
	streams: Option<(VideoStreamContext, AudioStreamContext)>,
	/// Application unit this session may own. Recorded before the launch is
	/// attempted so teardown stops it even when the launch was cancelled.
	application_unit: Option<&'static str>,
	/// Start latches of the live stream workers.
	start_latches: Vec<StartLatch>,
	/// When the launch was accepted, for status reporting.
	started_at: SystemTime,
	/// Authorization generation created by the launch; later generations
	/// come from `/resume`.
	first_generation: Option<u64>,
}

struct LiveSession<B: SessionBackend> {
	record: SessionRecord,
	/// `None` only while `transition` has checked the state out.
	state: Option<SessionState<B>>,
	transition: Option<Transition>,
}

enum Lifecycle<B: SessionBackend> {
	Idle,
	Live(Box<LiveSession<B>>),
	Stopping(TeardownWaiter),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TeardownStatus {
	Running,
	Completed,
	Failed,
}

/// Completion of one session teardown.
#[derive(Clone)]
struct TeardownWaiter(watch::Receiver<TeardownStatus>);

impl TeardownWaiter {
	async fn wait(mut self) -> Result<(), ()> {
		match self.0.wait_for(|status| *status != TeardownStatus::Running).await {
			Ok(status) if *status == TeardownStatus::Completed => Ok(()),
			_ => Err(()),
		}
	}
}

/// Ownership of one in-flight transition. It holds its session's completion
/// open: teardown cannot finish until the transition has handed back (or
/// dropped) everything it checked out.
struct TransitionTicket {
	epoch: u64,
	id: u64,
	stop: ShutdownManager<SessionShutdownReason>,
	_completion: DelayShutdownToken<SessionShutdownReason>,
}

struct PendingStreams {
	video: VideoStreamContext,
	audio: AudioStreamContext,
	/// Generation that announced them. PLAY commits them only if no later
	/// launch/resume replaced that generation.
	generation: u64,
}

/// Read-only view of the live session for status reporting. It carries no
/// key material: session keys stay inside the manager and stream workers.
pub(crate) struct SessionView {
	pub epoch: u64,
	pub started_at: SystemTime,
	pub application_title: String,
	pub application_id: i32,
	pub foreground_application: Option<moonshine_management::dto::ForegroundApplication>,
	pub resolution: (u32, u32),
	pub refresh_rate: u32,
	pub hdr: bool,
	pub audio_channels: crate::session::stream::audio::AudioChannels,
	pub audio_channel_mask: u32,
	/// Current authorized launch/resume client, or the launch client while initializing.
	pub client_ip: IpAddr,
	pub streams: Option<(VideoStreamContext, AudioStreamContext)>,
}

struct SessionManagerInner<B: SessionBackend> {
	lifecycle: Lifecycle<B>,

	/// State handed back by transitions that were cancelled or superseded.
	/// Drained by the teardown of their epoch after all workers completed.
	orphans: Vec<(u64, SessionState<B>)>,

	next_epoch: u64,
	next_transition: u64,

	/// Sender for session keys, used to update keys from the webserver in different subsystems.
	///
	/// Subsystems that get updated are: video encoder, audio encoder and input handler.
	keys_tx: Option<SessionKeysSender>,

	/// Key-scoped nonce owner. Never reset: a key that is reused after a
	/// resume, reconfigure or new launch continues its nonce sequences.
	key_ledger: KeyLedger,

	/// Stream contexts received via RTSP ANNOUNCE. For an active session these
	/// belong to the reconnecting client; they are published only after the
	/// live epoch has been paused, and PLAY commits them.
	pending: Option<PendingStreams>,

	/// Authenticated session-level settings from the most recent `/resume`.
	resume_request: Option<ResumeRequest>,

	/// Authorization of the current launch/resume generation. RTSP, control and
	/// media endpoint discovery accept only this client and generation.
	authorization_tx: Option<watch::Sender<StreamAuthorization>>,

	/// Next authorization generation. Never reset, so a stale grant from an
	/// earlier session cannot match a later one.
	next_generation: u64,

	/// Last client-driven protocol progress (launch, resume, ANNOUNCE, PLAY);
	/// status consumers bound how long a session waits for its client.
	progress_at: Instant,

	/// How the most recent session ended.
	last_stop: Option<StopRecord>,

	/// A teardown exceeded its deadline; the service is shutting down.
	teardown_failed: bool,
}

impl<B: SessionBackend> SessionManagerInner<B> {
	fn new() -> Self {
		Self {
			lifecycle: Lifecycle::Idle,
			orphans: Vec::new(),
			next_epoch: 1,
			next_transition: 1,
			keys_tx: None,
			key_ledger: KeyLedger::default(),
			pending: None,
			resume_request: None,
			authorization_tx: None,
			next_generation: 1,
			progress_at: Instant::now(),
			last_stop: None,
			teardown_failed: false,
		}
	}

	/// Snapshot published to status consumers (see `session::status`).
	fn status(&self) -> ManagerStatus {
		let lifecycle = match &self.lifecycle {
			Lifecycle::Idle => LifecycleStatus::Idle,
			Lifecycle::Stopping(_) if self.teardown_failed => LifecycleStatus::Failed,
			Lifecycle::Stopping(_) => LifecycleStatus::Stopping,
			Lifecycle::Live(live) => {
				let streaming = live.record.streams.is_some();
				LifecycleStatus::Live(LiveStatus {
					epoch: live.record.epoch,
					transition: live.transition.map(|transition| transition.kind.into()),
					streams_committed: streaming,
					stopping: live.record.stop.is_shutdown_triggered(),
					reconnect_pending: streaming && (self.pending.is_some() || self.resume_request.is_some()),
					generation: self.authorization_tx.as_ref().map(|tx| tx.borrow().generation()),
					first_generation: live.record.first_generation,
					awaiting_since: self.progress_at,
				})
			},
		};
		ManagerStatus {
			lifecycle,
			last_stop: self.last_stop.clone(),
		}
	}

	/// Start a new authorization generation for `client_ip`, replacing every
	/// identifier the previous generation handed out.
	fn rotate_authorization(&mut self, client_ip: IpAddr, vrr_requested: bool) -> Result<(), ()> {
		let authorization =
			StreamAuthorization::new(self.next_generation, client_ip)?.with_vrr_requested(vrr_requested);
		self.next_generation += 1;
		self.progress_at = Instant::now();
		match &self.authorization_tx {
			Some(tx) => {
				tx.send_replace(authorization);
			},
			None => self.authorization_tx = Some(watch::channel(authorization).0),
		}
		Ok(())
	}

	/// Whether `grant` belongs to the current generation.
	fn is_current(&self, grant: &StreamAuthorization) -> bool {
		self.authorization_tx
			.as_ref()
			.is_some_and(|tx| tx.borrow().generation() == grant.generation())
	}

	fn live(&mut self) -> Option<&mut LiveSession<B>> {
		match &mut self.lifecycle {
			Lifecycle::Live(live) => Some(live),
			_ => None,
		}
	}

	/// The live session if `ticket`'s transition may still commit into it.
	fn committable(&mut self, ticket: &TransitionTicket) -> Option<&mut LiveSession<B>> {
		self.live().filter(|live| {
			live.record.epoch == ticket.epoch
				&& live.transition.is_some_and(|t| t.id == ticket.id)
				&& !live.record.stop.is_shutdown_triggered()
		})
	}

	/// Issue a ticket and mark `kind` in progress. Fails if the session is
	/// stopping or another transition owns it.
	fn begin_transition(&mut self, kind: TransitionKind) -> Result<TransitionTicket, ()> {
		let id = self.next_transition;
		let Some(live) = self.live() else {
			tracing::warn!(?kind, "Rejecting session transition: no live session");
			return Err(());
		};
		if let Some(current) = live.transition {
			tracing::warn!(?kind, in_progress = ?current.kind, "Rejecting session transition: another transition is in progress");
			return Err(());
		}
		if live.record.stop.is_shutdown_triggered() {
			tracing::warn!(?kind, "Rejecting session transition: session is stopping");
			return Err(());
		}
		let completion = live.record.stop.delay_shutdown_token().map_err(|_| ())?;
		live.transition = Some(Transition { id, kind });
		let ticket = TransitionTicket {
			epoch: live.record.epoch,
			id,
			stop: live.record.stop.clone(),
			_completion: completion,
		};
		self.next_transition += 1;
		Ok(ticket)
	}

	/// Release `ticket`'s claim without committing state.
	fn end_transition(&mut self, ticket: &TransitionTicket) {
		if let Some(live) = self.live()
			&& live.record.epoch == ticket.epoch
			&& live.transition.is_some_and(|t| t.id == ticket.id)
		{
			live.transition = None;
		}
	}

	/// Hand state that can no longer be committed to its epoch's teardown.
	fn orphan(&mut self, ticket: &TransitionTicket, state: SessionState<B>) {
		tracing::debug!(
			epoch = ticket.epoch,
			state = state.name(),
			"Transition result superseded; handing it to session teardown"
		);
		self.orphans.push((ticket.epoch, state));
	}
}

impl<B: SessionBackend> Drop for SessionManagerInner<B> {
	fn drop(&mut self) {
		// Narrow fallback only; the shutdown supervisor normally tears the
		// session down. Never block here.
		if let Lifecycle::Live(live) = &self.lifecycle {
			let _ = live
				.record
				.stop
				.trigger_shutdown(SessionShutdownReason::ManagerShutdown);
		}
	}
}

/// Session manager over a [`SessionBackend`]; [`SessionManager`] is the
/// production instance. Generic so lifecycle tests can substitute subsystems.
pub(crate) struct SessionCore<B: SessionBackend> {
	inner: Arc<Mutex<SessionManagerInner<B>>>,
	backend: Arc<B>,
	/// Shutdown manager for the entire application.
	shutdown: ShutdownManager<ShutdownReason>,
	/// Latest [`ManagerStatus`], republished whenever the manager lock is
	/// released after a change.
	status: Arc<watch::Sender<ManagerStatus>>,
}

impl<B: SessionBackend> Clone for SessionCore<B> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			backend: self.backend.clone(),
			shutdown: self.shutdown.clone(),
			status: self.status.clone(),
		}
	}
}

/// The manager mutex guard. Releasing it publishes the manager status if it
/// changed, so every state change reaches status consumers without each
/// transition having to remember to report it.
struct InnerGuard<'a, B: SessionBackend> {
	guard: MutexGuard<'a, SessionManagerInner<B>>,
	status: &'a watch::Sender<ManagerStatus>,
}

impl<B: SessionBackend> Deref for InnerGuard<'_, B> {
	type Target = SessionManagerInner<B>;

	fn deref(&self) -> &Self::Target {
		&self.guard
	}
}

impl<B: SessionBackend> DerefMut for InnerGuard<'_, B> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.guard
	}
}

impl<B: SessionBackend> Drop for InnerGuard<'_, B> {
	fn drop(&mut self) {
		let status = self.guard.status();
		self.status.send_if_modified(|current| {
			let changed = *current != status;
			if changed {
				*current = status;
			}
			changed
		});
	}
}

impl<B: SessionBackend> SessionCore<B> {
	pub(crate) fn new(backend: B, shutdown: ShutdownManager<ShutdownReason>) -> Result<Self, ()> {
		let delay = shutdown.delay_shutdown_token().map_err(|e| {
			tracing::error!("Failed to create delay shutdown token: {e:?}");
		})?;
		let trigger = shutdown.trigger_shutdown_token(ShutdownReason::SessionManagerShutdown);
		let core = Self {
			inner: Arc::new(Mutex::new(SessionManagerInner::new())),
			backend: Arc::new(backend),
			shutdown,
			status: Arc::new(watch::channel(ManagerStatus::default()).0),
		};

		// Global shutdown owner: tear the session down (application included)
		// before releasing the service's shutdown, within the session deadline.
		let supervisor = core.clone();
		let runtime = tokio::runtime::Handle::try_current()
			.map_err(|e| tracing::error!("Session manager requires a Tokio runtime: {e}"))?;
		runtime.spawn(async move {
			// If this task ends for any other reason, stop the service.
			let _trigger = trigger;
			let _delay = delay;
			supervisor.shutdown.wait_shutdown_triggered().await;
			tracing::debug!("Global shutdown triggered, stopping active session.");
			if supervisor.stop(SessionShutdownReason::ManagerShutdown).await.is_err() {
				tracing::error!("Session did not stop cleanly during service shutdown");
			}
		});
		Ok(core)
	}

	async fn lock(&self) -> InnerGuard<'_, B> {
		InnerGuard {
			guard: self.inner.lock().await,
			status: &self.status,
		}
	}

	/// Follow the manager status (see `session::status`).
	pub(crate) fn subscribe_status(&self) -> watch::Receiver<ManagerStatus> {
		self.status.subscribe()
	}

	/// Subscribe before reading the snapshot so focus-only changes cannot be lost.
	pub(crate) async fn subscribe_foreground(
		&self,
	) -> Option<watch::Receiver<Option<moonshine_management::dto::ForegroundApplication>>> {
		self.lock().await.live().map(|live| live.record.foreground.clone())
	}

	/// Read-only view of the live session for status reporting.
	pub(crate) async fn session_view(&self) -> Option<SessionView> {
		let mut guard = self.lock().await;
		// Authorization is the source of current client identity; the context
		// retains the launch address. Read both under the manager lock so a
		// concurrent resume cannot mix session and authorization generations.
		let client_ip = guard.authorization_tx.as_ref().map(|tx| tx.borrow().client_ip());
		guard.live().map(|live| {
			let context = &live.record.context;
			SessionView {
				epoch: live.record.epoch,
				started_at: live.record.started_at,
				application_title: context.application.title.clone(),
				application_id: context.application_id,
				foreground_application: live.record.foreground.borrow().clone(),
				resolution: context.resolution,
				refresh_rate: context.refresh_rate,
				hdr: context.hdr,
				audio_channels: context.audio_channels,
				audio_channel_mask: context.audio_channel_mask,
				// Initialization publishes the record before creating its first grant.
				client_ip: client_ip.unwrap_or(context.client_ip),
				streams: live.record.streams.clone(),
			}
		})
	}

	/// Authorize an RTSP peer for the current launch/resume generation.
	pub(crate) async fn authorize_stream(&self, peer: IpAddr) -> Option<StreamAuthorization> {
		let guard = self.lock().await;
		let authorization = guard.authorization_tx.as_ref()?.borrow().clone();
		authorization.admits_peer(peer).then_some(authorization)
	}

	/// Context of the live session, including one inside a transition.
	pub(crate) async fn get_session_context(&self) -> Option<SessionContext> {
		let mut guard = self.lock().await;
		guard.live().map(|live| live.record.context.clone())
	}

	/// Open every stream start latch of the live session (bench/external start).
	pub(crate) async fn trigger_streams_start(&self) {
		let mut guard = self.lock().await;
		if let Some(live) = guard.live() {
			for latch in &live.record.start_latches {
				latch.open();
			}
		}
	}

	/// Begin tearing down the live session, or join the teardown in progress.
	/// `epoch` restricts the request to one session (stale watchdogs are no-ops).
	fn begin_teardown(
		&self,
		guard: &mut SessionManagerInner<B>,
		epoch: Option<u64>,
		reason: SessionShutdownReason,
	) -> Option<TeardownWaiter> {
		match &guard.lifecycle {
			Lifecycle::Idle => return None,
			Lifecycle::Stopping(waiter) => return Some(waiter.clone()),
			Lifecycle::Live(live) if epoch.is_some_and(|epoch| epoch != live.record.epoch) => return None,
			Lifecycle::Live(_) => {},
		}
		let (done, waiter) = watch::channel(TeardownStatus::Running);
		let waiter = TeardownWaiter(waiter);
		let Lifecycle::Live(live) = std::mem::replace(&mut guard.lifecycle, Lifecycle::Stopping(waiter.clone())) else {
			unreachable!("checked above");
		};
		// Retire every identifier immediately: nothing may negotiate, discover
		// endpoints or commit into a session that is being torn down.
		guard.keys_tx = None;
		guard.pending = None;
		guard.resume_request = None;
		guard.authorization_tx = None;
		tokio::spawn(self.clone().run_teardown(*live, reason, done));
		Some(waiter)
	}

	/// The single asynchronous owner of a session's teardown.
	async fn run_teardown(
		self,
		live: LiveSession<B>,
		reason: SessionShutdownReason,
		done: watch::Sender<TeardownStatus>,
	) {
		let LiveSession {
			record,
			state,
			transition,
		} = live;
		let epoch = record.epoch;
		tracing::info!(
			epoch,
			?reason,
			state = state.as_ref().map(SessionState::name),
			transition = ?transition.map(|t| t.kind),
			"Stopping session"
		);
		let deadline = tokio::time::Instant::now() + SESSION_TEARDOWN_DEADLINE;
		let result = tokio::time::timeout_at(deadline, async {
			let mut application = record.application_unit;
			let mut application_stopped = true;
			// Stop the application while the compositor and audio server still
			// serve it, so it can exit cleanly. A transition in flight is cancelled
			// first instead; its application is stopped after it handed back.
			if transition.is_none()
				&& let Some(unit) = application.take()
			{
				application_stopped = self.stop_application(unit).await;
			}
			let _ = record.stop.trigger_shutdown(reason);
			drop(state);
			// Every worker and transition holds this open until its sockets,
			// threads, GPU objects and checked-out state are released.
			record.stop.wait_shutdown_complete().await;
			let orphans: Vec<_> = {
				let mut guard = self.lock().await;
				let (orphans, rest) = std::mem::take(&mut guard.orphans)
					.into_iter()
					.partition(|(orphan_epoch, _)| *orphan_epoch <= epoch);
				guard.orphans = rest;
				orphans
			};
			drop(orphans);
			if let Some(unit) = application {
				application_stopped = self.stop_application(unit).await;
			}
			// Workers are released either way; only an established application
			// termination lets the session become idle.
			application_stopped
		})
		.await;

		let mut guard = self.lock().await;
		guard.last_stop = Some(StopRecord {
			epoch,
			reason,
			completed: result.is_ok(),
			at: SystemTime::now(),
		});
		let result = match result {
			Ok(true) => Ok(()),
			Ok(false) => Err("the application unit's termination could not be established"),
			Err(_) => Err("teardown exceeded its deadline"),
		};
		match result {
			Ok(()) => {
				guard.lifecycle = Lifecycle::Idle;
				drop(guard);
				tracing::info!(epoch, "Session stopped; ready for a new session.");
				done.send_replace(TeardownStatus::Completed);
			},
			Err(failure) => {
				// Workers may still own ports, the Pulse socket or GPU objects, or the
				// application may still run: never report idle. Restarting the service
				// is the only safe recovery.
				guard.teardown_failed = true;
				drop(guard);
				tracing::error!(
					epoch,
					failure,
					deadline_secs = SESSION_TEARDOWN_DEADLINE.as_secs(),
					"Session teardown failed; refusing new sessions and stopping the service"
				);
				let _ = self.shutdown.trigger_shutdown(ShutdownReason::SessionManagerShutdown);
				done.send_replace(TeardownStatus::Failed);
			},
		}
	}

	/// Whether the application unit's termination was established.
	async fn stop_application(&self, unit: &'static str) -> bool {
		match tokio::time::timeout(APPLICATION_STOP_TIMEOUT, self.backend.stop_application(unit)).await {
			Ok(Ok(())) => true,
			Ok(Err(())) => {
				tracing::error!(unit, "Failed to stop the application unit");
				false
			},
			Err(_) => {
				tracing::error!(
					unit,
					timeout_secs = APPLICATION_STOP_TIMEOUT.as_secs(),
					"Timed out stopping the application unit"
				);
				false
			},
		}
	}

	/// Watch a session for a stop from any worker and give it to teardown.
	fn spawn_watchdog(&self, epoch: u64, stop: ShutdownManager<SessionShutdownReason>) {
		let core = self.clone();
		tokio::spawn(async move {
			let reason = stop.wait_shutdown_triggered().await;
			// This task owns nothing that teardown cancels; it only hands over.
			let mut guard = core.lock().await;
			let requested = matches!(
				reason,
				SessionShutdownReason::UserStopped | SessionShutdownReason::ManagerShutdown
			);
			// An application exiting because teardown stopped it is expected.
			if requested || matches!(guard.lifecycle, Lifecycle::Stopping(_)) {
				tracing::debug!(?reason, "Session stop observed.");
			} else {
				tracing::warn!(?reason, "Session stopped unexpectedly.");
			}
			core.begin_teardown(&mut guard, Some(epoch), reason);
		});
	}

	/// Stop the session (if any) and wait until its teardown has completed.
	async fn stop(&self, reason: SessionShutdownReason) -> Result<(), ()> {
		let waiter = {
			let mut guard = self.lock().await;
			self.begin_teardown(&mut guard, None, reason)
		};
		match waiter {
			Some(waiter) => waiter.wait().await,
			None => Ok(()),
		}
	}

	/// Stop the session and return to `Idle`. Returns only after every owned
	/// worker, socket and the application unit have been released.
	pub(crate) async fn stop_session(&self) -> Result<(), ()> {
		self.stop(SessionShutdownReason::UserStopped).await
	}

	/// End a failed transition: the session cannot be left half-changed, so it
	/// is torn down deterministically. `state` is whatever the transition still
	/// owns; teardown stops its application before its workers.
	fn fail_transition(
		&self,
		guard: &mut SessionManagerInner<B>,
		ticket: &TransitionTicket,
		state: Option<SessionState<B>>,
	) {
		match guard.committable(ticket) {
			Some(live) => {
				live.transition = None;
				live.state = state;
				tracing::error!(epoch = ticket.epoch, "Session transition failed; stopping the session");
				self.begin_teardown(guard, Some(ticket.epoch), SessionShutdownReason::TransitionFailed);
			},
			None => {
				if let Some(state) = state {
					guard.orphan(ticket, state);
				}
				guard.end_transition(ticket);
			},
		}
	}

	/// Initialize a new session with the provided context.
	///
	/// The session is not launched until `launch_session` is called. A
	/// previous session that is still stopping is awaited first.
	pub(crate) async fn initialize_session(&self, mut context: SessionContext) -> Result<(), ()> {
		let (ticket, keys_tx, foreground_tx) = loop {
			let mut guard = self.lock().await;
			if self.shutdown.is_shutdown_triggered() {
				tracing::warn!("Service is shutting down; rejecting InitializeSession command.");
				return Err(());
			}
			match &guard.lifecycle {
				Lifecycle::Idle => {},
				Lifecycle::Live(_) => {
					tracing::warn!("Session already initialized, rejecting InitializeSession command.");
					return Err(());
				},
				Lifecycle::Stopping(waiter) => {
					let waiter = waiter.clone();
					drop(guard);
					tracing::info!("Waiting for the previous session to finish stopping.");
					if !matches!(
						tokio::time::timeout(SESSION_TEARDOWN_DEADLINE, waiter.wait()).await,
						Ok(Ok(()))
					) {
						tracing::warn!("Previous session did not stop; rejecting InitializeSession command.");
						return Err(());
					}
					continue;
				},
			}

			// Extract the raw keys from context and create the watch channel.
			let session_keys = match context.keys {
				SessionKeys::Keys(data) => data,
				SessionKeys::Rx(_) => {
					tracing::error!("Session keys already initialized as a watch receiver");
					return Err(());
				},
			};
			let (tx, rx) = watch::channel(guard.key_ledger.publish(session_keys));
			context.keys = SessionKeys::Rx(rx);

			let epoch = guard.next_epoch;
			guard.next_epoch += 1;
			let stop = ShutdownManager::new();
			let (foreground_tx, foreground) = watch::channel(None);
			guard.lifecycle = Lifecycle::Live(Box::new(LiveSession {
				record: SessionRecord {
					epoch,
					stop: stop.clone(),
					context: context.clone(),
					foreground,
					streams: None,
					application_unit: None,
					start_latches: Vec::new(),
					started_at: SystemTime::now(),
					first_generation: None,
				},
				state: None,
				transition: None,
			}));
			let ticket = guard.begin_transition(TransitionKind::Initialize)?;
			self.spawn_watchdog(epoch, stop);
			break (ticket, tx, foreground_tx);
		};

		let client_ip = context.client_ip;
		let vrr_requested = context.vrr_requested;
		let core = self.clone();
		let task = tokio::spawn(async move {
			let result = ticket
				.stop
				.wrap_cancel(core.backend.initialize(context, ticket.stop.clone(), foreground_tx))
				.await;
			let mut guard = core.lock().await;
			let outcome = match result {
				Ok(Ok(session)) => {
					let state = SessionState::Initialized(session);
					if guard.committable(&ticket).is_none() {
						guard.orphan(&ticket, state);
						Err(())
					} else if guard.rotate_authorization(client_ip, vrr_requested).is_err() {
						core.fail_transition(&mut guard, &ticket, Some(state));
						Err(())
					} else {
						// Set here so keys exist only for an initialized session.
						guard.keys_tx = Some(keys_tx);
						let generation = guard.authorization_tx.as_ref().map(|tx| tx.borrow().generation());
						let live = guard.committable(&ticket).expect("checked above");
						live.record.first_generation = generation;
						live.state = Some(state);
						live.transition = None;
						tracing::info!(
							epoch = ticket.epoch,
							"Session initialized successfully, waiting to be launched."
						);
						Ok(())
					}
				},
				Ok(Err(())) => {
					core.fail_transition(&mut guard, &ticket, None);
					Err(())
				},
				Err(_) => Err(()),
			};
			drop(guard);
			drop(ticket);
			outcome
		});
		task.await
			.map_err(|e| tracing::error!("Session initialization task failed: {e}"))?
	}

	/// Launch the session by starting the compositor and application, but don't start streams until RTSP ANNOUNCE is received.
	///
	/// The launch is owned by the manager: if the caller stops waiting (e.g.
	/// an HTTP timeout), it still commits or is cancelled by a session stop.
	pub(crate) async fn launch_session(&self) -> Result<(), ()> {
		let (ticket, session) = {
			let mut guard = self.lock().await;
			let Some(live) = guard.live() else {
				tracing::warn!("LaunchSession rejected: no active session");
				return Err(());
			};
			if !matches!(live.state, Some(SessionState::Initialized(_))) || live.transition.is_some() {
				tracing::warn!(
					state = live.state.as_ref().map(SessionState::name),
					transition = ?live.transition.map(|t| t.kind),
					"LaunchSession rejected: session is not waiting to be launched"
				);
				return Err(());
			}
			let ticket = guard.begin_transition(TransitionKind::Launch)?;
			let live = guard.live().expect("checked above");
			// Recorded before anything can create the unit.
			live.record.application_unit = Some(application_unit_name());
			let Some(SessionState::Initialized(session)) = live.state.take() else {
				unreachable!("checked above");
			};
			(ticket, session)
		};

		tracing::info!("Launching session (starting compositor and app).");
		let core = self.clone();
		let task = tokio::spawn(async move {
			let result = ticket.stop.wrap_cancel(core.backend.launch(session)).await;
			let mut guard = core.lock().await;
			let outcome = match result {
				Ok(Ok(launched)) => {
					let state = SessionState::Launched(launched);
					match guard.committable(&ticket) {
						Some(live) => {
							live.state = Some(state);
							live.transition = None;
							guard.progress_at = Instant::now();
							tracing::info!("Session launched successfully, waiting for RTSP ANNOUNCE.");
							Ok(())
						},
						None => {
							guard.orphan(&ticket, state);
							Err(())
						},
					}
				},
				Ok(Err(())) => {
					tracing::error!("Failed to launch session.");
					core.fail_transition(&mut guard, &ticket, None);
					Err(())
				},
				Err(_) => {
					tracing::info!("Session launch cancelled.");
					Err(())
				},
			};
			drop(guard);
			drop(ticket);
			outcome
		});
		task.await
			.map_err(|e| tracing::error!("Session launch task failed: {e}"))?
	}

	/// Set the video and audio stream contexts after receiving RTSP ANNOUNCE.
	///
	/// For an active session the live epoch is paused first; the contexts are
	/// published (and may be committed by PLAY) only after that barrier.
	pub(crate) async fn set_stream_context(
		&self,
		grant: &StreamAuthorization,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
		session_id_v1: bool,
	) -> Result<(), ()> {
		// RTSP validates first to return a precise error; this guard keeps any
		// other caller from pausing or replacing a working epoch with values
		// the encoders, timers or transport cannot use.
		if let Err(reason) = video_stream_context.validate().and_then(|()| {
			crate::session::negotiation::validate_audio_packet_duration(audio_stream_context.packet_duration_ms)
		}) {
			tracing::warn!(
				reason,
				"Rejecting invalid stream negotiation before changing the session"
			);
			return Err(());
		}
		let pending = PendingStreams {
			video: video_stream_context,
			audio: audio_stream_context,
			generation: grant.generation(),
		};
		let (ticket, pause) = {
			let mut guard = self.lock().await;
			if !guard.is_current(grant) {
				tracing::warn!(
					generation = grant.generation(),
					"Rejecting RTSP ANNOUNCE from a replaced launch/resume generation"
				);
				return Err(());
			}
			let resume_request = guard.resume_request.clone();
			let Some(live) = guard.live() else {
				tracing::warn!("SetStreamContext rejected: no active session");
				return Err(());
			};
			if let Some(transition) = live.transition {
				tracing::warn!(in_progress = ?transition.kind, "SetStreamContext rejected: a session transition is in progress");
				return Err(());
			}
			let (video, audio) = (&pending.video, &pending.audio);
			let pause = match &live.state {
				Some(SessionState::Launched(_)) => {
					tracing::debug!("Stream contexts received via RTSP ANNOUNCE.");
					None
				},
				Some(SessionState::Active(active)) => {
					let (active_video, active_audio) = live
						.record
						.streams
						.as_ref()
						.expect("active sessions record their streams");
					let changed = active_video.changed_fields(video);
					let audio_changed = active_audio != audio;
					tracing::info!(
						active_width = active_video.width,
						active_height = active_video.height,
						active_fps = active_video.fps,
						active_codec = %active_video.format.codec,
						active_chroma = %active_video.format.chroma,
						active_bit_depth = active_video.format.bit_depth.bits(),
						active_hdr = active_video.format.hdr,
						active_bitrate = active_video.bitrate,
						requested_width = video.width,
						requested_height = video.height,
						requested_fps = video.fps,
						requested_codec = %video.format.codec,
						requested_chroma = %video.format.chroma,
						requested_bit_depth = video.format.bit_depth.bits(),
						requested_hdr = video.format.hdr,
						requested_bitrate = video.bitrate,
						changed_fields = ?changed,
						audio_changed,
						"Reconnect negotiation received"
					);
					if let Some(request) = resume_request
						&& (request
							.resolution
							.is_some_and(|value| value != (video.width, video.height))
							|| request.refresh_rate.is_some_and(|value| value != video.fps)
							|| request.hdr.is_some_and(|value| value != video.format.hdr))
					{
						tracing::warn!(
							?request,
							"HTTP resume parameters differ from authoritative RTSP negotiation"
						);
					}
					// Every reconnect needs a barrier, including identical-mode resume.
					Some(self.backend.pause(active, true, true))
				},
				Some(SessionState::Initialized(_)) => {
					tracing::warn!("SetStreamContext rejected: session not yet launched (Initialized state)");
					return Err(());
				},
				None => unreachable!("state is checked out only by a transition"),
			};
			match pause {
				None => {
					publish_pending(&mut guard, pending, session_id_v1);
					return Ok(());
				},
				Some(pause) => (guard.begin_transition(TransitionKind::Announce)?, pause),
			}
		};

		let core = self.clone();
		let grant = grant.clone();
		let task = tokio::spawn(async move {
			let result = ticket.stop.wrap_cancel(pause).await;
			let mut guard = core.lock().await;
			let committable = guard.committable(&ticket).is_some();
			guard.end_transition(&ticket);
			let outcome = match result {
				Ok(Ok(())) if committable && guard.is_current(&grant) => {
					publish_pending(&mut guard, pending, session_id_v1);
					Ok(())
				},
				Ok(Ok(())) => {
					tracing::warn!("Discarding ANNOUNCE contexts from a replaced generation or stopping session");
					Err(())
				},
				// A failed pause may have paused one medium and not the other, and
				// means a media worker no longer answers. Neither the old epoch nor
				// a new negotiation can be trusted: hand the session to teardown.
				Ok(Err(())) => {
					if committable {
						tracing::error!(epoch = ticket.epoch, "Reconnect pause failed; stopping the session");
						core.begin_teardown(&mut guard, Some(ticket.epoch), SessionShutdownReason::TransitionFailed);
					}
					Err(())
				},
				Err(_) => Err(()),
			};
			drop(guard);
			drop(ticket);
			outcome
		});
		task.await.map_err(|e| tracing::error!("ANNOUNCE task failed: {e}"))?
	}

	/// Start the streams of a launched session, or commit a reconnect epoch of
	/// an active one.
	///
	/// `grant` must be the authorization under which the pending contexts were
	/// announced; PLAY from a replaced generation cannot commit them. Every
	/// prerequisite is checked before anything moves, so a duplicate,
	/// premature or stale PLAY leaves the session (and its application) as is.
	pub(crate) async fn start_session(&self, grant: &StreamAuthorization) -> Result<(), ()> {
		enum Work<B: SessionBackend> {
			Start(B::Launched, StartRequest),
			Resume(B::Active, ResumePlan),
		}
		let (ticket, work, video, audio) = {
			let mut guard = self.lock().await;
			let announced = guard.pending.as_ref().map(|pending| pending.generation);
			if !guard.is_current(grant) || announced != Some(grant.generation()) {
				tracing::warn!(
					generation = grant.generation(),
					pending_generation = ?announced,
					"Rejecting RTSP PLAY without a pending ANNOUNCE from the current generation"
				);
				return Err(());
			}
			let authorization_rx = guard
				.authorization_tx
				.as_ref()
				.map(watch::Sender::subscribe)
				.expect("a current grant implies an authorization channel");
			let generation = grant.generation();
			let capture_pacing = if grant.vrr_requested() {
				CapturePacing::Vrr
			} else {
				CapturePacing::Fixed
			};
			let Some(keys) = guard.keys_tx.as_ref().map(|keys_tx| keys_tx.borrow().clone()) else {
				tracing::warn!("StartSession rejected: the session has no published keys");
				return Err(());
			};
			let Some(live) = guard.live() else {
				tracing::warn!("StartSession rejected: no active session");
				return Err(());
			};
			match &live.state {
				Some(SessionState::Launched(_) | SessionState::Active(_)) => {},
				Some(SessionState::Initialized(_)) => {
					tracing::warn!("StartSession rejected: session not yet launched");
					return Err(());
				},
				None => {
					tracing::warn!("StartSession rejected: a session transition is in progress");
					return Err(());
				},
			}
			let ticket = guard.begin_transition(TransitionKind::Start)?;
			// Prerequisites hold; consume the announcement and check out the state.
			let PendingStreams { video, audio, .. } = guard.pending.take().expect("checked above");
			let resume_request = guard.resume_request.take();
			let live = guard.live().expect("checked above");
			let work = match live.state.take().expect("checked above") {
				SessionState::Launched(launched) => Work::<B>::Start(
					launched,
					StartRequest {
						video: video.clone(),
						audio: audio.clone(),
						capture_pacing,
						generation,
						authorization_rx,
						stop: live.record.stop.clone(),
					},
				),
				SessionState::Active(active) => {
					if let Some(transition) = live.transition.as_mut() {
						transition.kind = TransitionKind::Resume;
					}
					let (active_video, active_audio) = live
						.record
						.streams
						.as_ref()
						.expect("active sessions record their streams");
					let plan = match reconnect_decision(
						active_video,
						&video,
						active_audio,
						&audio,
						live.record.stop.is_shutdown_triggered(),
					) {
						ReconnectDecision::RejectShuttingDown => {
							unreachable!("begin_transition rejects stopping sessions")
						},
						ReconnectDecision::FastResume => {
							tracing::info!("Reconnect stream configuration unchanged; using fast resume path");
							ResumePlan {
								video: None,
								audio: audio.clone(),
								capture_pacing,
								generation,
								keys: keys.clone(),
							}
						},
						ReconnectDecision::Reconfigure {
							video_changed_fields,
							audio_changed,
						} => {
							if !video_changed_fields.is_empty() {
								tracing::info!(changed_fields = ?video_changed_fields, "Recreating video pipeline for changed reconnect configuration");
							}
							if audio_changed {
								tracing::info!("Recreating audio epoch for changed reconnect configuration");
							}
							ResumePlan {
								video: (!video_changed_fields.is_empty()).then(|| video.clone()),
								audio: audio.clone(),
								capture_pacing,
								generation,
								keys: keys.clone(),
							}
						},
					};
					tracing::debug!(?resume_request, "Committing reconnect PLAY");
					Work::Resume(active, plan)
				},
				SessionState::Initialized(_) => unreachable!("checked above"),
			};
			(ticket, work, video, audio)
		};

		let core = self.clone();
		let grant = grant.clone();
		let task = tokio::spawn(async move {
			let outcome = match work {
				Work::Start(launched, request) => {
					tracing::info!("Starting session streams.");
					let result = ticket.stop.wrap_cancel(core.backend.start(launched, request)).await;
					let mut guard = core.lock().await;
					match result {
						Ok(Ok((active, latches))) => {
							let state = SessionState::Active(active);
							match guard.committable(&ticket) {
								Some(live) => {
									// The backend applied the authoritative RTSP output mode
									// before starting streams; publish it like a reconnect.
									record_negotiated_mode(&mut live.record.context, &video, &audio);
									live.record.streams = Some((video, audio));
									live.record.start_latches = latches;
									live.state = Some(state);
									live.transition = None;
									guard.progress_at = Instant::now();
									Ok(())
								},
								None => {
									guard.orphan(&ticket, state);
									Err(())
								},
							}
						},
						Ok(Err(())) => {
							tracing::error!("Failed to start session streams.");
							core.fail_transition(&mut guard, &ticket, None);
							Err(())
						},
						Err(_) => Err(()),
					}
				},
				Work::Resume(mut active, plan) => {
					let result = ticket.stop.wrap_cancel(core.backend.resume(&mut active, plan)).await;
					let mut guard = core.lock().await;
					let state = SessionState::Active(active);
					let current = guard.is_current(&grant);
					match result {
						Ok(Ok(())) => match guard.committable(&ticket) {
							Some(live) => {
								// Record what the workers now run either way, so the next
								// reconnect compares against reality.
								record_negotiated_mode(&mut live.record.context, &video, &audio);
								live.record.streams = Some((video, audio));
								live.state = Some(state);
								live.transition = None;
								guard.progress_at = Instant::now();
								if current {
									Ok(())
								} else {
									// A newer `/resume` replaced this PLAY's generation while it
									// reconfigured. Its epoch was activated for the old
									// generation, which the media senders no longer serve;
									// the new client's ANNOUNCE/PLAY activates its own epoch.
									tracing::warn!(
										generation = grant.generation(),
										"Reconnect PLAY was superseded by a newer resume; not activating its epoch"
									);
									Err(())
								}
							},
							None => {
								guard.orphan(&ticket, state);
								Err(())
							},
						},
						// A partially applied reconfiguration cannot be trusted; the
						// application is stopped cleanly with the rest of the session.
						Ok(Err(())) => {
							tracing::error!("Reconnect reconfiguration failed.");
							core.fail_transition(&mut guard, &ticket, Some(state));
							Err(())
						},
						Err(_) => {
							guard.orphan(&ticket, state);
							Err(())
						},
					}
				},
			};
			drop(ticket);
			outcome
		});
		task.await
			.map_err(|e| tracing::error!("Session start task failed: {e}"))?
	}

	/// Update keys and retain authenticated session-level resume parameters.
	///
	/// A resume starts a new authorization generation for `client_ip`: the
	/// previous client's RTSP, control and media discovery stop being accepted.
	pub(crate) async fn resume_session(
		&self,
		keys: SessionKeyData,
		request: ResumeRequest,
		client_ip: IpAddr,
	) -> Result<(), ()> {
		let mut guard = self.lock().await;
		let streaming = match guard.live() {
			Some(live) if live.record.stop.is_shutdown_triggered() => {
				tracing::warn!("Session is shutting down; rejecting resume key update.");
				return Err(());
			},
			Some(live) => live.record.streams.is_some(),
			None => false,
		};
		if !streaming {
			tracing::warn!("No active streaming session to update keys for.");
			return Err(());
		}
		if guard.keys_tx.is_none() {
			tracing::warn!("Active streaming session has no key sender; rejecting resume.");
			return Err(());
		}
		guard.rotate_authorization(client_ip, request.vrr_requested)?;
		// Keys were validated at the HTTP boundary; the ledger keeps nonce
		// allocation continuous when the client resumes with the same key.
		let keys = guard.key_ledger.publish(keys);
		if let Some(keys_tx) = &guard.keys_tx {
			keys_tx.send_replace(keys);
		}
		// Contexts announced under the previous generation must not be committed.
		guard.pending = None;
		guard.resume_request = Some(request);

		Ok(())
	}
}

/// Publish the negotiated output/audio mode an epoch actually runs with.
fn record_negotiated_mode(context: &mut SessionContext, video: &VideoStreamContext, audio: &AudioStreamContext) {
	context.resolution = (video.width, video.height);
	context.refresh_rate = video.fps;
	context.hdr = video.format.hdr;
	context.audio_channels = audio.audio_config.channels;
	context.audio_channel_mask = audio.audio_config.channel_mask;
}

fn publish_pending<B: SessionBackend>(
	guard: &mut SessionManagerInner<B>,
	pending: PendingStreams,
	session_id_v1: bool,
) {
	guard.pending = Some(pending);
	guard.progress_at = Instant::now();
	if session_id_v1 && let Some(tx) = &guard.authorization_tx {
		tx.send_modify(StreamAuthorization::require_session_id);
	}
}

/// The production session manager.
#[derive(Clone)]
pub struct SessionManager {
	core: SessionCore<SystemSession>,
	stats_tx: broadcast::Sender<FrameStats>,
	/// Control-stream ownership of the current generation (see `session::status`).
	presence_tx: Arc<watch::Sender<ClientPresence>>,
}

impl SessionManager {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		compositor_config: CompositorConfig,
		video_config: VideoStreamConfig,
		audio_config: AudioStreamConfig,
		control_config: ControlStreamConfig,
		address: String,
		stream_timeout: u64,
		inhibit_sleep: bool,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let stats_tx = broadcast::channel(256).0;
		let presence_tx = Arc::new(watch::channel(ClientPresence::default()).0);
		let backend = SystemSession {
			compositor_config,
			video_config,
			audio_config,
			control_config,
			address,
			stream_timeout,
			inhibit_sleep,
			stats_tx: stats_tx.clone(),
			presence_tx: presence_tx.clone(),
		};
		Ok(Self {
			core: SessionCore::new(backend, shutdown)?,
			stats_tx,
			presence_tx,
		})
	}

	/// Follow the manager lifecycle (see `session::status`).
	pub(crate) fn subscribe_status(&self) -> watch::Receiver<ManagerStatus> {
		self.core.subscribe_status()
	}

	/// Follow control-stream ownership of the current generation.
	pub(crate) fn subscribe_presence(&self) -> watch::Receiver<ClientPresence> {
		self.presence_tx.subscribe()
	}

	/// Receiver for per-frame statistics, for management telemetry. The
	/// channel never blocks the pipeline: a receiver that falls behind loses
	/// the oldest samples. Drop it when no consumer needs telemetry.
	pub(crate) fn frame_stats_receiver(&self) -> broadcast::Receiver<FrameStats> {
		self.stats_tx.subscribe()
	}

	pub(crate) async fn subscribe_foreground(
		&self,
	) -> Option<watch::Receiver<Option<moonshine_management::dto::ForegroundApplication>>> {
		self.core.subscribe_foreground().await
	}

	/// Read-only view of the live session, if any.
	pub(crate) async fn session_view(&self) -> Option<SessionView> {
		self.core.session_view().await
	}

	/// Returns a receiver for per-frame encoding statistics.
	///
	/// Call **before** `initialize_session()` to receive stats from the start.
	/// Multiple receivers can be created — each receives a copy of every message.
	pub fn bench_stats_receiver(&self) -> broadcast::Receiver<FrameStats> {
		self.stats_tx.subscribe()
	}

	/// Start a new authorization generation with `keys`, exactly as an
	/// authenticated HTTPS `/resume` from `client_ip` would. For the benchmark's
	/// reconnect cycles, which have no Moonlight client; servers use `/resume`.
	/// `vrr_requested` is the client's `clientVrrRequested` parameter.
	pub async fn bench_resume(&self, keys: SessionKeyData, client_ip: IpAddr, vrr_requested: bool) -> Result<(), ()> {
		let request = ResumeRequest {
			vrr_requested,
			..ResumeRequest::default()
		};
		self.resume_session(keys, request, client_ip).await
	}

	/// Trigger the video and audio pipelines to start encoding.
	///
	/// In the normal flow, this is triggered by the control stream when the
	/// client sends `StartB`. Call this from external callers (e.g. bench binary)
	/// that have no Moonlight client. Must be called after `start_session()`.
	/// Idempotent, and equivalent to (and compatible with) a client `StartB`.
	pub async fn trigger_streams_start(&self) {
		self.core.trigger_streams_start().await;
	}

	/// Authorize an RTSP peer for the current launch/resume generation.
	///
	/// The returned grant must accompany the ANNOUNCE and PLAY it authorizes;
	/// they are rejected if a later launch/resume replaced its generation.
	pub async fn authorize_stream(&self, peer: IpAddr) -> Option<StreamAuthorization> {
		self.core.authorize_stream(peer).await
	}

	/// Set the video and audio stream contexts after receiving RTSP ANNOUNCE.
	///
	/// `session_id_v1` records that the client announced Moonlight's
	/// `ML_FF_SESSION_ID_V1`, after which media and control discovery require
	/// the generation's session identifiers.
	pub async fn set_stream_context(
		&self,
		grant: &StreamAuthorization,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
		session_id_v1: bool,
	) -> Result<(), ()> {
		self.core
			.set_stream_context(grant, video_stream_context, audio_stream_context, session_id_v1)
			.await
	}

	/// Get the current session context if there is a live session (including
	/// one in a transition); otherwise return `None`. A session that is
	/// stopping is not reported.
	pub async fn get_session_context(&self) -> Result<Option<SessionContext>, ()> {
		Ok(self.core.get_session_context().await)
	}

	/// Initialize a new session with the provided context.
	///
	/// The session is not launched until `launch_session` is called.
	pub async fn initialize_session(&self, context: SessionContext) -> Result<(), ()> {
		self.core.initialize_session(context).await
	}

	/// Launch the session by starting the compositor and application, but don't start streams until RTSP ANNOUNCE is received.
	pub async fn launch_session(&self) -> Result<(), ()> {
		self.core.launch_session().await
	}

	/// Start the video and audio streams.
	///
	/// Returns `Ok(())` only after all three streams (video, audio, control) are
	/// successfully constructed. Returns `Err(())` if any stream fails to initialize.
	pub async fn start_session(&self, grant: &StreamAuthorization) -> Result<(), ()> {
		self.core.start_session(grant).await
	}

	/// Stop the session and return to the idle state. Completes only after the
	/// application unit and every session worker have been released.
	pub async fn stop_session(&self) -> Result<(), ()> {
		self.core.stop_session().await
	}

	/// Update keys and retain authenticated session-level resume parameters.
	pub(crate) async fn resume_session(
		&self,
		keys: SessionKeyData,
		request: ResumeRequest,
		client_ip: IpAddr,
	) -> Result<(), ()> {
		self.core.resume_session(keys, request, client_ip).await
	}
}

#[cfg(test)]
impl SessionManager {
	/// Manager with default stream configuration, for RTSP/ingress tests.
	pub(crate) fn for_test(shutdown: ShutdownManager<ShutdownReason>) -> Self {
		Self::new(
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
			"127.0.0.1".into(),
			30,
			false,
			shutdown,
		)
		.unwrap()
	}

	/// Seed reporting state without opening the production compositor or audio
	/// sockets. D-Bus tests exercise subscriptions/snapshots over this record;
	/// backend lifetime behavior is covered by the lifecycle harness.
	pub(crate) async fn reporting_session_for_test(
		&self,
		context: SessionContext,
	) -> watch::Sender<Option<moonshine_management::dto::ForegroundApplication>> {
		let (sender, foreground) = watch::channel(None);
		let mut guard = self.core.lock().await;
		assert!(matches!(guard.lifecycle, Lifecycle::Idle));
		guard.lifecycle = Lifecycle::Live(Box::new(LiveSession {
			record: SessionRecord {
				epoch: 1,
				stop: ShutdownManager::new(),
				context,
				foreground,
				streams: None,
				application_unit: None,
				start_latches: Vec::new(),
				started_at: SystemTime::now(),
				first_generation: None,
			},
			state: None,
			transition: None,
		}));
		sender
	}

	/// Start an authorization generation without launching a session, as an
	/// authenticated `/launch` would.
	pub(crate) async fn authorize_client_for_test(&self, client_ip: IpAddr) -> StreamAuthorization {
		self.core.authorize_client_for_test(client_ip).await
	}
}

#[cfg(test)]
impl<B: SessionBackend> SessionCore<B> {
	pub(crate) async fn authorize_client_for_test(&self, client_ip: IpAddr) -> StreamAuthorization {
		let mut guard = self.lock().await;
		guard.rotate_authorization(client_ip, false).unwrap();
		guard.authorization_tx.as_ref().unwrap().borrow().clone()
	}
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::audio::{AudioChannels, AudioConfig};
	use crate::session::stream::video::{BitDepth, ChromaFormat, ColorRange, NegotiatedVideoFormat, VideoCodec};

	fn video(codec: VideoCodec) -> VideoStreamContext {
		VideoStreamContext {
			pyrowave_dialect: None,
			width: 1920,
			height: 1080,
			fps: 60,
			packet_size: 1392,
			bitrate: 20_000_000,
			minimum_fec_packets: 2,
			qos: true,
			format: NegotiatedVideoFormat::sdr(codec, ChromaFormat::Yuv420, BitDepth::Eight, ColorRange::Limited),
			max_reference_frames: 1,
			encrypt_video: false,
		}
	}

	fn audio() -> AudioStreamContext {
		AudioStreamContext {
			packet_duration_ms: 5,
			qos: true,
			audio_config: AudioConfig::from_channels(AudioChannels::Stereo, 0x3, true),
			encrypt_audio: false,
		}
	}

	fn assert_video_reconfigure(active: &VideoStreamContext, requested: &VideoStreamContext) {
		assert!(matches!(
			reconnect_decision(active, requested, &audio(), &audio(), false),
			ReconnectDecision::Reconfigure {
				audio_changed: false,
				..
			}
		));
	}

	#[tokio::test]
	async fn stream_grants_are_bound_to_client_and_generation() {
		let shutdown = ShutdownManager::new();
		let manager = SessionManager::for_test(shutdown);
		let client: IpAddr = "192.168.1.20".parse().unwrap();
		assert!(manager.authorize_stream(client).await.is_none(), "no launch yet");

		let first = manager.authorize_client_for_test(client).await;
		let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
		assert_eq!(manager.authorize_stream(mapped).await, Some(first.clone()));
		assert!(
			manager
				.authorize_stream("192.168.1.21".parse().unwrap())
				.await
				.is_none()
		);

		// A resume (new generation) invalidates grants handed out earlier, even
		// for the same client, and its PLAY cannot commit contexts it did not announce.
		let second = manager.authorize_client_for_test(client).await;
		assert!(second.generation() > first.generation());
		{
			let mut guard = manager.core.inner.lock().await;
			assert!(!guard.is_current(&first));
			assert!(guard.is_current(&second));
			guard.pending = Some(PendingStreams {
				video: video(VideoCodec::H264),
				audio: audio(),
				generation: first.generation(),
			});
		}
		assert!(manager.start_session(&first).await.is_err());
		assert!(manager.start_session(&second).await.is_err());
		assert_eq!(
			manager.core.inner.lock().await.pending.as_ref().map(|p| p.generation),
			Some(first.generation()),
			"rejected PLAY must not consume pending contexts"
		);
		assert!(
			manager
				.set_stream_context(&first, video(VideoCodec::H264), audio(), false)
				.await
				.is_err()
		);

		// Another client resuming takes the session over.
		let other: IpAddr = "192.168.1.30".parse().unwrap();
		let third = manager.authorize_client_for_test(other).await;
		assert!(manager.authorize_stream(client).await.is_none());
		assert_eq!(manager.authorize_stream(other).await, Some(third));
	}

	#[test]
	fn identical_reconnect_uses_fast_resume() {
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&audio(),
				&audio(),
				false
			),
			ReconnectDecision::FastResume
		);
	}

	#[test]
	fn resolution_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.width = 3840;
		requested.height = 2160;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn fps_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.fps = 120;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn bitrate_only_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.bitrate *= 2;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn codec_transitions_reconfigure() {
		for (from, to) in [
			(VideoCodec::H264, VideoCodec::Hevc),
			(VideoCodec::Hevc, VideoCodec::Av1),
			(VideoCodec::Av1, VideoCodec::PyroWave),
			(VideoCodec::PyroWave, VideoCodec::H264),
		] {
			assert_video_reconfigure(&video(from), &video(to));
		}
	}

	#[test]
	fn dynamic_range_transitions_reconfigure() {
		let sdr = video(VideoCodec::Hevc);
		let mut hdr = sdr.clone();
		hdr.format = NegotiatedVideoFormat::hdr10(VideoCodec::Hevc, ChromaFormat::Yuv420, ColorRange::Limited);
		assert_video_reconfigure(&sdr, &hdr);
		assert_video_reconfigure(&hdr, &sdr);
	}

	#[test]
	fn chroma_and_bit_depth_changes_reconfigure() {
		let active = video(VideoCodec::Hevc);
		let mut chroma = active.clone();
		chroma.format.chroma = ChromaFormat::Yuv444;
		assert_video_reconfigure(&active, &chroma);
		let mut ten_bit = active.clone();
		ten_bit.format.bit_depth = BitDepth::Ten;
		assert_video_reconfigure(&active, &ten_bit);
	}

	#[test]
	fn packet_size_change_reconfigures() {
		let active = video(VideoCodec::H264);
		let mut requested = active.clone();
		requested.packet_size = 1200;
		assert_video_reconfigure(&active, &requested);
	}

	#[test]
	fn packetizer_transport_reference_and_color_changes_reconfigure() {
		let active = video(VideoCodec::Hevc);
		for requested in [
			{
				let mut value = active.clone();
				value.minimum_fec_packets += 1;
				value
			},
			{
				let mut value = active.clone();
				value.qos = !value.qos;
				value
			},
			{
				let mut value = active.clone();
				value.max_reference_frames += 1;
				value
			},
			{
				let mut value = active.clone();
				value.format.range = ColorRange::Full;
				value
			},
		] {
			assert_video_reconfigure(&active, &requested);
		}
	}

	#[test]
	fn audio_context_change_reconfigures_audio() {
		let active_audio = audio();
		let mut requested_audio = active_audio.clone();
		requested_audio.audio_config = AudioConfig::from_channels(AudioChannels::Surround51, 0x3f, true);
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&active_audio,
				&requested_audio,
				false,
			),
			ReconnectDecision::Reconfigure {
				video_changed_fields: Vec::new(),
				audio_changed: true,
			}
		);
	}

	#[test]
	fn encryption_mode_change_reconfigures_but_key_refresh_does_not() {
		let active = video(VideoCodec::H264);
		let mut encrypted = active.clone();
		encrypted.encrypt_video = true;
		assert_video_reconfigure(&active, &encrypted);
		// Session keys are refreshed through a watch channel and deliberately do
		// not participate in negotiated-context equality.
		assert_eq!(
			reconnect_decision(&active, &active, &audio(), &audio(), false),
			ReconnectDecision::FastResume
		);
	}

	#[test]
	fn shutdown_rejects_reconnect() {
		assert_eq!(
			reconnect_decision(
				&video(VideoCodec::H264),
				&video(VideoCodec::H264),
				&audio(),
				&audio(),
				true
			),
			ReconnectDecision::RejectShuttingDown
		);
	}
}
