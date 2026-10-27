//! Admission and ownership of accepted TCP connections (HTTP, HTTPS, RTSP).
//!
//! Each accepted connection runs in its own task so a stalled peer (for
//! example one that never sends a TLS ClientHello) cannot block the accept
//! loop. A task holds:
//!
//! - one of a bounded number of per-listener slots, bounding concurrent
//!   connections and their buffers;
//! - a per-address share of those slots, so one host holding stalled
//!   connections cannot occupy the whole listener;
//! - a global [`DelayShutdownToken`], so global shutdown completes only after
//!   every handler has dropped its sockets and shared state (for example the
//!   session manager, which itself delays shutdown);
//! - global cancellation: when shutdown is triggered the handler future is
//!   dropped at its next await point, so shutdown is not delayed by idle peers.
//!
//! When an address (or the whole listener) is at its limit, a listener built
//! [`with_idle_eviction`](Ingress::with_idle_eviction) closes the longest-idle
//! connection that is not handling a request and admits the new one in its
//! place; otherwise, or when every connection is busy or recently active, the
//! new connection is closed immediately. Moonlight iOS needs this: it opens a
//! fresh HTTP session for every request (a status poll every two seconds, app
//! list and box art) and leaves each previous connection idle in keep-alive,
//! so refusing new connections would refuse its next request, which iOS
//! reports as "A TLS error caused the secure connection to fail". Eviction only
//! replaces abandoned connections, so a stalled host still cannot hold more
//! than its share, and its stalled sockets are the first to be replaced.
//!
//! Protocol-specific deadlines (TLS handshake, request headers, RTSP framing)
//! are applied by the owning server inside the handler future.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_shutdown::{DelayShutdownToken, ShutdownManager};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use crate::ShutdownReason;

/// Minimum interval between "connection limit reached" warnings per listener.
const SATURATION_WARNING_INTERVAL: Duration = Duration::from_secs(10);

/// A connection must have been idle (no request in progress, no bytes read
/// or written) for this long before a new connection may replace it. It keeps
/// a just-accepted connection, or a response still draining, from being
/// replaced by a burst of new connections.
const IDLE_EVICTION_AGE: Duration = Duration::from_secs(2);

/// Admission control for one listener.
#[derive(Clone)]
pub(crate) struct Ingress {
	name: &'static str,
	max_connections: usize,
	max_per_peer: usize,
	/// Idle age after which a connection may be replaced; `None` disables eviction.
	idle_eviction_age: Option<Duration>,
	state: Arc<Mutex<State>>,
	shutdown: ShutdownManager<ShutdownReason>,
	last_saturation_warning: Arc<Mutex<Option<Instant>>>,
}

#[derive(Default)]
struct State {
	next_id: u64,
	per_peer: HashMap<IpAddr, usize>,
	connections: HashMap<u64, Tracked>,
}

struct Tracked {
	ip: IpAddr,
	activity: Arc<ConnectionActivity>,
}

impl State {
	/// Forget a connection. Returns it if it was still counted (it is not
	/// once it has been evicted).
	fn remove(&mut self, id: u64) -> Option<Tracked> {
		let tracked = self.connections.remove(&id)?;
		if let Some(count) = self.per_peer.get_mut(&tracked.ip) {
			*count -= 1;
			if *count == 0 {
				self.per_peer.remove(&tracked.ip);
			}
		}
		Some(tracked)
	}
}

impl Ingress {
	/// Admit at most `max_connections` concurrent connections, a quarter of
	/// which (at least one) may come from any single address.
	pub(crate) fn new(name: &'static str, max_connections: usize, shutdown: ShutdownManager<ShutdownReason>) -> Self {
		Self {
			name,
			max_connections,
			max_per_peer: (max_connections / 4).max(1),
			idle_eviction_age: None,
			state: Default::default(),
			shutdown,
			last_saturation_warning: Default::default(),
		}
	}

	/// At a limit, replace the longest-idle connection instead of refusing the
	/// new one. Handlers must report their activity through
	/// [`IngressPermit::activity`] (requests in progress and socket I/O), or
	/// they are considered idle from admission on.
	pub(crate) fn with_idle_eviction(mut self) -> Self {
		self.idle_eviction_age = Some(IDLE_EVICTION_AGE);
		self
	}

	#[cfg(test)]
	pub(crate) fn with_idle_eviction_after(mut self, age: Duration) -> Self {
		self.idle_eviction_age = Some(age);
		self
	}

	/// Admit a newly accepted connection, or `None` to close it immediately
	/// because the listener is saturated or the server is shutting down.
	pub(crate) fn admit(&self, peer: SocketAddr) -> Option<IngressPermit> {
		if self.shutdown.is_shutdown_triggered() {
			return None;
		}
		let ip = peer.ip().to_canonical();
		let Ok(mut state) = self.state.lock() else {
			return None;
		};
		let peer_full = state.per_peer.get(&ip).is_some_and(|count| *count >= self.max_per_peer);
		if peer_full && !self.evict(&mut state, Some(ip)) {
			drop(state);
			self.warn_saturated(peer);
			return None;
		}
		if state.connections.len() >= self.max_connections && !self.evict(&mut state, None) {
			drop(state);
			self.warn_saturated(peer);
			return None;
		}
		let delay = self.shutdown.delay_shutdown_token().ok()?;
		let id = state.next_id;
		state.next_id += 1;
		let activity = Arc::new(ConnectionActivity::new());
		state.connections.insert(
			id,
			Tracked {
				ip,
				activity: activity.clone(),
			},
		);
		*state.per_peer.entry(ip).or_default() += 1;
		Some(IngressPermit {
			id,
			activity,
			state: self.state.clone(),
			_delay: delay,
			shutdown: self.shutdown.clone(),
		})
	}

	/// Close the longest-idle evictable connection (from `ip`, or from any
	/// address) and stop counting it. Its handler is cancelled at its next
	/// await point, so the listener can briefly own one socket more than its
	/// limit per eviction; an idle handler is already waiting, so that is
	/// immediate.
	fn evict(&self, state: &mut State, ip: Option<IpAddr>) -> bool {
		let Some(minimum_age) = self.idle_eviction_age else {
			return false;
		};
		let victim = state
			.connections
			.iter()
			.filter(|(_, tracked)| ip.is_none_or(|ip| tracked.ip == ip))
			.filter_map(|(id, tracked)| Some((*id, tracked.activity.idle_for()?)))
			.filter(|(_, idle)| *idle >= minimum_age)
			.max_by_key(|(_, idle)| *idle);
		let Some((id, idle)) = victim else {
			return false;
		};
		let Some(tracked) = state.remove(id) else {
			return false;
		};
		tracked.activity.evicted.notify_one();
		tracing::debug!(
			listener = self.name,
			peer = %tracked.ip,
			idle_ms = idle.as_millis() as u64,
			"Closed an idle connection to admit a new one"
		);
		true
	}

	fn warn_saturated(&self, peer: SocketAddr) {
		let Ok(mut last) = self.last_saturation_warning.lock() else {
			return;
		};
		if last.is_none_or(|at| at.elapsed() >= SATURATION_WARNING_INTERVAL) {
			*last = Some(Instant::now());
			tracing::warn!(
				listener = self.name,
				%peer,
				"Connection limit reached; closing new connections until existing ones finish"
			);
		}
	}

	#[cfg(test)]
	pub(crate) fn available(&self) -> usize {
		self.max_connections - self.state.lock().unwrap().connections.len()
	}
}

/// Whether a connection is handling a request and when it last made progress,
/// for idle eviction.
pub(crate) struct ConnectionActivity {
	started: Instant,
	/// Milliseconds after `started` of the last request end or socket I/O.
	last_active_ms: AtomicU64,
	requests: AtomicUsize,
	evicted: Notify,
}

impl ConnectionActivity {
	fn new() -> Self {
		Self {
			started: Instant::now(),
			last_active_ms: AtomicU64::new(0),
			requests: AtomicUsize::new(0),
			evicted: Notify::new(),
		}
	}

	fn touch(&self) {
		let now = self.started.elapsed().as_millis() as u64;
		self.last_active_ms.fetch_max(now, Ordering::Relaxed);
	}

	/// How long the connection has been idle, or `None` while it is handling
	/// a request (which may legitimately wait, for example for a pairing PIN).
	fn idle_for(&self) -> Option<Duration> {
		if self.requests.load(Ordering::Relaxed) > 0 {
			return None;
		}
		let last = Duration::from_millis(self.last_active_ms.load(Ordering::Relaxed));
		Some(self.started.elapsed().saturating_sub(last))
	}

	/// Activity of a connection served outside any listener.
	#[cfg(test)]
	pub(crate) fn untracked() -> Arc<Self> {
		Arc::new(Self::new())
	}

	/// Mark a request as in progress until the guard is dropped.
	pub(crate) fn request(self: &Arc<Self>) -> RequestGuard {
		self.requests.fetch_add(1, Ordering::Relaxed);
		RequestGuard(self.clone())
	}
}

/// A request in progress on a connection; see [`ConnectionActivity::request`].
pub(crate) struct RequestGuard(Arc<ConnectionActivity>);

impl Drop for RequestGuard {
	fn drop(&mut self) {
		self.0.touch();
		self.0.requests.fetch_sub(1, Ordering::Relaxed);
	}
}

/// A socket that records read and write progress as connection activity.
pub(crate) struct ActivityIo<T> {
	inner: T,
	activity: Arc<ConnectionActivity>,
}

impl<T> ActivityIo<T> {
	pub(crate) fn new(inner: T, activity: Arc<ConnectionActivity>) -> Self {
		Self { inner, activity }
	}
}

impl<T: AsyncRead + Unpin> AsyncRead for ActivityIo<T> {
	fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
		let before = buf.filled().len();
		let result = Pin::new(&mut self.inner).poll_read(cx, buf);
		if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
			self.activity.touch();
		}
		result
	}
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ActivityIo<T> {
	fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
		let result = Pin::new(&mut self.inner).poll_write(cx, buf);
		if matches!(result, Poll::Ready(Ok(written)) if written > 0) {
			self.activity.touch();
		}
		result
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.inner).poll_flush(cx)
	}

	fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.inner).poll_shutdown(cx)
	}
}

/// Ownership of one admitted connection. Dropping it releases the slot and
/// allows global shutdown to complete.
pub(crate) struct IngressPermit {
	id: u64,
	activity: Arc<ConnectionActivity>,
	state: Arc<Mutex<State>>,
	_delay: DelayShutdownToken<ShutdownReason>,
	shutdown: ShutdownManager<ShutdownReason>,
}

impl IngressPermit {
	/// Activity reporting for idle eviction.
	pub(crate) fn activity(&self) -> Arc<ConnectionActivity> {
		self.activity.clone()
	}

	/// Run a connection handler that is cancelled by global shutdown or by
	/// eviction.
	pub(crate) fn spawn<F>(self, handler: F) -> tokio::task::JoinHandle<()>
	where
		F: Future<Output = ()> + Send + 'static,
	{
		tokio::spawn(async move {
			// The handler (and everything it owns) is dropped before the permit.
			let evicted = self.activity.evicted.notified();
			let _ = self
				.shutdown
				.wrap_cancel(async {
					tokio::select! {
						_ = handler => {},
						_ = evicted => {},
					}
				})
				.await;
			drop(self);
		})
	}
}

impl Drop for IngressPermit {
	fn drop(&mut self) {
		if let Ok(mut state) = self.state.lock() {
			state.remove(self.id);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn peer(host: u8) -> SocketAddr {
		SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, host)), 1)
	}

	#[tokio::test]
	async fn one_address_cannot_occupy_the_listener() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 8, shutdown.clone());
		let attacker: SocketAddr = "192.168.1.66:1000".parse().unwrap();
		let mapped: SocketAddr = "[::ffff:192.168.1.66]:1001".parse().unwrap();
		// Eight slots allow two per address; IPv4-mapped IPv6 is the same address.
		let held = [ingress.admit(attacker).unwrap(), ingress.admit(mapped).unwrap()];
		assert!(ingress.admit(attacker).is_none());
		assert!(ingress.admit(mapped).is_none());
		let client = ingress.admit("192.168.1.20:2000".parse().unwrap());
		assert!(client.is_some(), "other clients are still admitted");
		drop(held);
		assert!(ingress.admit(attacker).is_some(), "released with its connections");
		assert!(ingress.state.lock().unwrap().per_peer.len() <= 1);
	}

	#[tokio::test]
	async fn admission_is_bounded_and_released_with_the_handler() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 2, shutdown.clone());
		let (release, released) = tokio::sync::oneshot::channel::<()>();
		let first = ingress.admit(peer(1)).unwrap().spawn(async move {
			let _ = released.await;
		});
		let second = ingress.admit(peer(2)).unwrap();
		assert!(ingress.admit(peer(3)).is_none(), "third connection exceeds the limit");
		drop(second);
		assert_eq!(ingress.available(), 1);
		release.send(()).unwrap();
		first.await.unwrap();
		assert_eq!(ingress.available(), 2);
	}

	/// Spawn a handler that runs until cancelled; the returned `Arc` has one
	/// more strong reference while the handler is alive.
	fn hold(permit: IngressPermit, owned: &Arc<()>) {
		let owned = owned.clone();
		permit.spawn(async move {
			let _owned = owned;
			std::future::pending::<()>().await;
		});
	}

	async fn settle() {
		tokio::time::sleep(Duration::from_millis(20)).await;
	}

	#[tokio::test]
	async fn idle_connections_from_an_address_make_room_for_its_new_ones() {
		let shutdown = ShutdownManager::new();
		let age = Duration::from_millis(100);
		let ingress = Ingress::new("test", 8, shutdown.clone()).with_idle_eviction_after(age);
		let phone: SocketAddr = "192.168.1.70:1000".parse().unwrap();
		let (oldest, newer) = (Arc::new(()), Arc::new(()));
		hold(ingress.admit(phone).unwrap(), &oldest);
		tokio::time::sleep(Duration::from_millis(10)).await;
		hold(ingress.admit(phone).unwrap(), &newer);
		assert!(ingress.admit(phone).is_none(), "recently active connections are kept");

		tokio::time::sleep(age).await;
		let replacement = ingress.admit(phone);
		assert!(replacement.is_some(), "an idle connection makes room");
		settle().await;
		assert_eq!(Arc::strong_count(&oldest), 1, "the longest-idle connection was closed");
		assert_eq!(Arc::strong_count(&newer), 2, "the other connection is untouched");
		assert_eq!(ingress.state.lock().unwrap().per_peer[&phone.ip()], 2);
		assert_eq!(ingress.available(), 6);
		shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
	}

	#[tokio::test]
	async fn requests_in_progress_and_recent_io_prevent_eviction() {
		let shutdown = ShutdownManager::new();
		let age = Duration::from_millis(100);
		let ingress = Ingress::new("test", 8, shutdown.clone()).with_idle_eviction_after(age);
		let phone: SocketAddr = "192.168.1.70:1000".parse().unwrap();
		let waiting = ingress.admit(phone).unwrap();
		let _pairing = waiting.activity().request();
		let streaming = ingress.admit(phone).unwrap();
		let (mut client, server) = tokio::io::duplex(64);
		let mut io = ActivityIo::new(server, streaming.activity());

		tokio::time::sleep(age).await;
		use tokio::io::{AsyncReadExt, AsyncWriteExt};
		client.write_all(b"x").await.unwrap();
		let mut byte = [0u8];
		io.read_exact(&mut byte).await.unwrap();
		assert!(ingress.admit(phone).is_none(), "neither connection is idle");
		drop(_pairing);
		assert!(ingress.admit(phone).is_none(), "a request that just ended is not idle");
		io.write_all(b"y").await.unwrap();
		tokio::time::sleep(age).await;
		assert!(ingress.admit(phone).is_some(), "idle again after the age");
		drop((waiting, streaming));
	}

	#[tokio::test]
	async fn a_full_listener_replaces_its_longest_idle_connection() {
		let shutdown = ShutdownManager::new();
		let age = Duration::from_millis(50);
		let ingress = Ingress::new("test", 4, shutdown.clone()).with_idle_eviction_after(age);
		let owned: Vec<_> = (0..4).map(|_| Arc::new(())).collect();
		for (host, owned) in (1..=4).zip(&owned) {
			hold(ingress.admit(peer(host)).unwrap(), owned);
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		assert!(ingress.admit(peer(5)).is_none());
		tokio::time::sleep(age).await;
		assert!(ingress.admit(peer(5)).is_some());
		settle().await;
		let alive: Vec<_> = owned.iter().map(|owned| Arc::strong_count(owned) == 2).collect();
		assert_eq!(
			alive,
			[false, true, true, true],
			"the first-admitted, longest idle, was closed"
		);
		shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
	}

	#[tokio::test]
	async fn shutdown_cancels_stalled_handlers_and_waits_for_them() {
		let shutdown = ShutdownManager::new();
		let ingress = Ingress::new("test", 4, shutdown.clone());
		let owned = Arc::new(());
		for host in 1..=3 {
			let owned = owned.clone();
			ingress.admit(peer(host)).unwrap().spawn(async move {
				let _owned = owned;
				std::future::pending::<()>().await;
			});
		}
		assert_eq!(Arc::strong_count(&owned), 4);
		shutdown.trigger_shutdown(ShutdownReason::AppQuit).unwrap();
		tokio::time::timeout(Duration::from_secs(1), shutdown.wait_shutdown_complete())
			.await
			.expect("stalled handlers must not delay shutdown");
		assert_eq!(Arc::strong_count(&owned), 1, "handlers released their state");
		assert!(ingress.admit(peer(4)).is_none(), "no admission after shutdown");
	}
}
