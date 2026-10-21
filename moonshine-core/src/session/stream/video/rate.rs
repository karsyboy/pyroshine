//! Bounded, generation-scoped live encoder controls. The empty hot path is
//! one atomic load; requests and status changes lock only when an update exists.
use serde::Serialize;
use std::sync::{
	Arc, Mutex,
	atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RateState {
	Requested,
	Staged,
	Applied,
	Superseded,
	Stale,
	Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RateUpdate {
	pub generation: u64,
	pub requested_bps: u32,
	pub accepted_bps: Option<u32>,
	pub state: RateState,
	/// Encoder display order, before packetization; not the wrapping client index.
	pub first_affected_frame: Option<u64>,
	pub error: Option<String>,
}

struct Command {
	id: u64,
	update: RateUpdate,
	reply: oneshot::Sender<RateUpdate>,
}
#[derive(Default)]
struct State {
	next: u64,
	closed: bool,
	command: Option<Command>,
	status: Option<(u64, RateUpdate)>,
}
struct Shared {
	generation: AtomicU64,
	pending: AtomicBool,
	state: Mutex<State>,
}

#[derive(Clone)]
pub(crate) struct RateControl(Arc<Shared>);
pub(crate) struct RateReceiver(Arc<Shared>);
pub(crate) struct PendingRate {
	shared: Arc<Shared>,
	command: Option<Command>,
}

pub(crate) fn channel(generation: u64) -> (RateControl, RateReceiver) {
	let shared = Arc::new(Shared {
		generation: AtomicU64::new(generation),
		pending: AtomicBool::new(false),
		state: Mutex::default(),
	});
	(RateControl(shared.clone()), RateReceiver(shared))
}

impl RateControl {
	pub fn request(&self, generation: u64, bitrate: u32) -> Result<oneshot::Receiver<RateUpdate>, String> {
		if bitrate == 0 {
			return Err("live bitrate must be non-zero".into());
		}
		let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.closed {
			return Err("video rate control is closed".into());
		}
		if generation != self.0.generation.load(Ordering::Acquire) {
			return Err("stale video generation".into());
		}
		state.next = state.next.checked_add(1).ok_or("rate request sequence exhausted")?;
		let id = state.next;
		let update = RateUpdate {
			generation,
			requested_bps: bitrate,
			accepted_bps: None,
			state: RateState::Requested,
			first_affected_frame: None,
			error: None,
		};
		let (reply, waiting) = oneshot::channel();
		if let Some(mut old) = state.command.replace(Command {
			id,
			update: update.clone(),
			reply,
		}) {
			old.update.state = RateState::Superseded;
			let _ = old.reply.send(old.update);
		}
		state.status = Some((id, update));
		self.0.pending.store(true, Ordering::Release);
		Ok(waiting)
	}
	pub fn status(&self) -> Option<RateUpdate> {
		self.0
			.state
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.status
			.as_ref()
			.map(|(_, update)| update.clone())
	}
}

impl RateReceiver {
	pub fn activate(&self, generation: u64) {
		self.0.generation.store(generation, Ordering::Release);
		let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
		if state
			.command
			.as_ref()
			.is_some_and(|c| c.update.generation != generation)
			&& let Some(mut command) = state.command.take()
		{
			command.update.state = RateState::Stale;
			let _ = command.reply.send(command.update);
		}
		if state.status.as_ref().is_some_and(|(_, s)| s.generation != generation) {
			state.status = None;
		}
	}
	pub fn take(&self) -> Option<PendingRate> {
		if !self.0.pending.load(Ordering::Acquire) {
			return None;
		}
		let command = {
			let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
			self.0.pending.store(false, Ordering::Release);
			state.command.take()?
		};
		let pending = PendingRate {
			shared: self.0.clone(),
			command: Some(command),
		};
		if pending.generation() != self.0.generation.load(Ordering::Acquire) {
			pending.finish(RateState::Stale, None, None);
			return None;
		}
		Some(pending)
	}
}

impl PendingRate {
	pub fn generation(&self) -> u64 {
		self.command.as_ref().expect("owned rate request").update.generation
	}
	pub fn accepted(&self) -> u32 {
		self.command
			.as_ref()
			.expect("owned rate request")
			.update
			.accepted_bps
			.expect("staged rate")
	}
	pub fn requested(&self) -> u32 {
		self.command.as_ref().expect("owned rate request").update.requested_bps
	}
	pub fn stage(&mut self, accepted: u32) {
		let command = self.command.as_mut().expect("owned rate request");
		command.update.accepted_bps = Some(accepted);
		command.update.state = RateState::Staged;
		let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.status.as_ref().is_some_and(|(id, _)| *id == command.id) {
			state.status = Some((command.id, command.update.clone()));
		}
	}
	pub fn finish(mut self, result: RateState, frame: Option<u64>, error: Option<String>) {
		let mut command = self.command.take().expect("owned rate request");
		command.update.state = if command.update.generation == self.shared.generation.load(Ordering::Acquire) {
			result
		} else {
			RateState::Stale
		};
		command.update.first_affected_frame = frame;
		command.update.error = error;
		let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.status.as_ref().is_some_and(|(id, _)| *id == command.id) {
			state.status = Some((command.id, command.update.clone()));
		}
		let _ = command.reply.send(command.update);
	}
}
impl Drop for PendingRate {
	fn drop(&mut self) {
		if let Some(mut command) = self.command.take() {
			command.update.state = if command.update.generation == self.shared.generation.load(Ordering::Acquire) {
				RateState::Failed
			} else {
				RateState::Stale
			};
			command.update.error = Some("encoder stopped before applying the rate".into());
			let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
			if state.status.as_ref().is_some_and(|(id, _)| *id == command.id) {
				state.status = Some((command.id, command.update.clone()));
			}
			let _ = command.reply.send(command.update);
		}
	}
}
impl Drop for RateReceiver {
	fn drop(&mut self) {
		let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
		state.closed = true;
		if let Some(mut command) = state.command.take() {
			command.update.state = RateState::Failed;
			command.update.error = Some("video encoder is closed".into());
			state.status = Some((command.id, command.update.clone()));
			let _ = command.reply.send(command.update);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn changes_coalesce_and_apply_only_after_installation() {
		let (control, receiver) = channel(1);
		let first = control.request(1, 10_000_000).unwrap();
		let second = control.request(1, 20_000_000).unwrap();
		assert_eq!(first.blocking_recv().unwrap().state, RateState::Superseded);
		let mut pending = receiver.take().unwrap();
		pending.stage(15_000_000);
		assert_eq!(control.status().unwrap().state, RateState::Staged);
		pending.finish(RateState::Applied, Some(7), None);
		let applied = second.blocking_recv().unwrap();
		assert_eq!(
			(
				applied.requested_bps,
				applied.accepted_bps,
				applied.first_affected_frame
			),
			(20_000_000, Some(15_000_000), Some(7))
		);
		assert_eq!(applied.state, RateState::Applied);
		assert!(receiver.take().is_none());
	}
	#[test]
	fn epochs_close_and_failed_installation_never_report_applied() {
		let (control, receiver) = channel(1);
		assert!(control.request(1, 0).is_err());
		let old = control.request(1, 10).unwrap();
		let mut staged = receiver.take().unwrap();
		staged.stage(10);
		receiver.activate(2);
		staged.finish(RateState::Applied, Some(9), None);
		assert_eq!(old.blocking_recv().unwrap().state, RateState::Stale);
		assert!(control.request(1, 10).is_err());
		let failed = control.request(2, 20).unwrap();
		drop(receiver.take());
		assert_eq!(failed.blocking_recv().unwrap().state, RateState::Failed);
		let closed = control.request(2, 30).unwrap();
		drop(receiver);
		assert_eq!(closed.blocking_recv().unwrap().state, RateState::Failed);
		assert!(control.request(2, 10).is_err());
	}
}
