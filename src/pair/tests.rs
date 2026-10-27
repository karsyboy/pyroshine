//! CLI acceptance over a private bus, without a GPU or a real client.

use std::io::{BufRead, BufReader, Cursor};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use moonshine_management::{INTERFACE, OBJECT_PATH};
use zbus::object_server::SignalEmitter;

use super::*;

struct PrivateBus {
	child: Child,
	address: String,
	_directory: tempfile::TempDir,
}

impl PrivateBus {
	fn start() -> Self {
		let directory = tempfile::tempdir().unwrap();
		let config = directory.path().join("bus.conf");
		std::fs::write(
			&config,
			format!(
				"<busconfig><type>session</type><listen>unix:dir={}</listen><auth>EXTERNAL</auth>\
				 <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/>\
				 <allow own=\"*\"/></policy></busconfig>",
				directory.path().display()
			),
		)
		.unwrap();
		let daemon = [
			"/usr/bin/dbus-daemon",
			"/bin/dbus-daemon",
			"/run/current-system/sw/bin/dbus-daemon",
		]
		.into_iter()
		.find(|path| std::path::Path::new(path).exists())
		.unwrap_or("dbus-daemon");
		let mut child = Command::new(daemon)
			.arg(format!("--config-file={}", config.display()))
			.args(["--nofork", "--print-address=1"])
			.stdout(Stdio::piped())
			.spawn()
			.expect("dbus-daemon is required for pairing tests");
		let mut address = String::new();
		BufReader::new(child.stdout.take().unwrap())
			.read_line(&mut address)
			.unwrap();
		Self {
			child,
			address: address.trim().into(),
			_directory: directory,
		}
	}

	async fn connect(&self) -> zbus::Connection {
		zbus::connection::Builder::address(self.address.as_str())
			.unwrap()
			.method_timeout(Duration::from_secs(2))
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

#[derive(Default)]
struct State {
	requests: Vec<PendingPairing>,
	approvals: Vec<(String, String, String, String)>,
	disabled: bool,
	replace_on_approval: bool,
	outcome: Option<PairingOutcome>,
	incompatible: bool,
}

struct FakeManagement(Arc<Mutex<State>>);

#[zbus::interface(name = "io.github.karsyboy.Pyroshine.Management1")]
impl FakeManagement {
	fn get_server(&self) -> String {
		serde_json::json!({
			"api_version": if self.0.lock().unwrap().incompatible { API_VERSION + 1 } else { API_VERSION },
			"version": "test", "name": "Test host", "pid": 1, "started_at_ms": 0,
			"config_path": "/unused/config.toml", "health": null, "pairing_enabled": true,
			"capabilities": { "codecs": [], "hdr_advertised": false, "dma_buf": true, "gpu": "Test GPU" },
			"listeners": { "address": "0.0.0.0", "http_port": 47989, "https_port": 47984,
				"rtsp_port": 48010, "video_port": 47998, "audio_port": 48000, "control_port": 47999 }
		})
		.to_string()
	}

	fn get_pairing(&self) -> String {
		let state = self.0.lock().unwrap();
		serde_json::to_string(&PairingSnapshot {
			enabled: !state.disabled,
			requests: state.requests.clone(),
		})
		.unwrap()
	}

	async fn approve_pairing(
		&self,
		client_id: &str,
		request: &str,
		pin: &str,
		label: &str,
		#[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
	) -> Result<(), ManagementError> {
		let outcome = {
			let mut state = self.0.lock().unwrap();
			state
				.approvals
				.push((client_id.into(), request.into(), pin.into(), label.into()));
			if state.replace_on_approval {
				return Err(ManagementError::Conflict(
					"The client sent a new pairing request.".into(),
				));
			}
			let pending = state
				.requests
				.iter()
				.find(|pending| pending.client_id == client_id)
				.unwrap();
			assert_eq!(pending.request, request, "CLI must echo the exact selected token");
			state.outcome.unwrap_or(PairingOutcome::Paired)
		};
		// An unrelated attempt sharing the client's ID must never count as
		// success. Send it before the real result, before the method reply.
		for (token, result) in [("unrelated-token", PairingOutcome::Paired), (request, outcome)] {
			let resolution = serde_json::to_string(&PairingResolution {
				client_id: client_id.into(),
				request: token.into(),
				outcome: result,
			})
			.unwrap();
			Self::pairing_resolved(&emitter, &resolution).await?;
		}
		Ok(())
	}

	#[zbus(signal)]
	async fn pairing_resolved(emitter: &SignalEmitter<'_>, resolution: &str) -> zbus::Result<()>;
}

struct Harness {
	bus: PrivateBus,
	state: Arc<Mutex<State>>,
	_server: zbus::Connection,
	client: zbus::Connection,
}

fn request(id: &str, token: &str) -> PendingPairing {
	PendingPairing {
		client_id: id.into(),
		request: token.into(),
		requester: "192.0.2.7".into(),
		fingerprint: Some(format!("certificate-{id}")),
		received_at_ms: 0,
		approval_expires_in_ms: 300_000,
		approved: false,
	}
}

impl Harness {
	async fn start(state: State) -> Self {
		assert_eq!(INTERFACE, "io.github.karsyboy.Pyroshine.Management1");
		let bus = PrivateBus::start();
		let state = Arc::new(Mutex::new(state));
		let server = zbus::connection::Builder::address(bus.address.as_str())
			.unwrap()
			.serve_at(OBJECT_PATH, FakeManagement(state.clone()))
			.unwrap()
			.name(BUS_NAME)
			.unwrap()
			.build()
			.await
			.unwrap();
		let client = bus.connect().await;
		Self {
			bus,
			state,
			_server: server,
			client,
		}
	}

	async fn pair(&self, input: &str) -> (Result<(), String>, String) {
		let proxy = connect(&self.client).await.unwrap();
		let mut output = Vec::new();
		let outcome = tokio::time::timeout(
			Duration::from_secs(5),
			pair(
				&proxy,
				"Living room",
				&mut Cursor::new(input),
				&mut output,
				Duration::from_millis(100),
			),
		)
		.await
		.expect("CLI must finish promptly");
		(outcome, String::from_utf8(output).unwrap())
	}
}

#[tokio::test]
async fn pin_submission_waits_for_exact_request_completion() {
	let harness = Harness::start(State {
		requests: vec![request("client", "secret-token")],
		..Default::default()
	})
	.await;
	let (result, output) = harness.pair("yes\n0123\n").await;
	assert_eq!(result, Ok(()));
	assert!(output.contains("Device paired."));
	assert!(output.contains("192.0.2.7"));
	assert!(output.contains("certificate-client"));
	assert!(!output.contains("0123") && !output.contains("secret-token"));
	assert_eq!(
		harness.state.lock().unwrap().approvals,
		vec![(
			"client".into(),
			"secret-token".into(),
			"0123".into(),
			"Living room".into()
		)]
	);
}

#[tokio::test]
async fn multiple_requests_require_selection_and_keep_the_selected_token() {
	let harness = Harness::start(State {
		requests: vec![request("first", "token-1"), request("second", "token-2")],
		..Default::default()
	})
	.await;
	let (result, output) = harness.pair("0\n3\n2\nyes\n1234\n").await;
	assert_eq!(result, Ok(()));
	assert!(output.contains("Choose a number from 1 to 2."));
	assert_eq!(harness.state.lock().unwrap().approvals[0].0, "second");
}

#[tokio::test]
async fn a_client_can_start_pairing_after_the_command_is_waiting() {
	let harness = Harness::start(State::default()).await;
	let state = harness.state.clone();
	let client = tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(20)).await;
		state.lock().unwrap().requests.push(request("client", "new-token"));
	});
	let proxy = connect(&harness.client).await.unwrap();
	let mut output = Vec::new();
	let result = tokio::time::timeout(
		Duration::from_secs(3),
		pair(
			&proxy,
			"",
			&mut Cursor::new("yes\n1234\n"),
			&mut output,
			Duration::from_secs(2),
		),
	)
	.await
	.unwrap();
	client.await.unwrap();
	assert_eq!(result, Ok(()));
	assert_eq!(harness.state.lock().unwrap().approvals[0].1, "new-token");
}

#[tokio::test]
async fn declined_confirmation_invalid_pin_and_eof_never_submit() {
	let harness = Harness::start(State {
		requests: vec![request("client", "token")],
		..Default::default()
	})
	.await;
	for input in ["no\n", "yes\n123\n", "yes\n１２３４\n", "yes\n", ""] {
		let (result, output) = harness.pair(input).await;
		assert!(result.is_err());
		assert!(!output.contains("Device paired."));
	}
	assert!(harness.state.lock().unwrap().approvals.is_empty());
}

#[tokio::test]
async fn wrong_pin_and_replacement_do_not_report_success() {
	let harness = Harness::start(State {
		requests: vec![request("client", "token")],
		outcome: Some(PairingOutcome::Failed),
		..Default::default()
	})
	.await;
	let (result, output) = harness.pair("yes\n1234\n").await;
	assert!(result.unwrap_err().contains("could not complete pairing"));
	assert!(!output.contains("Device paired."));
	harness.state.lock().unwrap().replace_on_approval = true;
	let (result, output) = harness.pair("yes\n1234\n").await;
	assert!(result.unwrap_err().contains("new pairing request"));
	assert!(!output.contains("Device paired."));
}

#[tokio::test]
async fn missing_disabled_and_already_approved_requests_do_not_submit() {
	let mut approved = request("client", "token");
	approved.approved = true;
	let harness = Harness::start(State {
		requests: vec![approved],
		..Default::default()
	})
	.await;
	let (result, _) = harness.pair("").await;
	assert!(result.unwrap_err().contains("No pairing request received"));
	harness.state.lock().unwrap().disabled = true;
	let (result, _) = harness.pair("").await;
	assert!(result.unwrap_err().contains("disabled"));
	harness.state.lock().unwrap().disabled = false;
	harness.state.lock().unwrap().incompatible = true;
	let (result, _) = harness.pair("").await;
	assert!(result.unwrap_err().contains("Incompatible management API"));
	assert!(harness.state.lock().unwrap().approvals.is_empty());
}

#[tokio::test]
async fn absent_server_has_an_actionable_error() {
	let bus = PrivateBus::start();
	let connection = bus.connect().await;
	let error = connect(&connection).await.err().unwrap();
	assert!(error.contains("Start the server"));
	assert!(error.contains("without sudo"));
}

#[tokio::test]
async fn a_test_instance_bus_is_used_instead_of_the_session_bus() {
	let harness = Harness::start(State::default()).await;
	// A live service may own the management name on the session bus; the
	// command must bind to the test instance's server, not merely to any owner.
	let connection = open(Some(&harness.bus.address)).await.unwrap();
	let proxy = connect(&connection).await.unwrap();
	assert_eq!(
		proxy.inner().destination().as_str(),
		harness._server.unique_name().unwrap().as_str()
	);
}

#[tokio::test]
async fn an_unreachable_test_instance_bus_does_not_fall_back() {
	let directory = tempfile::tempdir().unwrap();
	let address = format!("unix:path={}", directory.path().join("missing").display());
	let error = open(Some(&address)).await.err().unwrap();
	assert!(error.starts_with("Cannot reach your session bus"));
}

#[tokio::test]
async fn proxy_does_not_switch_to_a_restarted_server() {
	let harness = Harness::start(State {
		requests: vec![request("client", "token")],
		..Default::default()
	})
	.await;
	let proxy = connect(&harness.client).await.unwrap();
	let replacement = harness.bus.connect().await;
	harness._server.release_name(BUS_NAME).await.unwrap();
	// The original connection stays alive but its management object disappears.
	harness
		._server
		.object_server()
		.remove::<FakeManagement, _>(OBJECT_PATH)
		.await
		.unwrap();
	replacement
		.object_server()
		.at(OBJECT_PATH, FakeManagement(harness.state.clone()))
		.await
		.unwrap();
	replacement.request_name(BUS_NAME).await.unwrap();
	let mut output = Vec::new();
	let result = pair(
		&proxy,
		"",
		&mut Cursor::new("yes\n1234\n"),
		&mut output,
		Duration::from_millis(100),
	)
	.await;
	assert!(result.is_err());
	assert!(harness.state.lock().unwrap().approvals.is_empty());
}

#[test]
fn only_completed_pairing_reports_success() {
	for outcome in [
		PairingOutcome::Failed,
		PairingOutcome::Expired,
		PairingOutcome::Replaced,
		PairingOutcome::Cancelled,
		PairingOutcome::Rejected,
		PairingOutcome::Cleared,
	] {
		assert!(pairing_outcome(outcome).is_err());
	}
	assert!(pairing_outcome(PairingOutcome::Paired).is_ok());
}
