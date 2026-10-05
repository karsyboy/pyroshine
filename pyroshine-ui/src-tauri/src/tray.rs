//! System tray icon over the StatusNotifierItem D-Bus protocol (KDE Plasma
//! natively; GNOME with the AppIndicator extension; most other panels).

use std::collections::HashMap;
use std::sync::Arc;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, ToolTip, Tray, TrayMethods};
use moonshine_management::dto::{PairingSnapshot, SessionDetails, SessionPhase, SessionSnapshot};
use tauri::AppHandle;

use crate::daemon::{Daemon, Overview};
use crate::icons::{self, Badge};
use crate::notifications::Notifier;
use crate::window;

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
		let mut lines = vec![session.application.title.clone()];
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
	app: AppHandle,
	daemon: Arc<Daemon>,
	notifier: Notifier,
	view: View,
	icons: HashMap<Badge, Vec<Icon>>,
	/// Token the tray host provided for the click being handled.
	activation_token: Option<String>,
}

impl PyroTray {
	fn open(&mut self, page: &str) {
		window::show(&self.app, page, self.activation_token.take());
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
		self.activation_token = window::activation_token(&token);
	}

	/// Left click opens the window (at a waiting pairing request first).
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
				icon_name: "window-new".into(),
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
						let daemon = tray.daemon.clone();
						let notifier = tray.notifier.clone();
						tauri::async_runtime::spawn(async move {
							let result = match daemon.proxy().await {
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
				label: "_Quit Pyroshine UI".into(),
				icon_name: "application-exit".into(),
				activate: Box::new(|tray: &mut Self| tray.app.exit(0)),
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

/// Cheap, cloneable access to the running tray (absent when no session bus).
#[derive(Clone)]
pub struct TrayHandle(Option<ksni::Handle<PyroTray>>);

impl TrayHandle {
	async fn change(&self, change: impl FnOnce(&mut View) + Send) {
		if let Some(handle) = &self.0 {
			handle.update(|tray| change(&mut tray.view)).await;
		}
	}

	pub async fn update(&self, overview: &Overview) {
		let session = overview
			.session
			.as_ref()
			.and_then(|value| serde_json::from_value::<SessionSnapshot>(value.clone()).ok());
		let pairing = overview
			.pairing
			.as_ref()
			.and_then(|value| serde_json::from_value::<PairingSnapshot>(value.clone()).ok());
		self.change(|view| {
			view.connected = overview.connected;
			if let Some(session) = session {
				view.phase = session.phase;
				view.session = session.session;
			}
			if let Some(pairing) = pairing {
				view.requests = pairing.requests.into_iter().map(|request| request.request).collect();
			}
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
		let requests: Vec<_> = snapshot
			.requests
			.iter()
			.map(|request| request.request.clone())
			.collect();
		self.change(move |view| view.requests = requests).await;
	}

	pub async fn disconnected(&self) {
		self.change(|view| *view = View::default()).await;
	}
}

pub fn spawn(app: AppHandle, daemon: Arc<Daemon>, notifier: Notifier) -> TrayHandle {
	if daemon.connection().is_none() {
		return TrayHandle(None);
	}
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
		app,
		daemon,
		notifier,
		view: View::default(),
		icons,
		activation_token: None,
	};
	// The panel may start after the app (autostart), so a missing
	// StatusNotifierWatcher is not fatal: the icon appears once one does.
	match tauri::async_runtime::block_on(tray.assume_sni_available(true).spawn()) {
		Ok(handle) => TrayHandle(Some(handle)),
		Err(error) => {
			tracing::warn!("System tray unavailable: {error}");
			TrayHandle(None)
		},
	}
}
