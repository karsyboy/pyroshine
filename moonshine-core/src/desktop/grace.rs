//! Desktop-only attachment deadlines. Authorization generation checks belong
//! to the session manager; protocol traffic alone never resets these clocks.
use std::time::{Duration, Instant};
pub(crate) struct Grace {
	pub epoch: u64,
	pub lease: String,
	pub deadline: Option<Instant>,
	connected_once: bool,
	grace: Duration,
}
impl Grace {
	pub fn new(epoch: u64, lease: String, now: Instant, attach: Duration, grace: Duration) -> Self {
		Self {
			epoch,
			lease,
			deadline: Some(now + attach),
			connected_once: false,
			grace,
		}
	}
	pub fn observe(&mut self, now: Instant, attached: bool) {
		if attached {
			self.connected_once = true;
			self.deadline = None;
		} else if self.connected_once && self.deadline.is_none() {
			self.deadline = Some(now + self.grace);
		}
	}
	pub fn expired(&self, now: Instant) -> bool {
		self.deadline.is_some_and(|d| now >= d)
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn protocol_progress_does_not_extend_attachment_or_disconnect_deadlines() {
		let now = Instant::now();
		let mut grace = Grace::new(9, "lease".into(), now, Duration::from_secs(5), Duration::from_secs(30));
		grace.observe(now + Duration::from_secs(4), false);
		assert!(grace.expired(now + Duration::from_secs(5)));
		grace.observe(now + Duration::from_secs(4), true);
		assert!(!grace.expired(now + Duration::from_secs(100)));
		grace.observe(now + Duration::from_secs(10), false);
		grace.observe(now + Duration::from_secs(39), false);
		assert!(!grace.expired(now + Duration::from_secs(39)));
		assert!(grace.expired(now + Duration::from_secs(40)));
	}
	#[test]
	fn authenticated_reconnect_cancels_and_later_loss_gets_a_new_grace() {
		let now = Instant::now();
		let mut grace = Grace::new(2, "lease".into(), now, Duration::from_secs(5), Duration::from_secs(30));
		grace.observe(now, true);
		grace.observe(now, false);
		grace.observe(now + Duration::from_secs(29), true);
		assert!(!grace.expired(now + Duration::from_secs(50)));
		grace.observe(now + Duration::from_secs(50), false);
		assert!(grace.expired(now + Duration::from_secs(80)));
	}
	#[test]
	fn zero_grace_restores_on_disconnect() {
		let now = Instant::now();
		let mut grace = Grace::new(1, "lease".into(), now, Duration::from_secs(5), Duration::ZERO);
		grace.observe(now, true);
		grace.observe(now, false);
		assert!(grace.expired(now));
	}
}
