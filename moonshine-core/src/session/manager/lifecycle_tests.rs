//! Deterministic lifecycle tests for the session manager (STAB-001/002/003).
//!
//! A fake [`SessionBackend`] stands in for the compositor, systemd and stream
//! subsystems. Each operation can be held at a barrier or failed, every fake
//! resource is counted, fake workers register with the session exactly like
//! production workers, and the fake application unit records whether it was
//! created and stopped. No sleeps are used for ordering.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::session::lifecycle::WorkerGuard;
use crate::session::stream::audio::{AudioChannels, AudioConfig};
use crate::session::stream::video::{BitDepth, ChromaFormat, ColorRange, NegotiatedVideoFormat, VideoCodec};
use crate::session::{RemoteInputKey, RemoteInputKeyId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Op {
	Initialize,
	Launch,
	Start,
	Pause,
	Resume,
	StopApplication,
}

const ALL_OPS: [Op; 6] = [
	Op::Initialize,
	Op::Launch,
	Op::Start,
	Op::Pause,
	Op::Resume,
	Op::StopApplication,
];

/// One operation's barrier: entry count, release flag and fault switches.
struct Gate {
	entered: watch::Sender<usize>,
	released: watch::Sender<bool>,
	hold: AtomicBool,
	fail: AtomicBool,
}

impl Gate {
	fn new() -> Self {
		Self {
			entered: watch::channel(0).0,
			released: watch::channel(false).0,
			hold: AtomicBool::new(false),
			fail: AtomicBool::new(false),
		}
	}
}

/// Counts live fake resources; each `Resource` is one socket/thread/GPU owner.
#[derive(Default)]
struct Counters {
	resources: AtomicUsize,
	workers_started: AtomicUsize,
	unit_created: AtomicBool,
	unit_stops: AtomicUsize,
	/// Set if a new session was initialized while an old one still owned resources.
	overlapped: AtomicBool,
	order: std::sync::Mutex<Vec<Op>>,
}

struct Resource(Arc<Counters>);

impl Resource {
	fn new(counters: &Arc<Counters>) -> Self {
		counters.resources.fetch_add(1, Ordering::SeqCst);
		Self(counters.clone())
	}
}

impl Drop for Resource {
	fn drop(&mut self) {
		self.0.resources.fetch_sub(1, Ordering::SeqCst);
	}
}

struct FakeBackend {
	gates: HashMap<Op, Gate>,
	counters: Arc<Counters>,
	/// Workers released only when this opens (models slow worker exit).
	worker_exit: watch::Sender<bool>,
	/// Stop the session right before a successful launch returns.
	stop_on_launch_return: AtomicBool,
	/// The most recent session's stop, so tests can play a failing worker.
	session_stop: std::sync::Mutex<Option<ShutdownManager<SessionShutdownReason>>>,
	foreground_tx: std::sync::Mutex<Option<watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>>>,
	/// Every reconnect plan the backend ran.
	plans: std::sync::Mutex<Vec<PlanRecord>>,
	/// Capture pacing of every first PLAY.
	start_pacing: std::sync::Mutex<Vec<CapturePacing>>,
}

/// What a reconnect plan asked the workers to do, and for which authorization.
struct PlanRecord {
	video_recreated: bool,
	audio: AudioStreamContext,
	generation: u64,
	key_id: u32,
	capture_pacing: CapturePacing,
}

struct FakeSession {
	_resources: Vec<Resource>,
	stop: ShutdownManager<SessionShutdownReason>,
}

impl FakeBackend {
	fn new() -> Self {
		Self {
			gates: ALL_OPS.iter().map(|op| (*op, Gate::new())).collect(),
			counters: Arc::new(Counters::default()),
			worker_exit: watch::channel(true).0,
			stop_on_launch_return: AtomicBool::new(false),
			session_stop: std::sync::Mutex::new(None),
			foreground_tx: std::sync::Mutex::new(None),
			plans: std::sync::Mutex::new(Vec::new()),
			start_pacing: std::sync::Mutex::new(Vec::new()),
		}
	}

	async fn gate(&self, op: Op) -> Result<(), ()> {
		self.counters.order.lock().unwrap().push(op);
		let gate = &self.gates[&op];
		gate.entered.send_modify(|count| *count += 1);
		if gate.hold.load(Ordering::SeqCst) {
			let mut released = gate.released.subscribe();
			let _ = released.wait_for(|released| *released).await;
		}
		if gate.fail.load(Ordering::SeqCst) {
			Err(())
		} else {
			Ok(())
		}
	}

	/// Spawn a worker registered with the session like production workers.
	fn spawn_worker(&self, stop: &ShutdownManager<SessionShutdownReason>, start: Option<StartLatch>) -> Result<(), ()> {
		let guard = WorkerGuard::register(stop, SessionShutdownReason::VideoPacketHandlerStopped)?;
		let resource = Resource::new(&self.counters);
		let counters = self.counters.clone();
		let stop = stop.clone();
		let mut exit = self.worker_exit.subscribe();
		let waiter = start.map(|latch| latch.waiter());
		tokio::spawn(async move {
			let _guard = guard;
			let _resource = resource;
			if let Some(waiter) = waiter
				&& waiter.wait(&stop).await.is_ok()
			{
				counters.workers_started.fetch_add(1, Ordering::SeqCst);
			}
			stop.wait_shutdown_triggered().await;
			let _ = exit.wait_for(|exit| *exit).await;
		});
		Ok(())
	}

	fn session(&self, stop: &ShutdownManager<SessionShutdownReason>, resources: usize) -> FakeSession {
		FakeSession {
			_resources: (0..resources).map(|_| Resource::new(&self.counters)).collect(),
			stop: stop.clone(),
		}
	}
}

impl SessionBackend for FakeBackend {
	type Initialized = FakeSession;
	type Launched = FakeSession;
	type Active = FakeSession;

	async fn initialize(
		&self,
		_context: SessionContext,
		stop: ShutdownManager<SessionShutdownReason>,
		foreground_tx: watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	) -> Result<FakeSession, ()> {
		if self.counters.resources.load(Ordering::SeqCst) != 0 || self.counters.unit_created.load(Ordering::SeqCst) {
			self.counters.overlapped.store(true, Ordering::SeqCst);
		}
		*self.session_stop.lock().unwrap() = Some(stop.clone());
		*self.foreground_tx.lock().unwrap() = Some(foreground_tx);
		// Like the gamepad thread: a worker that exists from initialization.
		self.spawn_worker(&stop, None)?;
		let session = self.session(&stop, 1);
		self.gate(Op::Initialize).await?;
		Ok(session)
	}

	async fn launch(&self, session: FakeSession) -> Result<FakeSession, ()> {
		// systemd accepted the transient unit; the start job is still running.
		self.counters.unit_created.store(true, Ordering::SeqCst);
		self.gate(Op::Launch).await?;
		if self.stop_on_launch_return.load(Ordering::SeqCst) {
			let _ = session.stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
		}
		let launched = self.session(&session.stop, 2);
		drop(session);
		Ok(launched)
	}

	async fn start(&self, session: FakeSession, request: StartRequest) -> Result<(FakeSession, Vec<StartLatch>), ()> {
		self.start_pacing.lock().unwrap().push(request.capture_pacing);
		let latch = StartLatch::new();
		// Workers exist (and own resources) before the fallible step below.
		self.spawn_worker(&request.stop, Some(latch.clone()))?;
		self.spawn_worker(&request.stop, Some(latch.clone()))?;
		self.gate(Op::Start).await?;
		let active = self.session(&session.stop, 3);
		drop(session);
		Ok((active, vec![latch]))
	}

	fn pause(
		&self,
		_session: &FakeSession,
		video: bool,
		audio: bool,
	) -> impl Future<Output = Result<(), ()>> + Send + 'static {
		assert!(video && audio, "every reconnect pauses both media epochs");
		let gate = (
			self.gates[&Op::Pause].entered.clone(),
			self.gates[&Op::Pause].released.subscribe(),
			self.gates[&Op::Pause].hold.load(Ordering::SeqCst),
			self.gates[&Op::Pause].fail.load(Ordering::SeqCst),
		);
		let counters = self.counters.clone();
		async move {
			let (entered, mut released, hold, fail) = gate;
			counters.order.lock().unwrap().push(Op::Pause);
			entered.send_modify(|count| *count += 1);
			if hold {
				let _ = released.wait_for(|released| *released).await;
			}
			if fail { Err(()) } else { Ok(()) }
		}
	}

	async fn resume(&self, _session: &mut FakeSession, plan: ResumePlan) -> Result<(), ()> {
		assert!(matches!(plan.audio.packet_duration_ms, 5 | 10));
		self.plans.lock().unwrap().push(PlanRecord {
			video_recreated: plan.video.is_some(),
			audio: plan.audio,
			generation: plan.generation,
			key_id: plan.keys.key_id().get(),
			capture_pacing: plan.capture_pacing,
		});
		self.gate(Op::Resume).await
	}

	async fn stop_application(&self, _unit_name: &str) -> Result<(), ()> {
		self.gate(Op::StopApplication).await?;
		self.counters.unit_created.store(false, Ordering::SeqCst);
		self.counters.unit_stops.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}
}

struct Harness {
	core: SessionCore<FakeBackend>,
	shutdown: ShutdownManager<ShutdownReason>,
	client: IpAddr,
}

impl Harness {
	fn new() -> Self {
		let shutdown = ShutdownManager::new();
		Self {
			core: SessionCore::new(FakeBackend::new(), shutdown.clone()).unwrap(),
			shutdown,
			client: "127.0.0.1".parse().unwrap(),
		}
	}

	fn backend(&self) -> &FakeBackend {
		&self.core.backend
	}

	fn counters(&self) -> &Counters {
		&self.backend().counters
	}

	fn hold(&self, op: Op) {
		self.backend().gates[&op].hold.store(true, Ordering::SeqCst);
	}

	fn fail(&self, op: Op) {
		self.backend().gates[&op].fail.store(true, Ordering::SeqCst);
	}

	fn release(&self, op: Op) {
		self.backend().gates[&op].released.send_replace(true);
	}

	fn calls(&self, op: Op) -> usize {
		*self.backend().gates[&op].entered.borrow()
	}

	/// Wait until `op` has been entered `count` times.
	async fn entered(&self, op: Op, count: usize) {
		let mut entered = self.backend().gates[&op].entered.subscribe();
		tokio::time::timeout(Duration::from_secs(5), entered.wait_for(|n| *n >= count))
			.await
			.unwrap_or_else(|_| panic!("{op:?} was not entered"))
			.unwrap();
	}

	async fn phase(&self) -> &'static str {
		match &self.core.inner.lock().await.lifecycle {
			Lifecycle::Idle => "idle",
			Lifecycle::Stopping(_) => "stopping",
			Lifecycle::Live(live) => match (&live.state, live.transition) {
				(_, Some(transition)) => match transition.kind {
					TransitionKind::Initialize => "initializing",
					TransitionKind::Launch => "launching",
					TransitionKind::Start => "starting",
					TransitionKind::Announce => "announcing",
					TransitionKind::Resume => "resuming",
				},
				(Some(state), None) => state.name(),
				(None, None) => panic!("state absent without a transition"),
			},
		}
	}

	async fn wait_idle(&self) {
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let waiter = match &self.core.inner.lock().await.lifecycle {
					Lifecycle::Idle => return,
					Lifecycle::Stopping(waiter) => Some(waiter.clone()),
					Lifecycle::Live(_) => None,
				};
				match waiter {
					Some(waiter) => {
						let _ = waiter.wait().await;
					},
					None => tokio::task::yield_now().await,
				}
			}
		})
		.await
		.expect("session did not become idle");
	}

	/// Idle and nothing owned: no resources, no workers, no application unit.
	async fn assert_released(&self) {
		assert_eq!(self.phase().await, "idle");
		assert_eq!(self.counters().resources.load(Ordering::SeqCst), 0, "resources leaked");
		assert!(
			!self.counters().unit_created.load(Ordering::SeqCst),
			"application unit leaked"
		);
		assert!(self.core.inner.lock().await.orphans.is_empty());
		assert!(self.core.get_session_context().await.is_none());
		assert!(self.core.authorize_stream(self.client).await.is_none());
	}

	async fn initialize(&self) -> Result<(), ()> {
		self.core.initialize_session(context()).await
	}

	async fn grant(&self) -> StreamAuthorization {
		self.core.authorize_stream(self.client).await.unwrap()
	}

	async fn announce(&self, grant: &StreamAuthorization) -> Result<(), ()> {
		self.core.set_stream_context(grant, video(), audio(), false).await
	}

	async fn launched(&self) {
		self.initialize().await.unwrap();
		self.core.launch_session().await.unwrap();
		assert_eq!(self.phase().await, "launched");
	}

	async fn active(&self) -> StreamAuthorization {
		self.launched().await;
		let grant = self.grant().await;
		self.announce(&grant).await.unwrap();
		self.core.start_session(&grant).await.unwrap();
		assert_eq!(self.phase().await, "active");
		grant
	}

	/// An active session after a client `/resume`, with ANNOUNCE done.
	async fn resumed(&self) -> StreamAuthorization {
		self.active().await;
		self.core
			.resume_session(keys(), ResumeRequest::default(), self.client)
			.await
			.unwrap();
		let grant = self.grant().await;
		self.announce(&grant).await.unwrap();
		grant
	}
}

fn keys() -> SessionKeyData {
	keys_with_id(1)
}

fn keys_with_id(id: u32) -> SessionKeyData {
	SessionKeyData::new(RemoteInputKey::from_bytes([id as u8; 16]), RemoteInputKeyId::new(id))
}

fn context() -> SessionContext {
	SessionContext {
		application: Default::default(),
		application_id: 1,
		resolution: (1920, 1080),
		refresh_rate: 60,
		keys: SessionKeys::Keys(keys()),
		audio_channels: AudioChannels::Stereo,
		audio_channel_mask: 0x3,
		hdr: false,
		client_ip: "127.0.0.1".parse().unwrap(),
		vrr_requested: false,
	}
}

#[tokio::test]
async fn capture_pacing_follows_each_generations_vrr_request() {
	let h = Harness::new();
	let mut launch = context();
	launch.vrr_requested = true;
	h.core.initialize_session(launch).await.unwrap();
	h.core.launch_session().await.unwrap();
	let grant = h.grant().await;
	assert!(grant.vrr_requested());
	h.announce(&grant).await.unwrap();
	h.core.start_session(&grant).await.unwrap();
	assert_eq!(*h.backend().start_pacing.lock().unwrap(), [CapturePacing::Vrr]);

	// A standard Moonlight client resumes without `clientVrrRequested`, then
	// a VRR client resumes again. Both are unchanged-mode (fast) reconnects:
	// pacing alone never recreates the video pipeline.
	for (requested, expected) in [(false, CapturePacing::Fixed), (true, CapturePacing::Vrr)] {
		let request = ResumeRequest {
			vrr_requested: requested,
			..ResumeRequest::default()
		};
		h.core.resume_session(keys(), request, h.client).await.unwrap();
		let grant = h.grant().await;
		assert_eq!(grant.vrr_requested(), requested);
		h.announce(&grant).await.unwrap();
		h.core.start_session(&grant).await.unwrap();
		let plans = h.backend().plans.lock().unwrap();
		let plan = plans.last().unwrap();
		assert_eq!(plan.capture_pacing, expected);
		assert!(!plan.video_recreated);
		assert_eq!(plan.generation, grant.generation());
	}
}

#[tokio::test]
async fn foreground_changes_are_session_scoped_and_survive_reconnect() {
	use moonshine_management::dto::ForegroundApplication;
	let h = Harness::new();
	let mut context = context();
	context.application.title = "Steam".into();
	h.core.initialize_session(context).await.unwrap();
	h.core.launch_session().await.unwrap();
	let grant = h.grant().await;
	h.announce(&grant).await.unwrap();
	h.core.start_session(&grant).await.unwrap();
	let mut changes = h.core.subscribe_foreground().await.unwrap();
	let sender = h.backend().foreground_tx.lock().unwrap().as_ref().unwrap().clone();
	let epoch = h.core.session_view().await.unwrap().epoch;
	for title in ["Steam", "Grim Dawn", "Grim Dawn - Running", "Steam"] {
		sender.send_replace(Some(ForegroundApplication { title: title.into() }));
		changes.changed().await.unwrap();
		changes.borrow_and_update();
		let view = h.core.session_view().await.unwrap();
		assert_eq!(view.epoch, epoch);
		assert_eq!(view.foreground_application.unwrap().title, title);
		assert_eq!(view.application_title, "Steam");
		assert_eq!(view.application_id, 1);
	}

	// A disconnected stream retains the same session; a resume rotates only
	// its client authorization. Neither operation replaces compositor state.
	sender.send_replace(Some(ForegroundApplication {
		title: "Grim Dawn".into(),
	}));
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	assert_eq!(
		h.core
			.session_view()
			.await
			.unwrap()
			.foreground_application
			.unwrap()
			.title,
		"Grim Dawn"
	);
	let grant = h.grant().await;
	h.announce(&grant).await.unwrap();
	h.core.start_session(&grant).await.unwrap();
	assert_eq!(h.core.session_view().await.unwrap().epoch, epoch);
	sender.send_replace(None);
	assert!(h.core.session_view().await.unwrap().foreground_application.is_none());

	h.core.stop(SessionShutdownReason::UserStopped).await.unwrap();
	h.wait_idle().await;
	assert!(h.core.subscribe_foreground().await.is_none());
	h.initialize().await.unwrap();
	// An old compositor's sender cannot update a replacement session.
	sender.send_replace(Some(ForegroundApplication {
		title: "Old game".into(),
	}));
	assert!(h.core.session_view().await.unwrap().foreground_application.is_none());
	h.core.stop(SessionShutdownReason::UserStopped).await.unwrap();
	h.wait_idle().await;
}

fn video() -> VideoStreamContext {
	VideoStreamContext {
		pyrowave_dialect: None,
		width: 1920,
		height: 1080,
		fps: 60,
		packet_size: 1392,
		bitrate: 20_000_000,
		minimum_fec_packets: 2,
		qos: true,
		format: NegotiatedVideoFormat::sdr(
			VideoCodec::H264,
			ChromaFormat::Yuv420,
			BitDepth::Eight,
			ColorRange::Limited,
		),
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

/// Drive a session to the point where `op` is in flight, holding it there.
async fn hold_at(h: &Harness, op: Op) -> tokio::task::JoinHandle<Result<(), ()>> {
	h.hold(op);
	let core = h.core.clone();
	let task = match op {
		Op::Initialize => tokio::spawn(async move { core.initialize_session(context()).await }),
		Op::Launch => {
			h.initialize().await.unwrap();
			tokio::spawn(async move { core.launch_session().await })
		},
		Op::Start => {
			h.launched().await;
			let grant = h.grant().await;
			h.announce(&grant).await.unwrap();
			tokio::spawn(async move { core.start_session(&grant).await })
		},
		Op::Pause => {
			h.active().await;
			let grant = h.grant().await;
			tokio::spawn(async move { core.set_stream_context(&grant, video(), audio(), false).await })
		},
		Op::Resume => {
			let grant = h.resumed().await;
			tokio::spawn(async move { core.start_session(&grant).await })
		},
		Op::StopApplication => unreachable!("not a transition"),
	};
	h.entered(op, 1).await;
	task
}

const TRANSITIONS: [Op; 5] = [Op::Initialize, Op::Launch, Op::Start, Op::Pause, Op::Resume];

/// Full happy path, then a stop that releases everything, twice in a row.
#[tokio::test]
async fn full_lifecycle_releases_everything_and_relaunches_immediately() {
	let h = Harness::new();
	for round in 1..=2 {
		h.active().await;
		h.core.trigger_streams_start().await;
		h.core.stop_session().await.unwrap();
		h.assert_released().await;
		assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), round);
	}
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
}

/// STAB-002: stop cancels a transition at every await; the in-flight request
/// fails, teardown completes and a new session can follow immediately.
#[tokio::test]
async fn stop_cancels_every_transition_await() {
	for op in TRANSITIONS {
		let h = Harness::new();
		let task = hold_at(&h, op).await;
		assert_ne!(h.phase().await, "idle", "{op:?}: a transition must not look idle");
		h.core.stop_session().await.unwrap();
		assert!(task.await.unwrap().is_err(), "{op:?}: cancelled request must fail");
		h.assert_released().await;
		// Immediate relaunch with the barrier lifted.
		h.release(op);
		h.backend().gates[&op].hold.store(false, Ordering::SeqCst);
		h.active().await;
		h.core.stop_session().await.unwrap();
		h.assert_released().await;
		assert!(!h.counters().overlapped.load(Ordering::SeqCst), "{op:?}");
	}
}

/// STAB-002: a failure at each constructor, after earlier resources exist,
/// tears the whole session down instead of leaving partial state.
#[tokio::test]
async fn failure_of_each_transition_tears_down_deterministically() {
	for op in [Op::Initialize, Op::Launch, Op::Start, Op::Resume] {
		let h = Harness::new();
		h.fail(op);
		let result = match op {
			Op::Initialize => h.initialize().await,
			Op::Launch => {
				h.initialize().await.unwrap();
				h.core.launch_session().await
			},
			Op::Start => {
				h.launched().await;
				let grant = h.grant().await;
				h.announce(&grant).await.unwrap();
				h.core.start_session(&grant).await
			},
			Op::Resume => {
				let grant = h.resumed().await;
				h.core.start_session(&grant).await
			},
			_ => unreachable!(),
		};
		assert!(result.is_err(), "{op:?}");
		h.wait_idle().await;
		h.assert_released().await;
		let unit_recorded = op != Op::Initialize;
		assert_eq!(
			h.counters().unit_stops.load(Ordering::SeqCst) > 0,
			unit_recorded,
			"{op:?}"
		);
	}
}

/// STAB-002: the HTTP launch timeout drops the caller's future after systemd
/// created the unit. The manager still owns the launch; the follow-up stop
/// cancels it and stops the unit.
#[tokio::test]
async fn timeout_after_transient_unit_creation_stops_the_unit() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.hold(Op::Launch);
	assert!(
		tokio::time::timeout(Duration::from_millis(10), h.core.launch_session())
			.await
			.is_err()
	);
	assert!(h.counters().unit_created.load(Ordering::SeqCst));
	assert_eq!(h.phase().await, "launching", "launch survives its caller");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-002: an abandoned caller does not abandon the launch; it commits.
#[tokio::test]
async fn abandoned_launch_caller_still_commits() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.hold(Op::Launch);
	let _ = tokio::time::timeout(Duration::from_millis(10), h.core.launch_session()).await;
	h.release(Op::Launch);
	tokio::time::timeout(Duration::from_secs(5), async {
		while h.phase().await != "launched" {
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-002: duplicate PLAY and PLAY before ANNOUNCE are rejected without
/// touching the retained application or the live streams.
#[tokio::test]
async fn duplicate_and_premature_play_keep_the_application() {
	let h = Harness::new();
	h.launched().await;
	let grant = h.grant().await;
	assert!(h.core.start_session(&grant).await.is_err(), "PLAY before ANNOUNCE");
	assert_eq!(h.phase().await, "launched");
	h.announce(&grant).await.unwrap();
	h.core.start_session(&grant).await.unwrap();
	let resources = h.counters().resources.load(Ordering::SeqCst);
	assert!(h.core.start_session(&grant).await.is_err(), "duplicate PLAY");
	assert_eq!(h.phase().await, "active");
	assert_eq!(h.counters().resources.load(Ordering::SeqCst), resources);
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 0);
	assert!(h.counters().unit_created.load(Ordering::SeqCst));
	// A duplicate launch is equally harmless.
	assert!(h.core.launch_session().await.is_err());
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-002: PLAY racing an ANNOUNCE whose pause is still in flight cannot
/// commit; the contexts are published only after the pause barrier, so the
/// resumed epoch is never paused after it started.
#[tokio::test]
async fn concurrent_announce_and_play_are_ordered() {
	let h = Harness::new();
	h.active().await;
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	let grant = h.grant().await;
	h.hold(Op::Pause);
	let announce = {
		let core = h.core.clone();
		let grant = grant.clone();
		tokio::spawn(async move { core.set_stream_context(&grant, video(), audio(), false).await })
	};
	h.entered(Op::Pause, 1).await;
	assert!(
		h.core.start_session(&grant).await.is_err(),
		"PLAY during ANNOUNCE pause"
	);
	assert_eq!(h.calls(Op::Resume), 0);
	h.release(Op::Pause);
	announce.await.unwrap().unwrap();
	h.core.start_session(&grant).await.unwrap();
	let order = h.counters().order.lock().unwrap().clone();
	let pause = order.iter().rposition(|op| *op == Op::Pause).unwrap();
	let resume = order.iter().rposition(|op| *op == Op::Resume).unwrap();
	assert!(pause < resume);
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Reporting follows the authenticated generation, even before reconnect PLAY,
/// while the launch context, application and session lifetime remain retained.
#[tokio::test]
async fn retained_session_reports_current_authorized_client() {
	let h = Harness::new();
	let mut grant = h.active().await;
	let original = h.core.session_view().await.unwrap();
	assert_eq!(original.client_ip, h.client);
	let resources = h.counters().resources.load(Ordering::SeqCst);
	let mut status = h.core.subscribe_status();

	// Different IP, same IP, abandoned resume, then back to the launch client.
	for (address, play) in [
		("127.0.0.2", true),
		("127.0.0.2", true),
		("127.0.0.3", false),
		("127.0.0.1", true),
	] {
		let client: IpAddr = address.parse().unwrap();
		status.borrow_and_update();
		h.core
			.resume_session(keys(), ResumeRequest::default(), client)
			.await
			.unwrap();
		assert!(status.has_changed().unwrap(), "resume must wake management reporting");
		let next = h.core.authorize_stream(client).await.unwrap();
		assert!(next.generation() > grant.generation());
		assert_eq!(next.client_ip(), client);
		assert!(!h.core.inner.lock().await.is_current(&grant));
		if client != grant.client_ip() {
			assert!(h.core.authorize_stream(grant.client_ip()).await.is_none());
		}
		let view = h.core.session_view().await.unwrap();
		assert_eq!(view.client_ip, client, "accepted resume updates reporting before PLAY");
		assert_eq!(view.epoch, original.epoch);
		assert_eq!(view.started_at, original.started_at);
		assert_eq!(h.core.get_session_context().await.unwrap().client_ip, h.client);

		assert!(h.announce(&grant).await.is_err(), "late old ANNOUNCE");
		assert!(h.core.start_session(&grant).await.is_err(), "late old PLAY");
		if play {
			h.announce(&next).await.unwrap();
			h.core.start_session(&next).await.unwrap();
		}
		assert_eq!(h.core.session_view().await.unwrap().client_ip, client);
		assert_eq!(h.core.session_view().await.unwrap().epoch, original.epoch);
		assert_eq!(h.calls(Op::Initialize), 1);
		assert_eq!(h.calls(Op::Launch), 1);
		assert_eq!(h.calls(Op::Start), 1);
		assert_eq!(h.calls(Op::StopApplication), 0);
		assert!(h.counters().unit_created.load(Ordering::SeqCst));
		assert_eq!(h.counters().resources.load(Ordering::SeqCst), resources);
		grant = next;
	}
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

#[tokio::test]
async fn rejected_resume_preserves_reported_client_and_authorization() {
	let h = Harness::new();
	h.launched().await;
	let grant = h.grant().await;
	let original = h.core.session_view().await.unwrap();
	let mut status = h.core.subscribe_status();
	status.borrow_and_update();
	assert!(
		h.core
			.resume_session(keys(), ResumeRequest::default(), "127.0.0.2".parse().unwrap())
			.await
			.is_err()
	);
	assert!(!status.has_changed().unwrap());
	assert_eq!(h.core.authorize_stream(h.client).await, Some(grant));
	assert!(h.core.authorize_stream("127.0.0.2".parse().unwrap()).await.is_none());
	let view = h.core.session_view().await.unwrap();
	assert_eq!(view.client_ip, original.client_ip);
	assert_eq!(view.epoch, original.epoch);
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// An ANNOUNCE pause completing after another client's resume must not report
/// the old client's address or publish its pending stream contexts.
#[tokio::test]
async fn superseded_announce_cannot_restore_previous_client_address() {
	let h = Harness::new();
	let old = h.active().await;
	h.hold(Op::Pause);
	let announce = {
		let core = h.core.clone();
		let old = old.clone();
		tokio::spawn(async move { core.set_stream_context(&old, video(), audio(), false).await })
	};
	h.entered(Op::Pause, 1).await;
	let client = "127.0.0.2".parse().unwrap();
	h.core
		.resume_session(keys(), ResumeRequest::default(), client)
		.await
		.unwrap();
	assert_eq!(h.core.session_view().await.unwrap().client_ip, client);
	h.release(Op::Pause);
	assert!(announce.await.unwrap().is_err());
	assert_eq!(h.core.session_view().await.unwrap().client_ip, client);
	assert!(h.core.inner.lock().await.pending.is_none());
	let current = h.core.authorize_stream(client).await.unwrap();
	h.announce(&current).await.unwrap();
	h.core.start_session(&current).await.unwrap();
	assert_eq!(h.core.session_view().await.unwrap().client_ip, client);
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Stale generations: contexts announced before a newer `/resume` cannot be
/// committed, and the live session is unaffected.
#[tokio::test]
async fn stale_generation_cannot_commit() {
	let h = Harness::new();
	h.active().await;
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	let first = h.grant().await;
	h.announce(&first).await.unwrap();
	h.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	assert!(h.core.start_session(&first).await.is_err());
	assert!(h.announce(&first).await.is_err());
	assert_eq!(h.calls(Op::Resume), 0);
	assert_eq!(h.phase().await, "active");
	let second = h.grant().await;
	h.announce(&second).await.unwrap();
	h.core.start_session(&second).await.unwrap();
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Stale completion: a launch that finishes as its session stops is handed to
/// that session's teardown instead of being committed.
#[tokio::test]
async fn completion_after_stop_is_released_by_teardown() {
	let h = Harness::new();
	h.initialize().await.unwrap();
	h.backend().stop_on_launch_return.store(true, Ordering::SeqCst);
	assert!(h.core.launch_session().await.is_err());
	h.wait_idle().await;
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-003: stop completes only after delayed workers exit; meanwhile the
/// session is not idle and a replacement waits instead of overlapping.
#[tokio::test]
async fn teardown_waits_for_delayed_workers_before_replacement() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	let stop = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.entered(Op::StopApplication, 1).await;
	tokio::task::yield_now().await;
	assert_eq!(h.phase().await, "stopping");
	assert!(h.core.get_session_context().await.is_none());
	assert!(h.core.authorize_stream(h.client).await.is_none());
	assert!(
		h.counters().resources.load(Ordering::SeqCst) > 0,
		"workers still own resources"
	);
	let initialized = h.calls(Op::Initialize);
	let replacement = {
		let core = h.core.clone();
		tokio::spawn(async move { core.initialize_session(context()).await })
	};
	for _ in 0..10 {
		tokio::task::yield_now().await;
	}
	assert!(!stop.is_finished() && !replacement.is_finished());
	assert_eq!(
		h.calls(Op::Initialize),
		initialized,
		"replacement must not start before teardown"
	);
	h.backend().worker_exit.send_replace(true);
	stop.await.unwrap().unwrap();
	replacement.await.unwrap().unwrap();
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-003: a worker failure stops the session through the watchdog, which
/// hands over to teardown without cancelling it. This includes the control
/// worker itself exiting: client loss only detaches it, so its exit is fatal.
#[tokio::test]
async fn watchdog_triggered_cleanup_completes() {
	let h = Harness::new();
	for (round, reason) in [
		SessionShutdownReason::VideoEncoderStopped,
		SessionShutdownReason::ControlStreamStopped,
	]
	.into_iter()
	.enumerate()
	{
		h.active().await;
		let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
		stop.trigger_shutdown(reason).unwrap();
		h.wait_idle().await;
		h.assert_released().await;
		assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), round + 1, "{reason:?}");
	}
	// The next session starts normally.
	h.active().await;
	h.core.stop_session().await.unwrap();
}

/// STAB-003: the application exiting while the user cancels produces one
/// teardown, one unit stop and a successful cancel.
#[tokio::test]
async fn application_exit_during_cancel_has_one_owner() {
	let h = Harness::new();
	h.active().await;
	h.hold(Op::StopApplication);
	let cancel = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.entered(Op::StopApplication, 1).await;
	let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
	let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
	let second_cancel = {
		let core = h.core.clone();
		tokio::spawn(async move { core.stop_session().await })
	};
	h.release(Op::StopApplication);
	cancel.await.unwrap().unwrap();
	second_cancel.await.unwrap().unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
}

/// STAB-001 at the manager: stop before StartB releases workers; StartB
/// before workers poll and duplicate StartB start each worker once.
#[tokio::test]
async fn start_signal_contract_through_the_manager() {
	let h = Harness::new();
	h.active().await;
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().workers_started.load(Ordering::SeqCst), 0);

	h.active().await;
	h.core.trigger_streams_start().await;
	h.core.trigger_streams_start().await;
	tokio::time::timeout(Duration::from_secs(5), async {
		while h.counters().workers_started.load(Ordering::SeqCst) < 2 {
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	for _ in 0..10 {
		tokio::task::yield_now().await;
	}
	assert_eq!(h.counters().workers_started.load(Ordering::SeqCst), 2);
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// STAB-003: global shutdown completes only after the session's application
/// and workers have been released.
#[tokio::test]
async fn global_shutdown_includes_session_teardown() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	h.shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
	h.entered(Op::StopApplication, 1).await;
	assert!(
		tokio::time::timeout(Duration::from_millis(20), h.shutdown.wait_shutdown_complete())
			.await
			.is_err()
	);
	h.backend().worker_exit.send_replace(true);
	tokio::time::timeout(Duration::from_secs(5), h.shutdown.wait_shutdown_complete())
		.await
		.unwrap();
	h.assert_released().await;
	assert!(h.initialize().await.is_err(), "no new sessions during service shutdown");
}

/// STAB-003: a worker that never exits exceeds the documented deadline. The
/// session is never reported idle, replacement is refused and the service is
/// asked to stop.
#[tokio::test(start_paused = true)]
async fn teardown_deadline_is_a_terminal_failure() {
	let h = Harness::new();
	h.active().await;
	h.backend().worker_exit.send_replace(false);
	assert!(h.core.stop_session().await.is_err());
	assert_eq!(h.phase().await, "stopping");
	assert!(h.shutdown.is_shutdown_triggered());
	assert!(h.initialize().await.is_err());
	h.backend().worker_exit.send_replace(true);
}

/// STAB-003: the application stop is bounded and does not wedge teardown.
///
/// Only the bound is asserted here. Whether an unconfirmed stop may report
/// success and Idle is the separate contract in
/// `hung_application_stop_is_not_reported_idle` (review 2026-10-05 STAB-003).
#[tokio::test(start_paused = true)]
async fn hung_application_stop_is_bounded() {
	let h = Harness::new();
	h.active().await;
	h.hold(Op::StopApplication);
	let started = tokio::time::Instant::now();
	let _ = h.core.stop_session().await;
	assert!(started.elapsed() <= SESSION_TEARDOWN_DEADLINE);
	assert_eq!(h.counters().resources.load(Ordering::SeqCst), 0);
}

// Review 2026-10-05 characterizations. Each test states the corrected
// contract and is ignored with a "known defect" reason until the batch that
// fixes it; `scripts/known_defects.py` checks they still fail for the
// recorded reason (see `scripts/known_defects.toml`).

/// Review 2026-10-05 STAB-003: a failed application stop leaves the unit's
/// termination unverified. Teardown must not report success or Idle, and a new
/// session must not be initialized over that unresolved ownership.
#[tokio::test]
async fn failed_application_stop_is_not_reported_idle() {
	let h = Harness::new();
	h.active().await;
	h.fail(Op::StopApplication);
	let stopped = h.core.stop_session().await;
	assert!(
		h.counters().unit_created.load(Ordering::SeqCst),
		"fixture: a failed stop leaves the fake unit live"
	);
	let phase = h.phase().await;
	assert!(
		stopped.is_err() && phase != "idle",
		"review 2026-10-05 STAB-003: teardown reported {stopped:?} and phase {phase} with a live application unit"
	);
	let replacement = h.initialize().await;
	assert!(
		replacement.is_err() || !h.counters().overlapped.load(Ordering::SeqCst),
		"review 2026-10-05 STAB-003: a new session was initialized over unresolved application ownership"
	);
}

/// Review 2026-10-05 STAB-003: the same contract when the stop never answers
/// (bus loss, a stop job outliving its wait): the bounded teardown ends in a
/// failure state, not Idle.
#[tokio::test(start_paused = true)]
async fn hung_application_stop_is_not_reported_idle() {
	let h = Harness::new();
	h.active().await;
	h.hold(Op::StopApplication);
	let stopped = h.core.stop_session().await;
	let phase = h.phase().await;
	assert!(
		stopped.is_err() && phase != "idle",
		"review 2026-10-05 STAB-003: an unconfirmed application stop reported {stopped:?} and phase {phase}"
	);
	h.release(Op::StopApplication);
}

/// Review 2026-10-05 STAB-001: a reconnect PLAY whose authorization is
/// replaced by another `/resume` while its reconfiguration is in flight must
/// not commit. Either the replacing resume is refused until the transition
/// finishes, or the superseded PLAY fails and the newest grant completes.
#[tokio::test]
async fn superseded_reconnect_play_cannot_commit() {
	let h = Harness::new();
	let old = h.resumed().await;
	h.hold(Op::Resume);
	let play = {
		let core = h.core.clone();
		let old = old.clone();
		tokio::spawn(async move { core.start_session(&old).await })
	};
	h.entered(Op::Resume, 1).await;
	let replaced = h
		.core
		.resume_session(keys(), ResumeRequest::default(), h.client)
		.await
		.is_ok();
	h.release(Op::Resume);
	let committed = play.await.unwrap().is_ok();
	if replaced {
		assert!(
			!committed,
			"review 2026-10-05 STAB-001: PLAY of generation {} committed after a newer /resume replaced it",
			old.generation()
		);
		let current = h.grant().await;
		assert_ne!(current.generation(), old.generation());
		h.announce(&current).await.unwrap();
		h.core.start_session(&current).await.unwrap();
	} else {
		assert!(committed, "a refused resume must leave the in-flight PLAY to finish");
	}
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Review 2026-10-05 STAB-001: every reconnect plan carries the generation
/// and keys of the PLAY that produced it, snapshotted together; a superseded
/// PLAY's plan never carries the newer resume's keys, and the replacing PLAY's
/// plan carries its own.
#[tokio::test]
async fn reconnect_plans_snapshot_generation_and_keys_together() {
	let h = Harness::new();
	h.active().await;
	h.core
		.resume_session(keys_with_id(5), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	let first = h.grant().await;
	h.announce(&first).await.unwrap();
	h.hold(Op::Resume);
	let play = {
		let core = h.core.clone();
		let first = first.clone();
		tokio::spawn(async move { core.start_session(&first).await })
	};
	h.entered(Op::Resume, 1).await;
	h.core
		.resume_session(keys_with_id(6), ResumeRequest::default(), h.client)
		.await
		.unwrap();
	h.release(Op::Resume);
	assert!(play.await.unwrap().is_err(), "the superseded PLAY must not succeed");
	let second = h.grant().await;
	h.announce(&second).await.unwrap();
	h.core.start_session(&second).await.unwrap();
	let records: Vec<_> = h
		.backend()
		.plans
		.lock()
		.unwrap()
		.iter()
		.map(|plan| (plan.generation, plan.key_id))
		.collect();
	assert_eq!(records, vec![(first.generation(), 5), (second.generation(), 6)]);
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// A `/resume` cannot replace the authorization while the first PLAY is still
/// starting streams: there is no retained stream to resume yet. The PLAY then
/// completes normally.
#[tokio::test]
async fn resume_during_first_play_is_refused() {
	let h = Harness::new();
	let task = hold_at(&h, Op::Start).await;
	assert!(
		h.core
			.resume_session(keys(), ResumeRequest::default(), h.client)
			.await
			.is_err()
	);
	h.release(Op::Start);
	task.await.unwrap().unwrap();
	assert_eq!(h.phase().await, "active");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// Review 2026-10-05 STAB-007: a failed ANNOUNCE pause has side effects the
/// manager cannot see through `Result<(), ()>`; one subsystem may already be
/// paused. The session must not stay Active with a running-looking epoch:
/// it is handed to teardown (or, once the backend reports a finer outcome,
/// left in an explicitly defined state).
#[tokio::test]
async fn failed_announce_pause_has_a_defined_outcome() {
	let h = Harness::new();
	let grant = h.active().await;
	h.fail(Op::Pause);
	assert!(h.announce(&grant).await.is_err());
	let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
	let phase = h.phase().await;
	assert!(
		stop.is_shutdown_triggered() || phase != "active",
		"review 2026-10-05 STAB-007: failed ANNOUNCE pause left phase {phase} with no teardown"
	);
	h.wait_idle().await;
	h.assert_released().await;
}

/// Review 2026-10-05 BUG-003: the first PLAY must leave one coherent output
/// mode. If the authoritative RTSP mode differs from the HTTP launch mode it
/// is either rejected before streams start or applied and published, as a
/// reconnect PLAY already does.
#[tokio::test]
async fn first_play_publishes_the_negotiated_output_mode() {
	let h = Harness::new();
	h.launched().await;
	assert_eq!(h.core.get_session_context().await.unwrap().resolution, (1920, 1080));
	let grant = h.grant().await;
	let negotiated = VideoStreamContext {
		width: 2560,
		height: 1440,
		fps: 120,
		..video()
	};
	let accepted =
		h.core
			.set_stream_context(&grant, negotiated, audio(), false)
			.await
			.is_ok() && h.core.start_session(&grant).await.is_ok();
	if accepted {
		let context = h.core.get_session_context().await.unwrap();
		assert!(
			context.resolution == (2560, 1440) && context.refresh_rate == 120,
			"review 2026-10-05 BUG-003: first PLAY negotiated 2560x1440@120 but the session reports {:?}@{}",
			context.resolution,
			context.refresh_rate
		);
	}
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
}

/// One point of the repeated-session settings matrix. Each [`Self::step`]
/// changes exactly one negotiated axis, so consecutive cycles differ in one
/// property (or none, for the unchanged-resume axis).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Matrix {
	large: bool,
	high_fps: bool,
	high_bitrate: bool,
	codec: usize,
	encrypt: bool,
	hdr: bool,
	yuv444: bool,
	channels: usize,
	high_quality: bool,
	ten_ms: bool,
}

const AXES: [&str; 11] = [
	"unchanged",
	"resolution",
	"fps",
	"bitrate",
	"codec",
	"encryption",
	"dynamic range",
	"chroma",
	"audio channels",
	"audio quality",
	"packet duration",
];

const CODECS: [VideoCodec; 4] = [
	VideoCodec::H264,
	VideoCodec::Hevc,
	VideoCodec::Av1,
	VideoCodec::PyroWave,
];

impl Matrix {
	fn step(mut self, axis: usize) -> Self {
		match axis {
			0 => {},
			1 => self.large ^= true,
			2 => self.high_fps ^= true,
			3 => self.high_bitrate ^= true,
			4 => self.codec = (self.codec + 1) % CODECS.len(),
			5 => self.encrypt ^= true,
			6 => self.hdr ^= true,
			7 => self.yuv444 ^= true,
			8 => self.channels = (self.channels + 1) % 3,
			9 => self.high_quality ^= true,
			10 => self.ten_ms ^= true,
			_ => unreachable!(),
		}
		self
	}

	fn resolution(self) -> (u32, u32) {
		if self.large { (2560, 1440) } else { (1920, 1080) }
	}

	fn fps(self) -> u32 {
		if self.high_fps { 120 } else { 60 }
	}

	fn audio_channels(self) -> (AudioChannels, u32) {
		[
			(AudioChannels::Stereo, 0x3),
			(AudioChannels::Surround51, 0x3f),
			(AudioChannels::Surround71, 0x63f),
		][self.channels]
	}

	fn video(self) -> VideoStreamContext {
		let codec = CODECS[self.codec];
		let chroma = if self.yuv444 {
			ChromaFormat::Yuv444
		} else {
			ChromaFormat::Yuv420
		};
		let range = if codec == VideoCodec::PyroWave {
			ColorRange::Full
		} else {
			ColorRange::Limited
		};
		let (width, height) = self.resolution();
		VideoStreamContext {
			pyrowave_dialect: (codec == VideoCodec::PyroWave)
				.then_some(crate::session::stream::video::pyrowave_protocol::PyroWaveDialect::NativeWireV1),
			width,
			height,
			fps: self.fps(),
			bitrate: if self.high_bitrate { 80_000_000 } else { 20_000_000 },
			format: if self.hdr {
				NegotiatedVideoFormat::hdr10(codec, chroma, range)
			} else {
				NegotiatedVideoFormat::sdr(codec, chroma, BitDepth::Eight, range)
			},
			encrypt_video: self.encrypt,
			..video()
		}
	}

	fn audio(self) -> AudioStreamContext {
		let (channels, mask) = self.audio_channels();
		AudioStreamContext {
			packet_duration_ms: if self.ten_ms { 10 } else { 5 },
			audio_config: AudioConfig::from_channels(channels, mask, self.high_quality),
			encrypt_audio: self.encrypt,
			..audio()
		}
	}

	fn context(self) -> SessionContext {
		let (audio_channels, audio_channel_mask) = self.audio_channels();
		SessionContext {
			resolution: self.resolution(),
			refresh_rate: self.fps(),
			hdr: self.hdr,
			audio_channels,
			audio_channel_mask,
			..context()
		}
	}
}

async fn workers_started(h: &Harness, count: usize) {
	tokio::time::timeout(Duration::from_secs(5), async {
		while h.counters().workers_started.load(Ordering::SeqCst) < count {
			tokio::task::yield_now().await;
		}
	})
	.await
	.expect("stream workers did not start");
}

/// TEST-001: 100 complete launch → stream → stop → relaunch cycles through one
/// manager. Settings move along every negotiated axis, `StartB` is absent,
/// single or duplicated, and the stop comes from the user, the application
/// exiting or a failing worker. After every completed stop nothing remains:
/// no fake socket/thread/GPU owner, no application unit, no orphan, no stale
/// authorization, and the next initialization never overlaps old resources.
#[tokio::test]
async fn hundred_full_session_cycles_release_everything() {
	let h = Harness::new();
	let mut settings = Matrix::default();
	let mut started = 0;
	for cycle in 0..100 {
		settings = settings.step(cycle % AXES.len());
		h.core.initialize_session(settings.context()).await.unwrap();
		h.core.launch_session().await.unwrap();
		let grant = h.grant().await;
		h.core
			.set_stream_context(&grant, settings.video(), settings.audio(), false)
			.await
			.unwrap();
		h.core.start_session(&grant).await.unwrap();
		let context = h.core.get_session_context().await.unwrap();
		assert_eq!(context.resolution, settings.resolution());
		assert_eq!(context.hdr, settings.hdr);
		for _ in 0..cycle % 3 {
			h.core.trigger_streams_start().await;
		}
		if cycle % 3 != 0 {
			started += 2;
			workers_started(&h, started).await;
		}
		let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
		match (cycle / 3) % 3 {
			0 => h.core.stop_session().await.unwrap(),
			1 => {
				let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
				h.wait_idle().await;
			},
			_ => {
				let _ = stop.trigger_shutdown(SessionShutdownReason::VideoEncoderStopped);
				h.wait_idle().await;
			},
		}
		h.assert_released().await;
		assert!(h.core.start_session(&grant).await.is_err(), "cycle {cycle}: stale PLAY");
		assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), cycle + 1);
		assert_eq!(
			h.counters().workers_started.load(Ordering::SeqCst),
			started,
			"cycle {cycle}: a worker started without StartB"
		);
	}
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
}

/// TEST-001: 100 connect → stream → disconnect → resume cycles on one retained
/// application. Each cycle is a new authenticated generation (alternating a
/// fresh and a reused key) and changes one negotiated axis or nothing. Every
/// reconnect pauses both media epochs before it resumes, recreates video only
/// when a video property changed, always commits the audio epoch, rejects the
/// previous generation, and leaves resource ownership flat.
#[tokio::test]
async fn hundred_reconnect_cycles_alternate_every_negotiated_axis() {
	let h = Harness::new();
	let mut settings = Matrix::default();
	h.core.initialize_session(settings.context()).await.unwrap();
	h.core.launch_session().await.unwrap();
	let mut previous = h.grant().await;
	h.core
		.set_stream_context(&previous, settings.video(), settings.audio(), false)
		.await
		.unwrap();
	h.core.start_session(&previous).await.unwrap();
	h.core.trigger_streams_start().await;
	workers_started(&h, 2).await;
	let resources = h.counters().resources.load(Ordering::SeqCst);
	let mut fast_resumes = 0;
	for cycle in 0..100u32 {
		let axis = cycle as usize % AXES.len();
		let next = settings.step(axis);
		let key = RemoteInputKey::from_bytes([(cycle % 2) as u8 + 1; 16]);
		h.core
			.resume_session(
				SessionKeyData::new(key, RemoteInputKeyId::new(cycle)),
				ResumeRequest::default(),
				h.client,
			)
			.await
			.unwrap();
		let grant = h.grant().await;
		assert!(
			h.core
				.set_stream_context(&previous, next.video(), next.audio(), false)
				.await
				.is_err(),
			"cycle {cycle}: the previous generation cannot announce"
		);
		let (pauses, resumes) = (h.calls(Op::Pause), h.calls(Op::Resume));
		h.core
			.set_stream_context(&grant, next.video(), next.audio(), false)
			.await
			.unwrap();
		assert_eq!(h.calls(Op::Pause), pauses + 1, "cycle {cycle}: reconnect must pause");
		h.core.start_session(&grant).await.unwrap();
		h.core.trigger_streams_start().await;
		assert_eq!(h.calls(Op::Resume), resumes + 1);
		let order = h.counters().order.lock().unwrap().clone();
		assert!(order.iter().rposition(|op| *op == Op::Pause) < order.iter().rposition(|op| *op == Op::Resume));

		let PlanRecord {
			video_recreated, audio, ..
		} = h.backend().plans.lock().unwrap().pop().unwrap();
		assert_eq!(
			video_recreated,
			settings.video() != next.video(),
			"cycle {cycle}: {} change",
			AXES[axis]
		);
		fast_resumes += usize::from(!video_recreated);
		assert_eq!(audio, next.audio(), "cycle {cycle}: audio epoch is always committed");
		assert_eq!(h.phase().await, "active");
		assert_eq!(
			h.counters().resources.load(Ordering::SeqCst),
			resources,
			"cycle {cycle}"
		);
		assert_eq!(
			h.counters().unit_stops.load(Ordering::SeqCst),
			0,
			"application retained"
		);
		let context = h.core.get_session_context().await.unwrap();
		assert_eq!(context.resolution, next.resolution());
		assert_eq!(context.refresh_rate, next.fps());
		assert_eq!(context.hdr, next.hdr);
		assert_eq!(
			(context.audio_channels, context.audio_channel_mask),
			next.audio_channels()
		);
		settings = next;
		previous = grant;
	}
	// Unchanged and audio-only reconnects keep the video pipeline.
	assert!(fast_resumes >= 30, "{fast_resumes}");
	h.core.stop_session().await.unwrap();
	h.assert_released().await;
	assert_eq!(h.counters().unit_stops.load(Ordering::SeqCst), 1);
	assert!(!h.counters().overlapped.load(Ordering::SeqCst));
}

/// TEST-001/STAB-003: service shutdown (SIGTERM, Ctrl+C) while each transition
/// is in flight cancels it, completes the global shutdown only after the
/// session was released, and refuses later sessions.
#[tokio::test]
async fn service_shutdown_during_every_transition_completes() {
	for op in TRANSITIONS {
		let h = Harness::new();
		let task = hold_at(&h, op).await;
		h.shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
		tokio::time::timeout(Duration::from_secs(5), h.shutdown.wait_shutdown_complete())
			.await
			.unwrap_or_else(|_| panic!("{op:?}: service shutdown did not complete"));
		assert!(task.await.unwrap().is_err(), "{op:?}: cancelled request must fail");
		h.assert_released().await;
		assert!(h.initialize().await.is_err(), "{op:?}");
	}
}

mod status {
	use super::*;
	use crate::session::status::{ClientPresence, SessionPhase, derive_phase};

	fn presence(generation: u64, attached: bool, was_attached: bool) -> ClientPresence {
		ClientPresence {
			generation,
			attached,
			was_attached,
		}
	}

	fn phase(status: &watch::Receiver<ManagerStatus>, presence: ClientPresence) -> SessionPhase {
		derive_phase(&status.borrow(), &presence, Instant::now(), Duration::from_secs(60))
	}

	/// The published status follows every lifecycle step, and only the
	/// current generation's attached control peer makes a session streaming.
	#[tokio::test]
	async fn status_follows_launch_streaming_resume_and_canonical_stop() {
		let h = Harness::new();
		let status = h.core.subscribe_status();
		let none = ClientPresence::default();
		assert_eq!(phase(&status, none), SessionPhase::Idle);

		let launch = hold_at(&h, Op::Launch).await;
		assert_eq!(phase(&status, none), SessionPhase::Starting);
		h.release(Op::Launch);
		launch.await.unwrap().unwrap();
		assert_eq!(phase(&status, none), SessionPhase::Starting, "launched, awaiting PLAY");

		let grant = h.grant().await;
		h.announce(&grant).await.unwrap();
		h.core.start_session(&grant).await.unwrap();
		let first = grant.generation();
		assert_eq!(phase(&status, presence(first, false, false)), SessionPhase::Starting);
		assert_eq!(phase(&status, presence(first, true, true)), SessionPhase::Streaming);
		assert_eq!(
			phase(&status, presence(first, false, true)),
			SessionPhase::ClientDisconnected,
			"client loss retains the session"
		);

		h.core
			.resume_session(keys(), ResumeRequest::default(), h.client)
			.await
			.unwrap();
		assert_eq!(phase(&status, presence(first, false, true)), SessionPhase::Reconnecting);
		let grant = h.grant().await;
		h.announce(&grant).await.unwrap();
		assert_eq!(phase(&status, presence(first, false, true)), SessionPhase::Reconnecting);
		h.core.start_session(&grant).await.unwrap();
		let second = grant.generation();
		assert_eq!(
			phase(&status, presence(second, false, false)),
			SessionPhase::Reconnecting
		);
		assert_eq!(phase(&status, presence(second, true, true)), SessionPhase::Streaming);
		// The replaced generation's peer can never report streaming.
		assert_eq!(phase(&status, presence(first, true, true)), SessionPhase::Reconnecting);

		// The canonical stop is visible while teardown stops the application.
		h.hold(Op::StopApplication);
		let core = h.core.clone();
		let stop = tokio::spawn(async move { core.stop_session().await });
		h.entered(Op::StopApplication, 1).await;
		assert_eq!(phase(&status, presence(second, true, true)), SessionPhase::Stopping);
		h.release(Op::StopApplication);
		stop.await.unwrap().unwrap();
		h.assert_released().await;
		assert_eq!(phase(&status, presence(second, false, true)), SessionPhase::Idle);
		let last = status.borrow().last_stop.clone().unwrap();
		assert_eq!(last.reason, SessionShutdownReason::UserStopped);
		assert!(last.completed);
	}

	/// A worker failure ends the session through the same teardown and is
	/// reported as an unexpected stop.
	#[tokio::test]
	async fn worker_failure_is_reported_as_the_last_stop() {
		let h = Harness::new();
		let status = h.core.subscribe_status();
		h.active().await;
		let stop = h.backend().session_stop.lock().unwrap().clone().unwrap();
		stop.trigger_shutdown(SessionShutdownReason::CompositorStopped).unwrap();
		h.wait_idle().await;
		let last = status.borrow().last_stop.clone().unwrap();
		assert_eq!(last.reason, SessionShutdownReason::CompositorStopped);
		assert_eq!(phase(&status, ClientPresence::default()), SessionPhase::Idle);
	}

	/// A teardown past its deadline is never reported idle.
	#[tokio::test(start_paused = true)]
	async fn teardown_failure_is_an_error_phase() {
		let h = Harness::new();
		let status = h.core.subscribe_status();
		h.active().await;
		h.backend().worker_exit.send_replace(false);
		assert!(h.core.stop_session().await.is_err());
		assert_eq!(phase(&status, ClientPresence::default()), SessionPhase::Error);
		assert!(!status.borrow().last_stop.clone().unwrap().completed);
		h.backend().worker_exit.send_replace(true);
	}
}
