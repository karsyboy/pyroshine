//! Operator-facing failure causes.
//!
//! Session work logs its technical cause where it happens. What the operator
//! (Moonlight's error dialog, the web interface, the event log) needs instead is
//! a stable code, one sentence saying what failed, and, where Pyroshine knows
//! it, what to change. [`SessionFailure`] carries that from the launch path to
//! the HTTP response and the management interface; [`StopCause`] does the same
//! for a running session that a worker or the application ended.
//!
//! Neither type is used on a media path: both are built once, when something
//! has already failed.

use std::sync::{Arc, OnceLock};

/// Why a session could not be created, launched or started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFailure {
	/// Stable machine-readable code, for example `application_not_found`.
	pub code: &'static str,
	/// What failed, in one or two sentences.
	pub message: String,
	/// What the operator can do about it, when Pyroshine knows.
	pub hint: Option<String>,
}

impl SessionFailure {
	pub fn new(code: &'static str, message: impl Into<String>) -> Self {
		Self {
			code,
			message: message.into(),
			hint: None,
		}
	}

	pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
		self.hint = Some(hint.into());
		self
	}

	/// The failure as one line for Moonlight's error dialog: the message
	/// followed by the hint.
	pub fn client_message(&self) -> String {
		match &self.hint {
			Some(hint) => format!("{} {}", self.message, hint),
			None => self.message.clone(),
		}
	}

	/// The failure as the cause of the session's stop.
	pub fn stop_cause(&self) -> StopCause {
		StopCause {
			detail: self.message.clone(),
			hint: self.hint.clone(),
			unexpected: false,
			reported: true,
		}
	}

	/// A failure whose cause was only logged.
	pub fn see_log(code: &'static str, message: impl Into<String>) -> Self {
		Self::new(code, message).with_hint(SEE_SERVICE_LOG)
	}

	pub(crate) fn busy(message: impl Into<String>) -> Self {
		Self::new("session_busy", message).with_hint("Wait a moment and try again.")
	}

	pub(crate) fn shutting_down() -> Self {
		Self::new("service_shutdown", "Pyroshine is shutting down.")
	}

	/// The session was stopped (by the operator, a client or a failing worker)
	/// while this step ran.
	pub(crate) fn cancelled() -> Self {
		Self::new(
			"session_cancelled",
			"The session was stopped before it finished starting.",
		)
	}
}

impl std::fmt::Display for SessionFailure {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

/// A failure outside a session's lifetime that the operator should see, for
/// example a rejected launch or stream negotiation. Clients often show only a
/// status code for these; the management interface shows the cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
	pub code: &'static str,
	pub message: String,
	pub hint: Option<String>,
	pub at: std::time::SystemTime,
}

impl From<&SessionFailure> for Problem {
	fn from(failure: &SessionFailure) -> Self {
		Self {
			code: failure.code,
			message: failure.message.clone(),
			hint: failure.hint.clone(),
			at: std::time::SystemTime::now(),
		}
	}
}

impl Problem {
	pub fn new(code: &'static str, message: impl Into<String>, hint: Option<String>) -> Self {
		Self {
			code,
			message: message.into(),
			hint,
			at: std::time::SystemTime::now(),
		}
	}
}

/// Where operators report bugs that no setting can work around.
pub(crate) const ISSUES_URL: &str = "https://github.com/karsyboy/pyroshine/issues";

/// Where the technical cause of a failure was recorded.
pub(crate) const SEE_SERVICE_LOG: &str =
	"The Pyroshine service log has the details (for the packaged service: journalctl -u \"pyroshine@$USER\" -e).";

/// What ended a running session, beyond its [`SessionShutdownReason`].
///
/// [`SessionShutdownReason`]: crate::session::manager::SessionShutdownReason
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopCause {
	/// What happened, for example "Steam exited with status 1, 4 s after launch."
	pub detail: String,
	pub hint: Option<String>,
	/// The cause makes the stop unexpected even when its reason alone does
	/// not (an application that crashed rather than quit).
	pub unexpected: bool,
	/// The operator already saw this cause (a launch failure reported with
	/// the launch); the stop needs no event of its own.
	pub reported: bool,
}

/// The first [`StopCause`] recorded for one session.
///
/// A worker that ends the session records why *before* it triggers the stop,
/// so teardown, which reads the slot after every worker finished, sees it.
/// Teardown seals the slot when it begins: whatever happens afterwards is a
/// consequence of the stop, including the application exiting because
/// teardown stopped it (before it triggers the workers' stop). Later records
/// are ignored for the same reason.
#[derive(Clone, Debug, Default)]
pub(crate) struct StopCauseSlot(Arc<OnceLock<Option<StopCause>>>);

impl StopCauseSlot {
	pub fn record(&self, cause: StopCause) {
		let _ = self.0.set(Some(cause));
	}

	/// Keep the cause recorded so far, if any, and ignore later records.
	pub fn seal(&self) {
		let _ = self.0.set(None);
	}

	pub fn get(&self) -> Option<StopCause> {
		self.0.get().cloned().flatten()
	}
}

/// A duration for operator messages: "850 ms", "4 s", "2 min 5 s", "1 h 3 min".
pub(crate) fn human_duration(duration: std::time::Duration) -> String {
	let seconds = duration.as_secs();
	match seconds {
		0 => format!("{} ms", duration.as_millis()),
		1..60 => format!("{seconds} s"),
		60..3600 => format!("{} min {} s", seconds / 60, seconds % 60),
		_ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_first_recorded_cause_wins() {
		let slot = StopCauseSlot::default();
		assert_eq!(slot.get(), None);
		let first = StopCause {
			detail: "first".into(),
			hint: None,
			unexpected: false,
			reported: false,
		};
		slot.clone().record(first.clone());
		slot.record(StopCause {
			detail: "second".into(),
			hint: Some("ignored".into()),
			unexpected: true,
			reported: false,
		});
		assert_eq!(slot.get(), Some(first.clone()));
		slot.seal();
		assert_eq!(slot.get(), Some(first), "sealing keeps the recorded cause");

		// A slot sealed before any record stays empty.
		let sealed = StopCauseSlot::default();
		sealed.seal();
		sealed.record(StopCause {
			detail: "the application exited because teardown stopped it".into(),
			hint: None,
			unexpected: true,
			reported: false,
		});
		assert_eq!(sealed.get(), None);
	}

	#[test]
	fn durations_read_naturally() {
		use std::time::Duration;
		assert_eq!(human_duration(Duration::from_millis(850)), "850 ms");
		assert_eq!(human_duration(Duration::from_secs(4)), "4 s");
		assert_eq!(human_duration(Duration::from_secs(125)), "2 min 5 s");
		assert_eq!(human_duration(Duration::from_secs(3780)), "1 h 3 min");
	}

	#[test]
	fn client_message_appends_the_hint() {
		let failure = SessionFailure::new("x", "Something failed.").with_hint("Do this.");
		assert_eq!(failure.client_message(), "Something failed. Do this.");
		assert_eq!(SessionFailure::new("x", "Only.").client_message(), "Only.");
	}
}
