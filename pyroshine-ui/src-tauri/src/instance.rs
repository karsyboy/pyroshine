//! Single instance and deep links through the UI's session-bus name.
//!
//! Owning [`UI_BUS_NAME`] also tells the daemon that a desktop UI runs, so it
//! leaves operator notifications to the UI and enables live statistics. The
//! bus releases the name when the process exits or crashes.

use std::time::Duration;

use moonshine_management::{UI_BUS_NAME, UI_OBJECT_PATH, UiProxy};
use tokio::sync::mpsc;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

pub enum Instance {
	/// This process is the UI; `requests` receives pages other launches ask for.
	Primary {
		connection: zbus::Connection,
		requests: mpsc::UnboundedReceiver<String>,
	},
	/// Another instance runs (and was asked to show the page, if any).
	Forwarded,
	NoBus(zbus::Error),
}

struct UiObject {
	requests: mpsc::UnboundedSender<String>,
}

#[zbus::interface(name = "io.github.karsyboy.PyroshineUi1")]
impl UiObject {
	fn show(&self, page: &str) {
		let _ = self.requests.send(page.to_string());
	}
}

pub async fn acquire(page: Option<&str>) -> Instance {
	let (sender, requests) = mpsc::unbounded_channel();
	let connection = match zbus::connection::Builder::session()
		.map(|builder| builder.method_timeout(Duration::from_secs(30)))
		.and_then(|builder| builder.serve_at(UI_OBJECT_PATH, UiObject { requests: sender }))
	{
		Ok(builder) => builder.build().await,
		Err(error) => Err(error),
	};
	let connection = match connection {
		Ok(connection) => connection,
		Err(error) => return Instance::NoBus(error),
	};
	match connection
		.request_name_with_flags(UI_BUS_NAME, RequestNameFlags::DoNotQueue.into())
		.await
	{
		Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
			Instance::Primary { connection, requests }
		},
		Ok(_) | Err(zbus::Error::NameTaken) => {
			let Some(page) = page else {
				return Instance::Forwarded;
			};
			match UiProxy::new(&connection).await {
				Ok(proxy) => {
					if let Err(error) = proxy.show(page).await {
						eprintln!("Pyroshine is already open but did not respond: {error}");
					}
				},
				Err(error) => eprintln!("Pyroshine is already open but cannot be reached: {error}"),
			}
			Instance::Forwarded
		},
		Err(error) => Instance::NoBus(error),
	}
}
