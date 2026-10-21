//! The web management API over a real listener: authentication, browser
//! protections and the shared operations it exposes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::{Assets, Hosts, WebServer};
use crate::clients::{ClientManager, PendingClient, new_approval_token};
use crate::config::Config;
use crate::management::auth::Auth;
use crate::management::hub::EventKind;
use crate::management::{Management, ServerFacts};
use crate::session::manager::SessionManager;

const CONFIG: &str = "name = \"Test host\"\n# kept\naddress = \"0.0.0.0\"\n";

struct Server {
	address: SocketAddr,
	management: Arc<Management>,
	clients: ClientManager,
	shutdown: async_shutdown::ShutdownManager<crate::ShutdownReason>,
	_directory: tempfile::TempDir,
}

async fn start() -> Server {
	let directory = tempfile::tempdir().unwrap();
	let config_path = directory.path().join("config.toml");
	std::fs::write(&config_path, CONFIG).unwrap();
	let shutdown = async_shutdown::ShutdownManager::new();
	let sessions = SessionManager::for_test(shutdown.clone());
	let clients = ClientManager::isolated(directory.path().join("state.toml"));
	clients.ensure_cleanup(shutdown.clone());
	let mut config = Config::default();
	config.webserver.enable_pairing = true;
	let facts = ServerFacts {
		version: "test".into(),
		config_path,
		supported_codecs: crate::healthcheck::CODEC_H264,
		hdr_advertised: false,
		dma_buf: true,
		gpu_name: "Test GPU".into(),
		health: None,
	};
	let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	listener.set_nonblocking(true).unwrap();
	let address = listener.local_addr().unwrap();
	let management = Arc::new(Management::new(
		facts,
		&config,
		crate::app_scanner::Library::fixed(&config.applications),
		sessions,
		clients.clone(),
		Arc::new(Auth::open(directory.path().join("management.json"))),
		Some(format!("http://{address}/")),
		shutdown.clone(),
	));
	let server = WebServer {
		listener,
		tls: None,
		url: format!("http://{address}/"),
		assets: Assets::from_files(
			&[
				("index.html", "<!doctype html><title>Pyroshine</title>"),
				("assets/app.js", "export {}"),
			],
			Some(env!("CARGO_PKG_VERSION")),
		),
		hosts: Hosts::new(address.port(), address.ip(), &[]),
	};
	tokio::spawn({
		let management = management.clone();
		async move { crate::management::follow(&management).await }
	});
	tokio::spawn(server.serve(management.clone(), shutdown.clone()));
	Server {
		address,
		management,
		clients,
		shutdown,
		_directory: directory,
	}
}

struct Reply {
	status: u16,
	headers: Vec<(String, String)>,
	body: String,
}

impl Reply {
	fn header(&self, name: &str) -> Option<&str> {
		self.headers
			.iter()
			.find(|(key, _)| key.eq_ignore_ascii_case(name))
			.map(|(_, value)| value.as_str())
	}

	fn json(&self) -> Value {
		serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
	}

	fn kind(&self) -> String {
		self.json()["error"]["kind"].as_str().unwrap_or_default().to_string()
	}
}

/// A browser-like client: it keeps the cookie and sends same-origin headers.
struct Browser {
	address: SocketAddr,
	cookie: Option<String>,
	csrf: Option<String>,
}

impl Browser {
	fn new(server: &Server) -> Self {
		Self {
			address: server.address,
			cookie: None,
			csrf: None,
		}
	}

	fn host(&self) -> String {
		self.address.to_string()
	}

	async fn send(&mut self, method: &str, path: &str, extra: &[(&str, String)], body: Option<&str>) -> Reply {
		let mut request = format!(
			"{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
			self.host()
		);
		if let Some(cookie) = &self.cookie {
			request.push_str(&format!("Cookie: {cookie}\r\n"));
		}
		for (name, value) in extra {
			request.push_str(&format!("{name}: {value}\r\n"));
		}
		match body {
			Some(body) => request.push_str(&format!(
				"Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
				body.len()
			)),
			None => request.push_str("\r\n"),
		}
		let reply = raw(self.address, &request).await;
		if let Some(cookie) = reply.header("set-cookie") {
			let pair = cookie.split(';').next().unwrap().to_string();
			self.cookie = (!pair.ends_with('=')).then_some(pair);
		}
		if let Some(csrf) = serde_json::from_str::<Value>(&reply.body)
			.ok()
			.and_then(|body| body["csrf_token"].as_str().map(str::to_string))
		{
			self.csrf = Some(csrf);
		}
		reply
	}

	fn same_origin(&self) -> Vec<(&'static str, String)> {
		let mut headers = vec![
			("Origin", format!("http://{}", self.host())),
			("Sec-Fetch-Site", "same-origin".to_string()),
		];
		if let Some(csrf) = &self.csrf {
			headers.push(("X-CSRF-Token", csrf.clone()));
		}
		headers
	}

	async fn get(&mut self, path: &str) -> Reply {
		let headers = vec![("Sec-Fetch-Site", "same-origin".to_string())];
		self.send("GET", path, &headers, None).await
	}

	async fn post(&mut self, path: &str, body: Value) -> Reply {
		let headers = self.same_origin();
		self.send("POST", path, &headers, Some(&body.to_string())).await
	}

	async fn put(&mut self, path: &str, body: Value) -> Reply {
		let headers = self.same_origin();
		self.send("PUT", path, &headers, Some(&body.to_string())).await
	}

	async fn sign_in(&mut self, server: &Server) {
		let code = server.management.op_create_setup_code(false).unwrap();
		let reply = self
			.post(
				"/api/v1/auth/setup",
				json!({ "code": code, "password": "correct horse battery" }),
			)
			.await;
		assert_eq!(reply.status, 200, "{}", reply.body);
	}
}

async fn raw(address: SocketAddr, request: &str) -> Reply {
	let mut stream = TcpStream::connect(address).await.unwrap();
	stream.write_all(request.as_bytes()).await.unwrap();
	let mut response = Vec::new();
	tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
		.await
		.expect("response in time")
		.unwrap();
	let text = String::from_utf8_lossy(&response).to_string();
	let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
	let mut lines = head.lines();
	let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
	let headers = lines
		.filter_map(|line| line.split_once(':'))
		.map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
		.collect();
	Reply {
		status,
		headers,
		body: body.to_string(),
	}
}

#[tokio::test]
async fn the_api_requires_a_session_even_from_loopback() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	for path in [
		"/api/v1/overview",
		"/api/v1/server",
		"/api/v1/clients",
		"/api/v1/config",
		"/api/v1/diagnostics/report",
		"/api/v1/stream",
		"/api/v1/auth/sessions",
		"/api/v1/library/artwork?key=manual:0",
	] {
		let reply = browser.get(path).await;
		assert_eq!(
			(reply.status, reply.kind().as_str()),
			(401, "unauthenticated"),
			"{path}"
		);
	}
	for (path, body) in [
		("/api/v1/session/end", json!({})),
		("/api/v1/server/restart", json!({})),
		("/api/v1/clients/revoke", json!({ "fingerprint": "00".repeat(32) })),
		(
			"/api/v1/pairing/approve",
			json!({ "client_id": "a", "request": "b", "pin": "1234" }),
		),
		("/api/v1/diagnostics/bundle", json!({})),
	] {
		let reply = browser.post(path, body).await;
		assert_eq!(reply.status, 401, "{path}");
	}
	// A cookie that is not a live session is cleared and refused.
	browser.cookie = Some(format!("pyroshine_session={}", "ab".repeat(32)));
	let reply = browser.get("/api/v1/auth").await;
	assert_eq!(reply.json()["authenticated"], false);
	assert!(reply.header("set-cookie").unwrap().contains("Max-Age=0"));
}

#[tokio::test]
async fn first_run_setup_signs_in_with_a_hardened_cookie() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	let state = browser.get("/api/v1/auth").await.json();
	assert_eq!(
		(state["configured"].clone(), state["authenticated"].clone()),
		(json!(false), json!(false))
	);
	assert!(state["csrf_token"].is_null());

	let reply = browser
		.post(
			"/api/v1/auth/setup",
			json!({ "code": "0000-0000-0000-0000", "password": "correct horse battery" }),
		)
		.await;
	assert_eq!((reply.status, reply.kind().as_str()), (401, "invalid_credentials"));
	assert!(browser.cookie.is_none());

	let code = server.management.op_create_setup_code(false).unwrap();
	let reply = browser
		.post("/api/v1/auth/setup", json!({ "code": code, "password": "short" }))
		.await;
	assert_eq!(reply.status, 422);
	let reply = browser
		.post(
			"/api/v1/auth/setup",
			json!({ "code": code, "password": "correct horse battery" }),
		)
		.await;
	assert_eq!(reply.status, 200, "{}", reply.body);
	let cookie = reply.header("set-cookie").unwrap();
	for attribute in ["HttpOnly", "SameSite=Strict", "Path=/"] {
		assert!(cookie.contains(attribute), "{cookie}");
	}
	assert_eq!(reply.json()["authenticated"], true);
	assert!(browser.csrf.is_some());
	// The setup code is single use and the password cannot be replaced
	// without an explicit reset.
	assert!(server.management.op_create_setup_code(false).is_err());

	let overview = browser.get("/api/v1/overview").await;
	assert_eq!(overview.status, 200);
	let overview = overview.json();
	assert_eq!(overview["connected"], true);
	assert_eq!(overview["server"]["name"], "Pyroshine");
	assert_eq!(overview["session"]["phase"], "idle");
	assert_eq!(overview["web"]["assets_version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn changes_need_this_origin_and_the_session_csrf_token() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let rename = json!({ "fingerprint": "00".repeat(32), "label": "Laptop" }).to_string();

	// No token, a wrong token, or another site's origin.
	let host = browser.host();
	let attempts: Vec<Vec<(&str, String)>> = vec![
		vec![("Origin", format!("http://{host}"))],
		vec![("Origin", format!("http://{host}")), ("X-CSRF-Token", "0".repeat(64))],
		vec![
			("Origin", "http://attacker.example".into()),
			("X-CSRF-Token", browser.csrf.clone().unwrap()),
		],
		vec![
			("Sec-Fetch-Site", "cross-site".into()),
			("X-CSRF-Token", browser.csrf.clone().unwrap()),
		],
	];
	for headers in attempts {
		let reply = browser
			.send("POST", "/api/v1/clients/rename", &headers, Some(&rename))
			.await;
		assert_eq!(reply.status, 403, "{headers:?}: {}", reply.body);
	}
	// A form post cannot carry JSON.
	let mut headers = browser.same_origin();
	headers.push(("Content-Type", "application/x-www-form-urlencoded".into()));
	let form = format!(
		"POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: \
		 application/x-www-form-urlencoded\r\nContent-Length: 10\r\nConnection: close\r\n\r\npassword=x"
	);
	assert_eq!(raw(server.address, &form).await.status, 415);
	// The real page's request reaches the operation (no such client).
	let reply = browser
		.post(
			"/api/v1/clients/rename",
			json!({ "fingerprint": "00".repeat(32), "label": "Laptop" }),
		)
		.await;
	assert_eq!((reply.status, reply.kind().as_str()), (404, "not_found"));
}

#[tokio::test]
async fn desktop_controls_require_authentication_and_csrf_before_any_host_action() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	assert_eq!(browser.send("GET", "/api/v1/desktop", &[], None).await.status, 401);
	for path in ["/api/v1/desktop/authorize", "/api/v1/desktop/restore"] {
		assert_eq!(browser.send("POST", path, &[], Some("{}")).await.status, 401);
	}
	browser.sign_in(&server).await;
	for path in ["/api/v1/desktop/authorize", "/api/v1/desktop/restore"] {
		assert_eq!(browser.send("POST", path, &[], Some("{}")).await.status, 403);
	}
}

#[tokio::test]
async fn unknown_hosts_are_refused_to_defeat_dns_rebinding() {
	let server = start().await;
	let port = server.address.port();
	for host in [format!("attacker.example:{port}"), "127.0.0.1:1".into()] {
		let reply = raw(
			server.address,
			&format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
		)
		.await;
		assert_eq!(reply.status, 421, "{host}");
		let reply = raw(
			server.address,
			&format!("GET /api/v1/auth HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
		)
		.await;
		assert_eq!(reply.status, 421, "{host}");
	}
	let reply = raw(
		server.address,
		&format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"),
	)
	.await;
	assert_eq!(reply.status, 200);
	for header in [
		"content-security-policy",
		"x-content-type-options",
		"x-frame-options",
		"referrer-policy",
	] {
		assert!(reply.header(header).is_some(), "{header}");
	}
	assert!(
		reply
			.header("content-security-policy")
			.unwrap()
			.contains("frame-ancestors 'none'")
	);
	assert!(reply.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn bodies_are_bounded_and_routes_are_exact() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let huge = json!({ "password": "x".repeat(64 * 1024) });
	let reply = browser.post("/api/v1/auth/login", huge).await;
	assert_eq!(reply.status, 413);
	assert_eq!(browser.get("/api/v1/nothing").await.status, 404);
	assert_eq!(browser.get("/api/v2/overview").await.status, 404);
	assert_eq!(browser.get("/api/v1/session/end").await.status, 405);
	let reply = browser.get("/api/v1/overview").await;
	assert_eq!(reply.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn repeated_wrong_passwords_are_rate_limited() {
	let server = start().await;
	let mut owner = Browser::new(&server);
	owner.sign_in(&server).await;
	let mut guesser = Browser::new(&server);
	let mut statuses = Vec::new();
	for _ in 0..6 {
		let reply = guesser
			.post("/api/v1/auth/login", json!({ "password": "wrong password" }))
			.await;
		statuses.push(reply.status);
		if reply.status == 429 {
			assert!(reply.header("retry-after").is_some());
		}
	}
	assert_eq!(statuses, [401, 401, 401, 401, 401, 429]);
}

#[tokio::test]
async fn sign_out_and_password_changes_end_sessions() {
	let server = start().await;
	let mut first = Browser::new(&server);
	first.sign_in(&server).await;
	let mut second = Browser::new(&server);
	let reply = second
		.post("/api/v1/auth/login", json!({ "password": "correct horse battery" }))
		.await;
	assert_eq!(reply.status, 200);
	assert_ne!(first.cookie, second.cookie, "every sign-in gets a new session");
	let sessions = first.get("/api/v1/auth/sessions").await.json();
	assert_eq!(sessions.as_array().unwrap().len(), 2);
	assert!(
		!sessions
			.to_string()
			.contains(first.cookie.as_ref().unwrap().split('=').nth(1).unwrap())
	);

	let reply = first
		.post(
			"/api/v1/auth/password",
			json!({ "current": "correct horse battery", "new": "a new long password" }),
		)
		.await;
	assert_eq!(reply.status, 204, "{}", reply.body);
	assert_eq!(second.get("/api/v1/overview").await.status, 401, "other sessions end");
	assert_eq!(first.get("/api/v1/overview").await.status, 200);

	let reply = first.post("/api/v1/auth/logout", json!({})).await;
	assert_eq!(reply.status, 204);
	assert!(first.cookie.is_none(), "the cookie is cleared");
	first.cookie = second.cookie.clone();
	assert_eq!(first.get("/api/v1/overview").await.status, 401);
}

#[tokio::test]
async fn pairing_is_approved_for_exactly_the_shown_request() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let approval = new_approval_token().unwrap();
	server
		.clients
		.start_pairing(PendingClient {
			deadline: tokio::time::Instant::now() + Duration::from_secs(360),
			id: "0123456789ABCDEF".into(),
			pem: rcgen::generate_simple_self_signed(vec!["client".into()])
				.unwrap()
				.cert
				.pem(),
			salt: [3; 16],
			pin_notify: Arc::new(tokio::sync::Notify::new()),
			approval: approval.clone(),
			requester: "192.168.1.50".parse().unwrap(),
			received_at: SystemTime::now(),
			approval_deadline: tokio::time::Instant::now() + Duration::from_secs(300),
			label: None,
			key: None,
			server_secret: None,
			server_challenge: None,
			client_hash: None,
		})
		.unwrap();
	let pairing = browser.get("/api/v1/pairing").await.json();
	assert_eq!(pairing["requests"][0]["request"], approval.as_str());
	assert_eq!(pairing["requests"][0]["requester"], "192.168.1.50");

	let stale = browser
		.post(
			"/api/v1/pairing/approve",
			json!({ "client_id": "0123456789ABCDEF", "request": "0".repeat(32), "pin": "1234" }),
		)
		.await;
	assert_eq!((stale.status, stale.kind().as_str()), (409, "conflict"));
	let invalid = browser
		.post(
			"/api/v1/pairing/approve",
			json!({ "client_id": "0123456789ABCDEF", "request": approval, "pin": "12ab" }),
		)
		.await;
	assert_eq!(invalid.status, 422);
	let approved = browser
		.post(
			"/api/v1/pairing/approve",
			json!({ "client_id": "0123456789ABCDEF", "request": approval, "pin": "1234", "label": "Laptop" }),
		)
		.await;
	assert_eq!(approved.status, 204, "{}", approved.body);
	assert!(server.clients.pending_approval("0123456789ABCDEF").unwrap().approved);
	let again = browser
		.post(
			"/api/v1/pairing/approve",
			json!({ "client_id": "0123456789ABCDEF", "request": approval, "pin": "1234" }),
		)
		.await;
	assert_eq!(again.status, 409);
	let gone = browser
		.post(
			"/api/v1/pairing/reject",
			json!({ "client_id": "unknown", "request": approval }),
		)
		.await;
	assert_eq!(gone.status, 404);
}

#[tokio::test]
async fn configuration_saves_check_the_revision() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let loaded = browser.get("/api/v1/config").await.json();
	assert!(loaded["schema"]["sections"].is_array());
	let revision = loaded["document"]["revision"].as_str().unwrap().to_string();
	let mut values = loaded["document"]["values"].clone();
	values["name"] = json!("Renamed host");
	let report = browser
		.post("/api/v1/config/validate", json!({ "values": values }))
		.await
		.json();
	assert_eq!(report["valid"], true, "{report}");
	let stale = browser
		.put("/api/v1/config", json!({ "values": values, "revision": "stale" }))
		.await;
	assert_eq!((stale.status, stale.kind().as_str()), (409, "conflict"));
	let saved = browser
		.put("/api/v1/config", json!({ "values": values, "revision": revision }))
		.await;
	assert_eq!(saved.status, 200, "{}", saved.body);
	assert_eq!(saved.json()["restart_required"], true);
	let text = std::fs::read_to_string(server.management.op_server().config_path).unwrap();
	assert!(text.contains("Renamed host") && text.contains("# kept"), "{text}");
}

async fn read_until(reader: &mut BufReader<TcpStream>, seen: &mut String, needle: &str) {
	tokio::time::timeout(Duration::from_secs(5), async {
		while !seen.contains(needle) {
			let mut line = String::new();
			if reader.read_line(&mut line).await.unwrap() == 0 {
				break;
			}
			seen.push_str(&line);
		}
	})
	.await
	.unwrap_or_else(|_| panic!("no {needle} in {seen}"));
	assert!(seen.contains(needle), "stream ended before {needle}: {seen}");
}

#[tokio::test]
async fn the_event_stream_starts_with_state_and_ends_with_the_session() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let mut stream = TcpStream::connect(server.address).await.unwrap();
	let request = format!(
		"GET /api/v1/stream HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nAccept: text/event-stream\r\n\r\n",
		browser.host(),
		browser.cookie.as_ref().unwrap()
	);
	stream.write_all(request.as_bytes()).await.unwrap();
	let mut reader = BufReader::new(stream);
	let mut seen = String::new();
	read_until(&mut reader, &mut seen, "event: overview").await;
	assert!(seen.contains("text/event-stream"));
	assert!(
		server.management.hub.is_observed(),
		"an open page counts as an observer"
	);
	server
		.management
		.hub
		.publish(EventKind::Pairing, r#"{"enabled":true,"requests":[]}"#.into());
	read_until(&mut reader, &mut seen, "event: pairing").await;
	// Signing out elsewhere ends this stream.
	let mut other = Browser::new(&server);
	other.cookie = browser.cookie.clone();
	other.csrf = browser.csrf.clone();
	assert_eq!(other.post("/api/v1/auth/logout", json!({})).await.status, 204);
	read_until(&mut reader, &mut seen, "event: auth").await;
	let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
	while server.management.hub.is_observed() {
		assert!(
			tokio::time::Instant::now() < deadline,
			"the stream released its observer"
		);
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
}

#[tokio::test]
async fn the_support_bundle_downloads_as_an_attachment() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let headers = vec![("Origin", format!("http://{}", browser.host()))];
	let reply = browser
		.send("POST", "/api/v1/diagnostics/bundle", &headers, Some("{}"))
		.await;
	assert_eq!(reply.status, 403, "the bundle needs the CSRF token");
	let reply = browser.post("/api/v1/diagnostics/bundle", json!({})).await;
	assert_eq!(reply.status, 200);
	assert_eq!(reply.header("content-type"), Some("application/gzip"));
	let disposition = reply.header("content-disposition").unwrap();
	assert!(
		disposition.starts_with("attachment; filename=\"pyroshine-diag-"),
		"{disposition}"
	);
}

#[tokio::test]
async fn a_restart_is_a_protected_change_that_shuts_down_to_start_again() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	let headers = vec![("Origin", format!("http://{}", browser.host()))];
	let reply = browser
		.send("POST", "/api/v1/server/restart", &headers, Some("{}"))
		.await;
	assert_eq!(reply.status, 403, "a restart needs the CSRF token");
	assert!(!server.shutdown.is_shutdown_triggered());
	let reply = browser.post("/api/v1/server/restart", json!({})).await;
	assert_eq!(reply.status, 204, "{}", reply.body);
	let reason = tokio::time::timeout(Duration::from_secs(2), server.shutdown.wait_shutdown_triggered())
		.await
		.expect("the restart begins after the answer");
	assert_eq!(reason, crate::ShutdownReason::Restart);
}

#[tokio::test]
async fn artwork_of_unknown_applications_is_not_found() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	browser.sign_in(&server).await;
	for path in [
		"/api/v1/library/artwork?key=steam:404",
		"/api/v1/library/artwork?key=../../etc/passwd",
	] {
		assert_eq!(browser.get(path).await.status, 404, "{path}");
	}
	assert_eq!(browser.get("/api/v1/library/artwork").await.status, 422);
}

#[tokio::test]
async fn history_and_updates_are_served_to_the_signed_in_operator() {
	let server = start().await;
	let mut browser = Browser::new(&server);
	assert_eq!(browser.get("/api/v1/history").await.status, 401);
	assert_eq!(browser.get("/api/v1/updates").await.status, 401);
	browser.sign_in(&server).await;

	let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
	let history = loop {
		let history = browser.get("/api/v1/history?limit=5").await.json();
		if history["available"] == true || tokio::time::Instant::now() > deadline {
			break history;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	};
	assert_eq!(history["available"], true, "{history}");
	assert_eq!(history["total"], 0);
	assert_eq!(history["retention_days"], 90);
	assert_eq!(browser.get("/api/v1/history/12345").await.status, 404);
	assert_eq!(browser.get("/api/v1/history/not-a-number").await.status, 404);
	let cleared = browser
		.send("DELETE", "/api/v1/history", &browser.same_origin(), None)
		.await;
	assert_eq!(cleared.status, 204, "{}", cleared.body);

	let updates = browser.get("/api/v1/updates").await.json();
	assert_eq!(updates["automatic"], false, "checks are off until enabled");
	assert_eq!(updates["current_version"], env!("CARGO_PKG_VERSION"));
	let invalid = browser
		.put(
			"/api/v1/updates/settings",
			json!({ "automatic": true, "channel": "nightly" }),
		)
		.await;
	assert_eq!(invalid.status, 422);
	let set = browser
		.put(
			"/api/v1/updates/settings",
			json!({ "automatic": false, "channel": "prerelease" }),
		)
		.await;
	assert_eq!(set.json()["channel"], "prerelease");
	let dismiss = browser
		.post("/api/v1/updates/dismiss", json!({ "version": "../etc" }))
		.await;
	assert_eq!(dismiss.status, 422);
}
