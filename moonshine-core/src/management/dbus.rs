//! The D-Bus adapter behind [`moonshine_management::INTERFACE`], used by the
//! desktop tray and `pyroshine pair`/`pyroshine diag`.
//!
//! Every method checks that the caller runs as the same user as the daemon.
//! The session bus already only admits that user; the check keeps a
//! misconfigured bus from widening access to pairing, revocation and the
//! configuration file. Methods call the shared operations in `ops`; signals
//! are the hub's events (see `hub`).

use std::sync::Arc;

use futures_util::StreamExt;
use moonshine_management::{BUS_NAME, ManagementError, OBJECT_PATH, UI_BUS_NAME};
use tokio::sync::broadcast;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;

use super::hub::EventKind;
use super::{Management, json};

/// Recorded in the log as who asked for an operation.
const ACTOR: &str = "dbus";

pub(super) struct ManagementInterface {
	pub(super) management: Arc<Management>,
}

async fn authorize(connection: &zbus::Connection, header: &Header<'_>) -> Result<(), ManagementError> {
	let sender = header
		.sender()
		.ok_or_else(|| ManagementError::AccessDenied("Unknown caller.".into()))?;
	let proxy = zbus::fdo::DBusProxy::new(connection)
		.await
		.map_err(|e| ManagementError::Failed(e.to_string()))?;
	let uid = proxy
		.get_connection_unix_user(sender.clone().into())
		.await
		.map_err(|e| ManagementError::AccessDenied(format!("Cannot identify the caller: {e}")))?;
	// SAFETY: geteuid has no preconditions and cannot fail.
	if uid != unsafe { libc::geteuid() } {
		tracing::warn!(%sender, uid, "Rejected management call from another user");
		return Err(ManagementError::AccessDenied(
			"Only the user running Pyroshine can manage it.".into(),
		));
	}
	Ok(())
}

/// Run `work` on the server's Tokio runtime and wait for it from zbus's executor.
async fn on_runtime<T: Send + 'static>(
	management: &Arc<Management>,
	work: impl Future<Output = Result<T, ManagementError>> + Send + 'static,
) -> Result<T, ManagementError> {
	management
		.runtime
		.spawn(work)
		.await
		.map_err(|e| ManagementError::Failed(format!("The operation was interrupted: {e}")))?
}

#[zbus::interface(name = "io.github.karsyboy.Pyroshine.Management1")]
impl ManagementInterface {
	async fn get_server(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_server()))
	}

	async fn get_session(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_session()))
	}

	async fn end_session(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(&self.management, async move { management.op_end_session(ACTOR).await }).await
	}

	async fn restart(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_restart(ACTOR)
	}

	async fn get_pairing(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_pairing()))
	}

	async fn approve_pairing(
		&self,
		client_id: &str,
		request: &str,
		pin: &str,
		label: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		self.management
			.op_approve_pairing(client_id, request, pin, label, ACTOR)
	}

	async fn reject_pairing(
		&self,
		client_id: &str,
		request: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_reject_pairing(client_id, request, ACTOR)
	}

	async fn get_clients(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_clients().map(|clients| json(&clients))
	}

	async fn revoke_client(
		&self,
		fingerprint: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let fingerprint = fingerprint.to_string();
		on_runtime(&self.management, async move {
			management.op_revoke_client(&fingerprint, ACTOR).await
		})
		.await
	}

	async fn rename_client(
		&self,
		fingerprint: &str,
		label: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_rename_client(fingerprint, label)
	}

	async fn get_config(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_config().map(|document| json(&document))
	}

	async fn get_config_schema(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_config_schema()))
	}

	async fn validate_config(
		&self,
		values: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_validate_config(values).map(|report| json(&report))
	}

	async fn save_config(
		&self,
		values: &str,
		base_revision: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let (values, base_revision) = (values.to_string(), base_revision.to_string());
		let outcome = on_runtime(&self.management, async move {
			management.op_save_config(&values, &base_revision, ACTOR).await
		})
		.await?;
		Ok(json(&outcome))
	}

	async fn get_stats(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_stats()))
	}

	async fn get_events(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_events()))
	}

	/// Re-run the host checks (see `healthcheck::recheck_in_server`) and
	/// publish the result with `HealthChanged`.
	async fn run_health_check(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let report = on_runtime(
			&self.management,
			async move { Ok(management.op_run_health_check().await) },
		)
		.await?;
		Ok(json(&report))
	}

	async fn get_diagnostic_report(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(
			&self.management,
			async move { Ok(management.op_diagnostic_report().await) },
		)
		.await
	}

	async fn get_support_bundle(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<Vec<u8>, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(&self.management, async move { management.op_support_bundle().await }).await
	}

	async fn get_library(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.op_library()))
	}

	async fn rescan_library(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		self.management.op_rescan_library()
	}

	async fn set_application_hidden(
		&self,
		key: &str,
		hidden: bool,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let key = key.to_string();
		on_runtime(&self.management, async move {
			management.op_set_application_hidden(&key, hidden).await
		})
		.await
	}

	/// The browser address of the web management interface, or an empty
	/// string when it is disabled. The tray and launcher open it.
	async fn get_web_url(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(self.management.web_url().unwrap_or_default())
	}

	/// Issue a one-time code that sets (or, after `reset`, replaces) the web
	/// administrator password. Only the user running Pyroshine can ask, so
	/// the code proves that the browser's operator controls this account.
	async fn create_web_setup_code(
		&self,
		reset: bool,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(&self.management, async move { management.op_create_setup_code(reset) }).await
	}

	#[zbus(signal)]
	pub(super) async fn session_changed(emitter: &SignalEmitter<'_>, snapshot: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn pairing_changed(emitter: &SignalEmitter<'_>, snapshot: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn pairing_requested(emitter: &SignalEmitter<'_>, request: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn pairing_resolved(emitter: &SignalEmitter<'_>, resolution: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn clients_changed(emitter: &SignalEmitter<'_>, snapshot: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn config_saved(emitter: &SignalEmitter<'_>, saved: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn stats_updated(emitter: &SignalEmitter<'_>, stats: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn server_event(emitter: &SignalEmitter<'_>, event: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn health_changed(emitter: &SignalEmitter<'_>, report: &str) -> zbus::Result<()>;
	#[zbus(signal)]
	pub(super) async fn library_changed(emitter: &SignalEmitter<'_>, snapshot: &str) -> zbus::Result<()>;
}

/// Serve the interface on `bus` and forward hub events as signals until the
/// connection fails. Failure to connect or to own the name is logged; the
/// server (and its web interface) keeps running without D-Bus management.
pub(super) async fn serve(management: Arc<Management>, bus: zbus::Result<zbus::connection::Builder<'static>>) {
	// Subscribe before the name is visible, so no event published after a
	// client's first read can be missed.
	let events = management.hub.subscribe();
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
				 The web management interface is unaffected."
			);
			return;
		},
	};
	tracing::info!(
		name = BUS_NAME,
		"Desktop management interface available on the session bus"
	);
	let emitter = match SignalEmitter::new(&connection, OBJECT_PATH) {
		Ok(emitter) => emitter.into_owned(),
		Err(error) => {
			tracing::warn!("Cannot emit management signals: {error}");
			return;
		},
	};
	tokio::join!(follow_desktop(&connection, &management), forward(events, &emitter),);
}

/// Emit every hub event as its signal. History and update events have no
/// signal: only the web interface shows them.
async fn forward(mut events: broadcast::Receiver<super::hub::Event>, emitter: &SignalEmitter<'static>) {
	loop {
		let event = match events.recv().await {
			Ok(event) => event,
			Err(broadcast::error::RecvError::Lagged(missed)) => {
				tracing::debug!(missed, "D-Bus signals fell behind management events");
				continue;
			},
			Err(broadcast::error::RecvError::Closed) => return,
		};
		let json = &*event.json;
		let _ = match event.kind {
			EventKind::Session => ManagementInterface::session_changed(emitter, json).await,
			EventKind::Pairing => ManagementInterface::pairing_changed(emitter, json).await,
			EventKind::PairingRequested => ManagementInterface::pairing_requested(emitter, json).await,
			EventKind::PairingResolved => ManagementInterface::pairing_resolved(emitter, json).await,
			EventKind::Clients => ManagementInterface::clients_changed(emitter, json).await,
			EventKind::ConfigSaved => ManagementInterface::config_saved(emitter, json).await,
			EventKind::Stats => ManagementInterface::stats_updated(emitter, json).await,
			EventKind::ServerEvent => ManagementInterface::server_event(emitter, json).await,
			EventKind::Health => ManagementInterface::health_changed(emitter, json).await,
			EventKind::Library => ManagementInterface::library_changed(emitter, json).await,
			EventKind::History | EventKind::Update => Ok(()),
		};
	}
}

/// Track whether the desktop tray runs (it owns [`UI_BUS_NAME`]). While it
/// does, it shows pairing notifications, so the daemon's own fallback
/// notification is suppressed.
async fn follow_desktop(connection: &zbus::Connection, management: &Management) {
	let proxy = match zbus::fdo::DBusProxy::new(connection).await {
		Ok(proxy) => proxy,
		Err(error) => {
			tracing::warn!("Cannot follow the desktop tray's presence: {error}");
			return;
		},
	};
	let mut changes = match proxy.receive_name_owner_changed_with_args(&[(0, UI_BUS_NAME)]).await {
		Ok(changes) => changes,
		Err(error) => {
			tracing::warn!("Cannot follow the desktop tray's presence: {error}");
			return;
		},
	};
	let name = zbus::names::BusName::try_from(UI_BUS_NAME).expect("valid bus name");
	let set = |owned: bool| {
		if management.hub.desktop_present() != owned {
			tracing::debug!(present = owned, "Desktop tray presence changed");
		}
		management.clients.set_operator_ui_present(owned);
		management.hub.set_desktop_present(owned);
	};
	set(proxy.name_has_owner(name).await.unwrap_or(false));
	while let Some(change) = changes.next().await {
		if let Ok(args) = change.args() {
			set(args.new_owner().is_some());
		}
	}
	// The bus went away; nobody on it watches any more.
	set(false);
}
