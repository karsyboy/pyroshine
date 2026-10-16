//! Connection to the server's management interface on the session bus.
//!
//! The server may start before or after the tray and may restart while it
//! runs. Instead of polling, the tray follows ownership of the server's bus
//! name: when an owner appears it subscribes to the signals it shows and then
//! reads complete snapshots (in that order, so no change falls between
//! them); when the owner disappears it shows the server as unavailable. A
//! failed attachment is retried while the same owner stays (see `attach`).
//!
//! The tray only shows state and forwards the operator's End Session; every
//! other management task happens in the web interface.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use moonshine_management::dto::{PairingSnapshot, SessionSnapshot};
use moonshine_management::{BUS_NAME, ManagementError, ManagementProxy};
use tokio::sync::RwLock;

use crate::attach::{Attached, Attachment};
use crate::notifications::Notifier;
use crate::tray::TrayHandle;

/// Why a call to the server failed.
#[derive(Clone, Debug)]
pub struct UiError {
	pub kind: String,
	pub message: String,
}

impl UiError {
	pub fn unavailable() -> Self {
		Self {
			kind: "unavailable".into(),
			message: "Pyroshine is not running or cannot be reached.".into(),
		}
	}
}

impl From<zbus::Error> for UiError {
	fn from(error: zbus::Error) -> Self {
		let error = ManagementError::from(error);
		let kind = match &error {
			ManagementError::ZBus(zbus::Error::MethodError(name, _, _))
				if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
					|| name.as_str() == "org.freedesktop.DBus.Error.NoReply" =>
			{
				"unavailable"
			},
			// The server answers but does not offer this interface or method:
			// a version mismatch that retrying cannot fix.
			ManagementError::ZBus(zbus::Error::MethodError(name, _, _))
				if matches!(
					name.as_str(),
					"org.freedesktop.DBus.Error.UnknownMethod"
						| "org.freedesktop.DBus.Error.UnknownInterface"
						| "org.freedesktop.DBus.Error.UnknownObject"
						| "org.freedesktop.DBus.Error.InvalidArgs"
						| "org.freedesktop.DBus.Error.InvalidSignature"
				) =>
			{
				"incompatible"
			},
			error => error.kind(),
		};
		Self {
			kind: kind.into(),
			message: error.message(),
		}
	}
}

/// The server as the tray knows it.
pub struct Service {
	/// The session bus: notifications and the browser portal.
	connection: zbus::Connection,
	/// Where the server's management interface is (normally the session bus).
	management: zbus::Connection,
	proxy: RwLock<Option<ManagementProxy<'static>>>,
	/// The web interface's address from the server (`GetWebUrl`), kept so a
	/// click opens the right page even while the server restarts.
	web_url: RwLock<Option<String>>,
	/// The current attachment generation. An attempt publishes only while it
	/// is current, so a superseded attempt can never overwrite a newer owner's
	/// state even if it is still running.
	generation: AtomicU64,
}

/// Where server updates go.
#[derive(Clone)]
struct Sinks {
	tray: TrayHandle,
	notifier: Notifier,
}

impl Service {
	pub fn new(connection: zbus::Connection, management: zbus::Connection) -> Self {
		Self {
			connection,
			management,
			proxy: Default::default(),
			web_url: Default::default(),
			generation: AtomicU64::new(0),
		}
	}

	pub fn connection(&self) -> &zbus::Connection {
		&self.connection
	}

	pub async fn proxy(&self) -> Result<ManagementProxy<'static>, UiError> {
		self.proxy.read().await.clone().ok_or_else(UiError::unavailable)
	}

	/// The web interface's address, asking the server if it is not known yet.
	pub async fn web_url(&self) -> Option<String> {
		if let Some(url) = self.web_url.read().await.clone() {
			return Some(url);
		}
		let proxy = self.proxy().await.ok()?;
		let url = proxy.get_web_url().await.ok().filter(|url| !url.is_empty())?;
		*self.web_url.write().await = Some(url.clone());
		Some(url)
	}

	/// Open `page` of the web interface in the default browser.
	pub async fn open(&self, page: &str, activation: Option<String>) -> Result<(), String> {
		let base = self.web_url().await;
		let address = crate::browser::address(base.as_deref(), page);
		tracing::debug!(page, "Opening the web interface");
		crate::browser::open(Some(&self.connection), &address, activation).await
	}

	pub fn start(self: &Arc<Self>, tray: TrayHandle, notifier: Notifier) -> tokio::task::JoinHandle<()> {
		let service = self.clone();
		tokio::spawn(async move { service.follow(Sinks { tray, notifier }).await })
	}

	fn current(&self, generation: u64) -> bool {
		self.generation.load(Ordering::Acquire) == generation
	}

	/// Follow the server's bus name until the session bus goes away.
	async fn follow(self: Arc<Self>, sinks: Sinks) {
		let dbus = match zbus::fdo::DBusProxy::new(&self.management).await {
			Ok(dbus) => dbus,
			Err(error) => {
				tracing::error!("Cannot follow the Pyroshine service: {error}");
				return;
			},
		};
		let owners = match dbus.receive_name_owner_changed_with_args(&[(0, BUS_NAME)]).await {
			Ok(owners) => owners,
			Err(error) => {
				tracing::error!("Cannot follow the Pyroshine service: {error}");
				return;
			},
		};
		let name = zbus::names::BusName::try_from(BUS_NAME).expect("valid bus name");
		let owned = dbus.name_has_owner(name).await.unwrap_or(false);
		let owners = owners.map(|change| change.args().map(|args| args.new_owner().is_some()).unwrap_or(false));
		crate::attach::supervise(owners, owned, Supervisor { service: self, sinks }).await;
	}

	async fn detach(&self, sinks: &Sinks) {
		if self.proxy.write().await.take().is_some() {
			tracing::info!("Pyroshine is no longer available; waiting for it to return");
		}
		sinks.tray.disconnected().await;
		sinks.notifier.clear_pairing().await;
	}

	/// Subscribe, read snapshots, then forward signals until the owner leaves.
	/// Publishes only while `generation` is the current attachment.
	async fn attach(self: Arc<Self>, sinks: Sinks, generation: u64) -> Attached {
		let proxy = match ManagementProxy::builder(&self.management)
			.cache_properties(zbus::proxy::CacheProperties::No)
			.build()
			.await
		{
			Ok(proxy) => proxy,
			Err(error) => {
				tracing::warn!("Cannot reach Pyroshine: {error}");
				return Attached::failed(error.into());
			},
		};
		let streams = async {
			Ok::<_, zbus::Error>((
				proxy.receive_session_changed().await?,
				proxy.receive_pairing_changed().await?,
				proxy.receive_server_event().await?,
			))
		};
		let (mut session, mut pairing, mut events) = match streams.await {
			Ok(streams) => streams,
			Err(error) => {
				tracing::warn!("Cannot subscribe to Pyroshine: {error}");
				return Attached::failed(error.into());
			},
		};
		let snapshots = async {
			let (session, pairing) = tokio::try_join!(proxy.get_session(), proxy.get_pairing())?;
			// Servers older than the web interface have no address to offer.
			let url = proxy.get_web_url().await.ok().filter(|url| !url.is_empty());
			Ok::<_, zbus::Error>((session, pairing, url))
		};
		let (session_json, pairing_json, url) = match snapshots.await {
			Ok(snapshots) => snapshots,
			Err(error) => {
				tracing::warn!("Cannot read Pyroshine's state: {error}");
				return Attached::failed(error.into());
			},
		};
		let (Some(snapshot), Some(waiting)) = (
			parse::<SessionSnapshot>(&session_json),
			parse::<PairingSnapshot>(&pairing_json),
		) else {
			return Attached::failed(UiError {
				kind: "incompatible".into(),
				message: "Pyroshine answered with documents this tray does not understand; update both together."
					.into(),
			});
		};
		if !self.current(generation) {
			return Attached::Ended;
		}
		*self.proxy.write().await = Some(proxy);
		*self.web_url.write().await = url;
		tracing::info!("Connected to Pyroshine");
		sinks.tray.connected(&snapshot, &waiting).await;
		sinks.notifier.pairing(&waiting).await;

		loop {
			tokio::select! {
				Some(signal) = session.next() => {
					if !self.current(generation) { return Attached::Ended }
					let Some(snapshot) = signal.args().ok().and_then(|args| parse::<SessionSnapshot>(args.snapshot())) else { continue };
					sinks.tray.session(&snapshot).await;
				},
				Some(signal) = pairing.next() => {
					if !self.current(generation) { return Attached::Ended }
					let Some(snapshot) = signal.args().ok().and_then(|args| parse::<PairingSnapshot>(args.snapshot())) else { continue };
					sinks.tray.pairing(&snapshot).await;
					sinks.notifier.pairing(&snapshot).await;
				},
				Some(signal) = events.next() => {
					if !self.current(generation) { return Attached::Ended }
					if let Some(event) = signal.args().ok().and_then(|args| parse::<serde_json::Value>(args.event())) {
						sinks.notifier.server_event(&event).await;
					}
				},
				else => return Attached::Ended,
			}
		}
	}
}

/// The production [`Attachment`]: the server state and the tray's sinks.
struct Supervisor {
	service: Arc<Service>,
	sinks: Sinks,
}

impl Attachment for Supervisor {
	type Attempt = std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send>>;

	fn attach(&mut self, generation: u64) -> Self::Attempt {
		self.service.generation.store(generation, Ordering::Release);
		Box::pin(self.service.clone().attach(self.sinks.clone(), generation))
	}

	async fn detach(&mut self, generation: u64) {
		self.service.generation.store(generation, Ordering::Release);
		self.service.detach(&self.sinks).await;
	}

	async fn failed(&mut self, error: &UiError, retrying: bool) {
		if retrying {
			tracing::warn!("Attaching to Pyroshine failed ({}); retrying", error.message);
		} else {
			tracing::error!("Pyroshine is running but this tray cannot use it: {}", error.message);
		}
		*self.service.proxy.write().await = None;
		self.sinks.tray.disconnected().await;
	}
}

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
	serde_json::from_str(text).ok()
}
