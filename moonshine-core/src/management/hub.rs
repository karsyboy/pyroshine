//! Transport-neutral publication of management events.
//!
//! The management tasks (`follow_sessions`, `follow_pairing`, statistics,
//! library, history, updates) publish each change once to the [`Hub`]. Every transport
//! is a subscriber: the D-Bus service turns events into signals for the tray
//! and `pyroshine pair`, the web server turns them into Server-Sent Events for
//! browsers. A transport that falls behind loses events (the channel is
//! bounded) and resynchronizes from complete snapshots; a slow or absent
//! consumer never blocks a publisher.
//!
//! The hub also counts live observers. Statistics and library changes are
//! published only while an open browser tab watches them (the tray shows
//! neither), so a host nobody looks at does no serialization work for them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{broadcast, watch};

/// Events kept for subscribers that read slower than events arrive.
const CAPACITY: usize = 256;

/// What an event carries. The JSON body is a `moonshine_management::dto`
/// document; the kind names it on every transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
	/// [`moonshine_management::dto::SessionSnapshot`].
	Session,
	/// [`moonshine_management::dto::PairingSnapshot`].
	Pairing,
	/// [`moonshine_management::dto::PendingPairing`], once per new request.
	PairingRequested,
	/// [`moonshine_management::dto::PairingResolution`].
	PairingResolved,
	/// [`moonshine_management::dto::ClientsSnapshot`].
	Clients,
	/// [`moonshine_management::dto::ConfigSaved`].
	ConfigSaved,
	/// [`moonshine_management::dto::StreamStats`] (only while observed).
	Stats,
	/// [`moonshine_management::dto::ServerEvent`].
	ServerEvent,
	/// [`moonshine_management::dto::HealthReport`].
	Health,
	/// [`moonshine_management::dto::LibrarySnapshot`] (only while observed).
	Library,
	/// [`moonshine_management::dto::HistoryEntry`] of a session that ended.
	History,
	/// [`moonshine_management::dto::UpdateStatus`] after an automatic check.
	Update,
}

impl EventKind {
	/// Event name on the web event stream (`daemon://<name>` in the UI).
	pub(crate) fn name(self) -> &'static str {
		match self {
			Self::Session => "session",
			Self::Pairing => "pairing",
			Self::PairingRequested => "pairing-requested",
			Self::PairingResolved => "pairing-resolved",
			Self::Clients => "clients",
			Self::ConfigSaved => "config-saved",
			Self::Stats => "stats",
			Self::ServerEvent => "server-event",
			Self::Health => "health",
			Self::Library => "library",
			Self::History => "history",
			Self::Update => "update",
		}
	}
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
	pub kind: EventKind,
	pub json: Arc<str>,
}

pub(crate) struct Hub {
	events: broadcast::Sender<Event>,
	/// Whether the desktop tray owns its bus name (it shows notifications).
	desktop: watch::Sender<bool>,
	/// Open web event streams.
	web_streams: AtomicUsize,
	/// Recomputed whenever either observer count changes.
	observed: watch::Sender<bool>,
}

impl Default for Hub {
	fn default() -> Self {
		Self {
			events: broadcast::channel(CAPACITY).0,
			desktop: watch::channel(false).0,
			web_streams: AtomicUsize::new(0),
			observed: watch::channel(false).0,
		}
	}
}

impl Hub {
	pub(crate) fn publish(&self, kind: EventKind, json: String) {
		// No subscriber is not an error: transports come and go.
		let _ = self.events.send(Event {
			kind,
			json: json.into(),
		});
	}

	pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
		self.events.subscribe()
	}

	/// Whether anyone shows live state (statistics, library progress).
	pub(crate) fn observed(&self) -> watch::Receiver<bool> {
		self.observed.subscribe()
	}

	pub(crate) fn is_observed(&self) -> bool {
		*self.observed.borrow()
	}

	pub(crate) fn set_desktop_present(&self, present: bool) {
		self.desktop.send_replace(present);
	}

	pub(crate) fn desktop_present(&self) -> bool {
		*self.desktop.borrow()
	}

	/// Count a web event stream for as long as the guard lives.
	pub(crate) fn web_stream(self: &Arc<Self>) -> WebStreamGuard {
		self.web_streams.fetch_add(1, Ordering::AcqRel);
		self.recompute();
		WebStreamGuard { hub: self.clone() }
	}

	pub(crate) fn web_streams(&self) -> usize {
		self.web_streams.load(Ordering::Acquire)
	}

	fn recompute(&self) {
		let observed = self.web_streams.load(Ordering::Acquire) > 0;
		self.observed.send_if_modified(|current| {
			let changed = *current != observed;
			*current = observed;
			changed
		});
	}
}

pub(crate) struct WebStreamGuard {
	hub: Arc<Hub>,
}

impl Drop for WebStreamGuard {
	fn drop(&mut self) {
		self.hub.web_streams.fetch_sub(1, Ordering::AcqRel);
		self.hub.recompute();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn observation_follows_web_streams_only() {
		let hub = Arc::new(Hub::default());
		let observed = hub.observed();
		assert!(!*observed.borrow());
		let first = hub.web_stream();
		let second = hub.web_stream();
		assert!(hub.is_observed());
		assert_eq!(hub.web_streams(), 2);
		drop(first);
		assert!(hub.is_observed());
		drop(second);
		assert!(!hub.is_observed());
		// The tray shows neither statistics nor the library.
		hub.set_desktop_present(true);
		assert!(!hub.is_observed());
		assert!(hub.desktop_present());
	}

	#[tokio::test]
	async fn publication_without_subscribers_is_harmless_and_lagging_is_reported() {
		let hub = Hub::default();
		hub.publish(EventKind::Session, "{}".into());
		let mut receiver = hub.subscribe();
		for _ in 0..CAPACITY + 1 {
			hub.publish(EventKind::Stats, "{}".into());
		}
		assert!(matches!(
			receiver.recv().await,
			Err(broadcast::error::RecvError::Lagged(1))
		));
		assert_eq!(receiver.recv().await.unwrap().kind, EventKind::Stats);
	}
}
