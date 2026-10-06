//! Feedback routing for virtual controllers that outlive a controlling peer.
//!
//! A retained session keeps its virtual controllers across a Moonlight
//! reconnect so the running game never sees an unplug. Device lifetime and
//! input ownership are therefore separate: Inputtino's native callbacks
//! (rumble, LED, adaptive triggers) are installed once per device and must not
//! stay bound to the feedback channel of the peer that created it.
//!
//! [`FeedbackRoute`] is the indirection those callbacks hold. Revocation is
//! synchronous: after [`FeedbackRoute::revoke`] returns, no callback can obtain
//! a sender, so feedback produced while the device is unowned is dropped and can
//! never reach the next peer. A callback that obtained the previous owner's
//! sender just before revocation can only deliver into that owner's channel,
//! which the control stream has already replaced.
//!
//! Persistent device state is different from feedback events. Inputtino
//! deduplicates trigger effects per trigger and a game does not re-send LED or
//! trigger configuration to a device it believes never disconnected, so a
//! claim replays the device's *current* LED colour and per-trigger effects to
//! the new owner (and re-requests motion reports for motion-capable devices).
//! Rumble is transient and is never replayed.
//!
//! Delivery never blocks (review 2026-10-05 STAB-004). The control loop drains
//! the owner's bounded feedback channel while it may itself be waiting to queue
//! gamepad commands, so a blocking send from the gamepad thread or a native
//! callback could form a cycle that also stalls shutdown. When the channel is
//! full, feedback is coalesced per kind instead: the latest rumble supersedes an
//! undelivered one (a rumble stop is therefore never lost), and LED, trigger and
//! motion-enable state is re-sent from the current state. The gamepad timer
//! task retries pending feedback until the owner accepts it or is revoked.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, mpsc};

use crate::session::stream::control::FeedbackCommand;
use crate::session::stream::control::feedback::{EnableMotionEventCommand, SetLedCommand, TriggerEffectCommand};

/// `TriggerEffectCommand::trigger_event_flags` bits (DualSense output report).
const RIGHT_TRIGGER_EFFECT: u8 = 0x04;
const LEFT_TRIGGER_EFFECT: u8 = 0x08;
/// Motion report rate requested from the client, in Hz.
const MOTION_REPORT_RATE: u16 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TriggerState {
	effect_type: u8,
	data: [u8; 10],
}

/// Feedback the current owner's channel had no room for.
#[derive(Default)]
struct Pending {
	/// The latest undelivered rumble.
	rumble: Option<FeedbackCommand>,
	led: bool,
	triggers: bool,
	motion: bool,
}

impl Pending {
	fn any(&self) -> bool {
		self.rumble.is_some() || self.led || self.triggers || self.motion
	}
}

#[derive(Default)]
struct RouteState {
	owner: Option<mpsc::Sender<FeedbackCommand>>,
	led: Option<(u8, u8, u8)>,
	left_trigger: Option<TriggerState>,
	right_trigger: Option<TriggerState>,
	pending: Pending,
}

impl RouteState {
	fn record(&mut self, command: &FeedbackCommand) {
		match command {
			FeedbackCommand::SetLed(led) => self.led = Some(led.rgb),
			FeedbackCommand::TriggerEffect(effect) => {
				if effect.trigger_event_flags & LEFT_TRIGGER_EFFECT != 0 {
					self.left_trigger = Some(TriggerState {
						effect_type: effect.type_left,
						data: effect.left,
					});
				}
				if effect.trigger_event_flags & RIGHT_TRIGGER_EFFECT != 0 {
					self.right_trigger = Some(TriggerState {
						effect_type: effect.type_right,
						data: effect.right,
					});
				}
			},
			FeedbackCommand::Rumble(_) | FeedbackCommand::EnableMotionEvent(_) => {},
		}
	}

	fn led_command(&self, id: u16) -> Option<FeedbackCommand> {
		self.led.map(|rgb| FeedbackCommand::SetLed(SetLedCommand { id, rgb }))
	}

	fn trigger_command(&self, id: u16) -> Option<FeedbackCommand> {
		if self.left_trigger.is_none() && self.right_trigger.is_none() {
			return None;
		}
		let left = self.left_trigger.unwrap_or_default();
		let right = self.right_trigger.unwrap_or_default();
		let mut flags = 0;
		if self.left_trigger.is_some() {
			flags |= LEFT_TRIGGER_EFFECT;
		}
		if self.right_trigger.is_some() {
			flags |= RIGHT_TRIGGER_EFFECT;
		}
		Some(FeedbackCommand::TriggerEffect(TriggerEffectCommand {
			id,
			trigger_event_flags: flags,
			type_left: left.effect_type,
			type_right: right.effect_type,
			left: left.data,
			right: right.data,
		}))
	}

	/// Try to hand pending feedback to the owner, oldest kinds first: the
	/// persistent state a claim replays, then motion enables, then rumble.
	/// Stops at the first full send; returns whether nothing is left pending.
	fn flush(&mut self, id: u16) -> bool {
		let Some(owner) = self.owner.clone() else {
			self.pending = Pending::default();
			return true;
		};
		let send = |command: FeedbackCommand| match owner.try_send(command) {
			Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => true,
			Err(mpsc::error::TrySendError::Full(_)) => false,
		};
		if self.pending.led {
			if !self.led_command(id).is_none_or(send) {
				return false;
			}
			self.pending.led = false;
		}
		if self.pending.triggers {
			if !self.trigger_command(id).is_none_or(send) {
				return false;
			}
			self.pending.triggers = false;
		}
		if self.pending.motion {
			for motion_type in [
				inputtino::JoypadMotionType::ACCELERATION as u8,
				inputtino::JoypadMotionType::GYROSCOPE as u8,
			] {
				// Re-enabling an enabled motion type is harmless, so a partial
				// pair is simply sent again.
				if !send(FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
					id,
					report_rate: MOTION_REPORT_RATE,
					motion_type,
				})) {
					return false;
				}
			}
			self.pending.motion = false;
		}
		if let Some(rumble) = self.pending.rumble.take()
			&& !send(rumble.clone())
		{
			self.pending.rumble = Some(rumble);
			return false;
		}
		true
	}
}

/// Owner-switchable feedback destination for one virtual controller.
#[derive(Clone)]
pub(crate) struct FeedbackRoute {
	index: u8,
	/// Whether the device reports motion, so each owner must enable it.
	motion: bool,
	state: Arc<Mutex<RouteState>>,
	/// Wakes the gamepad timer task, which retries pending feedback.
	retry: Arc<Notify>,
}

impl FeedbackRoute {
	pub fn new(index: u8, motion: bool, retry: Arc<Notify>) -> Self {
		Self {
			index,
			motion,
			state: Default::default(),
			retry,
		}
	}

	fn state(&self) -> MutexGuard<'_, RouteState> {
		// Holders never panic while locked; recover the plain data if one did.
		self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	/// Deliver feedback to the current owner without blocking. Called from
	/// Inputtino callback threads, the Valve backend and the gamepad thread.
	///
	/// The lock is held only for non-blocking sends, so it orders deliveries
	/// against claim and revocation without ever waiting on the channel.
	pub fn deliver(&self, command: FeedbackCommand) {
		let id = u16::from(self.index);
		let mut state = self.state();
		state.record(&command);
		if state.owner.is_none() {
			return;
		}
		// Older pending feedback goes first; if it still does not fit, the new
		// command is coalesced behind it rather than overtaking it.
		let command = if state.flush(id) {
			match state.owner.as_ref().expect("checked above").try_send(command) {
				Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
				Err(mpsc::error::TrySendError::Full(command)) => command,
			}
		} else {
			command
		};
		match command {
			FeedbackCommand::Rumble(_) => state.pending.rumble = Some(command),
			FeedbackCommand::SetLed(_) => state.pending.led = true,
			FeedbackCommand::TriggerEffect(_) => state.pending.triggers = true,
			FeedbackCommand::EnableMotionEvent(_) => state.pending.motion = true,
		}
		drop(state);
		self.retry.notify_one();
	}

	/// Retry pending feedback. Returns whether some is still pending.
	pub fn flush_pending(&self) -> bool {
		let mut state = self.state();
		state.pending.any() && !state.flush(u16::from(self.index))
	}

	pub fn is_owned(&self) -> bool {
		self.state().owner.as_ref().is_some_and(|owner| !owner.is_closed())
	}

	/// Revoke the current owner; feedback is dropped until the next claim.
	pub fn revoke(&self) {
		let mut state = self.state();
		state.owner = None;
		state.pending = Pending::default();
	}

	/// Bind the route to `owner`. Returns `false` if it already was.
	///
	/// On a change, the device's persistent LED and trigger state and, for
	/// motion devices, the motion enables are queued to the new owner while the
	/// lock is held, so any callback that runs after the claim is ordered after
	/// the replay. What does not fit is retried; nothing here waits.
	pub fn claim(&self, owner: &mpsc::Sender<FeedbackCommand>) -> bool {
		let mut state = self.state();
		if state.owner.as_ref().is_some_and(|current| current.same_channel(owner)) {
			return false;
		}
		state.owner = Some(owner.clone());
		state.pending = Pending {
			rumble: None,
			led: state.led.is_some(),
			triggers: state.left_trigger.is_some() || state.right_trigger.is_some(),
			motion: self.motion,
		};
		if !state.flush(u16::from(self.index)) {
			drop(state);
			self.retry.notify_one();
		}
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::stream::control::feedback::RumbleCommand;

	fn rumble(level: u16) -> FeedbackCommand {
		FeedbackCommand::Rumble(RumbleCommand {
			id: 3,
			low_frequency: level,
			high_frequency: level,
		})
	}

	fn trigger(flags: u8, left: u8, right: u8) -> FeedbackCommand {
		FeedbackCommand::TriggerEffect(TriggerEffectCommand {
			id: 3,
			trigger_event_flags: flags,
			type_left: left,
			type_right: right,
			left: [left; 10],
			right: [right; 10],
		})
	}

	fn route(index: u8, motion: bool) -> FeedbackRoute {
		FeedbackRoute::new(index, motion, Arc::new(Notify::new()))
	}

	fn motion_enable(id: u16, motion_type: inputtino::JoypadMotionType) -> FeedbackCommand {
		FeedbackCommand::EnableMotionEvent(EnableMotionEventCommand {
			id,
			report_rate: MOTION_REPORT_RATE,
			motion_type: motion_type as u8,
		})
	}

	fn drain(rx: &mut mpsc::Receiver<FeedbackCommand>) -> Vec<FeedbackCommand> {
		std::iter::from_fn(|| rx.try_recv().ok()).collect()
	}

	/// Native callbacks run on Inputtino threads, never inside the runtime.
	fn from_native_thread(route: &FeedbackRoute, command: FeedbackCommand) {
		let route = route.clone();
		std::thread::spawn(move || route.deliver(command)).join().unwrap();
	}

	#[test]
	fn feedback_after_revocation_never_reaches_any_peer() {
		let route = route(3, false);
		let (old, mut old_rx) = mpsc::channel(10);
		let (new, mut new_rx) = mpsc::channel(10);
		route.claim(&old);
		from_native_thread(&route, rumble(1));
		assert_eq!(drain(&mut old_rx), vec![rumble(1)]);

		route.revoke();
		assert!(!route.is_owned());
		from_native_thread(&route, rumble(2));
		route.deliver(rumble(3));
		assert!(drain(&mut old_rx).is_empty(), "revoked owner receives nothing");

		route.claim(&new);
		assert!(drain(&mut new_rx).is_empty(), "rumble is transient and not replayed");
		from_native_thread(&route, rumble(4));
		assert_eq!(drain(&mut new_rx), vec![rumble(4)]);
		assert!(drain(&mut old_rx).is_empty());
	}

	#[test]
	fn closed_owner_is_not_considered_owned() {
		let route = route(0, false);
		let (owner, owner_rx) = mpsc::channel(10);
		route.claim(&owner);
		assert!(route.is_owned());
		drop(owner_rx);
		assert!(!route.is_owned());
	}

	#[test]
	fn reclaim_by_the_same_owner_is_a_no_op() {
		let route = route(1, true);
		let (owner, mut rx) = mpsc::channel(10);
		assert!(route.claim(&owner));
		assert_eq!(drain(&mut rx).len(), 2, "motion enables");
		assert!(!route.claim(&owner.clone()), "same channel, no re-enable");
		assert!(drain(&mut rx).is_empty());
	}

	#[test]
	fn new_owner_receives_current_persistent_state_and_motion_enable() {
		let route = route(3, true);
		let (old, mut old_rx) = mpsc::channel(10);
		route.claim(&old);
		from_native_thread(&route, FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (1, 2, 3) }));
		// Left and right effects arrive separately; both persist.
		from_native_thread(&route, trigger(LEFT_TRIGGER_EFFECT, 5, 0));
		from_native_thread(&route, trigger(RIGHT_TRIGGER_EFFECT, 0, 7));
		route.revoke();
		// State changes while unowned are not delivered but are current state.
		from_native_thread(&route, FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (9, 9, 9) }));
		// The old owner got its motion enables, the LED and both triggers.
		assert_eq!(drain(&mut old_rx).len(), 5);

		let (new, mut new_rx) = mpsc::channel(10);
		assert!(route.claim(&new));
		assert_eq!(
			drain(&mut new_rx),
			vec![
				FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (9, 9, 9) }),
				FeedbackCommand::TriggerEffect(TriggerEffectCommand {
					id: 3,
					trigger_event_flags: LEFT_TRIGGER_EFFECT | RIGHT_TRIGGER_EFFECT,
					type_left: 5,
					type_right: 7,
					left: [5; 10],
					right: [7; 10],
				}),
				motion_enable(3, inputtino::JoypadMotionType::ACCELERATION),
				motion_enable(3, inputtino::JoypadMotionType::GYROSCOPE),
			]
		);
	}

	#[test]
	fn devices_without_motion_or_state_replay_nothing() {
		let route = route(2, false);
		let (owner, mut rx) = mpsc::channel(10);
		assert!(route.claim(&owner));
		assert!(drain(&mut rx).is_empty());
	}

	/// Review 2026-10-05 STAB-004: delivery never blocks on a full channel.
	/// Undelivered rumble coalesces to the latest command (so a stop is never
	/// lost), persistent state and motion enables are re-sent from current
	/// state, and pending feedback is retried in order once there is room.
	#[test]
	fn full_channel_coalesces_and_retries_without_blocking() {
		let wake = Arc::new(Notify::new());
		let route = FeedbackRoute::new(3, true, wake.clone());
		let (owner, mut rx) = mpsc::channel(2);
		owner.try_send(rumble(100)).unwrap();
		owner.try_send(rumble(101)).unwrap();
		// A claim on a full channel queues its replay without waiting.
		assert!(route.claim(&owner));
		// Native callbacks return at once, in any number.
		for level in 1..=50 {
			from_native_thread(&route, rumble(level));
		}
		from_native_thread(&route, FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (1, 2, 3) }));
		from_native_thread(&route, rumble(0));
		assert_eq!(drain(&mut rx), vec![rumble(100), rumble(101)]);
		// Retries deliver the oldest kinds first, two at a time.
		assert!(route.flush_pending());
		assert_eq!(
			drain(&mut rx),
			vec![
				FeedbackCommand::SetLed(SetLedCommand { id: 3, rgb: (1, 2, 3) }),
				motion_enable(3, inputtino::JoypadMotionType::ACCELERATION),
			]
		);
		assert!(route.flush_pending());
		assert_eq!(
			drain(&mut rx),
			vec![
				motion_enable(3, inputtino::JoypadMotionType::ACCELERATION),
				motion_enable(3, inputtino::JoypadMotionType::GYROSCOPE)
			]
		);
		assert!(!route.flush_pending());
		assert_eq!(drain(&mut rx), vec![rumble(0)], "only the latest rumble, the stop");
		// The retry task was woken.
		assert!(futures_now(wake.notified()));
		// Revocation drops whatever is still pending.
		owner.try_send(rumble(7)).unwrap();
		owner.try_send(rumble(8)).unwrap();
		route.deliver(rumble(9));
		route.revoke();
		assert!(!route.flush_pending());
		assert_eq!(drain(&mut rx), vec![rumble(7), rumble(8)]);
	}

	/// Whether `future` is already ready.
	fn futures_now(future: impl std::future::Future<Output = ()>) -> bool {
		let mut future = std::pin::pin!(future);
		let waker = std::task::Waker::noop();
		future
			.as_mut()
			.poll(&mut std::task::Context::from_waker(waker))
			.is_ready()
	}
}
