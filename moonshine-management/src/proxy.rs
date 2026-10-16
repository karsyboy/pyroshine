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

	/// Recent [`crate::dto::ServerEvent`]s, oldest first, as a JSON array.
	fn get_events(&self) -> zbus::Result<String>;
	/// Run the host checks again and return the [`crate::dto::HealthReport`].
	/// While a session runs, checks that would disturb it (GPU encoder
	/// probes, port binding) report their startup result instead.
	fn run_health_check(&self) -> zbus::Result<String>;
	/// [`crate::dto::DiagnosticReport`].
	fn get_diagnostic_report(&self) -> zbus::Result<String>;
	/// A redacted support bundle: a gzip-compressed tar archive with the
	/// diagnostic report, recent statistics, host and GPU details, the
	/// configuration and the service log. See `pyroshine diag`.
	fn get_support_bundle(&self) -> zbus::Result<Vec<u8>>;

	/// [`crate::dto::LibrarySnapshot`].
	fn get_library(&self) -> zbus::Result<String>;
	/// Scan every application source now. Returns once the scan is queued;
	/// `LibraryChanged` reports its progress and result.
	fn rescan_library(&self) -> zbus::Result<()>;
	/// Hide (or show again) a discovered application from clients. The
	/// choice persists across restarts. Manual entries cannot be hidden.
	fn set_application_hidden(&self, key: &str, hidden: bool) -> zbus::Result<()>;

	/// The web management interface's address on this host (for example
	/// `http://127.0.0.1:47990/`), or an empty string when it is disabled.
	fn get_web_url(&self) -> zbus::Result<String>;
	/// A one-time code that sets the web administrator password within 15
	/// minutes. Fails with `Conflict` once a password is set, unless `reset`
	/// (which also signs out every browser when the code is used).
	fn create_web_setup_code(&self, reset: bool) -> zbus::Result<String>;

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
	/// A new [`crate::dto::HealthReport`] (after `RunHealthCheck`).
	#[zbus(signal)]
	fn health_changed(&self, report: String) -> zbus::Result<()>;
	/// A new [`crate::dto::LibrarySnapshot`] (scan started or finished,
	/// catalog changed). Sent while a web interface page is open.
	#[zbus(signal)]
	fn library_changed(&self, snapshot: String) -> zbus::Result<()>;
}

/// Proxy for the running desktop tray.
#[zbus::proxy(
	interface = "io.github.karsyboy.PyroshineUi1",
	default_service = "io.github.karsyboy.PyroshineUi",
	default_path = "/io/github/karsyboy/PyroshineUi"
)]
pub trait Ui {
	/// Open the web interface at `page` (for example `dashboard` or
	/// `clients?request=<token>`) in the default browser.
	fn show(&self, page: &str) -> zbus::Result<()>;
	/// [`Self::show`], passing the XDG activation token the caller received
	/// for the user's action, so the browser may take focus on Wayland.
	fn activate(&self, page: &str, activation_token: &str) -> zbus::Result<()>;
}
