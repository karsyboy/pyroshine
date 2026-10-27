use std::{
	collections::HashMap,
	convert::Infallible,
	net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
	path::PathBuf,
};

use async_shutdown::ShutdownManager;
use http_body_util::{BodyExt, Full};
use hyper::{
	Method, Request, Response, StatusCode,
	body::Bytes,
	header::{self, HeaderValue},
	service::service_fn,
};
use hyper_util::rt::tokio::{TokioIo, TokioTimer};
use network_interface::NetworkInterfaceConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::{
	ShutdownReason,
	app_scanner::Library,
	clients::{ClientManager, RevokeError},
	ingress::{ActivityIo, ConnectionActivity, Ingress},
	session::{
		APP_LAUNCH_HTTP_TIMEOUT_SECS, SessionContext, SessionKeyData, SessionKeys,
		application::SessionMode,
		failure::{Problem, SessionFailure},
		frame_limit::{FrameLimitConfig, FrameLimitMode},
		manager::{LiveBitrateRefusal, SessionManager},
		negotiation::{self, DisplayMode},
		stream::video::rate::{RateState, RateUpdate},
	},
	tls::TlsAcceptor,
};

use self::pairing::handle_pair_request;

use super::session::stream::audio::AudioChannels;

mod bandwidth;
pub(crate) use bandwidth::routed_link_mbps;
#[cfg(test)]
mod bandwidth_tests;
mod pairing;
#[cfg(test)]
mod security_tests;

/// Configuration for the embedded webserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebserverConfig {
	/// Port of the webserver.
	pub port: u16,

	/// Port of the HTTPS webserver.
	pub port_https: u16,

	/// Whether to allow new clients to pair.
	#[serde(default = "default_enable_pairing")]
	pub enable_pairing: bool,

	/// Path to the certificate for SSL encryption.
	pub certificate: PathBuf,

	/// Path to the private key for SSL encryption.
	pub private_key: PathBuf,
}

fn default_enable_pairing() -> bool {
	true
}

impl Default for WebserverConfig {
	fn default() -> Self {
		Self {
			port: 47989,
			port_https: 47984,
			enable_pairing: default_enable_pairing(),
			certificate: "$HOME/.config/moonshine/cert.pem".into(),
			private_key: "$HOME/.config/moonshine/key.pem".into(),
		}
	}
}

// The negative fourth value is to indicate that we are following the protocol introduced with Sunshine.
const SERVERINFO_APP_VERSION: &str = "7.1.431.-1";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";

#[derive(Clone)]
pub struct Webserver {
	probe_slots: std::sync::Arc<tokio::sync::Semaphore>,
	name: String,
	rtsp_port: u16,
	webserver_config: WebserverConfig,
	/// The application catalog. Each request reads the snapshot current when
	/// it starts; a refresh never changes a request (or a session) in flight.
	library: Library,
	unique_id: String,
	client_manager: ClientManager,
	session_manager: SessionManager,
	server_certs: String,
	supported_codecs: u32,
	hdr_supported: bool,
	/// Configured default frame limit, described in `/serverinfo`.
	frame_limit: FrameLimitConfig,
	/// Pending confirmations of `/launch` replacing the running application.
	replacements: std::sync::Arc<std::sync::Mutex<ReplacementConfirmations>>,
	shutdown: ShutdownManager<ShutdownReason>,
	limits: WebLimits,
}

/// Concurrent connections per HTTP/HTTPS listener. Moonlight uses a handful
/// (server polling, app list, a few parallel box-art downloads).
const MAX_HTTP_CONNECTIONS: usize = 64;
/// Hyper read buffer bound per connection. Pairing requests carry a hex PEM
/// certificate in the query string, which fits comfortably.
const MAX_HTTP_BUFFER_BYTES: usize = 64 * 1024;

/// Deadlines applied to every accepted HTTP/HTTPS connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WebLimits {
	/// Time for a client to complete the TLS handshake.
	pub tls_handshake_timeout: std::time::Duration,
	/// Time to receive request headers. Hyper also applies it while a keep-alive
	/// connection waits for the next request, bounding idle connections.
	pub header_read_timeout: std::time::Duration,
	/// Time an operator has to approve a pairing request.
	pub pairing_approval_timeout: std::time::Duration,
}

impl Default for WebLimits {
	fn default() -> Self {
		Self {
			tls_handshake_timeout: std::time::Duration::from_secs(10),
			header_read_timeout: std::time::Duration::from_secs(30),
			pairing_approval_timeout: std::time::Duration::from_secs(300),
		}
	}
}

impl Webserver {
	#[allow(clippy::result_unit_err)]
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		name: String,
		address: String,
		rtsp_port: u16,
		webserver_config: WebserverConfig,
		library: Library,
		supported_codecs: u32,
		hdr_supported: bool,
		unique_id: String,
		// Passing certificate content as string.
		server_certs: String,
		client_manager: ClientManager,
		session_manager: SessionManager,
		frame_limit: FrameLimitConfig,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		let server = Self {
			probe_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
			name,
			rtsp_port,
			webserver_config,
			library,
			unique_id,
			client_manager,
			session_manager,
			server_certs,
			supported_codecs,
			hdr_supported,
			frame_limit,
			replacements: Default::default(),
			shutdown: shutdown.clone(),
			limits: WebLimits::default(),
		};

		// Run HTTP webserver.
		let http_address = resolve_listen_address(&address, server.webserver_config.port)?;
		server.spawn_listener("http", http_address, None, ShutdownReason::HttpShutdown);

		// Operator revocation (`POST /unpair`) is restricted to loopback peers.
		// When the configured address does not accept loopback connections,
		// give the operator a loopback-only listener on the same port. Failing
		// to bind it only disables local revocation, not the GameStream service.
		if let Some(operator_address) = operator_listen_address(http_address) {
			tokio::spawn({
				let server = server.clone();
				async move {
					let shutdown = server.shutdown.clone();
					let _ = shutdown
						.wrap_cancel(async move {
							let listener = bind_listener(operator_address).map_err(|e| {
								tracing::warn!(
									"Failed to bind the local revocation listener on {operator_address}: {e}"
								)
							})?;
							tracing::debug!("Local revocation available on {operator_address}");
							let ingress = Ingress::new("http-operator", MAX_HTTP_CONNECTIONS, server.shutdown.clone());
							server.serve_listener(listener, None, ingress).await
						})
						.await;
				}
			});
		}

		// Run HTTPS webserver.
		let https_address = resolve_listen_address(&address, server.webserver_config.port_https)?;
		let acceptor = TlsAcceptor::from_config(
			server.webserver_config.certificate.clone(),
			server.webserver_config.private_key.clone(),
		)?;
		server.spawn_listener(
			"https",
			https_address,
			Some(std::sync::Arc::new(acceptor)),
			ShutdownReason::HttpsShutdown,
		);

		Ok(server)
	}

	/// Run a listener until global shutdown; a listener failure shuts down the server.
	fn spawn_listener(
		&self,
		name: &'static str,
		address: SocketAddr,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
		reason: ShutdownReason,
	) {
		let server = self.clone();
		tokio::spawn(async move {
			let shutdown = server.shutdown.clone();
			let _ = shutdown
				.wrap_cancel(shutdown.wrap_trigger_shutdown(reason, async move {
					let listener = bind_listener(address)
						.map_err(|e| tracing::error!("Failed to bind to address {address}: {e}"))?;
					tracing::debug!("{name} server listening for connections on {address}");
					let ingress =
						Ingress::new(name, MAX_HTTP_CONNECTIONS, server.shutdown.clone()).with_idle_eviction();
					server.serve_listener(listener, tls, ingress).await
				}))
				.await;
			tracing::debug!("{name} server shutting down.");
		});
	}

	/// Accept connections. Each one, including its TLS handshake, runs in its own
	/// bounded and cancellable task so a stalled client cannot block the others.
	async fn serve_listener(
		&self,
		listener: TcpListener,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
		ingress: Ingress,
	) -> Result<(), ()> {
		loop {
			let (connection, address) = listener
				.accept()
				.await
				.map_err(|e| tracing::error!("Failed to accept connection: {e}"))?;
			tracing::trace!("Accepted connection from {address}.");

			let Some(permit) = ingress.admit(address) else {
				continue;
			};
			let server = self.clone();
			let tls = tls.clone();
			let activity = permit.activity();
			permit.spawn(async move { server.serve_connection(connection, address, tls, activity).await });
		}
	}

	async fn serve_connection(
		&self,
		connection: tokio::net::TcpStream,
		address: SocketAddr,
		tls: Option<std::sync::Arc<TlsAcceptor>>,
		activity: std::sync::Arc<ConnectionActivity>,
	) {
		let peer_address = unmap_v4_mapped(address);
		let local_address = connection.local_addr().ok().map(unmap_v4_mapped);
		let mac_address = local_address.and_then(|address| get_mac_address(address.ip()).unwrap_or(None));
		let connection = ActivityIo::new(connection, activity.clone());

		let Some(acceptor) = tls else {
			self.serve_http(
				TokioIo::new(connection),
				local_address,
				peer_address,
				mac_address,
				false,
				None,
				activity,
			)
			.await;
			return;
		};

		let connection = match tokio::time::timeout(
			self.limits.tls_handshake_timeout,
			acceptor.accept(connection, peer_address),
		)
		.await
		{
			Ok(Ok(connection)) => connection,
			Ok(Err(())) => return,
			Err(_) => {
				tracing::debug!("TLS handshake from {peer_address} timed out.");
				return;
			},
		};

		// Extract peer certificate fingerprint from TLS connection for mTLS verification.
		let peer_cert_fingerprint = connection
			.get_ref()
			.1
			.peer_certificates()
			.and_then(|certs| certs.first())
			.map(|cert| hex::encode(Sha256::digest(cert.as_ref())));

		self.serve_http(
			TokioIo::new(connection),
			local_address,
			peer_address,
			mac_address,
			true,
			peer_cert_fingerprint,
			activity,
		)
		.await;
	}

	/// Serve HTTP/1.1 requests on an established (optionally TLS) connection.
	#[allow(clippy::too_many_arguments)]
	async fn serve_http<I>(
		&self,
		io: I,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<String>,
		activity: std::sync::Arc<ConnectionActivity>,
	) where
		I: hyper::rt::Read + hyper::rt::Write + Unpin,
	{
		let _ = hyper::server::conn::http1::Builder::new()
			.timer(TokioTimer::new())
			.header_read_timeout(self.limits.header_read_timeout)
			.max_buf_size(MAX_HTTP_BUFFER_BYTES)
			.serve_connection(
				io,
				service_fn(|request| {
					// In progress until the response is produced: a waiting
					// request (a pairing PIN, an app launch) is never evicted.
					let request_guard = activity.request();
					let response = self.serve(
						request,
						local_address,
						peer_address,
						mac_address.clone(),
						https,
						peer_cert_fingerprint.clone(),
					);
					async move {
						let response = response.await;
						drop(request_guard);
						response
					}
				}),
			)
			.await;
	}

	async fn serve(
		&self,
		request: Request<hyper::body::Incoming>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<String>,
	) -> Result<Response<bandwidth::ResponseBody>, Infallible> {
		let params = request
			.uri()
			.query()
			.map(|v| url::form_urlencoded::parse(v.as_bytes()).into_owned().collect())
			.unwrap_or_default();

		tracing::debug!("Received {} request for {}.", request.method(), request.uri().path());

		// Drain in-flight launch/resume/cancel operations before revoking trust
		// and stopping sessions. Requests starting afterward recheck durable trust.
		let _authorization = if https && matches!(request.uri().path(), "/launch" | "/resume" | "/cancel") {
			Some(self.client_manager.authorization_gate.read().await)
		} else {
			None
		};
		let response = if https {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => {
					self.server_info(
						params,
						local_address,
						peer_address,
						mac_address,
						https,
						peer_cert_fingerprint.as_ref(),
					)
					.await
				},
				(&Method::GET, "/pyrowave-bandwidth-probe") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					if self.supported_codecs & 0x01800000 == 0 {
						return Ok(not_found().map(BodyExt::boxed_unsync));
					}
					// Explicit calibration is unavailable during a session so a
					// bulk download never competes with interactive streaming.
					if !matches!(self.session_manager.get_session_context().await, Ok(None)) {
						return Ok(Response::builder()
							.status(StatusCode::CONFLICT)
							.body(Full::new(Bytes::from_static(
								b"End the stream before bandwidth calibration",
							)))
							.unwrap()
							.map(BodyExt::boxed_unsync));
					}
					let Ok(permit) = self.probe_slots.clone().try_acquire_owned() else {
						return Ok(Response::builder()
							.status(StatusCode::TOO_MANY_REQUESTS)
							.body(Full::new(Bytes::new()))
							.unwrap()
							.map(BodyExt::boxed_unsync));
					};
					return Ok(bandwidth::response(permit));
				},
				// UDP bandwidth calibration (see `crate::calibration`). Like the HTTPS
				// probe it is refused during a session; a session start preempts it.
				(&Method::GET, "/pyroshine-udp-probe") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					if !matches!(self.session_manager.get_session_context().await, Ok(None)) {
						return Ok(
							xml_error(409, "End the stream before bandwidth calibration").map(BodyExt::boxed_unsync)
						);
					}
					let link = crate::calibration::host_link_mbps(local_address, peer_address.ip());
					match self
						.session_manager
						.calibration()
						.start(
							&params,
							peer_cert_fingerprint.as_deref().unwrap_or_default(),
							peer_address,
							link,
						)
						.await
					{
						Ok(reply) => xml_response(reply.to_xml()),
						Err(error) => xml_error(error.status, &error.message),
					}
				},
				(&Method::GET, "/pyroshine-udp-probe-result") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					match self
						.session_manager
						.calibration()
						.result(&params, peer_cert_fingerprint.as_deref().unwrap_or_default())
						.await
					{
						Ok(xml) => xml_response(xml),
						Err(error) => xml_error(error.status, &error.message),
					}
				},
				(&Method::GET, "/applist") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.app_list()
				},
				(&Method::GET, "/appasset") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.app_asset(params).await
				},
				(&Method::GET, "/pair") => {
					if !self.webserver_config.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						peer_address,
						&self.server_certs,
						&self.client_manager,
						self.limits.pairing_approval_timeout,
						&self.shutdown,
					)
					.await
				},
				(&Method::GET, "/unpair") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					// TLS self-revocation targets the authenticated credential, never a
					// caller-supplied uniqueid or fingerprint.
					self.unpair(None, peer_cert_fingerprint.as_deref()).await
				},
				(&Method::GET, "/launch") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.launch(params, local_address, peer_address, peer_cert_fingerprint.clone())
						.await
				},
				(&Method::GET, "/resume") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.resume(params, local_address, peer_address, peer_cert_fingerprint.clone())
						.await
				},
				(&Method::GET, "/cancel") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.cancel(params).await
				},
				(&Method::GET, "/bitrate") => {
					if let Some(resp) = self.verify_paired_client(&peer_cert_fingerprint, peer_address.ip()) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.set_live_bitrate(params, peer_address).await
				},
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		} else {
			match (request.method(), request.uri().path()) {
				(&Method::GET, "/serverinfo") => {
					self.server_info(
						params,
						local_address,
						peer_address,
						mac_address,
						https,
						peer_cert_fingerprint.as_ref(),
					)
					.await
				},
				(&Method::GET, "/pair") => {
					if !self.webserver_config.enable_pairing {
						tracing::warn!("Pairing is disabled in configuration.");
						return Ok(bad_request("Pairing is disabled.".to_string()).map(BodyExt::boxed_unsync));
					}
					handle_pair_request(
						request,
						params,
						local_address,
						peer_address,
						&self.server_certs,
						&self.client_manager,
						self.limits.pairing_approval_timeout,
						&self.shutdown,
					)
					.await
				},
				// The loopback PIN page trusted any local process and user; approval
				// now needs the web interface's administrator sign-in, the tray or
				// `pyroshine pair` (all through the management operations).
				(&Method::GET, "/pin") | (&Method::POST, "/submit-pin") => self.operator_page_moved(),
				(&Method::POST, "/unpair") => {
					if let Some(resp) = self.verify_operator(&request, peer_address) {
						return Ok(resp.map(BodyExt::boxed_unsync));
					}
					self.unpair(
						params.get("uniqueid").map(String::as_str),
						params.get("fingerprint").map(String::as_str),
					)
					.await
				},
				(method, uri) => {
					tracing::warn!("Unhandled {method} request with URI '{uri}'");
					not_found()
				},
			}
		};

		Ok(response.map(BodyExt::boxed_unsync))
	}

	fn app_list(&self) -> Response<Full<Bytes>> {
		let response = app_list_xml(&self.library.catalog(), self.hdr_supported);
		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	async fn app_asset(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appasset' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		let application_id: i32 = match application_id.parse() {
			Ok(application_id) => application_id,
			Err(e) => {
				let message = format!("Failed to parse application ID: {e}");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let catalog = self.library.catalog();
		let entry = match catalog.find(application_id) {
			Some(entry) => entry,
			None => {
				let message = format!("Couldn't find application with ID {application_id}.");
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};
		if entry.artwork.is_empty() {
			let message = format!("No boxart defined for app '{}'.", entry.application.title);
			tracing::debug!("{message}");
			return bad_request(message);
		}

		// Decoding and resizing run on a blocking thread, bounded and cached
		// (see `app_scanner::artwork`), never on the network runtime.
		let buffer = match self.library.render_artwork(entry.artwork.clone()).await {
			Ok(buffer) => buffer,
			Err(reason) => {
				let message = format!("No usable boxart for app '{}': {reason}", entry.application.title);
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		let mut response = Response::new(Full::new(Bytes::from(buffer)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
		response
	}

	async fn server_info(
		&self,
		params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		mac_address: Option<String>,
		https: bool,
		peer_cert_fingerprint: Option<&String>,
	) -> Response<Full<Bytes>> {
		let session_context = match self.session_manager.get_session_context().await {
			Ok(session_context) => session_context,
			Err(()) => {
				let message = "Failed to get session context".to_string();
				tracing::warn!("{message}");
				return bad_request(message);
			},
		};

		// Seems we should only say we paired when using HTTPS.
		let paired = if https {
			match params.get("uniqueid") {
				Some(unique_id) if self.client_manager.is_paired(unique_id.clone()).unwrap_or(false) => "1",
				Some(_) | None => "0",
			}
		} else {
			"0"
		};

		let local_ip = local_address.map(|addr| addr.ip().to_string()).unwrap_or_default();

		// TODO: Check the use of some of these values, we leave most of them blank and Moonlight doesn't care.
		let mut response = "<root status_code=\"200\">".to_string();
		response += &format!("<hostname>{}</hostname>", escape_xml(&self.name));
		response += &format!("<appversion>{}</appversion>", SERVERINFO_APP_VERSION);
		response += &format!("<GfeVersion>{}</GfeVersion>", SERVERINFO_GFE_VERSION);
		response += &format!("<uniqueid>{}</uniqueid>", self.unique_id);
		response += &format!("<HttpsPort>{}</HttpsPort>", self.webserver_config.port_https);
		response += &format!("<ExternalPort>{}</ExternalPort>", self.webserver_config.port);
		response += &format!("<mac>{}</mac>", mac_address.unwrap_or("".to_string()));
		response += "<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"; // TODO: Check if HEVC is supported, set this to 0 if it is not.
		response += &format!("<LocalIP>{}</LocalIP>", escape_xml(local_ip));
		// HDR444 alias for record clients; native orthogonal HDR bit remains.
		let server_codec_mode_support = self.supported_codecs
			| if self.supported_codecs & 0x03000000 == 0x03000000 {
				0x04000000
			} else {
				0
			};
		response += &format!(
			"<ServerCodecModeSupport>{}</ServerCodecModeSupport>",
			server_codec_mode_support
		);
		let paired_peer = https
			&& peer_cert_fingerprint.is_some_and(|fp| {
				self.verify_paired_client(&Some(fp.clone()), peer_address.ip())
					.is_none()
			});
		if paired_peer {
			// UDP calibration works for every codec; clients without it ignore these.
			response += &format!(
				"<PyroshineUdpProbeVersion>{}</PyroshineUdpProbeVersion><PyroshineUdpProbeMaxKbps>{}</PyroshineUdpProbeMaxKbps>",
				crate::calibration::plan::PROTOCOL_VERSION,
				crate::calibration::plan::MAX_RATE_KBPS
			);
		}
		if paired_peer && self.supported_codecs & 0x01800000 != 0 {
			let speed = local_address
				.map(|a| bandwidth::routed_link_mbps(a.ip(), peer_address.ip()))
				.unwrap_or(0);
			response += &format!(
				"<PyroWaveHostLinkMbps>{speed}</PyroWaveHostLinkMbps><PyroWaveBandwidthProbeBytes>{}</PyroWaveBandwidthProbeBytes>",
				bandwidth::PROBE_BYTES
			);
		}
		if paired_peer && let Some((current, negotiated)) = self.session_manager.video_bitrate().await {
			// Rubylight's names; Nightjar's automatic bitrate starts from the
			// live encoder rate and treats the negotiated one as its ceiling.
			response += &format!(
				"<RustHostEncoderBitrateKbps>{}</RustHostEncoderBitrateKbps><RustHostInitialEncoderBitrateKbps>{}</RustHostInitialEncoderBitrateKbps>",
				current.div_ceil(1000),
				negotiated.div_ceil(1000)
			);
		}
		response += &frame_limiter_fields(&self.frame_limit);
		// Public like the rest of this document: it only tells an integration
		// whether `/applist` (paired clients only) carries launcher identities.
		response += &format!("<PyroshineAppSources>{APP_SOURCES_VERSION}</PyroshineAppSources>");
		// `/cancel?expectedAppId=` is checked under the session lifecycle lock.
		// Earlier builds ignored the parameter and stopped any game, so clients
		// must see this before relying on the check.
		response += &format!("<PyroshineConditionalCancel>{CONDITIONAL_CANCEL_VERSION}</PyroshineConditionalCancel>");
		response += "<SupportedDisplayMode></SupportedDisplayMode>";
		response += &format!("<PairStatus>{paired}</PairStatus>");
		response += &format!(
			"<currentgame>{}</currentgame>",
			session_context.clone().map(|s| s.application_id).unwrap_or(0)
		);
		// Public like `currentgame`, which it qualifies: what Steam reports
		// about the running game (launching, updating, running, exited).
		if session_context.is_some()
			&& let Some(game) = self.session_manager.steam_game().await
		{
			response += &format!("<PyroshineGameState>{}</PyroshineGameState>", game.state.as_str());
		}
		response += &format!(
			"<state>{}</state>",
			session_context
				.map(|_| "MOONSHINE_SERVER_BUSY")
				.unwrap_or("MOONSHINE_SERVER_FREE")
		);
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	fn operator_page_moved(&self) -> Response<Full<Bytes>> {
		let message = match self.client_manager.operator_page() {
			Some(page) => format!(
				"Pairing requests are approved in the Pyroshine web interface: open {page} on the host, \
				 or run `pyroshine pair`."
			),
			None => "Pairing requests are approved with `pyroshine pair` on the host.".to_string(),
		};
		Response::builder()
			.status(StatusCode::GONE)
			.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
			.header(header::CACHE_CONTROL, "no-store")
			.body(Full::new(Bytes::from(message)))
			.unwrap()
	}

	async fn unpair(&self, id: Option<&str>, fingerprint: Option<&str>) -> Response<Full<Bytes>> {
		match self
			.client_manager
			.revoke_and_stop_session(&self.session_manager, id, fingerprint)
			.await
		{
			Ok(()) => {},
			Err(RevokeError::SessionStopFailed) => {
				return bad_request("Trust revoked, but session shutdown failed.".to_string());
			},
			Err(RevokeError::NotRevoked) => {
				return bad_request(
					"Revocation failed or credential not found; legacy clients require fingerprint.".to_string(),
				);
			},
		}
		let xml = r#"<root status_code="200"/>"#;
		let mut response = Response::new(Full::new(Bytes::from(xml)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	/// Answer a failed `/launch` with the cause, which Moonlight shows in its
	/// error dialog, and report it to the management interface.
	fn launch_failed(&self, title: &str, failure: &SessionFailure) -> Response<Full<Bytes>> {
		if failure.code != SessionFailure::cancelled().code {
			let mut problem = Problem::from(failure);
			problem.message = format!("Launching {title} failed: {}", failure.message);
			self.session_manager.report_problem(problem);
		}
		xml_error(503, &failure.client_message())
	}

	async fn launch(
		&self,
		mut params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		client_identity: Option<String>,
	) -> Response<Full<Bytes>> {
		let application_id = match params.remove("appid") {
			Some(application_id) => application_id,
			None => {
				let message = format!("Expected 'appid' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let application_id: i32 = match application_id.parse() {
			Ok(application_id) => application_id,
			Err(e) => {
				let message = format!("Failed to parse application ID: {e}");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// Validate every negotiated value before the session is touched.
		let mode = match params.remove("mode") {
			Some(mode) => mode,
			None => {
				let message = format!("Expected 'mode' in launch request, got {:?}.", params.keys());
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};
		let DisplayMode {
			width,
			height,
			refresh_rate,
		} = match DisplayMode::parse(&mode) {
			Ok(mode) => mode,
			Err(reason) => {
				let message = format!("Invalid mode in launch request: {reason}.");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let keys = match request_keys(&mut params, "launch") {
			Ok(keys) => keys,
			Err(message) => {
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// TODO: localAudioPlayMode (host_audio) is not yet supported with the
		// per-session PulseServer approach.

		// Default: stereo (0x30002).
		let (audio_channels, audio_channel_mask) = match params.remove("surroundAudioInfo") {
			None => (AudioChannels::Stereo, 0x3),
			Some(value) => match negotiation::surround_audio_info(&value) {
				Ok(audio) => audio,
				Err(reason) => {
					let message = format!("Invalid launch request: {reason}.");
					tracing::warn!("{message}");
					return xml_error(400, &message);
				},
			},
		};

		let hdr_mode = match params.remove("hdrMode").map(|value| value.parse::<u32>()) {
			None => None,
			Some(Ok(value)) => Some(value != 0),
			Some(Err(error)) => return xml_error(400, &format!("Invalid hdrMode in launch request: {error}.")),
		};
		let hdr = hdr_mode == Some(true);
		let vrr_requested = match client_vrr_requested(&mut params) {
			Ok(requested) => requested,
			Err(error) => return xml_error(400, &format!("Invalid clientVrrRequested in launch request: {error}.")),
		};

		// The session takes its own copy of the entry: later catalog refreshes
		// cannot change or stop what this launch starts.
		let catalog = self.library.catalog();
		let application = match catalog.find(application_id) {
			Some(entry) => &entry.application,
			None => {
				let message = format!("Couldn't find application with ID {application_id}.");
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		// Another application runs: replace it only once this client confirms
		// by repeating the request (Vibepollo's protocol, which Nightjar follows
		// instead of cancelling first). Other clients cancel first and never
		// reach this; without confirmation the launch reports the session.
		if let Some(running) = self.session_manager.session_view().await
			&& running.application_id != application_id
			&& let Some(client) = client_identity.as_deref()
		{
			let now = std::time::Instant::now();
			let confirmed = self
				.replacements
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.arm_or_confirm(client, running.epoch, application_id, now);
			if !confirmed {
				tracing::info!(
					running = running.application_id,
					requested = application_id,
					"Launch of another application awaits the client's confirmation"
				);
				return xml_error(410, REPLACEMENT_CONFIRMATION);
			}
			tracing::info!(
				running = running.application_id,
				requested = application_id,
				"Client confirmed replacing the running application"
			);
			if self
				.session_manager
				.stop_session_if_application(running.application_id)
				.await
				.is_err()
			{
				return xml_error(503, "Failed to stop the running application");
			}
		}

		if application.mode == SessionMode::Desktop && hdr {
			// The client negotiates its codec profile before it launches. The
			// stream keeps that format and carries the desktop's SDR frames
			// (see `LaunchedSession::apply_output_mode`).
			tracing::info!("Desktop Mode streams SDR content; the client's HDR request falls back to SDR");
		}

		let initialize_result = self
			.session_manager
			.initialize_session(SessionContext {
				application: application.clone(),
				application_id,
				resolution: (width, height),
				refresh_rate,
				keys: SessionKeys::Keys(keys),
				audio_channels,
				audio_channel_mask,
				hdr,
				hdr_mode,
				client_ip: peer_address.ip(),
				client_identity,
				vrr_requested,
			})
			.await;

		if let Err(failure) = initialize_result {
			return self.launch_failed(&application.title, &failure);
		}

		match tokio::time::timeout(
			std::time::Duration::from_secs(APP_LAUNCH_HTTP_TIMEOUT_SECS),
			self.session_manager.launch_session(),
		)
		.await
		{
			Ok(Ok(())) => {},
			Ok(Err(failure)) => {
				let _ = self.session_manager.stop_session().await;
				return self.launch_failed(&application.title, &failure);
			},
			Err(_) => {
				tracing::error!("Timed out waiting for application launch result.");
				// Clean up the partially-initialized session to allow retries.
				let _ = self.session_manager.stop_session().await;
				let failure = SessionFailure::new(
					"launch_timeout",
					format!(
						"{} did not finish launching within {APP_LAUNCH_HTTP_TIMEOUT_SECS} seconds.",
						application.title
					),
				)
				.with_hint(
					"A pre_command may be waiting for something; check the application's hooks and the service log.",
				);
				return self.launch_failed(&application.title, &failure);
			},
		}

		let mut response = "<root status_code=\"200\">".to_string();
		response += "<gamesession>1</gamesession>";
		if let Some(addr) = local_address {
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(addr.ip()),
				self.rtsp_port
			);
		}
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));

		response
	}

	async fn resume(
		&self,
		mut params: HashMap<String, String>,
		local_address: Option<SocketAddr>,
		peer_address: SocketAddr,
		client_identity: Option<String>,
	) -> Response<Full<Bytes>> {
		// The whole request is validated before the session manager publishes
		// keys or rotates authorization, so a malformed resume changes nothing.
		let keys = match request_keys(&mut params, "resume") {
			Ok(keys) => keys,
			Err(message) => {
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		};

		let mut resume_request = crate::session::ResumeRequest {
			client_identity,
			..Default::default()
		};
		if let Some(mode) = params.remove("mode") {
			match DisplayMode::parse(&mode) {
				Ok(mode) => {
					resume_request.resolution = Some((mode.width, mode.height));
					resume_request.refresh_rate = Some(mode.refresh_rate);
				},
				Err(reason) => return xml_error(400, &format!("Invalid mode in resume request: {reason}.")),
			}
		}
		if let Some(hdr_mode) = params.remove("hdrMode") {
			match hdr_mode.parse::<u32>() {
				Ok(value) => resume_request.hdr = Some(value != 0),
				Err(error) => return xml_error(400, &format!("Invalid hdrMode in resume request: {error}.")),
			}
		}
		if let Some(surround_audio_info) = params.remove("surroundAudioInfo") {
			match negotiation::surround_audio_info(&surround_audio_info) {
				Ok((channels, mask)) => {
					resume_request.audio_channels = Some(channels);
					resume_request.audio_channel_mask = Some(mask);
				},
				Err(reason) => return xml_error(400, &format!("Invalid resume request: {reason}.")),
			}
		}
		// Per connection: a client that does not ask (any standard Moonlight)
		// gets fixed pacing even when the previous client asked for VRR.
		match client_vrr_requested(&mut params) {
			Ok(requested) => resume_request.vrr_requested = requested,
			Err(error) => return xml_error(400, &format!("Invalid clientVrrRequested in resume request: {error}.")),
		}

		match self
			.session_manager
			.resume_session(keys, resume_request, peer_address.ip())
			.await
		{
			Ok(()) => {},
			Err(()) => {
				let message = "Failed to update session keys".to_string();
				tracing::warn!("{message}");
				return xml_error(400, &message);
			},
		}

		let mut response = "<root status_code=\"200\">".to_string();
		if let Some(addr) = local_address {
			response += &format!(
				"<sessionUrl0>rtsp://{}:{}</sessionUrl0>",
				rtsp_host(addr.ip()),
				self.rtsp_port
			);
		}
		response += "<resume>1</resume>";
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));

		response
	}

	async fn cancel(&self, params: HashMap<String, String>) -> Response<Full<Bytes>> {
		let stopped = if let Some(expected) = params.get("expectedAppId") {
			let Ok(expected) = expected.parse::<i32>() else {
				return xml_error(400, "Invalid expectedAppId");
			};
			if expected <= 0 {
				return xml_error(400, "Invalid expectedAppId");
			}
			match self.session_manager.stop_session_if_application(expected).await {
				Ok(false) => return xml_error(409, "The running application changed"),
				Ok(true) => Ok(()),
				Err(()) => Err(()),
			}
		} else {
			self.session_manager.stop_session().await
		};
		if stopped.is_err() {
			let message = "Failed to stop session".to_string();
			tracing::warn!("{message}");
			return bad_request(message);
		}

		let mut response = "<root status_code=\"200\">".to_string();
		response += "<cancel>1</cancel>";
		response += "</root>";

		let mut response = Response::new(Full::new(Bytes::from(response)));
		response
			.headers_mut()
			.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
		response
	}

	/// `GET /bitrate?bitrate=KBPS`: change the encoder bitrate of the requesting
	/// client's own stream without reconnecting. This is the runtime call that
	/// Vibepollo and Foundation Sunshine define and their clients (Nightjar,
	/// Moonlight V+ and its client-side adaptive bitrate) send; the reply's
	/// `<bitrate>` is the accepted kbps, which the profile's maximum (or
	/// PyroWave's word-aligned frame budget) may lower, and `0` with an error
	/// status otherwise.
	async fn set_live_bitrate(
		&self,
		params: HashMap<String, String>,
		peer_address: SocketAddr,
	) -> Response<Full<Bytes>> {
		let Some(kbps) = params
			.get("bitrate")
			.and_then(|value| value.parse::<u32>().ok())
			.filter(|kbps| (1..=MAX_LIVE_BITRATE_KBPS).contains(kbps))
		else {
			return bitrate_reply(
				400,
				Some(&format!(
					"Expected bitrate in kbps between 1 and {MAX_LIVE_BITRATE_KBPS}"
				)),
				0,
			);
		};
		let update = match self
			.session_manager
			.client_set_bitrate(peer_address.ip(), kbps * 1000)
			.await
		{
			Ok(update) => update,
			Err(refusal @ LiveBitrateRefusal::NoStream) => return bitrate_reply(404, Some(&refusal.to_string()), 0),
			Err(refusal) => return bitrate_reply(409, Some(&refusal.to_string()), 0),
		};
		match tokio::time::timeout(LIVE_BITRATE_REPLY_TIMEOUT, update).await {
			Ok(Ok(RateUpdate {
				state: RateState::Applied,
				accepted_bps: Some(accepted),
				..
			})) => {
				let accepted = accepted.div_ceil(1000);
				tracing::info!(
					requested_kbps = kbps,
					accepted_kbps = accepted,
					"Client changed the live bitrate"
				);
				bitrate_reply(200, None, accepted)
			},
			Ok(Ok(update)) => {
				let message = update
					.error
					.unwrap_or_else(|| "The bitrate was replaced by a newer request or a reconnect".into());
				tracing::info!(requested_kbps = kbps, state = ?update.state, "Live bitrate not applied: {message}");
				bitrate_reply(409, Some(&message), 0)
			},
			Ok(Err(_)) => bitrate_reply(409, Some("The video encoder stopped"), 0),
			// No picture was encoded in time (a static VRR scene sends none): the
			// request stays staged for this generation and applies with the next
			// one, unless a newer request or a reconnect replaces it first.
			Err(_) => bitrate_reply(200, Some("Applies with the next frame"), kbps),
		}
	}

	/// Operator approval is a host-local administrative action served on the
	/// client-reachable HTTP listener; reject anything else with 403.
	fn verify_operator(
		&self,
		request: &Request<hyper::body::Incoming>,
		peer_address: SocketAddr,
	) -> Option<Response<Full<Bytes>>> {
		let reason = authorize_operator_request(peer_address, request.headers(), self.webserver_config.port).err()?;
		tracing::warn!(%peer_address, reason, "Rejected pairing approval request");
		Some(
			Response::builder()
				.status(StatusCode::FORBIDDEN)
				.body(Full::new(Bytes::from(
					"Pairing approval is only available from the host itself (http://localhost).",
				)))
				.unwrap(),
		)
	}

	/// Verify that the connecting client has presented a TLS certificate
	/// that belongs to a paired client. Returns `None` if authorized,
	/// or `Some(response)` with a 401 response if not.
	fn verify_paired_client(
		&self,
		peer_cert_fingerprint: &Option<String>,
		peer: IpAddr,
	) -> Option<Response<Full<Bytes>>> {
		match peer_cert_fingerprint {
			Some(fingerprint) => match self.client_manager.is_cert_paired(fingerprint) {
				Ok(true) => {
					self.client_manager.record_seen(fingerprint, peer);
					None
				},
				Ok(false) => {
					tracing::warn!("Client certificate not recognized (fingerprint: {fingerprint})");
					Some(unauthorized("Client certificate is not from a paired client."))
				},
				Err(()) => Some(bad_request("Failed to verify client certificate.".to_string())),
			},
			None => {
				tracing::warn!("No client certificate provided for protected endpoint.");
				Some(unauthorized("No client certificate provided."))
			},
		}
	}
}

/// Version of the `<PyroshineSource>` application metadata in `/applist`,
/// advertised as `<PyroshineAppSources>` in `/serverinfo`. Bump it only for
/// an incompatible change; adding elements is compatible.
pub(crate) const APP_SOURCES_VERSION: u32 = 1;

/// Version of conditional `/cancel?expectedAppId=`, advertised as
/// `<PyroshineConditionalCancel>` in `/serverinfo`.
pub(crate) const CONDITIONAL_CANCEL_VERSION: u32 = 1;

/// The `/applist` document for `catalog`.
///
/// Besides Moonlight's fields, each `<App>` carries one `<PyroshineSource>`
/// per launcher identity it starts (`steam:892970`; see
/// [`CatalogSnapshot::launcher_identities`]), so an integration such as
/// PyroDeck can find a Steam game by its app ID instead of by title.
/// Moonlight clients ignore unknown elements. The list is only served to
/// paired clients, like the titles themselves.
fn app_list_xml(catalog: &crate::app_scanner::CatalogSnapshot, hdr_supported: bool) -> String {
	let identities = catalog.launcher_identities();
	let mut response = "<root status_code=\"200\">".to_string();
	for entry in catalog.offered() {
		response += "<App>";

		// Desktop Mode streams what the desktop shares, which is SDR on
		// the desktops tested. Moonlight clients ignore this and may still
		// ask for HDR; such a stream falls back to SDR content.
		let hdr_supported = u8::from(hdr_supported && entry.application.mode.is_game());
		response += format!("<IsHdrSupported>{hdr_supported}</IsHdrSupported>").as_ref();
		response += format!("<AppTitle>{}</AppTitle>", escape_xml(&entry.application.title)).as_ref();
		response += format!("<ID>{}</ID>", entry.id).as_ref();
		for source in identities.get(entry.key.as_str()).into_iter().flatten() {
			response += format!("<PyroshineSource>{}</PyroshineSource>", escape_xml(source)).as_ref();
		}

		response += "</App>";
	}

	response += "</root>";
	response
}

/// Authorize a request to the operator route (`POST /unpair`). It can only
/// remove trust, never grant it.
///
/// - The peer must be loopback.
/// - `Host` must name a loopback host on our port, defeating DNS rebinding of
///   an attacker domain to 127.0.0.1.
/// - Browser metadata, when present, must describe a same-origin request on a
///   loopback origin, so another website cannot revoke a client through the
///   operator's browser (CSRF). Non-browser clients (curl) send neither header.
fn authorize_operator_request(peer: SocketAddr, headers: &header::HeaderMap, port: u16) -> Result<(), &'static str> {
	if !peer.ip().to_canonical().is_loopback() {
		return Err("non-loopback peer");
	}
	let host = headers
		.get(header::HOST)
		.and_then(|value| value.to_str().ok())
		.ok_or("missing Host")?;
	if !is_loopback_authority(host, port) {
		return Err("non-loopback Host");
	}
	if let Some(origin) = headers.get(header::ORIGIN) {
		let origin = origin.to_str().map_err(|_| "invalid Origin")?;
		let authority = origin.strip_prefix("http://").ok_or("cross-origin request")?;
		if !is_loopback_authority(authority, port) {
			return Err("cross-origin request");
		}
	}
	if let Some(site) = headers.get("sec-fetch-site")
		&& !matches!(site.as_bytes(), b"same-origin" | b"none")
	{
		return Err("cross-site request");
	}
	Ok(())
}

/// Whether `authority` (`host[:port]`) names a loopback host on `port`.
fn is_loopback_authority(authority: &str, port: u16) -> bool {
	let (host, authority_port) = match authority.strip_prefix('[') {
		Some(bracketed) => match bracketed.split_once(']') {
			Some((host, "")) => (host, None),
			Some((host, rest)) => match rest.strip_prefix(':') {
				Some(port) => (host, Some(port)),
				None => return false,
			},
			None => return false,
		},
		None => match authority.rsplit_once(':') {
			Some((host, port)) => (host, Some(port)),
			None => (authority, None),
		},
	};
	let port_matches = match authority_port {
		Some(authority_port) => authority_port.parse::<u16>().is_ok_and(|value| value == port),
		None => port == 80,
	};
	let loopback = host.eq_ignore_ascii_case("localhost")
		|| host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback());
	port_matches && loopback
}

fn resolve_listen_address(address: &str, port: u16) -> Result<SocketAddr, ()> {
	(address, port)
		.to_socket_addrs()
		.map_err(|e| tracing::error!("Failed to resolve address '{address}' port {port}: {e}"))?
		.next()
		.ok_or_else(|| tracing::error!("Failed to resolve address '{address}' port {port}"))
}

/// Address of the loopback listener for operator revocation, needed only when
/// the configured HTTP listener cannot accept loopback connections.
fn operator_listen_address(http_address: SocketAddr) -> Option<SocketAddr> {
	let ip = http_address.ip();
	if ip.is_unspecified() || ip.to_canonical().is_loopback() {
		return None;
	}
	Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_address.port()))
}

/// Bind a TCP listener for the webserver. When the address is IPv6 we disable
/// `IPV6_V6ONLY` so the single socket also accepts IPv4-mapped connections. This
/// lets clients reach us over whichever family mDNS advertised (avahi publishes
/// both an A and AAAA record), avoiding the "online/offline" flip-flop that
/// happens when one family has no listener.
fn bind_listener(address: SocketAddr) -> std::io::Result<TcpListener> {
	let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
	if address.is_ipv6() {
		socket.set_only_v6(false)?;
	}
	socket.set_reuse_address(true)?;
	socket.bind(&address.into())?;
	socket.listen(1024)?;
	socket.set_nonblocking(true)?;
	TcpListener::from_std(socket.into())
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) back to plain IPv4.
/// Connections arriving over IPv4 on a dual-stack socket report such an address;
/// normalizing keeps MAC lookups and session URLs using the real IPv4 address.
fn unmap_v4_mapped(addr: SocketAddr) -> SocketAddr {
	match addr.ip() {
		IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
			Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
			None => addr,
		},
		IpAddr::V4(_) => addr,
	}
}

/// `clientVrrRequested` launch/resume parameter: the client presents video
/// on a variable-refresh display and paces playback from RTP timestamps
/// (Pyrolight, Nonary's Moonlight fork). Hosts that do not know it ignore it;
/// absent means not requested.
fn client_vrr_requested(params: &mut HashMap<String, String>) -> Result<bool, std::num::ParseIntError> {
	params
		.remove("clientVrrRequested")
		.map_or(Ok(false), |value| value.parse::<u32>().map(|value| value != 0))
}

/// Format an IP address for use as the host part of an RTSP URL. IPv6 addresses
/// must be wrapped in brackets (`rtsp://[::1]:48010`) to be a valid URL.
fn rtsp_host(ip: IpAddr) -> String {
	match ip {
		IpAddr::V4(v4) => v4.to_string(),
		IpAddr::V6(v6) => format!("[{v6}]"),
	}
}

fn bad_request(message: String) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Full::new(Bytes::from(message)))
		.unwrap()
}

/// Validate the `rikey`/`rikeyid` pair of a launch/resume request. The error
/// is a client-facing message that never contains key material.
fn request_keys(params: &mut HashMap<String, String>, request: &str) -> Result<SessionKeyData, String> {
	let (Some(key), Some(key_id)) = (params.remove("rikey"), params.remove("rikeyid")) else {
		return Err(format!("Expected 'rikey' and 'rikeyid' in {request} request."));
	};
	SessionKeyData::from_params(&key, &key_id)
		.map_err(|error| format!("Invalid key parameters in {request} request: {error}."))
}

fn xml_response(xml: String) -> Response<Full<Bytes>> {
	let mut response = Response::new(Full::new(Bytes::from(xml)));
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
	response
		.headers_mut()
		.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

/// Vibepollo's prompt for replacing the running application. Clients match
/// this text exactly to ask the user, then repeat the launch to confirm.
const REPLACEMENT_CONFIRMATION: &str =
	"An app is already running. Launch this app again within 60 seconds to confirm that you want to close it.";

/// How long a client has to confirm a replacement.
const REPLACEMENT_CONFIRMATION_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// One pending replacement per client certificate.
#[derive(Default)]
struct ReplacementConfirmations(HashMap<String, PendingReplacement>);

struct PendingReplacement {
	/// Session epoch of the application to be replaced.
	epoch: u64,
	requested: i32,
	expires: std::time::Instant,
}

impl ReplacementConfirmations {
	/// True when `client` repeats an unexpired request for `requested` against
	/// the same session; otherwise (re)arm the confirmation and return false.
	fn arm_or_confirm(&mut self, client: &str, epoch: u64, requested: i32, now: std::time::Instant) -> bool {
		self.0.retain(|_, pending| pending.expires > now);
		if self
			.0
			.remove(client)
			.is_some_and(|pending| pending.epoch == epoch && pending.requested == requested)
		{
			return true;
		}
		self.0.insert(
			client.to_string(),
			PendingReplacement {
				epoch,
				requested,
				expires: now + REPLACEMENT_CONFIRMATION_WINDOW,
			},
		);
		false
	}
}

/// Vibepollo's description of the configured default frame limit. Clients
/// built for Vibepollo (Nightjar) also read the presence of
/// `FrameLimiterSupported` as the marker of Vibepollo's stream extensions:
/// they then send `clientVrrRequested`, decimal refresh rates and the runtime
/// `/bitrate` call, and `/launch` a different application without `/cancel`,
/// expecting the host to confirm the replacement. Pyroshine serves all four.
fn frame_limiter_fields(config: &FrameLimitConfig) -> String {
	let enabled = config.mode != FrameLimitMode::Off;
	let stream_rate = config.mode == FrameLimitMode::Stream;
	let millihertz = match config.mode {
		FrameLimitMode::Custom => config.fps.saturating_mul(1000),
		FrameLimitMode::Off | FrameLimitMode::Stream => 0,
	};
	format!(
		"<FrameLimiterSupported>1</FrameLimiterSupported><FrameLimiterEnabled>{}</FrameLimiterEnabled>\
		 <VirtualDisplayFrameLimiterEnabled>{}</VirtualDisplayFrameLimiterEnabled>\
		 <FrameLimiterFpsLimitMilliHz>{millihertz}</FrameLimiterFpsLimitMilliHz>",
		u8::from(enabled),
		u8::from(stream_rate)
	)
}

/// Largest live bitrate: the encoder's rate is in bits per second as a `u32`.
const MAX_LIVE_BITRATE_KBPS: u32 = u32::MAX / 1000;

/// How long `/bitrate` waits for the encoder to apply a rate before answering.
const LIVE_BITRATE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// A `/bitrate` reply. Errors keep the `<bitrate>0</bitrate>` element the
/// reference hosts send, which the clients read before the status.
fn bitrate_reply(status_code: u16, message: Option<&str>, kbps: u32) -> Response<Full<Bytes>> {
	let message = message
		.map(|message| format!(" status_message=\"{}\"", escape_xml(message)))
		.unwrap_or_default();
	xml_response(format!(
		"<root status_code=\"{status_code}\"{message}><bitrate>{kbps}</bitrate></root>"
	))
}

fn xml_error(status_code: u16, message: &str) -> Response<Full<Bytes>> {
	// Always return HTTP 200 so that Moonlight (Qt) reads the response body.
	// Qt treats HTTP 4xx/5xx as network errors and never reads the body,
	// so the XML error would be invisible to the client.
	// The actual status code is embedded in the XML body for Moonlight to parse.
	let body = format!(
		"<root status_code=\"{status_code}\" status_message=\"{}\"></root>",
		escape_xml(message)
	);
	match Response::builder()
		.status(StatusCode::OK)
		.body(Full::new(Bytes::from(body)))
	{
		Ok(mut response) => {
			response
				.headers_mut()
				.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
			response
		},
		Err(e) => {
			tracing::error!("Failed to build error response: {e}");
			bad_request("Failed to build error response.".to_string())
		},
	}
}

fn unauthorized(message: &str) -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::UNAUTHORIZED)
		.body(Full::new(Bytes::from(message.to_string())))
		.unwrap()
}

fn not_found() -> Response<Full<Bytes>> {
	Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body(Full::new(Bytes::from("NOT FOUND")))
		.unwrap()
}

fn get_mac_address(address: IpAddr) -> Result<Option<String>, ()> {
	let interfaces = network_interface::NetworkInterface::show()
		.map_err(|e| tracing::warn!("Failed to retrieve network interfaces: {e}"))?;

	for interface in interfaces {
		for interface_address in interface.addr {
			if interface_address.ip() == address {
				tracing::debug!(
					"Found MAC address for address {:?}: {:?}",
					address,
					interface.mac_addr.as_ref().unwrap_or(&"None".to_string())
				);
				return Ok(interface.mac_addr);
			}
		}
	}

	tracing::warn!("No interface found matching address {:?}", address);
	Ok(None)
}

pub(crate) fn escape_xml(input: impl AsRef<str>) -> String {
	input
		.as_ref()
		.replace("&", "&amp;")
		.replace("<", "&lt;")
		.replace(">", "&gt;")
		.replace("\"", "&quot;")
		.replace("'", "&apos;")
}

#[cfg(test)]
mod app_list_tests {
	use super::*;
	use crate::app_scanner::{CatalogEntry, CatalogSnapshot, EntryState, Origin, ScannerKind};
	use crate::session::application::ApplicationConfig;

	fn entry(key: &str, id: i32, title: &str, origin: Origin, state: EntryState) -> CatalogEntry {
		CatalogEntry {
			key: key.into(),
			id,
			origin,
			application: ApplicationConfig {
				title: title.into(),
				command: vec!["/usr/bin/steam".into()],
				..Default::default()
			},
			artwork: Vec::new(),
			artwork_origin: None,
			state,
		}
	}

	fn steam(native_id: &str) -> Origin {
		Origin::Discovered {
			scanner: 0,
			kind: ScannerKind::Steam,
			native_id: native_id.into(),
			unit: "/home/u/.local/share/Steam".into(),
		}
	}

	#[test]
	fn applist_reports_launcher_identities_beside_moonlight_fields() {
		let catalog = CatalogSnapshot::new(
			1,
			vec![
				entry(
					"manual:0",
					11,
					"Desktop & More",
					Origin::Manual { index: 0 },
					EntryState::Available,
				),
				entry("steam:892970", 22, "Valheim", steam("892970"), EntryState::Available),
				entry("steam:620", 0, "Portal 2", steam("620"), EntryState::Hidden),
			],
		);
		let xml = app_list_xml(&catalog, true);
		assert_eq!(
			xml,
			"<root status_code=\"200\">\
			 <App><IsHdrSupported>1</IsHdrSupported><AppTitle>Desktop &amp; More</AppTitle><ID>11</ID></App>\
			 <App><IsHdrSupported>1</IsHdrSupported><AppTitle>Valheim</AppTitle><ID>22</ID>\
			 <PyroshineSource>steam:892970</PyroshineSource></App>\
			 </root>"
		);
	}
}
