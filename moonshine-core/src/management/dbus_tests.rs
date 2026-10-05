//! End-to-end tests of the management interface over a private D-Bus
//! daemon, through the same client proxy the desktop UI uses.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use moonshine_management::dto::{
	ClientsSnapshot, ConfigDocument, ConfigSaved, PairingOutcome, PairingResolution, PairingSnapshot, PendingPairing,
	SaveOutcome, ServerInfo, SessionPhase, SessionSnapshot,
};
use moonshine_management::{ManagementError, ManagementProxy, UI_BUS_NAME};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::{Management, ServerFacts, config_store::ConfigStore, serve, server_info};
use crate::clients::{ClientManager, PendingClient, new_approval_token};
use crate::config::Config;
use crate::session::manager::SessionManager;

/// A `dbus-daemon` with a session-bus policy on a private socket.
struct PrivateBus {
	child: Child,
	address: String,
	_directory: tempfile::TempDir,
}

/// Absolute locations first: another test temporarily replaces `PATH`.
fn dbus_daemon() -> std::path::PathBuf {
	[
		"/usr/bin/dbus-daemon",
		"/bin/dbus-daemon",
		"/usr/local/bin/dbus-daemon",
		"/run/current-system/sw/bin/dbus-daemon",
	]
	.iter()
	.map(std::path::PathBuf::from)
	.find(|path| path.exists())
	.unwrap_or_else(|| "dbus-daemon".into())
}

impl PrivateBus {
	fn start() -> Self {
		let directory = tempfile::tempdir().unwrap();
		let config = directory.path().join("bus.conf");
		std::fs::write(
			&config,
			format!(
				r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
				directory.path().display()
			),
		)
		.unwrap();
		let mut child = Command::new(dbus_daemon())
			.arg(format!("--config-file={}", config.display()))
			.args(["--nofork", "--print-address=1"])
			.stdout(Stdio::piped())
			.spawn()
			.expect("dbus-daemon is required for the management interface tests");
		let mut address = String::new();
		BufReader::new(child.stdout.take().unwrap())
			.read_line(&mut address)
			.unwrap();
		Self {
			child,
			address: address.trim().to_string(),
			_directory: directory,
		}
	}

	async fn connect(&self) -> zbus::Connection {
		zbus::connection::Builder::address(self.address.as_str())
			.unwrap()
			.build()
			.await
			.unwrap()
	}
}

impl Drop for PrivateBus {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

const CONFIG: &str = "# Comment kept by saves.\nname = \"Test host\"\n";

struct Harness {
	bus: PrivateBus,
	clients: ClientManager,
	proxy: ManagementProxy<'static>,
	config_path: std::path::PathBuf,
	_directory: tempfile::TempDir,
	_server: tokio::task::JoinHandle<()>,
}

impl Harness {
	async fn start() -> Self {
		let bus = PrivateBus::start();
		let directory = tempfile::tempdir().unwrap();
		let config_path = directory.path().join("config.toml");
		std::fs::write(&config_path, CONFIG).unwrap();
		let shutdown = async_shutdown::ShutdownManager::new();
		let sessions = SessionManager::for_test(shutdown.clone());
		let clients = ClientManager::isolated(directory.path().join("state.toml"));
		clients.ensure_cleanup(shutdown);
		let config = Config::default();
		let facts = ServerFacts {
			version: "test".into(),
			config_path: config_path.clone(),
			supported_codecs: crate::healthcheck::CODEC_H264 | crate::healthcheck::CODEC_PYROWAVE,
			hdr_advertised: false,
			dma_buf: true,
			gpu_name: "Test GPU".into(),
			health: None,
		};
		let management = Arc::new(Management {
			server: server_info(&facts, &config),
			config: ConfigStore::open(config_path.clone()),
			pairing_enabled: true,
			client_patience: Duration::from_secs(60),
			session: watch::channel(SessionSnapshot {
				phase: SessionPhase::Idle,
				session: None,
				last_stop: None,
			})
			.0,
			stats: watch::channel(None).0,
			runtime: tokio::runtime::Handle::current(),
			sessions,
			clients: clients.clone(),
		});
		let builder = zbus::connection::Builder::address(bus.address.as_str());
		let server = tokio::spawn(serve(management, builder));
		let connection = bus.connect().await;
		let proxy = ManagementProxy::new(&connection).await.unwrap();
		// The service owns its name once it answers.
		let deadline = Instant::now() + Duration::from_secs(5);
		while proxy.get_server().await.is_err() {
			assert!(Instant::now() < deadline, "management service did not start");
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		Self {
			bus,
			clients,
			proxy,
			config_path,
			_directory: directory,
			_server: server,
		}
	}

	/// What `/pair?phrase=getservercert` registers for a Moonlight client.
	fn request_pairing(&self, client_id: &str) -> String {
		let approval = new_approval_token().unwrap();
		let certificate = rcgen::generate_simple_self_signed(vec!["client".into()])
			.unwrap()
			.cert
			.pem();
		self.clients
			.start_pairing(PendingClient {
				deadline: Instant::now() + Duration::from_secs(360),
				id: client_id.into(),
				pem: certificate,
				salt: [3; 16],
				pin_notify: Arc::new(Notify::new()),
				approval: approval.clone(),
				requester: "192.0.2.7".parse().unwrap(),
				received_at: SystemTime::now(),
				approval_deadline: Instant::now() + Duration::from_secs(300),
				label: None,
				key: None,
				server_secret: None,
				server_challenge: None,
				client_hash: None,
			})
			.unwrap();
		approval
	}
}

fn error_kind(error: zbus::Error) -> &'static str {
	ManagementError::from(error).kind()
}

async fn next<T: serde::de::DeserializeOwned>(stream: &mut (impl futures_util::Stream<Item = String> + Unpin)) -> T {
	let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
		.await
		.expect("signal arrives")
		.expect("signal stream open");
	serde_json::from_str(&message).unwrap()
}

#[tokio::test]
async fn server_session_and_capabilities_are_reported() {
	let harness = Harness::start().await;
	let server: ServerInfo = serde_json::from_str(&harness.proxy.get_server().await.unwrap()).unwrap();
	assert_eq!(server.api_version, moonshine_management::API_VERSION);
	assert_eq!(server.capabilities.gpu, "Test GPU");
	let codecs: Vec<_> = server
		.capabilities
		.codecs
		.iter()
		.map(|codec| codec.id.as_str())
		.collect();
	assert_eq!(codecs, ["h264", "pyrowave"]);

	let session: SessionSnapshot = serde_json::from_str(&harness.proxy.get_session().await.unwrap()).unwrap();
	assert_eq!(session.phase, SessionPhase::Idle);
	assert!(session.session.is_none());
	// Ending a session that does not exist is a no-op, not an error.
	harness.proxy.end_session().await.unwrap();
	assert_eq!(harness.proxy.get_stats().await.unwrap(), "null");
}

#[tokio::test]
async fn pairing_approval_is_bound_to_the_displayed_request() {
	let harness = Harness::start().await;
	let proxy = &harness.proxy;
	let mut requested = proxy
		.receive_pairing_requested()
		.await
		.unwrap()
		.map(|signal| signal.args().unwrap().request().clone());
	let mut resolved = proxy
		.receive_pairing_resolved()
		.await
		.unwrap()
		.map(|signal| signal.args().unwrap().resolution().clone());

	let shown = harness.request_pairing("moonlight");
	let request: PendingPairing = next(&mut requested).await;
	assert_eq!(request.request, shown);
	assert_eq!(request.requester, "192.0.2.7");
	assert_eq!(request.fingerprint.as_ref().map(String::len), Some(64));
	assert!(!request.approved);

	// The client replaces its request (same client ID) before the operator
	// submits: the PIN typed for the shown request must not approve it.
	let replacement = harness.request_pairing("moonlight");
	let replaced: PairingResolution = next(&mut resolved).await;
	assert_eq!(
		(replaced.request.as_str(), replaced.outcome),
		(shown.as_str(), PairingOutcome::Replaced)
	);
	let _: PendingPairing = next(&mut requested).await;
	let stale = proxy
		.approve_pairing("moonlight", &shown, "1234", "")
		.await
		.unwrap_err();
	assert_eq!(error_kind(stale), "conflict");
	let pending: PairingSnapshot = serde_json::from_str(&proxy.get_pairing().await.unwrap()).unwrap();
	assert!(!pending.requests[0].approved, "stale input approved nothing");

	let invalid = proxy
		.approve_pairing("moonlight", &replacement, "12a4", "")
		.await
		.unwrap_err();
	assert_eq!(error_kind(invalid), "invalid");
	proxy
		.approve_pairing("moonlight", &replacement, "4321", "Living room")
		.await
		.unwrap();
	let pending: PairingSnapshot = serde_json::from_str(&proxy.get_pairing().await.unwrap()).unwrap();
	assert!(pending.requests[0].approved);
	let again = proxy
		.approve_pairing("moonlight", &replacement, "4321", "")
		.await
		.unwrap_err();
	assert_eq!(error_kind(again), "conflict");

	// Rejection resolves exactly the named request.
	let other = harness.request_pairing("other-client");
	let _: PendingPairing = next(&mut requested).await;
	proxy.reject_pairing("other-client", &other).await.unwrap();
	let rejected: PairingResolution = next(&mut resolved).await;
	assert_eq!(
		(rejected.request.as_str(), rejected.outcome),
		(other.as_str(), PairingOutcome::Rejected)
	);
	let missing = proxy.reject_pairing("other-client", &other).await.unwrap_err();
	assert_eq!(error_kind(missing), "not_found");
}

#[tokio::test]
async fn paired_clients_are_listed_renamed_and_revoked_through_trust_state() {
	let harness = Harness::start().await;
	let proxy = &harness.proxy;
	let fingerprint = "ab".repeat(32);
	let state = harness.clients.persistent_state();
	state.pair("moonlight".into(), fingerprint.clone()).unwrap();
	state.pair("moonlight".into(), "cd".repeat(32)).unwrap();
	harness
		.clients
		.record_seen(&fingerprint, "::ffff:192.0.2.9".parse().unwrap());

	let clients: ClientsSnapshot = serde_json::from_str(&proxy.get_clients().await.unwrap()).unwrap();
	assert_eq!(clients.clients.len(), 2);
	let client = clients
		.clients
		.iter()
		.find(|client| client.fingerprint == fingerprint)
		.unwrap();
	assert_eq!(client.client_ids, ["moonlight"]);
	assert_eq!(client.last_address.as_deref(), Some("192.0.2.9"));
	assert!(client.paired_at_ms.is_some());

	let mut changed = proxy
		.receive_clients_changed()
		.await
		.unwrap()
		.map(|signal| signal.args().unwrap().snapshot().clone());
	proxy.rename_client(&fingerprint, "  Steam Deck ").await.unwrap();
	let renamed: ClientsSnapshot = next(&mut changed).await;
	let client = renamed
		.clients
		.iter()
		.find(|client| client.fingerprint == fingerprint)
		.unwrap();
	assert_eq!(client.label.as_deref(), Some("Steam Deck"));
	assert_eq!(
		error_kind(proxy.rename_client(&fingerprint, &"x".repeat(65)).await.unwrap_err()),
		"invalid"
	);

	// Revocation by fingerprint keeps the other certificate that shares the
	// (common) Moonlight client ID.
	proxy.revoke_client(&fingerprint.to_uppercase()).await.unwrap();
	let revoked: ClientsSnapshot = next(&mut changed).await;
	assert_eq!(revoked.clients.len(), 1);
	assert!(!harness.clients.is_cert_paired(&fingerprint).unwrap());
	assert!(harness.clients.is_cert_paired(&"cd".repeat(32)).unwrap());
	assert_eq!(
		error_kind(proxy.revoke_client(&fingerprint).await.unwrap_err()),
		"not_found"
	);
	assert_eq!(
		error_kind(proxy.revoke_client("not-a-fingerprint").await.unwrap_err()),
		"invalid"
	);
}

#[tokio::test]
async fn configuration_saves_are_validated_conflict_checked_and_announced() {
	let harness = Harness::start().await;
	let proxy = &harness.proxy;
	let document: ConfigDocument = serde_json::from_str(&proxy.get_config().await.unwrap()).unwrap();
	assert!(document.writable && !document.restart_required);
	assert_eq!(document.values["name"], "Test host");
	let schema: moonshine_management::dto::ConfigSchema =
		serde_json::from_str(&proxy.get_config_schema().await.unwrap()).unwrap();
	assert!(schema.fields.iter().any(|field| field.path == "stream.video.fec_mode"));

	let mut values = document.values.clone();
	values["stream"]["video"]["fec_mode"] = "auto".into();
	let report: moonshine_management::dto::ValidationReport =
		serde_json::from_str(&proxy.validate_config(&values.to_string()).await.unwrap()).unwrap();
	assert!(report.valid && report.restart_required);
	assert_eq!(report.changed_paths, ["stream.video.fec_mode"]);
	assert_eq!(
		std::fs::read_to_string(&harness.config_path).unwrap(),
		CONFIG,
		"validation writes nothing"
	);

	let mut invalid = values.clone();
	invalid["stream"]["video"]["port"] = invalid["stream"]["audio"]["port"].clone();
	let error = proxy
		.save_config(&invalid.to_string(), &document.revision)
		.await
		.unwrap_err();
	assert_eq!(error_kind(error), "invalid");

	let mut saved_signal = proxy
		.receive_config_saved()
		.await
		.unwrap()
		.map(|signal| signal.args().unwrap().saved().clone());
	let outcome: SaveOutcome = serde_json::from_str(
		&proxy
			.save_config(&values.to_string(), &document.revision)
			.await
			.unwrap(),
	)
	.unwrap();
	assert!(outcome.restart_required);
	let saved: ConfigSaved = next(&mut saved_signal).await;
	assert_eq!(saved.revision, outcome.revision);
	let text = std::fs::read_to_string(&harness.config_path).unwrap();
	assert!(
		text.starts_with("# Comment kept by saves.\nname = \"Test host\"\n"),
		"{text}"
	);
	assert!(text.contains("fec_mode = \"auto\""));

	// The first revision is stale now.
	let conflict = proxy
		.save_config(&values.to_string(), &document.revision)
		.await
		.unwrap_err();
	assert_eq!(error_kind(conflict), "conflict");
}

#[tokio::test]
async fn the_desktop_ui_name_suppresses_fallback_notifications() {
	let harness = Harness::start().await;
	assert!(!harness.clients.operator_ui_present());
	let ui = zbus::connection::Builder::address(harness.bus.address.as_str())
		.unwrap()
		.name(UI_BUS_NAME)
		.unwrap()
		.build()
		.await
		.unwrap();
	let deadline = Instant::now() + Duration::from_secs(5);
	while !harness.clients.operator_ui_present() {
		assert!(Instant::now() < deadline, "UI presence was not observed");
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	// A crashed or closed UI releases its name.
	drop(ui);
	let deadline = Instant::now() + Duration::from_secs(5);
	while harness.clients.operator_ui_present() {
		assert!(Instant::now() < deadline, "UI departure was not observed");
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
}
