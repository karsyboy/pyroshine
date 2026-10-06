//! Connection to the daemon's management interface.
//!
//! The daemon may start before or after the UI and may restart while it
//! runs. Instead of polling, the client follows ownership of the daemon's
//! bus name: when an owner appears it subscribes to every signal and then
//! reads complete snapshots (in that order, so no change falls between
//! them); when the owner disappears it reports the daemon as unavailable.
//! A failed attachment is retried while the same owner stays (see `attach.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use moonshine_management::dto::{PairingSnapshot, SessionSnapshot};
use moonshine_management::{BUS_NAME, ManagementError, ManagementProxy};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::attach::{Attached, Attachment};
use crate::notifications::Notifier;
use crate::tray::TrayHandle;

/// Error reported to the window.
#[derive(Clone, Debug, Serialize)]
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
			// The daemon answers but does not offer this app's interface: a
			// version mismatch that retrying cannot fix.
			ManagementError::ZBus(zbus::Error::MethodError(name, _, _))
				if matches!(
					name.as_str(),
					"org.freedesktop.DBus.Error.UnknownMethod"
						| "org.freedesktop.DBus.Error.UnknownInterface"
						| "org.freedesktop.DBus.Error.UnknownObject"
						| "org.freedesktop.DBus.Error.UnknownProperty"
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

/// Everything the window shows, kept current from signals.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Overview {
	pub connected: bool,
	pub server: Option<Value>,
	pub session: Option<Value>,
	pub pairing: Option<Value>,
	pub clients: Option<Value>,
	pub stats: Option<Value>,
	/// Why the daemon is running but this app is not attached to it.
	pub attach_error: Option<UiError>,
}

pub struct Daemon {
	connection: Option<zbus::Connection>,
	proxy: tokio::sync::RwLock<Option<ManagementProxy<'static>>>,
	overview: Mutex<Overview>,
	/// The current attachment generation. An attempt publishes only while it
	/// is current, so a superseded attempt can never overwrite a newer owner's
	/// state even if it is still running.
	generation: AtomicU64,
}

/// Where daemon updates go.
#[derive(Clone)]
struct Sinks {
	app: AppHandle,
	tray: TrayHandle,
	notifier: Notifier,
}

impl Daemon {
	pub fn new(connection: Option<zbus::Connection>) -> Self {
		Self {
			connection,
			proxy: Default::default(),
			overview: Default::default(),
			generation: AtomicU64::new(0),
		}
	}

	pub fn connection(&self) -> Option<zbus::Connection> {
		self.connection.clone()
	}

	pub fn overview(&self) -> Overview {
		self.overview
			.lock()
			.map(|overview| overview.clone())
			.unwrap_or_default()
	}

	pub async fn proxy(&self) -> Result<ManagementProxy<'static>, UiError> {
		self.proxy.read().await.clone().ok_or_else(UiError::unavailable)
	}

	pub fn start(self: &Arc<Self>, app: AppHandle, tray: TrayHandle, notifier: Notifier) {
		let Some(connection) = self.connection.clone() else {
			return;
		};
		let daemon = self.clone();
		let sinks = Sinks { app, tray, notifier };
		tauri::async_runtime::spawn(async move { daemon.follow(connection, sinks).await });
	}

	/// Re-read every snapshot (after the window reconnects or on request).
	pub async fn refresh(&self, app: &AppHandle) -> Result<Overview, UiError> {
		let proxy = self.proxy().await?;
		let overview = read_overview(&proxy).await?;
		self.replace(overview.clone());
		let _ = app.emit("daemon://overview", &overview);
		Ok(overview)
	}

	fn replace(&self, overview: Overview) {
		if let Ok(mut current) = self.overview.lock() {
			*current = overview;
		}
	}

	fn update(&self, change: impl FnOnce(&mut Overview)) {
		if let Ok(mut overview) = self.overview.lock() {
			change(&mut overview);
		}
	}

	fn current(&self, generation: u64) -> bool {
		self.generation.load(Ordering::Acquire) == generation
	}

	async fn follow(self: Arc<Self>, connection: zbus::Connection, sinks: Sinks) {
		let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
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
		crate::attach::supervise(
			owners,
			owned,
			Supervisor {
				daemon: self,
				connection,
				sinks,
			},
		)
		.await;
	}

	async fn detach(&self, sinks: &Sinks) {
		if self.proxy.write().await.take().is_some() {
			tracing::info!("Pyroshine is no longer available; waiting for it to return");
		}
		self.replace(Overview::default());
		let _ = sinks.app.emit("daemon://overview", &Overview::default());
		sinks.tray.disconnected().await;
		sinks.notifier.clear_pairing().await;
	}

	/// Subscribe, read snapshots, then forward signals until the owner leaves.
	/// Publishes only while `generation` is the current attachment.
	async fn attach(self: Arc<Self>, connection: zbus::Connection, sinks: Sinks, generation: u64) -> Attached {
		let proxy = match ManagementProxy::builder(&connection)
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
				proxy.receive_pairing_requested().await?,
				proxy.receive_pairing_resolved().await?,
				proxy.receive_clients_changed().await?,
				proxy.receive_config_saved().await?,
				proxy.receive_stats_updated().await?,
				proxy.receive_server_event().await?,
			))
		};
		let (mut session, mut pairing, mut requested, mut resolved, mut clients, mut saved, mut stats, mut events) =
			match streams.await {
				Ok(streams) => streams,
				Err(error) => {
					tracing::warn!("Cannot subscribe to Pyroshine: {error}");
					return Attached::failed(error.into());
				},
			};
		let overview = match read_overview(&proxy).await {
			Ok(overview) => overview,
			Err(error) => {
				tracing::warn!("Cannot read Pyroshine's state: {}", error.message);
				return Attached::failed(error);
			},
		};
		if !self.current(generation) {
			return Attached::Ended;
		}
		*self.proxy.write().await = Some(proxy);
		tracing::info!(
			version = overview
				.server
				.as_ref()
				.and_then(|server| server["version"].as_str())
				.unwrap_or("unknown"),
			"Connected to Pyroshine"
		);
		self.replace(overview.clone());
		let _ = sinks.app.emit("daemon://overview", &overview);
		sinks.tray.update(&overview).await;
		if let Some(pairing) = parse::<PairingSnapshot>(&overview.pairing) {
			sinks.notifier.pairing(&pairing).await;
		}

		// A superseded attachment stops before publishing anything more.
		macro_rules! current {
			() => {
				if !self.current(generation) {
					return Attached::Ended;
				}
			};
		}
		loop {
			tokio::select! {
				Some(signal) = session.next() => {
					current!();
					let Some(value) = signal.args().ok().and_then(|args| json(args.snapshot())) else { continue };
					self.update(|overview| overview.session = Some(value.clone()));
					if let Ok(snapshot) = serde_json::from_value::<SessionSnapshot>(value.clone()) {
						sinks.tray.session(&snapshot).await;
						if !snapshot.phase.eq(&moonshine_management::dto::SessionPhase::Streaming) {
							self.update(|overview| overview.stats = None);
						}
					}
					let _ = sinks.app.emit("daemon://session", &value);
				},
				Some(signal) = pairing.next() => {
					current!();
					let Some(value) = signal.args().ok().and_then(|args| json(args.snapshot())) else { continue };
					self.update(|overview| overview.pairing = Some(value.clone()));
					if let Ok(snapshot) = serde_json::from_value::<PairingSnapshot>(value.clone()) {
						sinks.tray.pairing(&snapshot).await;
						sinks.notifier.pairing(&snapshot).await;
					}
					let _ = sinks.app.emit("daemon://pairing", &value);
				},
				Some(signal) = requested.next() => {
					current!();
					if let Some(value) = signal.args().ok().and_then(|args| json(args.request())) {
						let _ = sinks.app.emit("daemon://pairing-requested", &value);
					}
				},
				Some(signal) = resolved.next() => {
					current!();
					if let Some(value) = signal.args().ok().and_then(|args| json(args.resolution())) {
						let _ = sinks.app.emit("daemon://pairing-resolved", &value);
					}
				},
				Some(signal) = clients.next() => {
					current!();
					let Some(value) = signal.args().ok().and_then(|args| json(args.snapshot())) else { continue };
					self.update(|overview| overview.clients = Some(value.clone()));
					let _ = sinks.app.emit("daemon://clients", &value);
				},
				Some(signal) = saved.next() => {
					current!();
					if let Some(value) = signal.args().ok().and_then(|args| json(args.saved())) {
						let _ = sinks.app.emit("daemon://config-saved", &value);
					}
				},
				Some(signal) = stats.next() => {
					current!();
					let Some(value) = signal.args().ok().and_then(|args| json(args.stats())) else { continue };
					self.update(|overview| overview.stats = Some(value.clone()));
					let _ = sinks.app.emit("daemon://stats", &value);
				},
				Some(signal) = events.next() => {
					current!();
					if let Some(value) = signal.args().ok().and_then(|args| json(args.event())) {
						sinks.notifier.server_event(&value).await;
						let _ = sinks.app.emit("daemon://server-event", &value);
					}
				},
				else => return Attached::Ended,
			}
		}
	}
}

/// The production [`Attachment`]: the daemon state, bus and UI sinks.
struct Supervisor {
	daemon: Arc<Daemon>,
	connection: zbus::Connection,
	sinks: Sinks,
}

impl Attachment for Supervisor {
	type Attempt = std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send>>;

	fn attach(&mut self, generation: u64) -> Self::Attempt {
		self.daemon.generation.store(generation, Ordering::Release);
		Box::pin(
			self.daemon
				.clone()
				.attach(self.connection.clone(), self.sinks.clone(), generation),
		)
	}

	async fn detach(&mut self, generation: u64) {
		self.daemon.generation.store(generation, Ordering::Release);
		self.daemon.detach(&self.sinks).await;
	}

	async fn failed(&mut self, error: &UiError, retrying: bool) {
		if retrying {
			tracing::warn!("Attaching to Pyroshine failed ({}); retrying", error.message);
		} else {
			tracing::error!("Pyroshine is running but this app cannot use it: {}", error.message);
		}
		*self.daemon.proxy.write().await = None;
		let overview = Overview {
			attach_error: Some(error.clone()),
			..Overview::default()
		};
		self.daemon.replace(overview.clone());
		let _ = self.sinks.app.emit("daemon://overview", &overview);
		self.sinks.tray.disconnected().await;
	}
}

fn json(text: &str) -> Option<Value> {
	serde_json::from_str(text).ok()
}

fn parse<T: serde::de::DeserializeOwned>(value: &Option<Value>) -> Option<T> {
	value
		.as_ref()
		.and_then(|value| serde_json::from_value(value.clone()).ok())
}

async fn read_overview(proxy: &ManagementProxy<'static>) -> Result<Overview, UiError> {
	let (server, session, pairing, clients, stats) = tokio::try_join!(
		proxy.get_server(),
		proxy.get_session(),
		proxy.get_pairing(),
		proxy.get_clients(),
		proxy.get_stats(),
	)?;
	Ok(Overview {
		connected: true,
		server: json(&server),
		session: json(&session),
		pairing: json(&pairing),
		clients: json(&clients),
		stats: json(&stats).filter(|stats| !stats.is_null()),
		attach_error: None,
	})
}
