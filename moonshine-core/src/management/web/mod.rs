//! The web management interface: the browser UI's files, a versioned JSON
//! API under `/api/v1/` and its event stream, on a listener of its own.
//!
//! It is the server's one complete management UI, the same with or without a
//! desktop. It needs nothing graphical: the frontend is static files built at
//! release time (see `assets`), and every operation calls the shared
//! management operations (`super::ops`), so it cannot diverge from the tray
//! or `pyroshine pair`.
//!
//! The listener is separate from the GameStream HTTP/HTTPS ports and never
//! shares routes or authentication with them. It binds to loopback unless the
//! operator configures another address, which requires TLS (see
//! [`ManagementConfig`]); remote browsers otherwise reach it through an SSH
//! tunnel or VPN. Every API request except sign-in needs an administrator
//! session (`super::auth`); the listener's address grants nothing. Requests
//! pass `http::guard` first: Host allow-list (DNS rebinding), same-origin
//! checks and a CSRF token for changes, size limits, then the session.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_shutdown::ShutdownManager;
use hyper::service::service_fn;
use hyper_util::rt::tokio::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use super::Management;
use crate::ShutdownReason;
use crate::ingress::Ingress;
use crate::tls::TlsAcceptor;
use crate::webserver::WebserverConfig;

mod api;
mod assets;
mod events;
mod http;
#[cfg(test)]
mod tests;

pub(crate) use self::assets::Assets;
use self::http::Hosts;

/// Default port: next to the GameStream ports (47984, 47989), where
/// Sunshine-family servers serve their web UI.
pub const DEFAULT_PORT: u16 = 47990;

/// `[management]`: the web management interface.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ManagementConfig {
	/// Serve the web management interface.
	pub enabled: bool,
	/// Address to listen on. Loopback by default: reach it from another
	/// device through an SSH tunnel or VPN. Any other address requires `tls`.
	pub address: String,
	/// TCP port of the web interface.
	pub port: u16,
	/// Serve HTTPS. Required when `address` is not a loopback address.
	pub tls: bool,
	/// Certificate for HTTPS (PEM). Defaults to the server's GameStream
	/// certificate, which browsers do not trust without an exception.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub certificate: Option<PathBuf>,
	/// Private key for `certificate` (PEM).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub private_key: Option<PathBuf>,
	/// Further host names browsers may use to reach the interface, for
	/// example `streambox.lan` or a reverse proxy's name (`name` or
	/// `name:port`). Loopback names and IP addresses are always accepted.
	pub allowed_hosts: Vec<String>,
	/// Directory with the web UI's files. Defaults to the packaged location.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub web_root: Option<PathBuf>,
	/// Show the desktop tray (status icon and pairing notifications) while
	/// the user's desktop session runs. Headless hosts never start it.
	pub tray: bool,
}

impl Default for ManagementConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			address: "127.0.0.1".into(),
			port: DEFAULT_PORT,
			tls: false,
			certificate: None,
			private_key: None,
			allowed_hosts: Vec::new(),
			web_root: None,
			tray: true,
		}
	}
}

impl ManagementConfig {
	/// Checks [`crate::config::Config::validate`] applies.
	pub(crate) fn validate(&self) -> Result<(), String> {
		let address = self
			.address
			.parse::<IpAddr>()
			.map_err(|error| format!("management.address '{}' is not an IP address: {error}", self.address))?;
		if self.port == 0 {
			return Err("management.port must be a fixed port between 1 and 65535, not 0".into());
		}
		if self.enabled && !address.to_canonical().is_loopback() && !self.tls {
			return Err(format!(
				"management.address '{}' is reachable from other devices; set management.tls = true, \
				 or keep 127.0.0.1 and use an SSH tunnel or VPN",
				self.address
			));
		}
		if self.certificate.is_some() != self.private_key.is_some() {
			return Err("management.certificate and management.private_key must be set together".into());
		}
		for host in &self.allowed_hosts {
			if !http::valid_host_entry(host) {
				return Err(format!(
					"management.allowed_hosts entry '{host}' is not a host name (optionally with :port)"
				));
			}
		}
		Ok(())
	}
}

/// Concurrent connections. Browsers open up to six per origin; each open
/// tab also holds one event stream.
const MAX_CONNECTIONS: usize = 128;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Everything a request needs.
pub(crate) struct Context {
	pub management: Arc<Management>,
	pub assets: Assets,
	pub hosts: Hosts,
	pub https: bool,
}

/// A bound, not yet serving, web interface.
pub(crate) struct WebServer {
	listener: std::net::TcpListener,
	tls: Option<Arc<TlsAcceptor>>,
	url: String,
	assets: Assets,
	hosts: Hosts,
}

impl WebServer {
	/// Bind the configured listener. Returns `None` (logged) when the
	/// interface is disabled or cannot start; the server runs on without it.
	pub(crate) fn bind(config: &ManagementConfig, webserver: &WebserverConfig) -> Option<Self> {
		if !config.enabled {
			tracing::info!("Web management interface disabled ([management] enabled = false)");
			return None;
		}
		if let Err(reason) = config.validate() {
			tracing::error!("Web management interface not started: {reason}");
			return None;
		}
		let ip: IpAddr = config.address.parse().ok()?;
		let address = SocketAddr::new(ip, config.port);
		let tls = if config.tls {
			let certificate = config
				.certificate
				.clone()
				.unwrap_or_else(|| webserver.certificate.clone());
			let private_key = config
				.private_key
				.clone()
				.unwrap_or_else(|| webserver.private_key.clone());
			let expand = |path: PathBuf| {
				shellexpand::full(&path.to_string_lossy())
					.map(|path| PathBuf::from(path.as_ref()))
					.unwrap_or(path)
			};
			match TlsAcceptor::for_browsers(expand(certificate), expand(private_key)) {
				Ok(acceptor) => Some(Arc::new(acceptor)),
				Err(()) => {
					tracing::error!("Web management interface not started: its TLS certificate cannot be loaded");
					return None;
				},
			}
		} else {
			None
		};
		let listener = match bind(address) {
			Ok(listener) => listener,
			Err(error) => {
				tracing::error!(
					"Web management interface not started: cannot listen on {address}: {error}. \
					 Change [management] port if another program uses it."
				);
				return None;
			},
		};
		let scheme = if tls.is_some() { "https" } else { "http" };
		let local = match ip {
			IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
			IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
			ip => ip,
		};
		let url = format!("{scheme}://{}/", SocketAddr::new(local, config.port));
		let hosts = Hosts::new(config.port, ip, &config.allowed_hosts);
		let assets = Assets::load(config.web_root.as_deref());
		if remote(ip) {
			tracing::warn!(
				%address,
				"The web management interface accepts connections from other devices (HTTPS, administrator sign-in required)"
			);
		}
		Some(Self {
			listener,
			tls,
			url,
			assets,
			hosts,
		})
	}

	/// The address a browser on this host opens.
	pub(crate) fn url(&self) -> String {
		self.url.clone()
	}

	/// Accept connections until shutdown.
	pub(crate) async fn serve(self, management: Arc<Management>, shutdown: ShutdownManager<ShutdownReason>) {
		let listener = match TcpListener::from_std(self.listener) {
			Ok(listener) => listener,
			Err(error) => {
				tracing::error!("Web management interface not started: {error}");
				return;
			},
		};
		tracing::info!(url = self.url, "Web management interface available");
		let context = Arc::new(Context {
			management,
			assets: self.assets,
			hosts: self.hosts,
			https: self.tls.is_some(),
		});
		let ingress = Ingress::new("Web management", MAX_CONNECTIONS, shutdown);
		loop {
			let (connection, peer) = match listener.accept().await {
				Ok(accepted) => accepted,
				Err(error) => {
					// Transient (for example out of file descriptors): retry.
					tracing::warn!("Web management: cannot accept a connection: {error}");
					tokio::time::sleep(Duration::from_millis(100)).await;
					continue;
				},
			};
			let Some(permit) = ingress.admit(peer) else {
				continue;
			};
			let context = context.clone();
			let tls = self.tls.clone();
			permit.spawn(async move { serve_connection(context, connection, peer, tls).await });
		}
	}
}

fn remote(ip: IpAddr) -> bool {
	!ip.to_canonical().is_loopback()
}

fn bind(address: SocketAddr) -> std::io::Result<std::net::TcpListener> {
	use socket2::{Domain, Protocol, Socket, Type};
	let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
	socket.set_reuse_address(true)?;
	if address.is_ipv6() {
		// `::` covers IPv4 too, like the GameStream listeners.
		socket.set_only_v6(!address.ip().is_unspecified())?;
	}
	socket.bind(&address.into())?;
	socket.listen(128)?;
	socket.set_nonblocking(true)?;
	Ok(socket.into())
}

async fn serve_connection(
	context: Arc<Context>,
	connection: tokio::net::TcpStream,
	peer: SocketAddr,
	tls: Option<Arc<TlsAcceptor>>,
) {
	let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
	match tls {
		None => serve_http(context, TokioIo::new(connection), peer).await,
		Some(acceptor) => match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(connection, peer)).await {
			Ok(Ok(stream)) => serve_http(context, TokioIo::new(stream), peer).await,
			Ok(Err(())) => {},
			Err(_) => tracing::debug!(%peer, "Web management TLS handshake timed out"),
		},
	}
}

async fn serve_http<I>(context: Arc<Context>, io: I, peer: SocketAddr)
where
	I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
	let _ = hyper::server::conn::http1::Builder::new()
		.timer(TokioTimer::new())
		.header_read_timeout(HEADER_READ_TIMEOUT)
		.max_buf_size(MAX_HEADER_BYTES)
		.serve_connection(
			io,
			service_fn(move |request| {
				let context = context.clone();
				async move { Ok::<_, std::convert::Infallible>(http::handle(context, request, peer).await) }
			}),
		)
		.await;
}
