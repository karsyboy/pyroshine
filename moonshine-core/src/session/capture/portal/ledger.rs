//! Loop-owned buffer identities. An address is valid only for one announcement;
//! removal and reannouncement must never let an old GPU completion requeue it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct BufferId {
	pub address: usize,
	stream: u64,
	pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
	Queued,
	Dequeued,
	Pending,
	Ready,
	Held,
}

pub(super) struct Ledger {
	stream: u64,
	next_generation: u64,
	buffers: HashMap<usize, (BufferId, State)>,
}

impl Default for Ledger {
	fn default() -> Self {
		Self {
			stream: NEXT_STREAM.fetch_add(1, Ordering::Relaxed),
			next_generation: 0,
			buffers: HashMap::new(),
		}
	}
}
impl Ledger {
	pub fn announce(&mut self, address: usize) -> Result<BufferId, &'static str> {
		if self.buffers.contains_key(&address) {
			return Err("the producer announced an existing buffer twice");
		}
		self.next_generation = self
			.next_generation
			.checked_add(1)
			.expect("buffer generation exhausted");
		let id = BufferId {
			address,
			stream: self.stream,
			generation: self.next_generation,
		};
		self.buffers.insert(address, (id, State::Queued));
		Ok(id)
	}

	pub fn remove(&mut self, address: usize) -> Option<BufferId> {
		self.buffers.remove(&address).map(|(id, _)| id)
	}

	pub fn dequeue(&mut self, address: usize) -> Result<BufferId, &'static str> {
		let (id, state) = self
			.buffers
			.get_mut(&address)
			.ok_or("the producer dequeued an unannounced buffer")?;
		if *state != State::Queued {
			return Err("the producer resent a buffer still owned by capture or the GPU");
		}
		*state = State::Dequeued;
		Ok(*id)
	}

	fn transition(&mut self, id: BufferId, from: State, to: State) -> bool {
		let Some((current, state)) = self.buffers.get_mut(&id.address) else {
			return false;
		};
		if *current != id || *state != from {
			return false;
		}
		*state = to;
		true
	}

	pub fn pending(&mut self, id: BufferId) -> bool {
		self.transition(id, State::Dequeued, State::Pending)
	}

	pub fn complete(&mut self, id: BufferId) -> bool {
		self.transition(id, State::Pending, State::Ready)
	}

	pub fn publish(&mut self, id: BufferId) -> bool {
		self.transition(id, State::Ready, State::Held)
	}

	/// The caller may queue the raw pointer only when this returns true.
	pub fn release(&mut self, id: BufferId) -> bool {
		let Some((current, state)) = self.buffers.get_mut(&id.address) else {
			return false;
		};
		if *current != id || *state == State::Queued {
			return false;
		}
		*state = State::Queued;
		true
	}

	pub fn pool_size(&self) -> usize {
		self.buffers.len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn removal_and_address_reuse_cannot_release_a_new_buffer() {
		for published in [false, true] {
			let mut ledger = Ledger::default();
			let old = ledger.announce(7).unwrap();
			assert_eq!(ledger.dequeue(7), Ok(old));
			assert!(ledger.pending(old));
			if published {
				assert!(ledger.complete(old));
				assert!(ledger.publish(old));
			}
			assert_eq!(ledger.remove(7), Some(old));
			let new = ledger.announce(7).unwrap();
			assert_ne!(old, new);
			assert!(!ledger.release(old));
			assert_eq!(ledger.dequeue(7), Ok(new));
			assert!(!ledger.publish(old));
			assert!(ledger.release(new));
			assert!(!ledger.release(new));
		}
	}

	#[test]
	fn old_stream_release_cannot_complete_its_replacement() {
		let mut old = Ledger::default();
		let mut new = Ledger::default();
		let old_id = old.announce(1).unwrap();
		let new_id = new.announce(1).unwrap();
		new.dequeue(1).unwrap();
		assert_ne!(old_id, new_id);
		assert!(!new.release(old_id));
		assert!(new.release(new_id));
	}

	#[test]
	fn producer_cannot_resend_an_unread_buffer() {
		let mut ledger = Ledger::default();
		let id = ledger.announce(1).unwrap();
		assert!(ledger.announce(1).is_err());
		assert!(!ledger.publish(id));
		assert_eq!(ledger.dequeue(1), Ok(id));
		assert!(ledger.dequeue(1).is_err());
		assert!(ledger.pending(id));
		assert!(ledger.dequeue(1).is_err());
		assert!(!ledger.publish(id), "incomplete producer pixels cannot be published");
		assert!(ledger.complete(id));
		assert!(ledger.publish(id));
		assert!(ledger.dequeue(1).is_err());
		assert!(ledger.release(id));
		assert_eq!(ledger.dequeue(1), Ok(id));
	}

	#[test]
	fn pool_resize_and_all_buffers_held_then_released() {
		let mut ledger = Ledger::default();
		let ids: Vec<_> = (1..=4).map(|address| ledger.announce(address).unwrap()).collect();
		for &id in &ids {
			assert_eq!(ledger.dequeue(id.address), Ok(id));
			assert!(ledger.pending(id));
			assert!(!ledger.publish(id), "incomplete producer pixels cannot be published");
			assert!(ledger.complete(id));
			assert!(ledger.publish(id));
		}
		assert_eq!(ledger.pool_size(), 4);
		ledger.remove(2);
		assert!(!ledger.release(ids[1]));
		assert_eq!(ledger.pool_size(), 3);
		for id in [ids[0], ids[2], ids[3]] {
			assert!(ledger.release(id));
		}
		assert!(ledger.announce(5).is_ok());
		assert_eq!(ledger.pool_size(), 4);
	}
}
