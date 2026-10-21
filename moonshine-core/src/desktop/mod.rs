//! Managed outputs on an existing desktop. The independently supervised guard
//! owns topology and audio; capture continues directly through PipeWire.
mod audio;
pub(crate) mod grace;
mod guard;
mod native;
mod preferences;
mod wire;

use serde::{Deserialize, Serialize};

pub(crate) use guard::{cleanup_complete, recovery_pending};
pub use guard::{restore, run_guard, status, status_read_only};
pub(crate) use wire::Client;
pub(crate) type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
	pub width: u32,
	pub height: u32,
	pub refresh_millihertz: u32,
}

impl Mode {
	pub(crate) fn validate(self) -> Result<Self> {
		if self.width == 0
			|| self.height == 0
			|| self.width > 16384
			|| self.height > 16384
			|| self.refresh_millihertz == 0
			|| self.refresh_millihertz > 1_000_000
		{
			return Err("Unsupported desktop mode (maximum 16384 pixels per axis and 1000 Hz)".into());
		}
		Ok(self)
	}
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Output {
	pub key: String,
	pub name: String,
	pub enabled: bool,
	/// Some native virtual outputs can only be removed by closing their owner.
	#[serde(default = "default_can_disable")]
	pub can_disable: bool,
	pub mode: Mode,
	pub x: i32,
	pub y: i32,
	pub scale: f64,
	pub transform: u32,
	pub priority: u32,
	pub group: u32,
	pub hdr: u32,
	pub wcg: u32,
	pub replication: String,
}

fn default_can_disable() -> bool {
	true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
	pub outputs: Vec<Output>,
	pub layout_mode: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Target {
	pub id: String,
	pub node_id: u32,
	pub serial: Option<u64>,
	#[serde(default)]
	pub observed_mode: Option<Mode>,
	pub output: String,
	pub mode: Mode,
}

pub use moonshine_management::dto::DesktopStatus;
impl From<Mode> for moonshine_management::dto::DesktopOutputMode {
	fn from(mode: Mode) -> Self {
		Self {
			width: mode.width,
			height: mode.height,
			refresh_millihertz: mode.refresh_millihertz,
		}
	}
}

/// Compare observed output settings, allowing only normal refresh rounding.
fn matches(observed: &Output, expected: &Output) -> bool {
	observed.enabled == expected.enabled
		&& (!expected.enabled
			|| (observed.mode.width == expected.mode.width
				&& observed.mode.height == expected.mode.height
				&& observed
					.mode
					.refresh_millihertz
					.abs_diff(expected.mode.refresh_millihertz)
					<= 100 && observed.x == expected.x
				&& observed.y == expected.y
				&& (observed.scale - expected.scale).abs() < 0.001
				&& observed.transform == expected.transform
				&& observed.priority == expected.priority
				&& observed.hdr == expected.hdr
				&& observed.wcg == expected.wcg
				&& observed.replication == expected.replication))
}

/// Ask the host user for persistent input-only authorization, without taking
/// over displays. Capture selection and its old restore token are not reused.
pub async fn authorize() -> Result<()> {
	use crate::session::capture::portal::{dbus, token};
	let connection = zbus::Connection::session().await.map_err(|e| e.to_string())?;
	dbus::register(&connection, "pyroshine").await;
	let session = tokio::time::timeout(
		std::time::Duration::from_secs(45),
		dbus::PortalSession::start(&connection, dbus::SessionRequest { restore_token: None }),
	)
	.await
	.map_err(|_| "Desktop input authorization timed out")?
	.map_err(|e| e.to_string())?;
	let required = dbus::DEVICE_KEYBOARD | dbus::DEVICE_POINTER;
	if session.devices & required != required {
		return Err("Keyboard and pointer authorization is required".into());
	}
	if let (Some(path), Some(value)) = (token::default_path(), &session.restore_token) {
		token::store(&path, value);
	}
	session.close().await;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn rejects_invalid_modes_before_mutation() {
		for mode in [
			Mode::default(),
			Mode {
				width: 1920,
				height: 1080,
				refresh_millihertz: 0,
			},
			Mode {
				width: u32::MAX,
				height: 1080,
				refresh_millihertz: 60000,
			},
		] {
			assert!(mode.validate().is_err());
		}
		assert!(
			Mode {
				width: 2560,
				height: 1440,
				refresh_millihertz: 120000
			}
			.validate()
			.is_ok()
		);
	}
}
