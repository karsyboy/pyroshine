//! Client-side proxies. The daemon implements the same method and signal
//! names; every structured value is a JSON document described in [`crate::dto`].

/// Proxy for the daemon's management interface.
#[zbus::proxy(
	interface = "io.github.karsyboy.Pyroshine.Management1",
	default_service = "io.github.karsyboy.Pyroshine",
	default_path = "/io/github/karsyboy/Pyroshine"
)]
pub trait Management {
	/// [`crate::dto::ServerInfo`].
	fn get_server(&self) -> zbus::Result<String>;
	/// [`crate::dto::SessionSnapshot`].
	fn get_session(&self) -> zbus::Result<String>;
	/// End the current session through the daemon's canonical teardown. This
	/// also stops the launched application.
	fn end_session(&self) -> zbus::Result<()>;

	/// [`crate::dto::PairingSnapshot`].
	fn get_pairing(&self) -> zbus::Result<String>;
	/// Approve exactly the pending request identified by `request` with the
	/// PIN shown by the client. `label` optionally names the client once paired.
	fn approve_pairing(&self, client_id: &str, request: &str, pin: &str, label: &str) -> zbus::Result<()>;
	/// Reject exactly the pending request identified by `request`.
	fn reject_pairing(&self, client_id: &str, request: &str) -> zbus::Result<()>;

	/// [`crate::dto::ClientsSnapshot`].
	fn get_clients(&self) -> zbus::Result<String>;
	/// Revoke one paired certificate (and its known client IDs).
	fn revoke_client(&self, fingerprint: &str) -> zbus::Result<()>;
	/// Set (or clear, with an empty string) the operator's name for a client.
	fn rename_client(&self, fingerprint: &str, label: &str) -> zbus::Result<()>;

	/// [`crate::dto::ConfigDocument`].
	fn get_config(&self) -> zbus::Result<String>;
	/// [`crate::dto::ConfigSchema`].
	fn get_config_schema(&self) -> zbus::Result<String>;
	/// Validate a complete configuration value without writing it;
	/// returns [`crate::dto::ValidationReport`].
	fn validate_config(&self, values: &str) -> zbus::Result<String>;
	/// Validate and persist a complete configuration value; returns
	/// [`crate::dto::SaveOutcome`]. Fails with `Conflict` if the file changed
	/// since `base_revision` was read.
	fn save_config(&self, values: &str, base_revision: &str) -> zbus::Result<String>;

	/// Latest [`crate::dto::StreamStats`], or JSON `null`.
	fn get_stats(&self) -> zbus::Result<String>;

	#[zbus(signal)]
	fn session_changed(&self, snapshot: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn pairing_changed(&self, snapshot: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn pairing_requested(&self, request: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn pairing_resolved(&self, resolution: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn clients_changed(&self, snapshot: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn config_saved(&self, saved: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn stats_updated(&self, stats: String) -> zbus::Result<()>;
	#[zbus(signal)]
	fn server_event(&self, event: String) -> zbus::Result<()>;
}

/// Proxy for a running desktop UI instance.
#[zbus::proxy(
	interface = "io.github.karsyboy.PyroshineUi1",
	default_service = "io.github.karsyboy.PyroshineUi",
	default_path = "/io/github/karsyboy/PyroshineUi"
)]
pub trait Ui {
	/// Show the main window at `page` (for example `dashboard` or
	/// `clients?request=<token>`).
	fn show(&self, page: &str) -> zbus::Result<()>;
	/// [`Self::show`], activating the window with the XDG activation token
	/// the caller received for the user's action (Wayland focus requires one).
	fn activate(&self, page: &str, activation_token: &str) -> zbus::Result<()>;
}
