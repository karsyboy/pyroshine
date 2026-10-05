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
//! through a bounded `broadcast` channel; frame statistics only while the
//! desktop UI is present and a client is streaming (see `telemetry`).
//! Signals are emitted from management tasks; a slow or crashed UI cannot
//! block any of them, and the daemon runs unchanged when the session bus name
//! cannot be acquired.

mod config_store;
mod dbus;
#[cfg(test)]
mod dbus_tests;
pub(crate) mod schema;
pub(crate) mod telemetry;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_shutdown::ShutdownManager;
use futures_util::StreamExt;
use moonshine_management::dto::{
	ApplicationSummary, AudioDetails, Capabilities, ClientsSnapshot, CodecCapability, HealthCheck, HealthReport,
	LastStop, Listeners, PairedClient, PairingOutcome, PairingResolution, PairingSnapshot, PendingPairing,
	RequestedMode, ServerEvent, ServerInfo, SessionDetails, SessionPhase, SessionSnapshot, StreamStats, VideoDetails,
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
use crate::healthcheck::{self, CheckOutcome};
use crate::session::manager::{SessionManager, SessionView};
use crate::session::status::{self, ClientPresence, ManagerStatus};
use crate::session::stream::video::{ColorPrimaries, ColorRange, MatrixCoefficients, TransferFunction, VideoCodec};

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

/// Shared state behind the D-Bus interface and the event tasks.
pub(crate) struct Management {
	sessions: SessionManager,
	clients: ClientManager,
	config: ConfigStore,
	server: ServerInfo,
	pairing_enabled: bool,
	/// How long a session waits for its client before it is reported as
	/// disconnected (the configured stream timeout).
	client_patience: Duration,
	session: watch::Sender<SessionSnapshot>,
	stats: watch::Sender<Option<StreamStats>>,
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
		let management = Arc::new(Management {
			server: server_info(&facts, config),
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
			runtime: tokio::runtime::Handle::current(),
			sessions,
			clients,
		});
		let task = tokio::spawn(async move {
			let _ = shutdown
				.wrap_cancel(serve(management, zbus::connection::Builder::session()))
				.await;
		});
		Self { tasks: vec![task] }
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
			if let Some(stop) = snapshot
				.last_stop
				.as_ref()
				.filter(|stop| stop.unexpected || !stop.completed)
			{
				let event = ServerEvent {
					level: if stop.completed { "warning" } else { "error" }.into(),
					code: stop.reason.clone(),
					message: if stop.completed {
						stop.message.clone()
					} else {
						"The session did not stop within its deadline; Pyroshine is restarting.".into()
					},
					at_ms: stop.at_ms,
				};
				let _ = ManagementInterface::server_event(emitter, &json(&event)).await;
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

/// Aggregate frame statistics while the UI is present and a client streams.
async fn follow_stats(
	management: &Management,
	emitter: &zbus::object_server::SignalEmitter<'static>,
	ui: watch::Receiver<bool>,
) {
	let mut session = management.session.subscribe();
	let mut ui_changes = ui.clone();
	let (active_tx, active) = watch::channel(false);
	loop {
		let streaming = {
			let snapshot = session.borrow_and_update();
			(snapshot.phase == SessionPhase::Streaming)
				.then(|| snapshot.session.as_ref().map(|session| session.epoch))
				.flatten()
		};
		let wanted = streaming.filter(|_| *ui_changes.borrow_and_update());
		active_tx.send_replace(wanted.is_some());
		if let Some(epoch) = wanted {
			let receiver = management.sessions.frame_stats_receiver();
			let aggregation = telemetry::aggregate(receiver, epoch, active.clone(), &management.stats, |stats| {
				let message = json(stats);
				let emitter = emitter.clone();
				tokio::spawn(async move {
					let _ = ManagementInterface::stats_updated(&emitter, &message).await;
				});
			});
			tokio::pin!(aggregation);
			// Aggregate until the phase, epoch or UI presence changes.
			loop {
				tokio::select! {
					() = &mut aggregation => break,
					changed = session.changed() => if changed.is_err() { return },
					changed = ui_changes.changed() => if changed.is_err() { return },
				}
				let snapshot = session.borrow().clone();
				let still = snapshot.phase == SessionPhase::Streaming
					&& snapshot.session.as_ref().map(|session| session.epoch) == Some(epoch)
					&& *ui_changes.borrow();
				if !still {
					active_tx.send_replace(false);
					(&mut aggregation).await;
					break;
				}
			}
			continue;
		}
		management.stats.send_replace(None);
		tokio::select! {
			changed = session.changed() => if changed.is_err() { return },
			changed = ui_changes.changed() => if changed.is_err() { return },
		}
	}
}

impl Management {
	async fn session_snapshot(&self, manager: &ManagerStatus, client: &ClientPresence) -> SessionSnapshot {
		let phase = status::derive_phase(manager, client, Instant::now(), self.client_patience);
		let session = match phase {
			SessionPhase::Idle | SessionPhase::Stopping | SessionPhase::Error => None,
			_ => self.sessions.session_view().await.map(session_details),
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
					unexpected,
					completed: stop.completed,
					at_ms: unix_millis(stop.at),
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

fn session_details(view: SessionView) -> SessionDetails {
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
		audio: audio.map(|audio| AudioDetails {
			channels: audio.audio_config.channels as u8,
			channel_mask: audio.audio_config.channel_mask,
			high_quality: audio.audio_config.high_quality,
			opus_bitrate_bps: audio.audio_config.stream_config.bitrate,
			packet_duration_ms: audio.packet_duration_ms,
			encrypted: audio.encrypt_audio,
		}),
	}
}

/// Codec profiles in advertisement order, with their capability bits.
const CODECS: &[(u32, &str, &str)] = &[
	(healthcheck::CODEC_H264, "h264", "H.264"),
	(healthcheck::CODEC_H264_HIGH_8444, "h264_444", "H.264 4:4:4"),
	(healthcheck::CODEC_HEVC, "hevc", "HEVC"),
	(healthcheck::CODEC_HEVC_MAIN10, "hevc_main10", "HEVC Main10"),
	(healthcheck::CODEC_HEVC_REXT_8444, "hevc_444", "HEVC 4:4:4"),
	(healthcheck::CODEC_HEVC_REXT_10444, "hevc_444_10", "HEVC 4:4:4 10-bit"),
	(healthcheck::CODEC_AV1_MAIN8, "av1", "AV1"),
	(healthcheck::CODEC_AV1_MAIN10, "av1_main10", "AV1 10-bit"),
	(healthcheck::CODEC_AV1_HIGH_8444, "av1_444", "AV1 4:4:4"),
	(healthcheck::CODEC_AV1_HIGH_10444, "av1_444_10", "AV1 4:4:4 10-bit"),
	(healthcheck::CODEC_PYROWAVE, "pyrowave", "PyroWave"),
	(healthcheck::CODEC_PYROWAVE_444, "pyrowave_444", "PyroWave 4:4:4"),
	(healthcheck::CODEC_PYROWAVE_HDR, "pyrowave_hdr", "PyroWave HDR10"),
];

fn server_info(facts: &ServerFacts, config: &Config) -> ServerInfo {
	ServerInfo {
		api_version: moonshine_management::API_VERSION,
		version: facts.version.clone(),
		name: config.name.clone(),
		pid: std::process::id(),
		started_at_ms: unix_millis(SystemTime::now()),
		config_path: facts.config_path.display().to_string(),
		capabilities: Capabilities {
			codecs: CODECS
				.iter()
				.filter(|(bit, _, _)| facts.supported_codecs & bit != 0)
				.map(|(_, id, label)| CodecCapability {
					id: (*id).into(),
					label: (*label).into(),
				})
				.collect(),
			hdr_advertised: facts.hdr_advertised,
			dma_buf: facts.dma_buf,
			gpu: facts.gpu_name.clone(),
		},
		health: facts.health.as_ref().map(|report| HealthReport {
			all_fatal_passed: report.all_fatal_passed,
			checks: report
				.checks
				.iter()
				.map(|check| HealthCheck {
					name: check.name.into(),
					outcome: match check.outcome {
						CheckOutcome::Passed => "passed",
						CheckOutcome::Warning => "warning",
						CheckOutcome::Failed => "failed",
					}
					.into(),
					message: check.message.clone(),
					duration_ms: check.duration_ms,
				})
				.collect(),
		}),
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
