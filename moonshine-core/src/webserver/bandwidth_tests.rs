//! Real loopback HTTPS requests through the normal TLS and Webserver handlers.
use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(
	server: Webserver,
	acceptor: Option<TlsAcceptor>,
	tls: Option<rustls::ClientConfig>,
	path: &str,
) -> Vec<u8> {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let task = tokio::spawn(async move {
		let (socket, peer) = listener.accept().await.unwrap();
		if let Some(acceptor) = acceptor {
			let connection = acceptor.accept(socket).await.unwrap();
			let fingerprint = connection
				.get_ref()
				.1
				.peer_certificates()
				.and_then(|c| c.first())
				.map(|c| hex::encode(Sha256::digest(c.as_ref())));
			hyper::server::conn::http1::Builder::new()
				.serve_connection(
					TokioIo::new(connection),
					service_fn(move |req| {
						let server = server.clone();
						let fingerprint = fingerprint.clone();
						async move { server.serve(req, Some(address), peer, None, true, fingerprint).await }
					}),
				)
				.await
				.unwrap();
		} else {
			hyper::server::conn::http1::Builder::new()
				.serve_connection(
					TokioIo::new(socket),
					service_fn(move |req| {
						let server = server.clone();
						async move { server.serve(req, Some(address), peer, None, false, None).await }
					}),
				)
				.await
				.unwrap();
		}
	});
	let socket = tokio::net::TcpStream::connect(address).await.unwrap();
	let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
	let mut response = Vec::new();
	if let Some(config) = tls {
		let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
			.connect(ServerName::try_from("localhost").unwrap(), socket)
			.await
			.unwrap();
		stream.write_all(request.as_bytes()).await.unwrap();
		stream.read_to_end(&mut response).await.unwrap();
	} else {
		let mut stream = socket;
		stream.write_all(request.as_bytes()).await.unwrap();
		stream.read_to_end(&mut response).await.unwrap();
	}
	task.await.unwrap();
	response
}

#[tokio::test]
async fn authenticated_probe_discovery_exact_payload_and_admission() {
	let directory = tempfile::tempdir().unwrap();
	let host = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
	let host_cert = directory.path().join("host.pem");
	let host_key = directory.path().join("host.key");
	std::fs::write(&host_cert, host.cert.pem()).unwrap();
	std::fs::write(&host_key, host.signing_key.serialize_pem()).unwrap();
	let (client_cert, client_key) = crate::tls::create_certificate().unwrap();
	let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut client_cert.as_bytes())
		.collect::<Result<_, _>>()
		.unwrap();
	let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut client_key.as_bytes())
		.unwrap()
		.unwrap();
	let mut roots = rustls::RootCertStore::empty();
	roots.add(host.cert.der().clone()).unwrap();
	let paired = rustls::ClientConfig::builder()
		.with_root_certificates(roots.clone())
		.with_client_auth_cert(certs.clone(), key)
		.unwrap();
	let anonymous = rustls::ClientConfig::builder()
		.with_root_certificates(roots)
		.with_no_client_auth();
	let shutdown = ShutdownManager::new();
	let session_manager = SessionManager::new(
		Default::default(),
		Default::default(),
		Default::default(),
		Default::default(),
		"127.0.0.1".into(),
		30,
		false,
		shutdown.clone(),
	)
	.unwrap();
	let client_manager = ClientManager::isolated(directory.path().join("state.toml"));
	client_manager
		.persistent_state()
		.add_paired_cert(hex::encode(Sha256::digest(certs[0].as_ref())))
		.unwrap();
	let server = Webserver {
		probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
		name: "ProbeTest".into(),
		rtsp_port: 1,
		webserver_config: WebserverConfig {
			certificate: host_cert.clone(),
			private_key: host_key.clone(),
			..Default::default()
		},
		library: crate::app_scanner::Library::fixed(&[]),
		unique_id: "test".into(),
		client_manager,
		session_manager,
		server_certs: host.cert.pem(),
		supported_codecs: crate::healthcheck::CODEC_PYROWAVE,
		hdr_supported: false,
		shutdown,
		limits: WebLimits::default(),
	};
	let acceptor = || TlsAcceptor::from_config(&host_cert, &host_key).unwrap();
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(paired.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	let offset = response.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
	assert!(response.starts_with(b"HTTP/1.1 200"));
	assert_eq!(response.len() - offset, bandwidth::PROBE_BYTES);
	assert!(response[offset..].iter().all(|b| *b == 0x50));
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(anonymous.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	assert!(response.starts_with(b"HTTP/1.1 401"));
	let response = request(server.clone(), None, None, "/pyrowave-bandwidth-probe").await;
	assert!(response.starts_with(b"HTTP/1.1 404"));
	let response = request(server.clone(), Some(acceptor()), Some(paired.clone()), "/serverinfo").await;
	assert!(
		String::from_utf8_lossy(&response)
			.contains("<PyroWaveBandwidthProbeBytes>33554432</PyroWaveBandwidthProbeBytes>")
	);
	let response = request(server.clone(), Some(acceptor()), Some(anonymous), "/serverinfo").await;
	assert!(!String::from_utf8_lossy(&response).contains("PyroWaveBandwidthProbeBytes"));
	let permit = server.probe_slots.clone().try_acquire_owned().unwrap();
	let response = request(
		server.clone(),
		Some(acceptor()),
		Some(paired.clone()),
		"/pyrowave-bandwidth-probe",
	)
	.await;
	assert!(response.starts_with(b"HTTP/1.1 429"));
	drop(permit);
	let mut unsupported = server;
	unsupported.supported_codecs = crate::healthcheck::CODEC_H264;
	let response = request(unsupported, Some(acceptor()), Some(paired), "/pyrowave-bandwidth-probe").await;
	assert!(response.starts_with(b"HTTP/1.1 404"));
}

struct Fixture {
	_directory: tempfile::TempDir,
	server: Webserver,
	paired: rustls::ClientConfig,
	anonymous: rustls::ClientConfig,
	host_cert: std::path::PathBuf,
	host_key: std::path::PathBuf,
}

impl Fixture {
	fn new(supported_codecs: u32, video_port: u16) -> Self {
		let directory = tempfile::tempdir().unwrap();
		let host = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
		let host_cert = directory.path().join("host.pem");
		let host_key = directory.path().join("host.key");
		std::fs::write(&host_cert, host.cert.pem()).unwrap();
		std::fs::write(&host_key, host.signing_key.serialize_pem()).unwrap();
		let (client_cert, client_key) = crate::tls::create_certificate().unwrap();
		let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut client_cert.as_bytes())
			.collect::<Result<_, _>>()
			.unwrap();
		let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut client_key.as_bytes())
			.unwrap()
			.unwrap();
		let mut roots = rustls::RootCertStore::empty();
		roots.add(host.cert.der().clone()).unwrap();
		let paired = rustls::ClientConfig::builder()
			.with_root_certificates(roots.clone())
			.with_client_auth_cert(certs.clone(), key)
			.unwrap();
		let anonymous = rustls::ClientConfig::builder()
			.with_root_certificates(roots)
			.with_no_client_auth();
		let shutdown = ShutdownManager::new();
		let session_manager = SessionManager::new(
			Default::default(),
			crate::session::stream::video::VideoStreamConfig {
				port: video_port,
				..Default::default()
			},
			Default::default(),
			Default::default(),
			"127.0.0.1".into(),
			30,
			false,
			shutdown.clone(),
		)
		.unwrap();
		let client_manager = ClientManager::isolated(directory.path().join("state.toml"));
		client_manager
			.persistent_state()
			.add_paired_cert(hex::encode(Sha256::digest(certs[0].as_ref())))
			.unwrap();
		let server = Webserver {
			probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
			name: "ProbeTest".into(),
			rtsp_port: 1,
			webserver_config: WebserverConfig {
				certificate: host_cert.clone(),
				private_key: host_key.clone(),
				..Default::default()
			},
			library: crate::app_scanner::Library::fixed(&[]),
			unique_id: "test".into(),
			client_manager,
			session_manager,
			server_certs: host.cert.pem(),
			supported_codecs,
			hdr_supported: false,
			shutdown,
			limits: WebLimits::default(),
		};
		Self {
			_directory: directory,
			server,
			paired,
			anonymous,
			host_cert,
			host_key,
		}
	}

	async fn get(&self, paired: bool, path: &str) -> String {
		let acceptor = TlsAcceptor::from_config(&self.host_cert, &self.host_key).unwrap();
		let tls = if paired { &self.paired } else { &self.anonymous };
		String::from_utf8_lossy(&request(self.server.clone(), Some(acceptor), Some(tls.clone()), path).await)
			.into_owned()
	}
}

/// Protocol compatibility of the UDP calibration: discovery only for paired
/// HTTPS peers and for every codec set, version negotiation, the HTTPS probe
/// left unchanged, and a complete run through the real handlers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_probe_negotiation_and_complete_run_over_https() {
	use crate::calibration::{StartReply, client};
	let video_port = std::net::UdpSocket::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port();
	// A host without PyroWave still offers UDP calibration (any codec), but
	// not the PyroWave-only HTTPS probe.
	let fixture = Fixture::new(crate::healthcheck::CODEC_H264, video_port);
	let info = fixture.get(true, "/serverinfo").await;
	assert!(
		info.contains("<PyroshineUdpProbeVersion>1</PyroshineUdpProbeVersion>"),
		"{info}"
	);
	assert!(info.contains("<PyroshineUdpProbeMaxKbps>3000000</PyroshineUdpProbeMaxKbps>"));
	assert!(!info.contains("PyroWaveBandwidthProbeBytes"));
	// Unpaired peers learn nothing and cannot start a run.
	let info = fixture.get(false, "/serverinfo").await;
	assert!(!info.contains("PyroshineUdpProbe"));
	let refused = fixture
		.get(false, "/pyroshine-udp-probe?version=1&maxkbps=20000&fps=60&shape=burst")
		.await;
	assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
	assert!(
		fixture
			.get(false, "/pyroshine-udp-probe-result?run=00")
			.await
			.starts_with("HTTP/1.1 401")
	);
	// Version negotiation: a future client version is refused with a reason.
	let future = fixture
		.get(true, "/pyroshine-udp-probe?version=2&maxkbps=20000&fps=60&shape=burst")
		.await;
	assert!(
		future.contains("status_code=\"400\"") && future.contains("not supported"),
		"{future}"
	);

	// A complete run: HTTPS start, UDP probe, HTTPS result.
	let start = fixture
		.get(
			true,
			"/pyroshine-udp-probe?version=1&maxkbps=30000&steps=3&stepms=250&fps=60&shape=burst&packetsize=1392",
		)
		.await;
	let body = &start[start.find("<root").expect("XML body")..];
	let reply = StartReply::from_xml(body).unwrap();
	assert_eq!(reply.udp_port, video_port);
	let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
	let run = client::receive(
		&socket,
		SocketAddr::from(([127, 0, 0, 1], reply.udp_port)),
		&reply,
		client::ClientPolicy::default(),
		cancel,
	)
	.await
	.unwrap();
	let result = fixture
		.get(
			true,
			&format!("/pyroshine-udp-probe-result?{}", client::result_query(&reply, &run, 0)),
		)
		.await;
	assert!(result.contains("<Outcome>completed</Outcome>"), "{result}");
	assert_eq!(result.matches("<Verdict>clean</Verdict>").count(), 3, "{result}");
	assert!(!result.contains("<RecommendedKbps>0</RecommendedKbps>"), "{result}");

	// The PyroWave HTTPS probe is unchanged on a PyroWave host.
	let pyrowave = Fixture::new(crate::healthcheck::CODEC_PYROWAVE, video_port);
	let info = pyrowave.get(true, "/serverinfo").await;
	assert!(info.contains("<PyroWaveBandwidthProbeBytes>33554432</PyroWaveBandwidthProbeBytes>"));
	assert!(info.contains("<PyroshineUdpProbeVersion>1</PyroshineUdpProbeVersion>"));
}
