//! System tray icon over the StatusNotifierItem D-Bus protocol (KDE Plasma
//! natively; GNOME with the AppIndicator extension; most other panels).
//!
//! The icon and menu show the server's authoritative session state (no state
//! machine of their own). Clicks open web interface pages in the browser;
//! only End Session acts directly, through the server's management
//! interface, so it works without a browser.

use std::collections::HashMap;
use std::sync::Arc;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, ToolTip, Tray, TrayMethods};
use moonshine_management::dto::{PairingSnapshot, SessionDetails, SessionPhase, SessionSnapshot};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::icons::{self, Badge};
use crate::notifications::Notifier;
use crate::service::Service;

/// What the tray shows.
#[derive(Clone, Debug)]
struct View {
	connected: bool,
	phase: SessionPhase,
	session: Option<SessionDetails>,
	/// Pending pairing requests, newest last (token).
	requests: Vec<String>,
}

impl Default for View {
	fn default() -> Self {
		Self {
			connected: false,
			phase: SessionPhase::Idle,
			session: None,
			requests: Vec::new(),
		}
	}
}

impl View {
	fn badge(&self) -> Badge {
		if !self.connected {
			return Badge::Unavailable;
		}
		match self.phase {
			SessionPhase::Idle => Badge::None,
			SessionPhase::Starting | SessionPhase::Reconnecting => Badge::Busy,
			SessionPhase::Streaming => Badge::Streaming,
			SessionPhase::ClientDisconnected => Badge::Retained,
			SessionPhase::Stopping => Badge::Stopping,
			SessionPhase::Error => Badge::Error,
		}
	}

	fn status_line(&self) -> String {
		if !self.connected {
			return "Pyroshine is not running".into();
		}
		match self.phase {
			SessionPhase::Idle => "Ready for connections",
			SessionPhase::Starting => "Starting session…",
			SessionPhase::Streaming => "Streaming",
			SessionPhase::ClientDisconnected => "Session running · client disconnected",
			SessionPhase::Reconnecting => "Client reconnecting…",
			SessionPhase::Stopping => "Ending session…",
			SessionPhase::Error => "Session error · service restarting",
		}
		.into()
	}

	/// Application, mode and client of the live session.
	fn session_lines(&self) -> Vec<String> {
		let Some(session) = &self.session else {
			return Vec::new();
		};
		let mut lines = vec![
			session
				.foreground_application
				.as_ref()
				.map(|app| &app.title)
				.unwrap_or(&session.application.title)
				.clone(),
		];
		match &session.video {
			Some(video) => lines.push(format!(
				"{}×{} @ {} Hz · {}{}",
				video.width,
				video.height,
				video.fps,
				video.codec_label,
				if video.dynamic_range == "SDR" { "" } else { " · HDR" }
			)),
			None => lines.push(format!(
				"{}×{} @ {} Hz",
				session.requested.width, session.requested.height, session.requested.refresh_rate
			)),
		}
		lines.push(format!("Client {}", session.client_address));
		lines
	}
}

pub struct PyroTray {
	runtime: Handle,
	service: Arc<Service>,
	notifier: Notifier,
	quit: Arc<Notify>,
	view: View,
	icons: HashMap<Badge, Vec<Icon>>,
	/// Token the tray host provided for the click being handled.
	activation_token: Option<String>,
}

impl PyroTray {
	fn open(&mut self, page: &str) {
		let service = self.service.clone();
		let notifier = self.notifier.clone();
		let token = self.activation_token.take();
		let page = page.to_string();
		self.runtime.spawn(async move {
			if let Err(error) = service.open(&page, token).await {
				tracing::warn!("Cannot open the browser: {error}");
				notifier.error("Could not open Pyroshine in the browser", &error).await;
			}
		});
	}

	/// Ask the server for a web setup code and show it with the setup page.
	fn setup_web_access(&mut self) {
		let service = self.service.clone();
		let notifier = self.notifier.clone();
		let token = self.activation_token.take();
		self.runtime.spawn(async move {
			let code = match service.proxy().await {
				Ok(proxy) => proxy.create_web_setup_code(false).await.map_err(crate::service::UiError::from),
				Err(error) => Err(error),
			};
			match code {
				Ok(code) => {
					notifier
						.info(
							"Pyroshine setup code",
							&format!("Enter {code} in the browser within 15 minutes to choose the administrator password."),
						)
						.await;
					if let Err(error) = service.open("dashboard", token).await {
						notifier.error("Could not open Pyroshine in the browser", &error).await;
					}
				},
				Err(error) if error.kind == "conflict" => {
					notifier
						.info(
							"Web access is already set up",
							"Sign in with the administrator password. To replace a forgotten one, run pyroshine web setup --reset in a terminal.",
						)
						.await;
					let _ = service.open("dashboard", token).await;
				},
				Err(error) => notifier.error("Could not get a setup code", &error.message).await,
			}
		});
	}
}

impl Tray for PyroTray {
	fn id(&self) -> String {
		"pyroshine-ui".into()
	}

	fn title(&self) -> String {
		"Pyroshine".into()
	}

	fn category(&self) -> Category {
		Category::ApplicationStatus
	}

	fn status(&self) -> Status {
		if self.view.connected && !self.view.requests.is_empty() {
			Status::NeedsAttention
		} else {
			Status::Active
		}
	}

	fn icon_pixmap(&self) -> Vec<Icon> {
		self.icons[&self.view.badge()].clone()
	}

	fn attention_icon_pixmap(&self) -> Vec<Icon> {
		self.icons[&Badge::Pairing].clone()
	}

	fn tool_tip(&self) -> ToolTip {
		let mut description = self.view.session_lines();
		if !self.view.requests.is_empty() {
			description.push(pairing_label(self.view.requests.len()));
		}
		ToolTip {
			title: format!("Pyroshine — {}", self.view.status_line()),
			description: description.join("\n"),
			..Default::default()
		}
	}

	/// Plasma passes the activation token for a click on the item just
	/// before activating it.
	fn provide_xdg_activation_token(&mut self, token: String) {
		self.activation_token = crate::browser::activation_token(&token);
	}

	/// Left click opens the dashboard (at a waiting pairing request first).
	fn activate(&mut self, _x: i32, _y: i32) {
		let page = match self.view.requests.last() {
			Some(request) => format!("clients?request={request}"),
			None => "dashboard".into(),
		};
		self.open(&page);
	}

	fn menu(&self) -> Vec<MenuItem<Self>> {
		let info = |label: String| -> MenuItem<Self> {
			StandardItem {
				label: label.replace('_', "__"),
				enabled: false,
				..Default::default()
			}
			.into()
		};
		let mut items = vec![info(self.view.status_line())];
		items.extend(self.view.session_lines().into_iter().map(info));
		items.push(MenuItem::Separator);
		items.push(
			StandardItem {
				label: "_Open Pyroshine".into(),
				icon_name: "internet-web-browser".into(),
				activate: Box::new(|tray: &mut Self| tray.open("dashboard")),
				..Default::default()
			}
			.into(),
		);
		let pairing = match self.view.requests.last() {
			Some(request) => {
				let page = format!("clients?request={request}");
				StandardItem {
					label: format!("{}…", pairing_label(self.view.requests.len())),
					icon_name: "dialog-password".into(),
					activate: Box::new(move |tray: &mut Self| tray.open(&page)),
					..Default::default()
				}
			},
			None => StandardItem {
				label: "_Pair a Client…".into(),
				icon_name: "list-add".into(),
				activate: Box::new(|tray: &mut Self| tray.open("clients")),
				..Default::default()
			},
		};
		items.push(pairing.into());
		if self.view.connected && self.view.phase.can_end() {
			let application = self
				.view
				.session
				.as_ref()
				.map(|session| session.application.title.replace('_', "__"))
				.unwrap_or_else(|| "the application".into());
			items.push(MenuItem::Separator);
			items.push(
				StandardItem {
					label: format!("_End Session (closes {application})"),
					icon_name: "media-playback-stop".into(),
					activate: Box::new(|tray: &mut Self| {
						let service = tray.service.clone();
						let notifier = tray.notifier.clone();
						tray.runtime.spawn(async move {
							let result = match service.proxy().await {
								Ok(proxy) => proxy.end_session().await.map_err(Into::into),
								Err(error) => Err(error),
							};
							if let Err(error) = result {
								notifier.error("Could not end the session", &error.message).await;
							}
						});
					}),
					..Default::default()
				}
				.into(),
			);
		}
		items.push(MenuItem::Separator);
		items.push(
			StandardItem {
				label: "Session _History".into(),
				icon_name: "document-open-recent".into(),
				activate: Box::new(|tray: &mut Self| tray.open("history")),
				..Default::default()
			}
			.into(),
		);
		items.push(
			StandardItem {
				label: "_Updates".into(),
				icon_name: "system-software-update".into(),
				activate: Box::new(|tray: &mut Self| tray.open("updates")),
				..Default::default()
			}
			.into(),
		);
		items.push(
			StandardItem {
				label: "Set Up _Web Access…".into(),
				icon_name: "dialog-password".into(),
				enabled: self.view.connected,
				activate: Box::new(|tray: &mut Self| tray.setup_web_access()),
				..Default::default()
			}
			.into(),
		);
		items.push(MenuItem::Separator);
		items.push(
			StandardItem {
				// Only the tray exits: the server, its web interface and any
				// stream keep running.
				label: "_Quit Pyroshine Tray".into(),
				icon_name: "application-exit".into(),
				activate: Box::new(|tray: &mut Self| tray.quit.notify_one()),
				..Default::default()
			}
			.into(),
		);
		items
	}
}

fn pairing_label(count: usize) -> String {
	match count {
		1 => "Pairing request waiting".into(),
		count => format!("{count} pairing requests waiting"),
	}
}

/// Cheap, cloneable access to the running tray (absent without a tray host
/// connection).
#[derive(Clone)]
pub struct TrayHandle(Option<ksni::Handle<PyroTray>>);

impl TrayHandle {
	async fn change(&self, change: impl FnOnce(&mut View) + Send) {
		if let Some(handle) = &self.0 {
			handle.update(|tray| change(&mut tray.view)).await;
		}
	}

	pub async fn connected(&self, session: &SessionSnapshot, pairing: &PairingSnapshot) {
		let (session, requests) = (session.clone(), requests(pairing));
		self.change(move |view| {
			view.connected = true;
			view.phase = session.phase;
			view.session = session.session;
			view.requests = requests;
		})
		.await;
	}

	pub async fn session(&self, snapshot: &SessionSnapshot) {
		let snapshot = snapshot.clone();
		self.change(move |view| {
			view.phase = snapshot.phase;
			view.session = snapshot.session;
		})
		.await;
	}

	pub async fn pairing(&self, snapshot: &PairingSnapshot) {
		let requests = requests(snapshot);
		self.change(move |view| view.requests = requests).await;
	}

	pub async fn disconnected(&self) {
		self.change(|view| *view = View::default()).await;
	}
}

/// Request tokens waiting for the operator, newest last.
fn requests(snapshot: &PairingSnapshot) -> Vec<String> {
	snapshot
		.requests
		.iter()
		.filter(|request| !request.approved)
		.map(|request| request.request.clone())
		.collect()
}

pub async fn spawn(service: Arc<Service>, notifier: Notifier, quit: Arc<Notify>) -> TrayHandle {
	let icons = [
		Badge::None,
		Badge::Streaming,
		Badge::Retained,
		Badge::Busy,
		Badge::Stopping,
		Badge::Error,
		Badge::Pairing,
		Badge::Unavailable,
	]
	.into_iter()
	.map(|badge| (badge, icons::pixmaps(badge)))
	.collect();
	let tray = PyroTray {
		runtime: Handle::current(),
		service,
		notifier,
		quit,
		view: View::default(),
		icons,
		activation_token: None,
	};
	// The panel may start after the tray (autostart), so a missing
	// StatusNotifierWatcher is not fatal: the icon appears once one does.
	match tray.assume_sni_available(true).spawn().await {
		Ok(handle) => TrayHandle(Some(handle)),
		Err(error) => {
			tracing::warn!("System tray unavailable: {error}");
			TrayHandle(None)
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn retained_session_lines_prefer_foreground_and_fall_back_to_launch_entry() {
		let snapshot: SessionSnapshot =
			serde_json::from_str(include_str!("../../pyroshine-ui/src/api/session.fixture.json")).unwrap();
		let mut view = View {
			connected: true,
			phase: snapshot.phase,
			session: snapshot.session,
			..Default::default()
		};
		assert_eq!(view.session_lines()[0], "Grim Dawn");
		assert_eq!(view.badge(), Badge::Retained);
		let session = view.session.as_mut().unwrap();
		assert_eq!(session.application.title, "Steam");
		assert_eq!(session.application.id, 42);
		session.foreground_application = None;
		assert_eq!(view.session_lines()[0], "Steam");
	}
}
