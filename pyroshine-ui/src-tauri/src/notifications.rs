//! Desktop notifications through `org.freedesktop.Notifications`.
//!
//! While the UI runs it owns operator notifications (the daemon's own
//! fallback is suppressed). Pairing requests share one notification that is
//! replaced, not repeated, so a client retrying pairing cannot flood the
//! desktop; clicking it opens the pending request. Error notifications are
//! limited to one per minute and skipped while the window is visible.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use moonshine_management::dto::PairingSnapshot;
use serde_json::Value;
use tauri::AppHandle;
use tokio::sync::Mutex;
use zbus::zvariant::Value as Variant;

use crate::window;

#[zbus::proxy(
	interface = "org.freedesktop.Notifications",
	default_service = "org.freedesktop.Notifications",
	default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
	#[allow(clippy::too_many_arguments)]
	fn notify(
		&self,
		app_name: &str,
		replaces_id: u32,
		app_icon: &str,
		summary: &str,
		body: &str,
		actions: &[&str],
		hints: HashMap<&str, Variant<'_>>,
		expire_timeout: i32,
	) -> zbus::Result<u32>;

	fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

const ERROR_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct State {
	/// The pairing notification currently shown.
	pairing: Option<u32>,
	/// Requests the operator was already told about.
	notified: HashSet<String>,
	/// Request a click on the pairing notification opens.
	latest: Option<String>,
	error: Option<u32>,
	last_error: Option<Instant>,
}

struct Inner {
	app: AppHandle,
	proxy: NotificationsProxy<'static>,
	state: Mutex<State>,
}

impl Inner {
	/// Open the page of a clicked notification. The server sends the click's
	/// activation token (`ActivationToken`, notification spec 1.2) right
	/// before `ActionInvoked`; one ordered stream of both keeps them paired.
	async fn follow_clicks(&self, connection: zbus::Connection) {
		// The bus matches the sender against the server owning the name, so
		// other clients cannot fake clicks.
		let rule = zbus::MatchRule::builder()
			.msg_type(zbus::message::Type::Signal)
			.sender("org.freedesktop.Notifications")
			.and_then(|rule| rule.interface("org.freedesktop.Notifications"))
			.and_then(|rule| rule.path("/org/freedesktop/Notifications"))
			.map(|rule| rule.build());
		let stream = match rule {
			Ok(rule) => zbus::MessageStream::for_match_rule(rule, &connection, Some(32)).await,
			Err(error) => Err(error),
		};
		let mut stream = match stream {
			Ok(stream) => stream,
			Err(error) => {
				tracing::warn!("Cannot follow notification clicks: {error}");
				return;
			},
		};
		let mut tokens: HashMap<u32, String> = HashMap::new();
		while let Some(message) = stream.next().await {
			let Ok(message) = message else { continue };
			let header = message.header();
			let Some(member) = header.member() else { continue };
			let Ok((id, text)) = message.body().deserialize::<(u32, String)>() else {
				// NotificationClosed carries (id, reason).
				if member.as_str() == "NotificationClosed"
					&& let Ok((id, _)) = message.body().deserialize::<(u32, u32)>()
				{
					tokens.remove(&id);
				}
				continue;
			};
			let state = self.state.lock().await;
			let page = if Some(id) == state.pairing {
				match &state.latest {
					Some(request) => format!("clients?request={request}"),
					None => "clients".into(),
				}
			} else if Some(id) == state.error {
				"dashboard".into()
			} else {
				continue;
			};
			drop(state);
			match member.as_str() {
				"ActivationToken" => {
					tokens.insert(id, text);
				},
				"ActionInvoked" => {
					let token = tokens.remove(&id).and_then(|token| window::activation_token(&token));
					window::show(&self.app, &page, token);
				},
				_ => {},
			}
		}
	}
}

#[derive(Clone)]
pub struct Notifier(Option<Arc<Inner>>);

impl Notifier {
	pub fn new(app: AppHandle, connection: Option<zbus::Connection>) -> Self {
		let Some(connection) = connection else {
			return Self(None);
		};
		let proxy = match tauri::async_runtime::block_on(NotificationsProxy::new(&connection)) {
			Ok(proxy) => proxy,
			Err(error) => {
				tracing::warn!("Desktop notifications unavailable: {error}");
				return Self(None);
			},
		};
		let inner = Arc::new(Inner {
			app,
			proxy,
			state: Mutex::default(),
		});
		let listener = inner.clone();
		tauri::async_runtime::spawn(async move { listener.follow_clicks(connection).await });
		Self(Some(inner))
	}

	async fn notify(inner: &Inner, replaces: Option<u32>, summary: &str, body: &str, urgency: u8) -> Option<u32> {
		let hints = HashMap::from([
			("desktop-entry", Variant::from("pyroshine-ui")),
			("urgency", Variant::from(urgency)),
		]);
		match inner
			.proxy
			.notify(
				"Pyroshine",
				replaces.unwrap_or(0),
				"pyroshine",
				summary,
				body,
				&["default", "Open"],
				hints,
				-1,
			)
			.await
		{
			Ok(id) => Some(id),
			Err(error) => {
				tracing::warn!("Cannot show a notification: {error}");
				None
			},
		}
	}

	/// Follow the pending requests: notify about new ones, close the
	/// notification when none is left.
	pub async fn pairing(&self, snapshot: &PairingSnapshot) {
		let Some(inner) = &self.0 else { return };
		let waiting: Vec<_> = snapshot.requests.iter().filter(|request| !request.approved).collect();
		let mut state = inner.state.lock().await;
		state
			.notified
			.retain(|token| waiting.iter().any(|request| &request.request == token));
		if waiting.is_empty() {
			if let Some(id) = state.pairing.take() {
				let _ = inner.proxy.close_notification(id).await;
			}
			state.latest = None;
			return;
		}
		let fresh: Vec<_> = waiting
			.iter()
			.filter(|request| !state.notified.contains(&request.request))
			.collect();
		let Some(newest) = fresh.last() else {
			return;
		};
		let body = if waiting.len() == 1 {
			format!(
				"A Moonlight client at {} wants to pair. Enter the PIN it shows.",
				newest.requester
			)
		} else {
			format!(
				"{} clients want to pair. The newest is at {}.",
				waiting.len(),
				newest.requester
			)
		};
		state.latest = Some(newest.request.clone());
		for request in &fresh {
			state.notified.insert(request.request.clone());
		}
		let replaces = state.pairing;
		state.pairing = Self::notify(inner, replaces, "Pairing request", &body, 2)
			.await
			.or(replaces);
	}

	pub async fn clear_pairing(&self) {
		self.pairing(&PairingSnapshot {
			enabled: false,
			requests: Vec::new(),
		})
		.await;
	}

	/// Backend errors the operator should know about.
	pub async fn server_event(&self, event: &Value) {
		if event["level"] == "error" || event["level"] == "warning" {
			let message = event["message"].as_str().unwrap_or("Pyroshine reported a problem.");
			self.error("Pyroshine session ended", message).await;
		}
	}

	pub async fn error(&self, summary: &str, message: &str) {
		let Some(inner) = &self.0 else { return };
		if window::is_visible(&inner.app) {
			return;
		}
		let mut state = inner.state.lock().await;
		if state.last_error.is_some_and(|last| last.elapsed() < ERROR_INTERVAL) {
			return;
		}
		state.last_error = Some(Instant::now());
		let replaces = state.error;
		state.error = Self::notify(inner, replaces, summary, message, 1).await.or(replaces);
	}
}
