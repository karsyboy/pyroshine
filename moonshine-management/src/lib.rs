//! The management contract shared by the Pyroshine daemon, its web
//! interface, the desktop tray and the `pyroshine pair`/`diag`/`web`
//! commands.
//!
//! The documents are the [`dto`] types: the web interface's JSON API
//! (`/api/v1/`) returns them, and so does the daemon's object on the user's
//! D-Bus session bus. The session bus is private to the user that runs
//! Pyroshine, so the D-Bus interface is a host-local, same-user control
//! surface for the tray and the commands; it is never reachable from the
//! GameStream listeners. Methods take scalar arguments and return JSON
//! documents, so every side agrees on one versioned contract. Signals carry
//! the same documents, which lets the tray follow state without polling.
//!
//! The API exposes intentional operations and sanitized views only: no
//! session keys, pairing secrets, PINs or private keys cross it.

pub mod dto;
mod proxy;

pub use proxy::{ManagementProxy, UiProxy};

/// Well-known name the daemon owns on the session bus.
pub const BUS_NAME: &str = "io.github.karsyboy.Pyroshine";
/// Object path of the management object.
pub const OBJECT_PATH: &str = "/io/github/karsyboy/Pyroshine";
/// Management interface name. The trailing number is the API generation.
pub const INTERFACE: &str = "io.github.karsyboy.Pyroshine.Management1";
/// Version reported by `GetServer`; additive changes keep it.
pub const API_VERSION: u32 = 1;

/// Name the desktop tray owns while it runs (one per session). Its presence
/// tells the daemon that the tray shows pairing notifications.
pub const UI_BUS_NAME: &str = "io.github.karsyboy.PyroshineUi";
/// Object path of the tray's activation object.
pub const UI_OBJECT_PATH: &str = "/io/github/karsyboy/PyroshineUi";
/// Tray activation interface: open a web interface page in the browser.
pub const UI_INTERFACE: &str = "io.github.karsyboy.PyroshineUi1";

/// Name of a support bundle collected at `unix_seconds`:
/// `pyroshine-diag-YYYYMMDD-HHMMSS` in UTC. It is the archive's top-level
/// directory and the stem of the file the daemon's callers save it as.
pub fn support_bundle_name(unix_seconds: u64) -> String {
	let (days, time) = ((unix_seconds / 86_400) as i64, unix_seconds % 86_400);
	// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = doy - (153 * mp + 2) / 5 + 1;
	let month = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = yoe + era * 400 + i64::from(month <= 2);
	format!(
		"pyroshine-diag-{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
		time / 3600,
		time / 60 % 60,
		time % 60
	)
}

/// D-Bus error names returned by the management interface.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.karsyboy.Pyroshine.Error")]
pub enum ManagementError {
	#[zbus(error)]
	ZBus(zbus::Error),
	/// The caller is not allowed to use the management interface.
	AccessDenied(String),
	/// The request was well formed but its values are invalid.
	Invalid(String),
	/// The target (pairing request, client, session) does not exist (anymore).
	NotFound(String),
	/// The resource changed since the caller read it.
	Conflict(String),
	/// The configuration file cannot be written by the daemon.
	ReadOnly(String),
	/// The operation was attempted and failed.
	Failed(String),
}

impl ManagementError {
	/// Human-readable message, without the D-Bus error name.
	pub fn message(&self) -> String {
		match self {
			Self::ZBus(zbus::Error::MethodError(_, Some(message), _)) => message.clone(),
			Self::ZBus(error) => error.to_string(),
			Self::AccessDenied(message)
			| Self::Invalid(message)
			| Self::NotFound(message)
			| Self::Conflict(message)
			| Self::ReadOnly(message)
			| Self::Failed(message) => message.clone(),
		}
	}

	/// Short machine-readable kind, used by the UI to choose a presentation.
	pub fn kind(&self) -> &'static str {
		match self {
			Self::ZBus(_) => "unavailable",
			Self::AccessDenied(_) => "access_denied",
			Self::Invalid(_) => "invalid",
			Self::NotFound(_) => "not_found",
			Self::Conflict(_) => "conflict",
			Self::ReadOnly(_) => "read_only",
			Self::Failed(_) => "failed",
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn support_bundles_are_named_by_utc_time() {
		assert_eq!(support_bundle_name(1_791_993_542), "pyroshine-diag-20261014-155902");
		assert_eq!(support_bundle_name(0), "pyroshine-diag-19700101-000000");
		assert_eq!(support_bundle_name(951_782_400), "pyroshine-diag-20000229-000000");
	}

	#[test]
	fn error_names_round_trip_through_dbus_errors() {
		let error = ManagementError::Conflict("changed".into());
		let name = zbus::DBusError::name(&error).to_string();
		assert_eq!(name, "io.github.karsyboy.Pyroshine.Error.Conflict");
		assert_eq!(error.message(), "changed");
		assert_eq!(error.kind(), "conflict");
	}
}
