//! Desktop notifications through `org.freedesktop.Notifications`.
//!
//! While the tray runs it owns operator notifications (the server's own
//! fallback is suppressed while the tray holds its bus name, so the desktop
//! never gets both). Pairing requests share one notification that is
//! replaced, not repeated, so a client retrying pairing cannot flood the
//! desktop; clicking it opens the pending request in the web interface.
//! Error notifications are limited to one per minute. Without the tray, the
//! web interface still lists pending requests and recent problems.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use moonshine_management::dto::PairingSnapshot;
use serde_json::Value;
use tokio::sync::Mutex;
use zbus::zvariant::Value as Variant;

use crate::service::Service;

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
	info: Option<u32>,
}

struct Inner {
	service: Arc<Service>,
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
				"diagnostics".into()
			} else if Some(id) == state.info {
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
					let token = tokens
						.remove(&id)
						.and_then(|token| crate::browser::activation_token(&token));
					if let Err(error) = self.service.open(&page, token).await {
						tracing::warn!("Cannot open the browser: {error}");
					}
				},
				_ => {},
			}
		}
	}
}

#[derive(Clone)]
pub struct Notifier(Option<Arc<Inner>>);

impl Notifier {
	pub async fn new(service: Arc<Service>) -> Self {
		let connection = service.connection().clone();
		let proxy = match NotificationsProxy::new(&connection).await {
			Ok(proxy) => proxy,
			Err(error) => {
				tracing::warn!("Desktop notifications unavailable: {error}");
				return Self(None);
			},
		};
		let inner = Arc::new(Inner {
			service,
			proxy,
			state: Mutex::default(),
		});
		let listener = inner.clone();
		tokio::spawn(async move { listener.follow_clicks(connection).await });
		Self(Some(inner))
	}

	async fn notify(inner: &Inner, replaces: Option<u32>, summary: &str, body: &str, urgency: u8) -> Option<u32> {
		let hints = HashMap::from([
			("desktop-entry", Variant::from("pyroshine")),
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
			let (summary, body) = server_event_text(event);
			self.error(summary, &body).await;
		}
	}

	/// A message the operator asked for (a setup code); replaces the last one.
	pub async fn info(&self, summary: &str, message: &str) {
		let Some(inner) = &self.0 else { return };
		let mut state = inner.state.lock().await;
		let replaces = state.info;
		state.info = Self::notify(inner, replaces, summary, message, 1).await.or(replaces);
	}

	pub async fn error(&self, summary: &str, message: &str) {
		let Some(inner) = &self.0 else { return };
		let mut state = inner.state.lock().await;
		if state.last_error.is_some_and(|last| last.elapsed() < ERROR_INTERVAL) {
			return;
		}
		state.last_error = Some(Instant::now());
		let replaces = state.error;
		state.error = Self::notify(inner, replaces, summary, message, 1).await.or(replaces);
	}
}

/// Codes of events that report how a session ended; every other event is a
/// launch or negotiation the daemon rejected.
const STOP_CODES: &[&str] = &[
	"application_stopped",
	"video_transport_failed",
	"video_encoder_failed",
	"audio_transport_failed",
	"audio_server_failed",
	"audio_encoder_failed",
	"control_stream_failed",
	"input_failed",
	"compositor_failed",
	"transition_failed",
	"service_shutdown",
];

/// Notification summary and body: the event's message, then what to do.
fn server_event_text(event: &Value) -> (&'static str, String) {
	let message = event["message"].as_str().unwrap_or("Pyroshine reported a problem.");
	let summary = if event["code"].as_str().is_some_and(|code| STOP_CODES.contains(&code)) {
		"Pyroshine session ended"
	} else {
		"Pyroshine could not start the stream"
	};
	let body = match event["hint"].as_str() {
		Some(hint) => format!("{message}\n{hint}"),
		None => message.to_string(),
	};
	(summary, body)
}

#[cfg(test)]
mod server_event_tests {
	use serde_json::json;

	#[test]
	fn notifications_name_the_kind_and_carry_the_hint() {
		let (summary, body) = super::server_event_text(&json!({
			"level": "error",
			"code": "codec_unsupported",
			"message": "The client requested AV1.",
			"hint": "Choose H.264.",
			"at_ms": 0,
		}));
		assert_eq!(summary, "Pyroshine could not start the stream");
		assert_eq!(body, "The client requested AV1.\nChoose H.264.");
		let (summary, body) = super::server_event_text(&json!({
			"level": "warning",
			"code": "application_stopped",
			"message": "Steam crashed (SIGSEGV, core dumped), 4 s after launch.",
			"at_ms": 0,
		}));
		assert_eq!(summary, "Pyroshine session ended");
		assert_eq!(body, "Steam crashed (SIGSEGV, core dumped), 4 s after launch.");
	}
}
