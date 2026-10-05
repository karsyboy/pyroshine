//! Single instance and deep links through the UI's session-bus name.
//!
//! Owning [`UI_BUS_NAME`] also tells the daemon that a desktop UI runs, so it
//! leaves operator notifications to the UI and enables live statistics. The
//! bus releases the name when the process exits or crashes.

use std::time::Duration;

use moonshine_management::{UI_BUS_NAME, UI_OBJECT_PATH, UiProxy};
use tokio::sync::mpsc;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

/// A page another process asked for, with the activation token of the user
/// action that asked (see `window`).
pub type ShowRequest = (String, Option<String>);

pub enum Instance {
	/// This process is the UI; `requests` receives pages other launches ask for.
	Primary {
		connection: zbus::Connection,
		requests: mpsc::UnboundedReceiver<ShowRequest>,
	},
	/// Another instance runs (and was asked to show the page, if any).
	Forwarded,
	NoBus(zbus::Error),
}

struct UiObject {
	requests: mpsc::UnboundedSender<ShowRequest>,
}

#[zbus::interface(name = "io.github.karsyboy.PyroshineUi1")]
impl UiObject {
	fn show(&self, page: &str) {
		let _ = self.requests.send((page.to_string(), None));
	}

	fn activate(&self, page: &str, activation_token: &str) {
		let _ = self
			.requests
			.send((page.to_string(), crate::window::activation_token(activation_token)));
	}
}

/// Become the UI instance, or forward `page` (with `activation`, this
/// launch's token) to the running one.
pub async fn acquire(page: Option<&str>, activation: Option<&str>) -> Instance {
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
					let shown = match activation {
						Some(token) => match proxy.activate(page, token).await {
							// An instance from before Activate existed.
							Err(zbus::Error::MethodError(..)) => proxy.show(page).await,
							result => result,
						},
						None => proxy.show(page).await,
					};
					if let Err(error) = shown {
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
