//! The D-Bus object behind [`moonshine_management::INTERFACE`].
//!
//! Every method checks that the caller runs as the same user as the daemon.
//! The session bus already only admits that user; the check keeps a
//! misconfigured bus from widening access to pairing, revocation and the
//! configuration file.

use std::sync::Arc;

use moonshine_management::ManagementError;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;

use super::{Management, json};
use crate::clients::{RevokeError, normalize_client_label};

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

fn fingerprint(value: &str) -> Result<String, ManagementError> {
	let value = value.trim().to_ascii_lowercase();
	if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		Ok(value)
	} else {
		Err(ManagementError::Invalid(
			"A certificate fingerprint is 64 hexadecimal digits.".into(),
		))
	}
}

#[zbus::interface(name = "io.github.karsyboy.Pyroshine.Management1")]
impl ManagementInterface {
	async fn get_server(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.server()))
	}

	async fn get_session(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&*self.management.session.borrow()))
	}

	/// End the session through the same teardown as Moonlight's quit
	/// (`/cancel`): the application unit is stopped and every worker joined.
	async fn end_session(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(&self.management, async move {
			// Like `/cancel`, drain behind an in-progress revocation.
			let _authorization = management.clients.authorization_gate.read().await;
			tracing::info!("Ending the session at the host operator's request");
			management.sessions.stop_session().await.map_err(|()| {
				ManagementError::Failed("The session did not stop cleanly; see the Pyroshine log.".into())
			})
		})
		.await
	}

	async fn get_pairing(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.pairing()))
	}

	/// Approve the pending request `request` of `client_id` with the PIN the
	/// client shows. The PIN applies to that exact request only.
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
		if pin.is_empty() || pin.len() > 16 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
			return Err(ManagementError::Invalid(
				"Enter the PIN shown by Moonlight (digits only).".into(),
			));
		}
		let label = normalize_client_label(label).map_err(|reason| ManagementError::Invalid(reason.into()))?;
		let clients = &self.management.clients;
		let pending = clients.pending_approval(client_id).ok_or_else(|| {
			ManagementError::NotFound(
				"This pairing request expired or was cancelled. Start pairing again in Moonlight.".into(),
			)
		})?;
		if pending.approval != request {
			return Err(ManagementError::Conflict(
				"The client sent a new pairing request. Check the new request and enter its PIN.".into(),
			));
		}
		if pending.approved {
			return Err(ManagementError::Conflict(
				"A PIN was already entered for this request.".into(),
			));
		}
		clients
			.register_pin_with_label(client_id, pin, request, label)
			.map_err(|()| ManagementError::Failed("The PIN could not be applied to this request.".into()))?;
		tracing::info!(requester = %pending.requester, "Pairing PIN entered through the local management interface");
		Ok(())
	}

	async fn reject_pairing(
		&self,
		client_id: &str,
		request: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		if self.management.clients.reject_pairing(client_id, request) {
			tracing::info!("Pairing request rejected in the desktop app");
			Ok(())
		} else {
			Err(ManagementError::NotFound(
				"This pairing request no longer exists.".into(),
			))
		}
	}

	async fn get_clients(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management
			.clients_snapshot()
			.map(|clients| json(&clients))
			.map_err(|()| ManagementError::Failed("Pairing state is unavailable; see the Pyroshine log.".into()))
	}

	/// Revoke one certificate through the canonical revocation, which also
	/// ends any active session.
	async fn revoke_client(
		&self,
		fingerprint: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let fingerprint = self::fingerprint(fingerprint)?;
		let management = self.management.clone();
		let target = fingerprint.clone();
		let result = on_runtime(&self.management, async move {
			Ok(management
				.clients
				.revoke_and_stop_session(&management.sessions, None, Some(&target))
				.await)
		})
		.await?;
		match result {
			Ok(()) => {
				tracing::info!(%fingerprint, "Client revoked in the desktop app");
				Ok(())
			},
			Err(RevokeError::NotRevoked) => Err(ManagementError::NotFound(
				"This client is not paired (or pairing state could not be saved).".into(),
			)),
			Err(RevokeError::SessionStopFailed) => Err(ManagementError::Failed(
				"The client was revoked, but ending the active session failed.".into(),
			)),
		}
	}

	async fn rename_client(
		&self,
		fingerprint: &str,
		label: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		let fingerprint = self::fingerprint(fingerprint)?;
		let label = normalize_client_label(label).map_err(|reason| ManagementError::Invalid(reason.into()))?;
		self.management
			.clients
			.set_client_label(&fingerprint, label)
			.map(|_| ())
			.map_err(|()| ManagementError::NotFound("This client is not paired.".into()))
	}

	async fn get_config(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management.config.document().map(|document| json(&document))
	}

	async fn get_config_schema(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&super::schema::config_schema()))
	}

	async fn validate_config(
		&self,
		values: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		self.management.config.validate(values).map(|report| json(&report))
	}

	/// Persist a configuration. Nothing is reloaded: the outcome tells
	/// whether a restart is needed to apply it.
	async fn save_config(
		&self,
		values: &str,
		base_revision: &str,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
		#[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let (values, base_revision) = (values.to_string(), base_revision.to_string());
		let outcome = on_runtime(&self.management, async move {
			management.config.save(&values, &base_revision).await
		})
		.await?;
		let saved = moonshine_management::dto::ConfigSaved {
			revision: outcome.revision.clone(),
			restart_required: outcome.restart_required,
		};
		let _ = Self::config_saved(&emitter, &json(&saved)).await;
		Ok(json(&outcome))
	}

	async fn get_stats(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&*self.management.stats.borrow()))
	}

	async fn get_events(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.events()))
	}

	/// Re-run the host checks (see `healthcheck::recheck_in_server`) and
	/// publish the result with `HealthChanged`.
	async fn run_health_check(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
		#[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let report = on_runtime(&self.management, async move { Ok(management.run_health_check().await) }).await?;
		let report = json(&report);
		let _ = Self::health_changed(&emitter, &report).await;
		Ok(report)
	}

	async fn get_diagnostic_report(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		on_runtime(&self.management, async move {
			Ok(management.redacted_diagnostic_report().await)
		})
		.await
	}

	/// A redacted support bundle (gzip-compressed tar).
	async fn get_support_bundle(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<Vec<u8>, ManagementError> {
		authorize(connection, &header).await?;
		let management = self.management.clone();
		let bundle = on_runtime(&self.management, async move { Ok(management.support_bundle().await) }).await?;
		if bundle.is_empty() {
			return Err(ManagementError::Failed(
				"The support bundle could not be collected; see the Pyroshine log.".into(),
			));
		}
		Ok(bundle)
	}

	async fn get_library(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<String, ManagementError> {
		authorize(connection, &header).await?;
		Ok(json(&self.management.library_snapshot()))
	}

	/// Queue a scan of every source; `LibraryChanged` follows its progress.
	async fn rescan_library(
		&self,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		Ok(self.management.library.rescan()?)
	}

	async fn set_application_hidden(
		&self,
		key: &str,
		hidden: bool,
		#[zbus(connection)] connection: &zbus::Connection,
		#[zbus(header)] header: Header<'_>,
	) -> Result<(), ManagementError> {
		authorize(connection, &header).await?;
		if key.is_empty() || key.len() > 1024 {
			return Err(ManagementError::Invalid("An application key is required.".into()));
		}
		Ok(self.management.library.set_hidden(key, hidden).await?)
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
