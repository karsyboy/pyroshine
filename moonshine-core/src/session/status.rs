//! Externally visible session state.
//!
//! The manager's lifecycle (`Idle`, `Live`, `Stopping`) says whether a session
//! owns resources, not whether a client is streaming: a session deliberately
//! survives client loss so Moonlight can resume it. Whether media flows is
//! decided by the control stream, which dispatches only the peer that
//! authenticated the current launch/resume generation (`stream/control/peers.rs`).
//!
//! The manager publishes a [`ManagerStatus`] snapshot whenever its state
//! changes and the control stream publishes [`ClientPresence`] whenever its
//! active peer changes. [`derive_phase`] combines both into the public
//! [`SessionPhase`]. Neither publication is on a media path: both happen on
//! lifecycle transitions and peer ownership changes only.

use std::time::{Duration, Instant, SystemTime};

pub use moonshine_management::dto::SessionPhase;

use crate::session::manager::SessionShutdownReason;

/// Manager transition in progress, as visible to status consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransitionStatus {
	Initialize,
	Launch,
	Start,
	Announce,
	Resume,
}

/// Snapshot of the session manager, published on every change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagerStatus {
	pub lifecycle: LifecycleStatus,
	/// How the most recent session ended.
	pub last_stop: Option<StopRecord>,
}

impl Default for ManagerStatus {
	fn default() -> Self {
		Self {
			lifecycle: LifecycleStatus::Idle,
			last_stop: None,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleStatus {
	Idle,
	Live(LiveStatus),
	Stopping,
	/// Teardown exceeded its deadline; the service is shutting down.
	Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveStatus {
	pub epoch: u64,
	pub transition: Option<TransitionStatus>,
	/// RTSP PLAY committed stream contexts and the stream workers exist.
	pub streams_committed: bool,
	/// The session's stop was triggered; teardown is about to own it.
	pub stopping: bool,
	/// A streaming session accepted `/resume` or a reconnect ANNOUNCE that PLAY
	/// has not committed yet.
	pub reconnect_pending: bool,
	/// Current launch/resume authorization generation.
	pub generation: Option<u64>,
	/// Generation created by the session's `/launch`.
	pub first_generation: Option<u64>,
	/// Last protocol progress (launch, resume, ANNOUNCE, PLAY). A session
	/// that waits longer than the client's patience for its client is
	/// reported as disconnected rather than starting or reconnecting forever.
	pub awaiting_since: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StopRecord {
	pub epoch: u64,
	pub reason: SessionShutdownReason,
	/// Teardown released everything within its deadline.
	pub completed: bool,
	pub at: SystemTime,
}

/// Control-stream ownership of the current generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientPresence {
	/// Authorization generation the control stream follows.
	pub generation: u64,
	/// A peer authenticated this generation and owns input and media.
	pub attached: bool,
	/// A peer has owned this generation (it may still own it). Together with
	/// `attached == false` this means the client left (disconnect or timeout).
	pub was_attached: bool,
}

/// Combine manager and control-stream state into the public phase.
///
/// `client_patience` bounds how long a session waits for its client before it
/// is reported as disconnected; the server uses the stream timeout, the same
/// patience the control stream gives a silent peer.
pub(crate) fn derive_phase(
	status: &ManagerStatus,
	presence: &ClientPresence,
	now: Instant,
	client_patience: Duration,
) -> SessionPhase {
	let live = match &status.lifecycle {
		LifecycleStatus::Idle => return SessionPhase::Idle,
		LifecycleStatus::Stopping => return SessionPhase::Stopping,
		LifecycleStatus::Failed => return SessionPhase::Error,
		LifecycleStatus::Live(live) => live,
	};
	if live.stopping {
		return SessionPhase::Stopping;
	}
	match live.transition {
		Some(TransitionStatus::Initialize | TransitionStatus::Launch | TransitionStatus::Start) => {
			return SessionPhase::Starting;
		},
		Some(TransitionStatus::Announce | TransitionStatus::Resume) => return SessionPhase::Reconnecting,
		None => {},
	}
	let current = live.streams_committed && !live.reconnect_pending && live.generation == Some(presence.generation);
	if current && presence.attached {
		return SessionPhase::Streaming;
	}
	if current && presence.was_attached {
		// The client left and nobody resumed yet.
		return SessionPhase::ClientDisconnected;
	}
	// Waiting for the current generation's client to finish negotiating.
	if now.saturating_duration_since(live.awaiting_since) >= client_patience {
		return SessionPhase::ClientDisconnected;
	}
	if live.generation == live.first_generation {
		SessionPhase::Starting
	} else {
		SessionPhase::Reconnecting
	}
}

/// When [`derive_phase`] can change without a new status or presence value:
/// the end of the client's patience while the session awaits its client.
pub(crate) fn patience_deadline(
	status: &ManagerStatus,
	presence: &ClientPresence,
	client_patience: Duration,
) -> Option<Instant> {
	let LifecycleStatus::Live(live) = &status.lifecycle else {
		return None;
	};
	if live.stopping || live.transition.is_some() {
		return None;
	}
	let current = live.streams_committed && !live.reconnect_pending && live.generation == Some(presence.generation);
	if current && (presence.attached || presence.was_attached) {
		return None;
	}
	Some(live.awaiting_since + client_patience)
}

/// Explanation of a stop reason for the operator.
pub(crate) fn describe_stop(reason: SessionShutdownReason) -> (&'static str, &'static str, bool) {
	use SessionShutdownReason as Reason;
	match reason {
		Reason::ManagerShutdown => ("service_shutdown", "Pyroshine is shutting down.", false),
		Reason::UserStopped => ("user_stopped", "The session was ended.", false),
		Reason::ApplicationStopped => ("application_stopped", "The application exited.", false),
		Reason::VideoPacketHandlerStopped => (
			"video_transport_failed",
			"The video transport stopped unexpectedly.",
			true,
		),
		Reason::VideoEncoderStopped => ("video_encoder_failed", "The video encoder stopped unexpectedly.", true),
		Reason::AudioPacketHandlerStopped => (
			"audio_transport_failed",
			"The audio transport stopped unexpectedly.",
			true,
		),
		Reason::PulseServerStopped => ("audio_server_failed", "The audio server stopped unexpectedly.", true),
		Reason::AudioEncoderStopped => ("audio_encoder_failed", "The audio encoder stopped unexpectedly.", true),
		Reason::ControlStreamStopped => (
			"control_stream_failed",
			"The control stream stopped unexpectedly.",
			true,
		),
		Reason::InputHandlerStopped => ("input_failed", "Input handling stopped unexpectedly.", true),
		Reason::CompositorStopped => ("compositor_failed", "The compositor stopped unexpectedly.", true),
		Reason::TransitionFailed => (
			"transition_failed",
			"Starting or reconfiguring the stream failed.",
			true,
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const PATIENCE: Duration = Duration::from_secs(60);

	fn live(f: impl FnOnce(&mut LiveStatus)) -> ManagerStatus {
		let mut live = LiveStatus {
			epoch: 1,
			transition: None,
			streams_committed: true,
			stopping: false,
			reconnect_pending: false,
			generation: Some(1),
			first_generation: Some(1),
			awaiting_since: Instant::now(),
		};
		f(&mut live);
		ManagerStatus {
			lifecycle: LifecycleStatus::Live(live),
			last_stop: None,
		}
	}

	fn presence(generation: u64, attached: bool, was_attached: bool) -> ClientPresence {
		ClientPresence {
			generation,
			attached,
			was_attached,
		}
	}

	fn phase(status: &ManagerStatus, presence: ClientPresence) -> SessionPhase {
		derive_phase(status, &presence, Instant::now(), PATIENCE)
	}

	#[test]
	fn lifecycle_without_a_live_session() {
		let none = ClientPresence::default();
		assert_eq!(phase(&ManagerStatus::default(), none), SessionPhase::Idle);
		let stopping = ManagerStatus {
			lifecycle: LifecycleStatus::Stopping,
			last_stop: None,
		};
		assert_eq!(phase(&stopping, none), SessionPhase::Stopping);
		let failed = ManagerStatus {
			lifecycle: LifecycleStatus::Failed,
			last_stop: None,
		};
		assert_eq!(phase(&failed, none), SessionPhase::Error);
		// A triggered stop is reported before teardown takes the session over.
		assert_eq!(
			phase(&live(|l| l.stopping = true), presence(1, true, true)),
			SessionPhase::Stopping
		);
	}

	#[test]
	fn launch_and_first_negotiation_are_starting() {
		for transition in [
			TransitionStatus::Initialize,
			TransitionStatus::Launch,
			TransitionStatus::Start,
		] {
			let status = live(|l| {
				l.transition = Some(transition);
				l.streams_committed = false;
			});
			assert_eq!(phase(&status, ClientPresence::default()), SessionPhase::Starting);
		}
		// Launched, waiting for ANNOUNCE/PLAY, then PLAY committed but the
		// control stream has not authenticated yet.
		let launched = live(|l| l.streams_committed = false);
		assert_eq!(phase(&launched, ClientPresence::default()), SessionPhase::Starting);
		assert_eq!(phase(&live(|_| {}), presence(1, false, false)), SessionPhase::Starting);
	}

	#[test]
	fn only_an_attached_current_peer_is_streaming() {
		assert_eq!(phase(&live(|_| {}), presence(1, true, true)), SessionPhase::Streaming);
		// A peer of an older generation never makes a session streaming.
		let resumed = live(|l| l.generation = Some(2));
		assert_eq!(phase(&resumed, presence(1, true, true)), SessionPhase::Reconnecting);
	}

	#[test]
	fn client_loss_retains_a_disconnected_session() {
		assert_eq!(
			phase(&live(|_| {}), presence(1, false, true)),
			SessionPhase::ClientDisconnected
		);
		// The application keeps running long after the client left.
		let long_ago = live(|l| l.awaiting_since = Instant::now() - PATIENCE * 10);
		assert_eq!(
			phase(&long_ago, presence(1, false, true)),
			SessionPhase::ClientDisconnected
		);
	}

	#[test]
	fn resume_is_reconnecting_until_the_new_peer_attaches() {
		// `/resume` rotated the generation; ANNOUNCE/PLAY pending.
		let resumed = live(|l| {
			l.generation = Some(2);
			l.reconnect_pending = true;
		});
		assert_eq!(phase(&resumed, presence(1, false, true)), SessionPhase::Reconnecting);
		for transition in [TransitionStatus::Announce, TransitionStatus::Resume] {
			let status = live(|l| {
				l.generation = Some(2);
				l.transition = Some(transition);
			});
			assert_eq!(phase(&status, presence(1, false, true)), SessionPhase::Reconnecting);
		}
		// PLAY committed; the control stream adopted the generation.
		let committed = live(|l| l.generation = Some(2));
		assert_eq!(phase(&committed, presence(2, false, false)), SessionPhase::Reconnecting);
		assert_eq!(phase(&committed, presence(2, true, true)), SessionPhase::Streaming);
	}

	#[test]
	fn an_abandoned_launch_or_resume_becomes_disconnected() {
		let stale = Instant::now() - PATIENCE - Duration::from_secs(1);
		let abandoned_launch = live(|l| {
			l.streams_committed = false;
			l.awaiting_since = stale;
		});
		assert_eq!(
			phase(&abandoned_launch, ClientPresence::default()),
			SessionPhase::ClientDisconnected
		);
		let abandoned_resume = live(|l| {
			l.generation = Some(2);
			l.reconnect_pending = true;
			l.awaiting_since = stale;
		});
		assert_eq!(
			phase(&abandoned_resume, presence(1, false, true)),
			SessionPhase::ClientDisconnected
		);
		assert!(patience_deadline(&abandoned_resume, &presence(1, false, true), PATIENCE).is_some());
		assert!(patience_deadline(&live(|_| {}), &presence(1, true, true), PATIENCE).is_none());
	}

	#[test]
	fn every_stop_reason_is_described() {
		use SessionShutdownReason as Reason;
		for reason in [
			Reason::ManagerShutdown,
			Reason::UserStopped,
			Reason::ApplicationStopped,
			Reason::VideoPacketHandlerStopped,
			Reason::VideoEncoderStopped,
			Reason::AudioPacketHandlerStopped,
			Reason::PulseServerStopped,
			Reason::AudioEncoderStopped,
			Reason::ControlStreamStopped,
			Reason::InputHandlerStopped,
			Reason::CompositorStopped,
			Reason::TransitionFailed,
		] {
			let (code, message, _) = describe_stop(reason);
			assert!(!code.is_empty() && !message.is_empty());
		}
		assert!(!describe_stop(Reason::UserStopped).2);
		assert!(describe_stop(Reason::CompositorStopped).2);
	}
}
