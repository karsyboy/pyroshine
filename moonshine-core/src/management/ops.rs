//! Management operations shared by every transport.
//!
//! Each operation validates its arguments and calls the daemon's own
//! component: sessions end through `SessionManager`, pairing approval goes
//! through `ClientManager` with the exact request token, revocation is the
//! canonical revoke-and-stop path, configuration edits go through the
//! configuration store. Transports only authenticate the caller and map the
//! result; they never reimplement a rule. The operations run on the server's
//! Tokio runtime (the D-Bus adapter hands them over; the web server already
//! runs there).

use moonshine_management::ManagementError;
use moonshine_management::dto::{
	ClientsSnapshot, ConfigDocument, ConfigSaved, ConfigSchema, HealthReport, LibrarySnapshot, PairingSnapshot,
	SaveOutcome, ServerEvent, ServerInfo, SessionSnapshot, StreamStats, ValidationReport,
};

use super::hub::EventKind;
use super::{Management, json};
use crate::ShutdownReason;
use crate::clients::{RevokeError, normalize_client_label};

/// How long a restart waits after answering, so the caller gets the answer.
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// A certificate fingerprint as 64 lowercase hexadecimal digits.
pub(crate) fn fingerprint(value: &str) -> Result<String, ManagementError> {
	let value = value.trim().to_ascii_lowercase();
	if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		Ok(value)
	} else {
		Err(ManagementError::Invalid(
			"A certificate fingerprint is 64 hexadecimal digits.".into(),
		))
	}
}

/// Bounds for identifiers that come from a transport, before they are looked up.
fn bounded<'a>(value: &'a str, what: &str, max: usize) -> Result<&'a str, ManagementError> {
	if value.is_empty() || value.len() > max {
		return Err(ManagementError::Invalid(format!("A valid {what} is required.")));
	}
	Ok(value)
}

impl Management {
	pub(crate) fn op_server(&self) -> ServerInfo {
		self.server()
	}

	pub(crate) fn op_session(&self) -> SessionSnapshot {
		self.session.borrow().clone()
	}

	pub(crate) fn op_stats(&self) -> Option<StreamStats> {
		self.stats.borrow().clone()
	}

	pub(crate) fn op_events(&self) -> Vec<ServerEvent> {
		self.events()
	}

	/// End the session through the same teardown as Moonlight's quit
	/// (`/cancel`): the application unit is stopped and every worker joined.
	pub(crate) async fn op_end_session(&self, actor: &str) -> Result<(), ManagementError> {
		// Like `/cancel`, drain behind an in-progress revocation.
		let _authorization = self.clients.authorization_gate.read().await;
		tracing::info!(actor, "Ending the session at the host operator's request");
		self.sessions
			.stop_session()
			.await
			.map_err(|()| ManagementError::Failed("The session did not stop cleanly; see the Pyroshine log.".into()))
	}

	/// Restart the server: the normal shutdown (a session ends through its
	/// teardown), after which the process starts its executable again (see
	/// the server's `main`). The answer is sent before the shutdown begins.
	pub(crate) fn op_restart(&self, actor: &str) -> Result<(), ManagementError> {
		tracing::info!(actor, "Restarting at the host operator's request");
		let shutdown = self.shutdown.clone();
		self.runtime.spawn(async move {
			tokio::time::sleep(RESTART_DELAY).await;
			// Already shutting down: that shutdown wins.
			let _ = shutdown.trigger_shutdown(ShutdownReason::Restart);
		});
		Ok(())
	}

	/// Show the desktop tray again after it was hidden from its menu. Without
	/// a desktop session, with `[management] tray = false` or while the tray
	/// is shown, nothing changes.
	pub(crate) fn op_show_tray(&self) {
		self.tray_requests.notify_one();
	}

	pub(crate) fn op_pairing(&self) -> PairingSnapshot {
		self.pairing()
	}

	/// Approve the pending request `request` of `client_id` with the PIN the
	/// client shows. The PIN applies to that exact request only.
	pub(crate) fn op_approve_pairing(
		&self,
		client_id: &str,
		request: &str,
		pin: &str,
		label: &str,
		actor: &str,
	) -> Result<(), ManagementError> {
		let client_id = bounded(client_id, "client ID", 256)?;
		let request = bounded(request, "pairing request", 128)?;
		if pin.is_empty() || pin.len() > 16 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
			return Err(ManagementError::Invalid(
				"Enter the PIN shown by Moonlight (digits only).".into(),
			));
		}
		let label = normalize_client_label(label).map_err(|reason| ManagementError::Invalid(reason.into()))?;
		let pending = self.clients.pending_approval(client_id).ok_or_else(|| {
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
		self.clients
			.register_pin_with_label(client_id, pin, request, label)
			.map_err(|()| ManagementError::Failed("The PIN could not be applied to this request.".into()))?;
		tracing::info!(actor, requester = %pending.requester, "Pairing PIN entered through the management interface");
		Ok(())
	}

	pub(crate) fn op_reject_pairing(&self, client_id: &str, request: &str, actor: &str) -> Result<(), ManagementError> {
		let client_id = bounded(client_id, "client ID", 256)?;
		let request = bounded(request, "pairing request", 128)?;
		if self.clients.reject_pairing(client_id, request) {
			tracing::info!(actor, "Pairing request rejected through the management interface");
			Ok(())
		} else {
			Err(ManagementError::NotFound(
				"This pairing request no longer exists.".into(),
			))
		}
	}

	pub(crate) fn op_clients(&self) -> Result<ClientsSnapshot, ManagementError> {
		self.clients_snapshot()
			.map_err(|()| ManagementError::Failed("Pairing state is unavailable; see the Pyroshine log.".into()))
	}

	/// Revoke one certificate through the canonical revocation, which also
	/// ends any active session.
	pub(crate) async fn op_revoke_client(&self, fingerprint: &str, actor: &str) -> Result<(), ManagementError> {
		let fingerprint = self::fingerprint(fingerprint)?;
		match self
			.clients
			.revoke_and_stop_session(&self.sessions, None, Some(&fingerprint))
			.await
		{
			Ok(()) => {
				tracing::info!(actor, %fingerprint, "Client revoked through the management interface");
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

	pub(crate) fn op_rename_client(&self, fingerprint: &str, label: &str) -> Result<(), ManagementError> {
		let fingerprint = self::fingerprint(fingerprint)?;
		let label = normalize_client_label(label).map_err(|reason| ManagementError::Invalid(reason.into()))?;
		self.clients
			.set_client_label(&fingerprint, label)
			.map(|_| ())
			.map_err(|()| ManagementError::NotFound("This client is not paired.".into()))
	}

	pub(crate) fn op_config(&self) -> Result<ConfigDocument, ManagementError> {
		self.config.document()
	}

	pub(crate) fn op_config_schema(&self) -> ConfigSchema {
		super::schema::config_schema()
	}

	pub(crate) fn op_validate_config(&self, values: &str) -> Result<ValidationReport, ManagementError> {
		self.config.validate(values)
	}

	/// Persist a configuration. Nothing is reloaded: the outcome tells
	/// whether a restart is needed to apply it.
	pub(crate) async fn op_save_config(
		&self,
		values: &str,
		base_revision: &str,
		actor: &str,
	) -> Result<SaveOutcome, ManagementError> {
		let outcome = self.config.save(values, base_revision).await?;
		tracing::info!(
			actor,
			changed = outcome.changed_paths.len(),
			restart_required = outcome.restart_required,
			"Configuration saved through the management interface"
		);
		let saved = ConfigSaved {
			revision: outcome.revision.clone(),
			restart_required: outcome.restart_required,
		};
		self.hub.publish(EventKind::ConfigSaved, json(&saved));
		Ok(outcome)
	}

	/// Re-run the host checks and publish the result.
	pub(crate) async fn op_run_health_check(&self) -> HealthReport {
		let report = self.run_health_check().await;
		self.hub.publish(EventKind::Health, json(&report));
		report
	}

	pub(crate) async fn op_diagnostic_report(&self) -> String {
		self.redacted_diagnostic_report().await
	}

	/// A redacted support bundle (gzip-compressed tar).
	pub(crate) async fn op_support_bundle(&self) -> Result<Vec<u8>, ManagementError> {
		let bundle = self.support_bundle().await;
		if bundle.is_empty() {
			return Err(ManagementError::Failed(
				"The support bundle could not be collected; see the Pyroshine log.".into(),
			));
		}
		Ok(bundle)
	}

	/// A one-time code for the web interface's administrator setup (see `auth`).
	pub(crate) fn op_create_setup_code(&self, reset: bool) -> Result<String, ManagementError> {
		self.auth.create_setup_code(reset)
	}

	pub(crate) fn op_library(&self) -> LibrarySnapshot {
		self.library_snapshot()
	}

	/// The box art of the library entry `key` as clients get it (PNG,
	/// rendered and cached like `/appasset`), including entries clients are
	/// not offered.
	pub(crate) async fn op_library_artwork(&self, key: &str) -> Result<Vec<u8>, ManagementError> {
		let key = bounded(key, "application key", 1024)?;
		let candidates = {
			let catalog = self.library.catalog();
			let entry = catalog
				.find_key(key)
				.ok_or_else(|| ManagementError::NotFound("No such application.".into()))?;
			entry.artwork.clone()
		};
		if candidates.is_empty() {
			return Err(ManagementError::NotFound("This application has no artwork.".into()));
		}
		self.library
			.render_artwork(candidates)
			.await
			.map_err(|reason| ManagementError::NotFound(format!("No usable artwork: {reason}")))
	}

	/// Queue a scan of every source; library events follow its progress.
	pub(crate) fn op_rescan_library(&self) -> Result<(), ManagementError> {
		Ok(self.library.rescan()?)
	}

	pub(crate) async fn op_set_application_hidden(&self, key: &str, hidden: bool) -> Result<(), ManagementError> {
		if key.is_empty() || key.len() > 1024 {
			return Err(ManagementError::Invalid("An application key is required.".into()));
		}
		Ok(self.library.set_hidden(key, hidden).await?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fingerprints_are_normalized_and_bounded() {
		let upper = "AB".repeat(32);
		assert_eq!(fingerprint(&format!(" {upper} ")).unwrap(), "ab".repeat(32));
		assert!(matches!(fingerprint("abc"), Err(ManagementError::Invalid(_))));
		assert!(matches!(
			fingerprint(&"zz".repeat(32)),
			Err(ManagementError::Invalid(_))
		));
	}

	#[test]
	fn transport_identifiers_are_bounded_before_lookup() {
		assert!(bounded("", "client ID", 8).is_err());
		assert!(bounded("123456789", "client ID", 8).is_err());
		assert_eq!(bounded("abc", "client ID", 8).unwrap(), "abc");
	}
}
