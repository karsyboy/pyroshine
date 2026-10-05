//! Connection to the daemon's management interface.
//!
//! The daemon may start before or after the UI and may restart while it
//! runs. Instead of polling, the client follows ownership of the daemon's
//! bus name: when an owner appears it subscribes to every signal and then
//! reads complete snapshots (in that order, so no change falls between
//! them); when the owner disappears it reports the daemon as unavailable.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use moonshine_management::dto::{PairingSnapshot, SessionSnapshot};
use moonshine_management::{BUS_NAME, ManagementError, ManagementProxy};
use serde::Serialize;
use serde_json::Value;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter};

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
}

pub struct Daemon {
	connection: Option<zbus::Connection>,
	proxy: tokio::sync::RwLock<Option<ManagementProxy<'static>>>,
	overview: Mutex<Overview>,
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

	async fn follow(self: Arc<Self>, connection: zbus::Connection, sinks: Sinks) {
		let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
			Ok(dbus) => dbus,
			Err(error) => {
				tracing::error!("Cannot follow the Pyroshine service: {error}");
				return;
			},
		};
		let mut owners = match dbus.receive_name_owner_changed_with_args(&[(0, BUS_NAME)]).await {
			Ok(owners) => owners,
			Err(error) => {
				tracing::error!("Cannot follow the Pyroshine service: {error}");
				return;
			},
		};
		let name = zbus::names::BusName::try_from(BUS_NAME).expect("valid bus name");
		let mut attached: Option<JoinHandle<()>> = None;
		let mut owned = dbus.name_has_owner(name).await.unwrap_or(false);
		loop {
			if let Some(task) = attached.take() {
				task.abort();
			}
			if owned {
				attached = Some(tauri::async_runtime::spawn(
					self.clone().attach(connection.clone(), sinks.clone()),
				));
			} else {
				self.detach(&sinks).await;
			}
			let Some(change) = owners.next().await else {
				return;
			};
			owned = change.args().map(|args| args.new_owner().is_some()).unwrap_or(false);
		}
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
	async fn attach(self: Arc<Self>, connection: zbus::Connection, sinks: Sinks) {
		let proxy = match ManagementProxy::builder(&connection)
			.cache_properties(zbus::proxy::CacheProperties::No)
			.build()
			.await
		{
			Ok(proxy) => proxy,
			Err(error) => {
				tracing::warn!("Cannot reach Pyroshine: {error}");
				return;
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
					return;
				},
			};
		let overview = match read_overview(&proxy).await {
			Ok(overview) => overview,
			Err(error) => {
				tracing::warn!("Cannot read Pyroshine's state: {}", error.message);
				return;
			},
		};
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

		loop {
			tokio::select! {
				Some(signal) = session.next() => {
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
					let Some(value) = signal.args().ok().and_then(|args| json(args.snapshot())) else { continue };
					self.update(|overview| overview.pairing = Some(value.clone()));
					if let Ok(snapshot) = serde_json::from_value::<PairingSnapshot>(value.clone()) {
						sinks.tray.pairing(&snapshot).await;
						sinks.notifier.pairing(&snapshot).await;
					}
					let _ = sinks.app.emit("daemon://pairing", &value);
				},
				Some(signal) = requested.next() => {
					if let Some(value) = signal.args().ok().and_then(|args| json(args.request())) {
						let _ = sinks.app.emit("daemon://pairing-requested", &value);
					}
				},
				Some(signal) = resolved.next() => {
					if let Some(value) = signal.args().ok().and_then(|args| json(args.resolution())) {
						let _ = sinks.app.emit("daemon://pairing-resolved", &value);
					}
				},
				Some(signal) = clients.next() => {
					let Some(value) = signal.args().ok().and_then(|args| json(args.snapshot())) else { continue };
					self.update(|overview| overview.clients = Some(value.clone()));
					let _ = sinks.app.emit("daemon://clients", &value);
				},
				Some(signal) = saved.next() => {
					if let Some(value) = signal.args().ok().and_then(|args| json(args.saved())) {
						let _ = sinks.app.emit("daemon://config-saved", &value);
					}
				},
				Some(signal) = stats.next() => {
					let Some(value) = signal.args().ok().and_then(|args| json(args.stats())) else { continue };
					self.update(|overview| overview.stats = Some(value.clone()));
					let _ = sinks.app.emit("daemon://stats", &value);
				},
				Some(signal) = events.next() => {
					if let Some(value) = signal.args().ok().and_then(|args| json(args.event())) {
						sinks.notifier.server_event(&value).await;
						let _ = sinks.app.emit("daemon://server-event", &value);
					}
				},
				else => return,
			}
		}
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
	})
}
