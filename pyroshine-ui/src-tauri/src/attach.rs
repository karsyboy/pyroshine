//! Supervision of the attachment to the daemon (review 2026-10-05 UI-001).
//!
//! Ownership of the daemon's bus name says whether a daemon exists; it does
//! not say whether this app is attached to it. An attachment can fail after
//! the name appeared (a bus hiccup, a daemon still starting up) without the
//! owner ever changing, so each attempt's outcome is supervised too: transient
//! failures are retried with bounded backoff while the same owner stays,
//! persistent ones are reported and wait for a new owner, and an owner change
//! abandons the current attempt at once.

use std::future::Future;
use std::time::Duration;

use futures_util::{Stream, StreamExt};

use crate::daemon::UiError;

/// How one attachment attempt ended.
#[derive(Debug)]
pub enum Attached {
	/// The attachment ran and its signal streams ended.
	Ended,
	/// Failed in a way a later attempt may not (bus or daemon not ready).
	Transient(UiError),
	/// The daemon answers but this app cannot use it (incompatible or denied).
	Persistent(UiError),
}

impl Attached {
	/// Classify an attachment error by its kind.
	pub fn failed(error: UiError) -> Self {
		match error.kind.as_str() {
			"incompatible" | "access_denied" => Self::Persistent(error),
			_ => Self::Transient(error),
		}
	}
}

/// Retry delays: doubling from `INITIAL` to `MAX`, reset by a new owner.
pub const INITIAL_RETRY: Duration = Duration::from_millis(500);
pub const MAX_RETRY: Duration = Duration::from_secs(30);

/// Effects of supervision, injected so the policy is testable without Tauri.
pub trait Attachment: Send + 'static {
	type Attempt: Future<Output = Attached> + Send + 'static;

	/// Start an attempt for owner `generation`. Publications of an attempt
	/// must be ignored once a later generation started.
	fn attach(&mut self, generation: u64) -> Self::Attempt;

	/// No daemon owns the name: report it unavailable and retire `generation`.
	fn detach(&mut self, generation: u64) -> impl Future<Output = ()> + Send;

	/// An attempt failed; `retrying` says whether another will follow.
	fn failed(&mut self, error: &UiError, retrying: bool) -> impl Future<Output = ()> + Send;
}

/// Follow `owners` (whether the name has an owner, one item per
/// `NameOwnerChanged`) starting from `owned`, until the stream ends.
pub async fn supervise(
	mut owners: impl Stream<Item = bool> + Unpin + Send,
	mut owned: bool,
	mut attachment: impl Attachment,
) {
	let mut generation = 0u64;
	loop {
		generation += 1;
		if !owned {
			attachment.detach(generation).await;
			match owners.next().await {
				Some(next) => owned = next,
				None => return,
			}
			continue;
		}
		let mut retry = INITIAL_RETRY;
		// Attempts for this owner, until it changes or leaves.
		let changed = 'owner: loop {
			let mut attempt = tokio::spawn(attachment.attach(generation));
			let outcome = tokio::select! {
				change = owners.next() => {
					attempt.abort();
					break 'owner change;
				},
				outcome = &mut attempt => outcome,
			};
			let failure = match outcome {
				Ok(Attached::Ended) => None,
				Ok(Attached::Transient(error)) => Some((error, true)),
				Ok(Attached::Persistent(error)) => Some((error, false)),
				Err(error) => Some((
					UiError {
						kind: "unavailable".into(),
						message: format!("The connection to Pyroshine failed: {error}"),
					},
					true,
				)),
			};
			let retrying = failure.as_ref().is_none_or(|(_, retrying)| *retrying);
			if let Some((error, retrying)) = &failure {
				attachment.failed(error, *retrying).await;
			} else {
				// A completed attachment starts the backoff over.
				retry = INITIAL_RETRY;
			}
			if !retrying {
				// Only a new owner can change a persistent failure.
				break 'owner owners.next().await;
			}
			tokio::select! {
				change = owners.next() => break 'owner change,
				() = tokio::time::sleep(retry) => {},
			}
			retry = (retry * 2).min(MAX_RETRY);
			generation += 1;
		};
		match changed {
			Some(next) => owned = next,
			None => return,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::VecDeque;
	use std::sync::{Arc, Mutex};

	use super::*;

	#[derive(Clone, Default)]
	struct Fake {
		/// Outcomes of successive attempts; an empty queue attaches forever.
		outcomes: Arc<Mutex<VecDeque<Attached>>>,
		log: Arc<Mutex<Vec<String>>>,
	}

	impl Fake {
		fn with(outcomes: impl IntoIterator<Item = Attached>) -> Self {
			let fake = Self::default();
			fake.outcomes.lock().unwrap().extend(outcomes);
			fake
		}
		fn log(&self) -> Vec<String> {
			self.log.lock().unwrap().clone()
		}
	}

	impl Attachment for Fake {
		type Attempt = std::pin::Pin<Box<dyn Future<Output = Attached> + Send>>;

		fn attach(&mut self, generation: u64) -> Self::Attempt {
			let at = tokio::time::Instant::now();
			self.log
				.lock()
				.unwrap()
				.push(format!("attach {generation} at {}ms", at.elapsed().as_millis()));
			let outcome = self.outcomes.lock().unwrap().pop_front();
			// Records when an abandoned attempt is dropped.
			struct Dropped(Arc<Mutex<Vec<String>>>, u64);
			impl Drop for Dropped {
				fn drop(&mut self) {
					self.0.lock().unwrap().push(format!("dropped {}", self.1));
				}
			}
			let dropped = Dropped(self.log.clone(), generation);
			Box::pin(async move {
				match outcome {
					Some(outcome) => {
						// Completed, not abandoned.
						std::mem::forget(dropped);
						outcome
					},
					None => {
						let _dropped = dropped;
						std::future::pending().await
					},
				}
			})
		}

		async fn detach(&mut self, generation: u64) {
			self.log.lock().unwrap().push(format!("detach {generation}"));
		}

		async fn failed(&mut self, error: &UiError, retrying: bool) {
			self.log
				.lock()
				.unwrap()
				.push(format!("failed {} retrying={retrying}", error.kind));
		}
	}

	/// A stream of owner changes fed by the returned sender; dropping the
	/// sender ends it (UI shutdown).
	fn owner_stream() -> (
		tokio::sync::mpsc::UnboundedSender<bool>,
		std::pin::Pin<Box<dyn Stream<Item = bool> + Send>>,
	) {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|owned| (owned, rx)) });
		(tx, Box::pin(stream))
	}

	fn transient() -> Attached {
		Attached::Transient(UiError::unavailable())
	}

	fn attempts(log: &[String]) -> usize {
		log.iter().filter(|line| line.starts_with("attach")).count()
	}

	/// The same owner fails its first attachments, then attaches: retries
	/// follow without any owner change, with growing delays.
	#[tokio::test(start_paused = true)]
	async fn same_owner_transient_failures_are_retried() {
		let fake = Fake::with([transient(), transient()]);
		let (owners_tx, owners) = owner_stream();
		let started = tokio::time::Instant::now();
		let task = tokio::spawn(supervise(owners, true, fake.clone()));
		tokio::time::sleep(Duration::from_secs(10)).await;
		let log = fake.log();
		assert_eq!(attempts(&log), 3, "{log:?}");
		assert_eq!(log.iter().filter(|line| line.contains("retrying=true")).count(), 2);
		// 0.5 s then 1 s of backoff before the third attempt.
		assert!(started.elapsed() >= Duration::from_secs(10));
		drop(owners_tx);
		task.await.unwrap();
	}

	/// A new owner during the backoff is attached at once, as a new
	/// generation; the abandoned retry never runs.
	#[tokio::test(start_paused = true)]
	async fn new_owner_during_retry_attaches_immediately() {
		let fake = Fake::with([transient()]);
		let (owners_tx, owners) = owner_stream();
		let task = tokio::spawn(supervise(owners, true, fake.clone()));
		tokio::time::sleep(Duration::from_millis(100)).await;
		owners_tx.send(true).unwrap();
		tokio::time::sleep(Duration::from_millis(10)).await;
		let log = fake.log();
		assert_eq!(attempts(&log), 2, "{log:?}");
		assert!(log[2].starts_with("attach 2"), "{log:?}");
		// Nothing more: the second attempt holds the attachment.
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert_eq!(attempts(&fake.log()), 2);
		drop(owners_tx);
		task.await.unwrap();
	}

	/// The daemon restarts while snapshots are still pending: the pending
	/// attempt is dropped (it can publish nothing) and the new owner attached.
	#[tokio::test(start_paused = true)]
	async fn owner_restart_abandons_a_pending_attempt() {
		let fake = Fake::default();
		let (owners_tx, owners) = owner_stream();
		let task = tokio::spawn(supervise(owners, true, fake.clone()));
		tokio::time::sleep(Duration::from_millis(10)).await;
		owners_tx.send(true).unwrap();
		tokio::time::sleep(Duration::from_millis(10)).await;
		let log = fake.log();
		assert!(log.contains(&"dropped 1".to_string()), "{log:?}");
		assert!(log.iter().any(|line| line.starts_with("attach 2")), "{log:?}");
		drop(owners_tx);
		task.await.unwrap();
	}

	/// A persistent failure is reported once and not retried until the owner
	/// changes; the owner leaving reports the daemon unavailable.
	#[tokio::test(start_paused = true)]
	async fn persistent_failure_waits_for_a_new_owner() {
		let fake = Fake::with([Attached::Persistent(UiError {
			kind: "incompatible".into(),
			message: "too old".into(),
		})]);
		let (owners_tx, owners) = owner_stream();
		let task = tokio::spawn(supervise(owners, true, fake.clone()));
		tokio::time::sleep(Duration::from_secs(120)).await;
		assert_eq!(
			fake.log(),
			vec!["attach 1 at 0ms", "failed incompatible retrying=false"]
		);
		owners_tx.send(false).unwrap();
		owners_tx.send(true).unwrap();
		tokio::time::sleep(Duration::from_millis(10)).await;
		let log = fake.log();
		assert!(log[2].starts_with("detach"), "{log:?}");
		assert!(log[3].starts_with("attach"), "{log:?}");
		drop(owners_tx);
		task.await.unwrap();
	}

	/// UI shutdown (the owner stream ends) stops supervision and abandons a
	/// pending attempt; the backoff is bounded by `MAX_RETRY`.
	#[tokio::test(start_paused = true)]
	async fn shutdown_ends_supervision_and_backoff_is_bounded() {
		let fake = Fake::with((0..20).map(|_| transient()));
		let (owners_tx, owners) = owner_stream();
		let task = tokio::spawn(supervise(owners, true, fake.clone()));
		tokio::time::sleep(Duration::from_secs(600)).await;
		// 0.5+1+2+4+8+16 = 31.5 s, then every 30 s: about 25 attempts in 10 min,
		// capped by the 20 queued failures plus one lasting attachment.
		assert_eq!(attempts(&fake.log()), 21);
		drop(owners_tx);
		tokio::time::timeout(Duration::from_secs(1), task)
			.await
			.unwrap()
			.unwrap();
	}
}
