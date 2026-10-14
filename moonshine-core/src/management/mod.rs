//! Local management interface for the optional desktop UI.
//!
//! The service exports [`moonshine_management::INTERFACE`] on the user's
//! session bus. It is a control surface over the daemon's own components, not
//! a parallel implementation: sessions end through `SessionManager`, pairing
//! approval goes through `ClientManager` with its request tokens, revocation
//! uses the same path as the loopback `/unpair` route, and configuration
//! edits go through the configuration store (`config_store`).
//!
//! Nothing here is on a media path. Session state arrives through `watch`
//! channels published on lifecycle and peer-ownership changes; pairing events
//! through a bounded `broadcast` channel; frame statistics and GPU readings
//! while a client is streaming (see `telemetry`). Statistics are signalled
//! only while the desktop UI is present, and the last two minutes are kept
//! for support bundles (`crate::support`).
//! Signals are emitted from management tasks; a slow or crashed UI cannot
//! block any of them, and the daemon runs unchanged when the session bus name
//! cannot be acquired.

mod config_store;
mod dbus;
#[cfg(test)]
mod dbus_tests;
mod findings;
pub(crate) mod schema;
pub(crate) mod telemetry;

use std::collections::VecDeque;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_shutdown::ShutdownManager;
use futures_util::StreamExt;
use moonshine_management::dto::{
	ApplicationSummary, AudioDetails, Capabilities, ClientsSnapshot, CodecCapability, DiagnosticReport, EncoderDetails,
	HealthCheck, HealthReport, LastStop, Listeners, PairedClient, PairingOutcome, PairingResolution, PairingSnapshot,
	PendingPairing, RequestedMode, ServerEvent, ServerInfo, SessionDetails, SessionPhase, SessionSnapshot, StreamStats,
	VideoDetails,
};
use moonshine_management::{BUS_NAME, OBJECT_PATH, UI_BUS_NAME};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use self::config_store::ConfigStore;
use self::dbus::ManagementInterface;
use self::telemetry::unix_millis;
use crate::ShutdownReason;
use crate::clients::{ClientManager, PairingEvent};
use crate::config::Config;
use crate::gpu_sensors::{self, GpuSampler};
use crate::healthcheck::{self, CheckOutcome};
use crate::session::manager::{SessionManager, SessionView};
use crate::session::status::{self, ClientPresence, ManagerStatus};
use crate::session::stream::audio::{AudioQuality, AudioStreamContext};
use crate::session::stream::video::{ColorPrimaries, ColorRange, MatrixCoefficients, TransferFunction, VideoCodec};
use crate::support;

/// Facts established at startup, reported by `GetServer`.
pub struct ServerFacts {
	pub version: String,
	pub config_path: PathBuf,
	pub supported_codecs: u32,
	pub hdr_advertised: bool,
	pub dma_buf: bool,
	pub gpu_name: String,
	/// The startup health check, unless `--no-health-check` skipped it.
	pub health: Option<healthcheck::HealthReport>,
}

/// Events kept for `GetEvents` and the diagnostic report.
const EVENT_LOG: usize = 50;
/// Statistics windows kept for support bundles: two minutes.
const STATS_HISTORY: usize = 120;

/// Shared state behind the D-Bus interface and the event tasks.
pub(crate) struct Management {
	sessions: SessionManager,
	clients: ClientManager,
	config: ConfigStore,
	/// `health` is replaced by `RunHealthCheck`; the rest is fixed at startup.
	server: Mutex<ServerInfo>,
	/// The running configuration, for re-running the health check.
	running_config: Config,
	/// The latest health check, from startup or `RunHealthCheck`.
	health: Mutex<Option<healthcheck::HealthReport>>,
	/// Serializes `RunHealthCheck`: GPU probes must not run concurrently.
	health_running: tokio::sync::Mutex<()>,
	/// Recent operator-relevant events, oldest first.
	events: Mutex<VecDeque<ServerEvent>>,
	pairing_enabled: bool,
	/// How long a session waits for its client before it is reported as
	/// disconnected (the configured stream timeout).
	client_patience: Duration,
	session: watch::Sender<SessionSnapshot>,
	stats: watch::Sender<Option<StreamStats>>,
	/// Recent statistics windows, oldest first. Kept after the stream ends,
	/// so a bundle collected afterwards still shows it.
	stats_history: Mutex<VecDeque<StreamStats>>,
	/// The server's Tokio runtime. zbus serves methods on its own executor,
	/// so work that needs Tokio (session teardown, blocking file I/O) is
	/// handed to this runtime.
	runtime: tokio::runtime::Handle,
}

/// Running management service. Dropping it releases the bus name and stops
/// its tasks; it never affects sessions.
pub struct ManagementService {
	tasks: Vec<JoinHandle<()>>,
}

impl Drop for ManagementService {
	fn drop(&mut self) {
		for task in &self.tasks {
			task.abort();
		}
	}
}

impl ManagementService {
	/// Start serving on the session bus. Failure to connect or to own the
	/// name is logged; the server keeps running without the management API.
	pub fn spawn(
		facts: ServerFacts,
		config: &Config,
		sessions: SessionManager,
		clients: ClientManager,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Self {
		let management = Arc::new(Management::new(facts, config, sessions, clients));
		let task = tokio::spawn(async move {
			let _ = shutdown.wrap_cancel(serve(management, management_bus())).await;
		});
		Self { tasks: vec![task] }
	}
}

/// The bus the management interface is served on: the session bus, or
/// `MOONSHINE_MANAGEMENT_BUS` (a D-Bus address) for a test instance that runs
/// beside a live service on the same account and must not take over its name
/// (applications still start through the session bus's systemd).
fn management_bus() -> zbus::Result<zbus::connection::Builder<'static>> {
	match std::env::var("MOONSHINE_MANAGEMENT_BUS") {
		Ok(address) => {
			tracing::info!(address, "Serving the management interface on MOONSHINE_MANAGEMENT_BUS");
			zbus::connection::Builder::address(address.as_str())
		},
		Err(_) => zbus::connection::Builder::session(),
	}
}

impl Management {
	fn new(facts: ServerFacts, config: &Config, sessions: SessionManager, clients: ClientManager) -> Self {
		Self {
			server: Mutex::new(server_info(&facts, config)),
			running_config: config.clone(),
			health: Mutex::new(facts.health),
			health_running: tokio::sync::Mutex::new(()),
			events: Mutex::new(VecDeque::with_capacity(EVENT_LOG)),
			config: ConfigStore::open(facts.config_path),
			pairing_enabled: config.webserver.enable_pairing,
			client_patience: Duration::from_secs(config.stream.timeout.max(1)),
			session: watch::channel(SessionSnapshot {
				phase: SessionPhase::Idle,
				session: None,
				last_stop: None,
			})
			.0,
			stats: watch::channel(None).0,
			stats_history: Mutex::new(VecDeque::with_capacity(STATS_HISTORY)),
			runtime: tokio::runtime::Handle::current(),
			sessions,
			clients,
		}
	}

	fn server(&self) -> ServerInfo {
		self.server.lock().unwrap().clone()
	}

	/// Keep `event` for `GetEvents` and emit it as `ServerEvent`.
	async fn publish_event(&self, emitter: &zbus::object_server::SignalEmitter<'static>, event: ServerEvent) {
		{
			let mut events = self.events.lock().unwrap();
			if events.len() == EVENT_LOG {
				events.pop_front();
			}
			events.push_back(event.clone());
		}
		let _ = ManagementInterface::server_event(emitter, &json(&event)).await;
	}

	fn events(&self) -> Vec<ServerEvent> {
		self.events.lock().unwrap().iter().cloned().collect()
	}

	/// Re-run the health check without disturbing a live session (see
	/// [`healthcheck::recheck_in_server`]) and make it the reported one.
	async fn run_health_check(&self) -> HealthReport {
		let _running = self.health_running.lock().await;
		let session_active = self.session.borrow().phase != SessionPhase::Idle;
		let previous = self.health.lock().unwrap().clone();
		let config = self.running_config.clone();
		let advertised = self.server.lock().unwrap().capabilities.codecs.clone();
		let report = tokio::task::spawn_blocking(move || {
			healthcheck::recheck_in_server(&config, previous.as_ref(), session_active)
		})
		.await
		.expect("health check task");
		let mut dto = health_report(&report, true);
		// Clients see the codecs probed at startup until Pyroshine restarts.
		let now: Vec<_> = codec_capabilities(report.supported_codecs);
		if !session_active && now != advertised {
			dto.checks.push(HealthCheck {
				name: "Advertised codecs".into(),
				outcome: "warning".into(),
				message: "The verified codec profiles changed since Pyroshine started.".into(),
				duration_ms: 0,
				id: Some("advertised_codecs".into()),
				hint: Some("Restart Pyroshine so clients are offered the profiles this GPU now passes.".into()),
				fatal: false,
			});
		}
		*self.health.lock().unwrap() = Some(report);
		self.server.lock().unwrap().health = Some(dto.clone());
		dto
	}

	/// The capture GPU's render node, as the compositor selects it.
	fn capture_node(&self) -> Option<PathBuf> {
		healthcheck::find_render_node(&self.running_config.compositor.gpu).ok()
	}

	/// The diagnostic report, including host details (read on a blocking thread).
	async fn diagnostic_report(&self) -> DiagnosticReport {
		let capture = self.capture_node();
		let host = tokio::task::spawn_blocking(move || {
			let service =
				gpu_sensors::process_cgroup("self").and_then(|cgroup| support::ServiceUnit::from_cgroup(&cgroup));
			support::host_info(capture.as_deref(), service.as_ref())
		})
		.await
		.ok();
		DiagnosticReport {
			generated_at_ms: unix_millis(SystemTime::now()),
			server: self.server(),
			session: self.session.borrow().clone(),
			stats: self.stats.borrow().clone(),
			events: self.events(),
			host,
		}
	}

	/// The diagnostic report as redacted JSON, for "Copy diagnostics".
	async fn redacted_diagnostic_report(&self) -> String {
		let report = self.diagnostic_report().await;
		let mut redactor = support::Redactor::for_host();
		for (value, placeholder) in crate::state::identifying_values(self.clients.persistent_state().path()) {
			redactor.word(&value, &placeholder);
		}
		redactor.word(&self.running_config.name, "<server-name>");
		let mut value = serde_json::to_value(&report).expect("management documents serialize");
		redactor.json(&mut value);
		value.to_string()
	}

	/// A redacted support bundle (see [`support`]).
	async fn support_bundle(&self) -> Vec<u8> {
		let sources = support::BundleSources {
			report: Some(self.diagnostic_report().await),
			stats_history: self.stats_history.lock().unwrap().iter().cloned().collect(),
			health: None,
			config_path: PathBuf::from(self.server().config_path),
			state_path: Some(self.clients.persistent_state().path().to_path_buf()),
			capture_node: self.capture_node(),
			service: gpu_sensors::process_cgroup("self").and_then(|cgroup| support::ServiceUnit::from_cgroup(&cgroup)),
			notes: Vec::new(),
		};
		match tokio::task::spawn_blocking(move || support::build(sources, SystemTime::now())).await {
			Ok(bundle) => bundle.archive,
			Err(error) => {
				tracing::warn!("Support bundle collection failed: {error}");
				Vec::new()
			},
		}
	}

	fn record_stats(&self, stats: &StreamStats) {
		let mut history = self.stats_history.lock().unwrap();
		if history.len() == STATS_HISTORY {
			history.pop_front();
		}
		history.push_back(stats.clone());
	}
}

async fn serve(management: Arc<Management>, bus: zbus::Result<zbus::connection::Builder<'static>>) {
	let connection = match bus.and_then(|builder| builder.name(BUS_NAME)).and_then(|builder| {
		builder.serve_at(
			OBJECT_PATH,
			ManagementInterface {
				management: management.clone(),
			},
		)
	}) {
		Ok(builder) => builder.build().await,
		Err(error) => Err(error),
	};
	let connection = match connection {
		Ok(connection) => connection,
		Err(error) => {
			tracing::warn!(
				"Desktop management interface unavailable ({error}); Pyroshine continues without it. \
				 Pairing approval remains available on the local web page."
			);
			return;
		},
	};
	tracing::info!(
		name = BUS_NAME,
		"Desktop management interface available on the session bus"
	);
	let emitter = match zbus::object_server::SignalEmitter::new(&connection, OBJECT_PATH) {
		Ok(emitter) => emitter.into_owned(),
		Err(error) => {
			tracing::warn!("Cannot emit management signals: {error}");
			return;
		},
	};

	let (ui_tx, ui_rx) = watch::channel(false);
	tokio::join!(
		follow_ui(&connection, &management, ui_tx),
		follow_sessions(&management, &emitter),
		follow_pairing(&management, &emitter),
		follow_stats(&management, &emitter, ui_rx),
		follow_problems(&management, &emitter),
	);
}

fn json<T: serde::Serialize>(value: &T) -> String {
	serde_json::to_string(value).expect("management documents serialize")
}

/// Track whether the desktop UI runs (it owns [`UI_BUS_NAME`]).
async fn follow_ui(connection: &zbus::Connection, management: &Management, present: watch::Sender<bool>) {
	let proxy = match zbus::fdo::DBusProxy::new(connection).await {
		Ok(proxy) => proxy,
		Err(error) => {
			tracing::warn!("Cannot follow the desktop UI's presence: {error}");
			return;
		},
	};
	let mut changes = match proxy.receive_name_owner_changed_with_args(&[(0, UI_BUS_NAME)]).await {
		Ok(changes) => changes,
		Err(error) => {
			tracing::warn!("Cannot follow the desktop UI's presence: {error}");
			return;
		},
	};
	let name = zbus::names::BusName::try_from(UI_BUS_NAME).expect("valid bus name");
	let set = |owned: bool| {
		if *present.borrow() != owned {
			tracing::debug!(present = owned, "Desktop UI presence changed");
		}
		management.clients.set_operator_ui_present(owned);
		present.send_replace(owned);
	};
	set(proxy.name_has_owner(name).await.unwrap_or(false));
	while let Some(change) = changes.next().await {
		if let Ok(args) = change.args() {
			set(args.new_owner().is_some());
		}
	}
}

/// Recompute the public session state on lifecycle, client ownership or
/// compositor foreground changes, or when a waiting session runs out of patience.
async fn follow_sessions(management: &Management, emitter: &zbus::object_server::SignalEmitter<'static>) {
	let mut status = management.sessions.subscribe_status();
	let mut presence = management.sessions.subscribe_presence();
	let mut reported_stop = status.borrow().last_stop.clone();
	loop {
		let manager = status.borrow_and_update().clone();
		let client = *presence.borrow_and_update();
		let mut foreground = management.sessions.subscribe_foreground().await;
		if let Some(receiver) = foreground.as_mut() {
			receiver.borrow_and_update();
		}
		let snapshot = management.session_snapshot(&manager, &client).await;
		let changed = management.session.send_if_modified(|current| {
			let changed = *current != snapshot;
			if changed {
				*current = snapshot.clone();
			}
			changed
		});
		if changed {
			let _ = ManagementInterface::session_changed(emitter, &json(&snapshot)).await;
		}
		if manager.last_stop != reported_stop {
			reported_stop = manager.last_stop.clone();
			// A launch failure's cause was already reported by the launch.
			let reported = manager
				.last_stop
				.as_ref()
				.and_then(|stop| stop.cause.as_ref())
				.is_some_and(|cause| cause.reported);
			let failure = manager.last_stop.as_ref().and_then(|stop| stop.failure);
			if let Some(stop) = snapshot
				.last_stop
				.as_ref()
				.filter(|stop| (stop.unexpected && !reported) || !stop.completed)
			{
				let event = ServerEvent {
					level: if stop.completed { "warning" } else { "error" }.into(),
					code: stop.reason.clone(),
					message: match failure {
						None => stop.detail.clone().unwrap_or_else(|| stop.message.clone()),
						Some(failure) => failure.message().into(),
					},
					at_ms: stop.at_ms,
					hint: stop.hint.clone().filter(|_| stop.completed),
				};
				management.publish_event(emitter, event).await;
			}
		}

		// A closed producer is a stopped compositor. Wait for lifecycle teardown
		// instead of repeatedly waking on the closed channel.
		let wait_foreground = async {
			if let Some(receiver) = foreground.as_mut()
				&& receiver.changed().await.is_ok()
			{
				return;
			}
			std::future::pending::<()>().await;
		};
		let deadline = status::patience_deadline(&manager, &client, management.client_patience);
		let wait_patience = async {
			match deadline {
				Some(deadline) if deadline > Instant::now() => {
					tokio::time::sleep_until(deadline.into()).await;
				},
				_ => std::future::pending().await,
			}
		};
		tokio::select! {
			changed = status.changed() => if changed.is_err() { return },
			changed = presence.changed() => if changed.is_err() { return },
			() = wait_patience => {},
			() = wait_foreground => {},
		}
	}
}

/// Forward problems reported by the launch and negotiation paths.
async fn follow_problems(management: &Management, emitter: &zbus::object_server::SignalEmitter<'static>) {
	let mut problems = management.sessions.subscribe_problems();
	loop {
		match problems.recv().await {
			Ok(problem) => {
				let event = ServerEvent {
					level: "error".into(),
					code: problem.code.into(),
					message: problem.message,
					at_ms: unix_millis(problem.at),
					hint: problem.hint,
				};
				management.publish_event(emitter, event).await;
			},
			Err(broadcast::error::RecvError::Lagged(missed)) => {
				tracing::debug!(missed, "Management missed reported problems");
			},
			Err(broadcast::error::RecvError::Closed) => return,
		}
	}
}

/// Forward pairing and trust events as signals.
async fn follow_pairing(management: &Management, emitter: &zbus::object_server::SignalEmitter<'static>) {
	let mut events = management.clients.subscribe();
	loop {
		let (pairing_changed, clients_changed) = match events.recv().await {
			Ok(PairingEvent::Requested { client_id, approval }) => {
				if let Some(request) = management.pending(&client_id, &approval) {
					let _ = ManagementInterface::pairing_requested(emitter, &json(&request)).await;
				}
				(true, false)
			},
			Ok(PairingEvent::Approved { .. }) => (true, false),
			Ok(PairingEvent::Resolved {
				client_id,
				approval,
				outcome,
			}) => {
				let resolution = PairingResolution {
					client_id,
					request: approval,
					outcome,
				};
				let _ = ManagementInterface::pairing_resolved(emitter, &json(&resolution)).await;
				(true, outcome == PairingOutcome::Paired)
			},
			Ok(PairingEvent::TrustChanged) => (false, true),
			// Missed events: send complete snapshots instead.
			Err(broadcast::error::RecvError::Lagged(_)) => (true, true),
			Err(broadcast::error::RecvError::Closed) => return,
		};
		if pairing_changed {
			let _ = ManagementInterface::pairing_changed(emitter, &json(&management.pairing())).await;
		}
		if clients_changed && let Ok(clients) = management.clients_snapshot() {
			let _ = ManagementInterface::clients_changed(emitter, &json(&clients)).await;
		}
	}
}

/// Aggregate frame statistics and GPU readings while a client streams.
/// Every window is kept for support bundles; it is signalled while the
/// desktop UI is present.
async fn follow_stats(
	management: &Management,
	emitter: &zbus::object_server::SignalEmitter<'static>,
	ui: watch::Receiver<bool>,
) {
	let mut session = management.session.subscribe();
	let (active_tx, active) = watch::channel(false);
	loop {
		let streaming = {
			let snapshot = session.borrow_and_update();
			(snapshot.phase == SessionPhase::Streaming)
				.then(|| snapshot.session.as_ref().map(|session| session.epoch))
				.flatten()
		};
		active_tx.send_replace(streaming.is_some());
		if let Some(epoch) = streaming {
			let receiver = management.sessions.frame_stats_receiver();
			let (target, application) = {
				let snapshot = session.borrow();
				let details = snapshot.session.as_ref();
				let video = details.and_then(|session| session.video.as_ref());
				(
					findings::StreamTarget {
						fps: video.map_or(0, |video| video.fps),
						bitrate_bps: video.map_or(0, |video| video.bitrate_bps),
					},
					details.map_or_else(String::new, |session| session.application.title.clone()),
				)
			};
			let capture = management.capture_node();
			// Opening may load NVML; keep it off the async workers.
			let gpu = tokio::task::spawn_blocking(move || capture.as_deref().and_then(GpuSampler::open))
				.await
				.ok()
				.flatten()
				.map(|sampler| telemetry::GpuProbe::new(sampler, application));
			let aggregation = telemetry::aggregate(
				receiver,
				epoch,
				target,
				gpu,
				active.clone(),
				&management.stats,
				|stats| {
					management.record_stats(stats);
					if !*ui.borrow() {
						return;
					}
					let message = json(stats);
					let emitter = emitter.clone();
					tokio::spawn(async move {
						let _ = ManagementInterface::stats_updated(&emitter, &message).await;
					});
				},
			);
			tokio::pin!(aggregation);
			// Aggregate until the phase or epoch changes.
			loop {
				tokio::select! {
					() = &mut aggregation => break,
					changed = session.changed() => if changed.is_err() { return },
				}
				let snapshot = session.borrow().clone();
				let still = snapshot.phase == SessionPhase::Streaming
					&& snapshot.session.as_ref().map(|session| session.epoch) == Some(epoch);
				if !still {
					active_tx.send_replace(false);
					(&mut aggregation).await;
					break;
				}
			}
			continue;
		}
		management.stats.send_replace(None);
		if session.changed().await.is_err() {
			return;
		}
	}
}

impl Management {
	async fn session_snapshot(&self, manager: &ManagerStatus, client: &ClientPresence) -> SessionSnapshot {
		let phase = status::derive_phase(manager, client, Instant::now(), self.client_patience);
		let session = match phase {
			SessionPhase::Idle | SessionPhase::Stopping | SessionPhase::Error => None,
			_ => {
				let gpu = self.server.lock().unwrap().capabilities.gpu.clone();
				self.sessions
					.session_view()
					.await
					.map(|view| session_details(view, &gpu))
			},
		};
		SessionSnapshot {
			phase,
			session,
			last_stop: manager.last_stop.as_ref().map(|stop| {
				let (reason, message, unexpected) = status::describe_stop(stop.reason);
				LastStop {
					epoch: stop.epoch,
					reason: reason.into(),
					message: message.into(),
					unexpected: unexpected || stop.cause.as_ref().is_some_and(|cause| cause.unexpected),
					completed: stop.completed(),
					at_ms: unix_millis(stop.at),
					detail: stop.cause.as_ref().map(|cause| cause.detail.clone()),
					hint: stop.cause.as_ref().and_then(|cause| cause.hint.clone()),
				}
			}),
		}
	}

	fn pending(&self, client_id: &str, approval: &str) -> Option<PendingPairing> {
		self.clients
			.pending_approval(client_id)
			.filter(|pending| pending.approval == approval)
			.map(pending_pairing)
	}

	fn pairing(&self) -> PairingSnapshot {
		PairingSnapshot {
			enabled: self.pairing_enabled,
			requests: self
				.clients
				.pending_approvals()
				.into_iter()
				.map(pending_pairing)
				.collect(),
		}
	}

	fn clients_snapshot(&self) -> Result<ClientsSnapshot, ()> {
		let (clients, legacy_client_ids) = self.clients.paired_clients()?;
		Ok(ClientsSnapshot {
			clients: clients
				.into_iter()
				.map(|client| PairedClient {
					fingerprint: client.credential.fingerprint,
					client_ids: client.credential.client_ids,
					label: client.credential.label,
					paired_at_ms: client.credential.paired_at.map(|seconds| seconds.saturating_mul(1000)),
					last_seen_ms: client.last_seen.map(|(time, _)| unix_millis(time)),
					last_address: client.last_seen.map(|(_, address)| display_address(address)),
				})
				.collect(),
			legacy_client_ids,
		})
	}
}

fn display_address(address: IpAddr) -> String {
	address.to_canonical().to_string()
}

fn pending_pairing(pending: crate::clients::PendingApproval) -> PendingPairing {
	PendingPairing {
		client_id: pending.client_id,
		request: pending.approval,
		requester: display_address(pending.requester),
		fingerprint: pending.fingerprint,
		received_at_ms: unix_millis(pending.received_at),
		approval_expires_in_ms: pending
			.approval_deadline
			.saturating_duration_since(tokio::time::Instant::now())
			.as_millis() as u64,
		approved: pending.approved,
	}
}

fn session_details(view: SessionView, gpu: &str) -> SessionDetails {
	let (video, audio) = match view.streams {
		Some((video, audio)) => (Some(video), Some(audio)),
		None => (None, None),
	};
	SessionDetails {
		epoch: view.epoch,
		application: ApplicationSummary {
			id: view.application_id,
			title: view.application_title,
		},
		foreground_application: view.foreground_application,
		client_address: display_address(view.client_ip),
		started_at_ms: unix_millis(view.started_at),
		requested: RequestedMode {
			width: view.resolution.0,
			height: view.resolution.1,
			refresh_rate: view.refresh_rate,
			hdr: view.hdr,
			audio_channels: view.audio_channels as u8,
			audio_channel_mask: view.audio_channel_mask,
		},
		video: video.map(|video| {
			let format = video.format;
			VideoDetails {
				codec: match format.codec {
					VideoCodec::H264 => "h264",
					VideoCodec::Hevc => "hevc",
					VideoCodec::Av1 => "av1",
					VideoCodec::PyroWave => "pyrowave",
				}
				.into(),
				codec_label: format.codec.to_string(),
				chroma: format.chroma.to_string(),
				bit_depth: format.bit_depth.bits(),
				dynamic_range: if format.hdr { "HDR10" } else { "SDR" }.into(),
				transfer: match format.transfer {
					TransferFunction::Bt709 => "BT.709",
					TransferFunction::Pq => "PQ",
				}
				.into(),
				primaries: match format.primaries {
					ColorPrimaries::Bt709 => "BT.709",
					ColorPrimaries::Bt2020 => "BT.2020",
				}
				.into(),
				matrix: match format.matrix {
					MatrixCoefficients::Bt709 => "BT.709",
					MatrixCoefficients::Bt2020Ncl => "BT.2020 NCL",
				}
				.into(),
				range: match format.range {
					ColorRange::Limited => "limited",
					ColorRange::Full => "full",
				}
				.into(),
				width: video.width,
				height: video.height,
				fps: video.fps,
				bitrate_bps: video.bitrate as u64,
				packet_size: video.packet_size as u32,
				encrypted: video.encrypt_video,
				minimum_fec_packets: video.minimum_fec_packets,
				max_reference_frames: video.max_reference_frames,
				encoder: Some(match format.codec {
					VideoCodec::PyroWave => EncoderDetails {
						backend: "pyrowave".into(),
						backend_label: "PyroWave (Vulkan compute)".into(),
						gpu: gpu.into(),
					},
					_ => EncoderDetails {
						backend: "vulkan_video".into(),
						backend_label: "Vulkan Video".into(),
						gpu: gpu.into(),
					},
				}),
				pyrowave_dialect: video.pyrowave_dialect.map(|dialect| {
					match dialect {
						crate::session::stream::video::pyrowave_protocol::PyroWaveDialect::NativeWireV1 => {
							"native_wire_v1"
						},
						crate::session::stream::video::pyrowave_protocol::PyroWaveDialect::RecordFramed => {
							"record_framed"
						},
					}
					.into()
				}),
			}
		}),
		audio: audio.map(audio_details),
	}
}

fn audio_details(audio: AudioStreamContext) -> AudioDetails {
	AudioDetails {
		channels: audio.audio_config.channels as u8,
		channel_mask: audio.audio_config.channel_mask,
		high_quality: audio.audio_config.high_quality,
		opus_bitrate_bps: audio.opus_bitrate(),
		packet_duration_ms: audio.packet_duration_ms,
		encrypted: audio.encrypt_audio,
		quality: Some(
			match audio.audio_config.quality {
				AudioQuality::Standard => "standard",
				AudioQuality::High => "high",
				AudioQuality::Maximum => "maximum",
			}
			.into(),
		),
		quality_requested: audio.audio_config.quality_requested,
		opus_streams: audio.audio_config.stream_config.streams,
		opus_coupled_streams: audio.audio_config.stream_config.coupled_streams,
		sample_rate_hz: audio.sample_rate_hz(),
	}
}

fn codec_capabilities(supported: u32) -> Vec<CodecCapability> {
	healthcheck::CODEC_PROFILES
		.iter()
		.filter(|(bit, _, _)| supported & bit != 0)
		.map(|(_, id, label)| CodecCapability {
			id: (*id).into(),
			label: (*label).into(),
		})
		.collect()
}

/// The health report for the desktop app. Each check's message is split into
/// what it found (the first line) and what to do (the remaining lines, which
/// the command-line report prints indented below it).
pub(crate) fn health_report(report: &healthcheck::HealthReport, rechecked: bool) -> HealthReport {
	HealthReport {
		all_fatal_passed: report.all_fatal_passed,
		checks: report
			.checks
			.iter()
			.map(|check| {
				let mut lines = check.message.lines().map(str::trim).filter(|line| !line.is_empty());
				let message = lines.next().unwrap_or_default().to_string();
				let rest: Vec<_> = lines.collect();
				let hint = (check.outcome != CheckOutcome::Passed && !rest.is_empty()).then(|| rest.join("\n"));
				HealthCheck {
					name: check.name.into(),
					outcome: match check.outcome {
						CheckOutcome::Passed => "passed",
						CheckOutcome::Warning => "warning",
						CheckOutcome::Failed => "failed",
					}
					.into(),
					message: if hint.is_some() || rest.is_empty() {
						message
					} else {
						// A passed check keeps every line it reported.
						std::iter::once(message.as_str())
							.chain(rest)
							.collect::<Vec<_>>()
							.join("\n")
					},
					duration_ms: check.duration_ms,
					id: Some(check_id(check.name)),
					hint,
					fatal: check.outcome == CheckOutcome::Failed,
				}
			})
			.collect(),
		checked_at_ms: Some(unix_millis(SystemTime::now())),
		rechecked,
	}
}

/// A check's stable identifier from its name: `"Render access"` is
/// `render_access`, `"kcmp(2)"` is `kcmp`.
fn check_id(name: &str) -> String {
	let mut id = String::new();
	for word in name
		.split(|c: char| !c.is_ascii_alphabetic())
		.filter(|word| !word.is_empty())
	{
		if !id.is_empty() {
			id.push('_');
		}
		id.push_str(&word.to_ascii_lowercase());
	}
	id
}

fn server_info(facts: &ServerFacts, config: &Config) -> ServerInfo {
	ServerInfo {
		api_version: moonshine_management::API_VERSION,
		version: facts.version.clone(),
		name: config.name.clone(),
		pid: std::process::id(),
		started_at_ms: unix_millis(SystemTime::now()),
		config_path: facts.config_path.display().to_string(),
		capabilities: Capabilities {
			codecs: codec_capabilities(facts.supported_codecs),
			hdr_advertised: facts.hdr_advertised,
			dma_buf: facts.dma_buf,
			gpu: facts.gpu_name.clone(),
		},
		health: facts.health.as_ref().map(|report| health_report(report, false)),
		listeners: Listeners {
			address: config.address.clone(),
			http_port: config.webserver.port,
			https_port: config.webserver.port_https,
			rtsp_port: config.stream.port,
			video_port: config.stream.video.port,
			audio_port: config.stream.audio.port,
			control_port: config.stream.control.port,
		},
		pairing_enabled: config.webserver.enable_pairing,
	}
}

#[cfg(test)]
mod audio_details_tests {
	use super::*;
	use crate::session::stream::audio::{AudioChannels, AudioConfig};

	/// The dashboard reports the level, who chose it, the Opus layout and the
	/// bitrate the encoder actually uses.
	#[test]
	fn audio_details_report_quality_layout_and_encoded_bitrate() {
		let mut config =
			AudioConfig::from_channels(AudioChannels::Stereo, 0x3, false).with_quality(AudioQuality::Maximum);
		config.quality_requested = true;
		let stereo = audio_details(AudioStreamContext {
			packet_duration_ms: 5,
			qos: false,
			audio_config: config,
			encrypt_audio: true,
		});
		assert_eq!(stereo.quality.as_deref(), Some("maximum"));
		assert!(stereo.quality_requested);
		assert!(!stereo.high_quality);
		assert_eq!(stereo.opus_bitrate_bps, 512_000);
		assert_eq!((stereo.opus_streams, stereo.opus_coupled_streams), (1, 1));
		assert_eq!(stereo.sample_rate_hz, 48_000);
		assert!(stereo.encrypted);

		// High-quality 7.1 (2048 kbit/s) is capped by the 10 ms packet size.
		let surround = audio_details(AudioStreamContext {
			packet_duration_ms: 10,
			qos: false,
			audio_config: AudioConfig::from_channels(AudioChannels::Surround71, 0x63f, true),
			encrypt_audio: false,
		});
		assert_eq!(surround.quality.as_deref(), Some("standard"));
		assert!(!surround.quality_requested);
		assert!(surround.high_quality);
		assert_eq!(surround.opus_bitrate_bps, 1_088_000);
		assert_eq!((surround.opus_streams, surround.opus_coupled_streams), (8, 0));
	}
}

#[cfg(test)]
mod health_report_tests {
	use super::*;
	use crate::healthcheck::{CheckResult, HealthReport as CoreReport};

	fn report(checks: Vec<CheckResult>) -> CoreReport {
		CoreReport {
			all_fatal_passed: checks.iter().all(|check| check.outcome != CheckOutcome::Failed),
			checks,
			supported_codecs: 0,
			hdr_supported: false,
			dma_buf_supported: false,
			gpu_name: String::new(),
			duration: Duration::ZERO,
		}
	}

	fn check(name: &'static str, outcome: CheckOutcome, message: &str) -> CheckResult {
		CheckResult {
			name,
			outcome,
			message: message.into(),
			duration_ms: 1,
		}
	}

	/// The command line prints a check's remedy indented below its finding;
	/// the desktop app receives the two separately.
	#[test]
	fn failed_checks_separate_the_finding_from_the_remedy() {
		let dto = health_report(
			&report(vec![
				check(
					"Render access",
					CheckOutcome::Failed,
					"  Cannot open /dev/dri/renderD128: Permission denied\n  The node is owned by group 'render'.\n  \
					 Add your user to this group: `sudo usermod -aG render $USER`",
				),
				check("kcmp(2)", CheckOutcome::Passed, "Available"),
				check(
					"Sleep inhibit",
					CheckOutcome::Warning,
					"  logind returned no file descriptor.",
				),
			]),
			true,
		);
		assert!(!dto.all_fatal_passed && dto.rechecked && dto.checked_at_ms.is_some());
		let access = &dto.checks[0];
		assert_eq!(access.id.as_deref(), Some("render_access"));
		assert_eq!(access.message, "Cannot open /dev/dri/renderD128: Permission denied");
		assert_eq!(
			access.hint.as_deref(),
			Some("The node is owned by group 'render'.\nAdd your user to this group: `sudo usermod -aG render $USER`")
		);
		assert!(access.fatal);
		assert_eq!(dto.checks[1].id.as_deref(), Some("kcmp"));
		assert_eq!((dto.checks[1].hint.as_ref(), dto.checks[1].fatal), (None, false));
		assert_eq!(dto.checks[2].message, "logind returned no file descriptor.");
		assert_eq!(dto.checks[2].hint, None);
	}

	/// A re-run while a session streams never probes the GPU or binds the
	/// stream ports: it repeats the previous GPU results and reports the
	/// ports as the server's own.
	#[test]
	fn a_recheck_during_a_session_reuses_gpu_results_and_skips_port_binding() {
		let mut previous = report(vec![
			check("Vulkan", CheckOutcome::Passed, "Test GPU, Vulkan 1.4"),
			check("Codecs", CheckOutcome::Passed, "H.264 4:2:0 8-bit"),
			check("uinput", CheckOutcome::Warning, "  stale"),
		]);
		previous.supported_codecs = crate::healthcheck::CODEC_H264;
		let config = Config::default();
		let recheck = healthcheck::recheck_in_server(&config, Some(&previous), true);
		assert_eq!(recheck.supported_codecs, crate::healthcheck::CODEC_H264);
		let names: Vec<_> = recheck.checks.iter().map(|check| check.name).collect();
		assert_eq!(&names[..2], ["Vulkan", "Codecs"], "GPU results first, others re-run");
		assert!(recheck.checks[0].message.contains("do not run while a session streams"));
		let ports = recheck.checks.iter().find(|check| check.name == "Ports").unwrap();
		assert_eq!(ports.outcome, CheckOutcome::Passed, "{}", ports.message);
		assert!(ports.message.contains("in use by the session"), "{}", ports.message);
		// Without a previous report the GPU is not probed either.
		let recheck = healthcheck::recheck_in_server(&config, None, true);
		assert_eq!(recheck.checks[0].name, "GPU");
		assert_eq!(recheck.checks[0].outcome, CheckOutcome::Warning);
	}
}
