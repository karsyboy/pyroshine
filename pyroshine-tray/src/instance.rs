//! One tray per desktop session, through the tray's session-bus name.
//!
//! Owning [`UI_BUS_NAME`] also tells the server that a tray runs, so it
//! leaves pairing notifications to the tray. The bus releases the name when
//! the process exits or crashes, so a restarted tray takes over cleanly and
//! a second autostart exits at once. Other processes (the application
//! launcher) ask the running tray to open a page with the click's activation
//! token.

use std::time::Duration;

use moonshine_management::{UI_BUS_NAME, UI_OBJECT_PATH, UiProxy};
use tokio::sync::mpsc;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

/// A page another process asked for, with the activation token of the user
/// action that asked.
pub type ShowRequest = (String, Option<String>);

pub enum Instance {
	/// This process is the tray; `requests` receives pages other launches ask for.
	Primary {
		connection: zbus::Connection,
		requests: mpsc::UnboundedReceiver<ShowRequest>,
	},
	/// Another tray runs in this session.
	Running,
}

struct TrayObject {
	requests: mpsc::UnboundedSender<ShowRequest>,
}

#[zbus::interface(name = "io.github.karsyboy.PyroshineUi1")]
impl TrayObject {
	fn show(&self, page: &str) {
		let _ = self.requests.send((crate::browser::page(page), None));
	}

	fn activate(&self, page: &str, activation_token: &str) {
		let _ = self.requests.send((
			crate::browser::page(page),
			crate::browser::activation_token(activation_token),
		));
	}
}

pub async fn session_bus() -> zbus::Result<zbus::Connection> {
	zbus::connection::Builder::session()?
		.method_timeout(Duration::from_secs(30))
		.build()
		.await
}

/// The bus the server's management interface is on: the session bus, or
/// `PYROSHINE_MANAGEMENT_BUS` (a D-Bus address) to follow a test instance
/// that serves on its own bus (`MOONSHINE_MANAGEMENT_BUS`) beside a live one.
/// The icon, notifications and the browser portal stay on the session bus.
pub async fn management_bus(session: &zbus::Connection) -> zbus::Result<zbus::Connection> {
	match std::env::var("PYROSHINE_MANAGEMENT_BUS") {
		Ok(address) => {
			zbus::connection::Builder::address(address.as_str())?
				.method_timeout(Duration::from_secs(30))
				.build()
				.await
		},
		Err(_) => Ok(session.clone()),
	}
}

/// Become the session's tray, or report that one runs.
pub async fn acquire(connection: zbus::Connection) -> zbus::Result<Instance> {
	let (sender, requests) = mpsc::unbounded_channel();
	connection
		.object_server()
		.at(UI_OBJECT_PATH, TrayObject { requests: sender })
		.await?;
	match connection
		.request_name_with_flags(UI_BUS_NAME, RequestNameFlags::DoNotQueue.into())
		.await
	{
		Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
			Ok(Instance::Primary { connection, requests })
		},
		Ok(_) | Err(zbus::Error::NameTaken) => Ok(Instance::Running),
		Err(error) => Err(error),
	}
}

/// Ask a running tray to open `page`. False when no tray answers.
pub async fn forward(connection: &zbus::Connection, page: &str, activation: Option<&str>) -> bool {
	let dbus = match zbus::fdo::DBusProxy::new(connection).await {
		Ok(dbus) => dbus,
		Err(_) => return false,
	};
	let name = zbus::names::BusName::try_from(UI_BUS_NAME).expect("valid bus name");
	if !dbus.name_has_owner(name).await.unwrap_or(false) {
		return false;
	}
	let Ok(proxy) = UiProxy::new(connection).await else {
		return false;
	};
	let shown = match activation {
		Some(token) => proxy.activate(page, token).await,
		None => proxy.show(page).await,
	};
	shown.is_ok()
}
