//! `GET /api/v1/stream`: live state as Server-Sent Events.
//!
//! A stream starts with an `overview` event (the complete state, read after
//! subscribing to the hub, so no change falls between) and then carries every
//! hub event under its name (`session`, `pairing`, `stats`, …), each with an
//! increasing `id`. A browser that reconnects (`EventSource` does so on its
//! own) gets a fresh `overview`, which replaces whatever it missed; the same
//! happens when a stream falls behind the hub. Comments every
//! [`HEARTBEAT`] keep proxies from closing an idle stream and let the server
//! notice a closed tab.
//!
//! A stream is bound to its session: when the session ends (sign-out,
//! password change, revocation, expiry) the server sends `auth` and closes
//! it. At most [`MAX_STREAMS`] streams are open at once.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper::body::{Body as HttpBody, Bytes, Frame};
use hyper::header::{self, HeaderValue};
use hyper::{Response, body::SizeHint};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use super::Context;
use super::http::{Body, error};
use crate::management::Management;
use crate::management::auth::Session;

pub(crate) const MAX_STREAMS: usize = 16;
const HEARTBEAT: Duration = Duration::from_secs(20);
/// Frames buffered for a slow browser before the stream resynchronizes.
const BUFFER: usize = 64;

/// A response body fed by the stream's task. Dropping it (the browser left)
/// closes the channel, which ends the task.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl HttpBody for ChannelBody {
	type Data = Bytes;
	type Error = Infallible;

	fn poll_frame(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
		self.get_mut()
			.0
			.poll_recv(cx)
			.map(|bytes| bytes.map(|bytes| Ok(Frame::data(bytes))))
	}

	fn size_hint(&self) -> SizeHint {
		SizeHint::default()
	}
}

fn frame(id: u64, event: &str, data: &str) -> Bytes {
	// JSON documents are single-line; `data:` must not contain a newline.
	debug_assert!(!data.contains('\n'));
	Bytes::from(format!("id: {id}\nevent: {event}\ndata: {data}\n\n"))
}

fn overview(management: &Management) -> String {
	json!({
		"connected": true,
		"server": management.op_server(),
		"session": management.op_session(),
		"pairing": management.op_pairing(),
		"clients": management.op_clients().ok(),
		"stats": management.op_stats(),
		"web": { "url": management.web_url() },
	})
	.to_string()
}

pub(crate) async fn stream(context: &Context, session: Session) -> Response<Body> {
	let management = context.management.clone();
	if management.hub.web_streams() >= MAX_STREAMS {
		return error(
			StatusCode::SERVICE_UNAVAILABLE,
			"unavailable",
			"Too many Pyroshine pages are open. Close some tabs and reload.",
		);
	}
	let observing = management.hub.web_stream();
	let (sender, receiver) = mpsc::channel(BUFFER);
	tokio::spawn(async move {
		let _observing = observing;
		run(management, session, sender).await;
	});
	let mut response = Response::new(ChannelBody(receiver).boxed());
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	// Reverse proxies must not buffer the stream.
	headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
	response
}

async fn run(management: Arc<Management>, session: Session, sender: mpsc::Sender<Bytes>) {
	let mut events = management.hub.subscribe();
	let mut revocations = management.auth.revocations();
	let mut id = 0u64;
	let mut next_id = || {
		id += 1;
		id
	};
	// A reconnecting browser waits three seconds.
	if sender.send(Bytes::from_static(b"retry: 3000\n\n")).await.is_err() {
		return;
	}
	if sender
		.send(frame(next_id(), "overview", &overview(&management)))
		.await
		.is_err()
	{
		return;
	}
	let mut heartbeat = tokio::time::interval(HEARTBEAT);
	heartbeat.tick().await;
	loop {
		let message = tokio::select! {
			() = sender.closed() => return,
			event = events.recv() => match event {
				Ok(event) => frame(next_id(), event.kind.name(), &event.json),
				// Fell behind: start over from a complete state.
				Err(broadcast::error::RecvError::Lagged(_)) => frame(next_id(), "overview", &overview(&management)),
				Err(broadcast::error::RecvError::Closed) => return,
			},
			_ = heartbeat.tick() => {
				if !management.auth.is_live(&session) {
					let _ = sender.send(frame(next_id(), "auth", r#"{"reason":"expired"}"#)).await;
					return;
				}
				Bytes::from_static(b": heartbeat\n\n")
			},
			changed = revocations.changed() => {
				if changed.is_err() || !management.auth.is_live(&session) {
					let _ = sender.send(frame(next_id(), "auth", r#"{"reason":"signed_out"}"#)).await;
					return;
				}
				continue;
			},
		};
		// A browser that cannot keep up loses this frame and resynchronizes
		// with the next overview instead of holding events in memory.
		match sender.try_send(message) {
			Ok(()) => {},
			Err(mpsc::error::TrySendError::Full(_)) => {
				tracing::debug!(
					session = session.id,
					"Web event stream is behind; it will resynchronize"
				);
				if sender
					.send(frame(next_id(), "overview", &overview(&management)))
					.await
					.is_err()
				{
					return;
				}
			},
			Err(mpsc::error::TrySendError::Closed(_)) => return,
		}
	}
}
