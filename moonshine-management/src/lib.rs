//! Local management API shared by the Pyroshine daemon and its optional
//! desktop UI.
//!
//! The daemon exports one object on the user's D-Bus session bus. The session
//! bus is private to the user that runs Pyroshine, so this is a host-local,
//! same-user control surface; it is never reachable from the GameStream
//! listeners. Methods take scalar arguments and return JSON documents whose
//! shapes are the [`dto`] types, so both sides agree on one versioned
//! contract. Signals carry the same documents, which lets the UI follow state
//! without polling.
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

/// Name the desktop UI owns while it runs. Its presence tells the daemon
/// that pairing notifications and live statistics have a consumer.
pub const UI_BUS_NAME: &str = "io.github.karsyboy.PyroshineUi";
/// Object path of the UI's activation object.
pub const UI_OBJECT_PATH: &str = "/io/github/karsyboy/PyroshineUi";
/// UI activation interface (single instance, deep links from notifications).
pub const UI_INTERFACE: &str = "io.github.karsyboy.PyroshineUi1";

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
	fn error_names_round_trip_through_dbus_errors() {
		let error = ManagementError::Conflict("changed".into());
		let name = zbus::DBusError::name(&error).to_string();
		assert_eq!(name, "io.github.karsyboy.Pyroshine.Error.Conflict");
		assert_eq!(error.message(), "changed");
		assert_eq!(error.kind(), "conflict");
	}
}
