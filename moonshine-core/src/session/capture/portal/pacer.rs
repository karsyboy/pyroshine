//! Request scheduling is enabled only after binding the exact producer node.
//! Callbacks only record edges; the owning loop triggers outside their borrows.
use pipewire as pw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const HEARTBEAT: Duration = Duration::from_millis(250);
const LOST_CYCLE: Duration = Duration::from_millis(100);

pub(super) fn supports_request(value: Option<&str>) -> bool {
	value.and_then(|v| v.trim().parse::<u32>().ok()).is_some_and(|v| v > 0)
}

/// A missing/unknown node, denied bind or bounded discovery failure retains
/// producer-driven capture. Registry properties alone are not authoritative.
pub(super) fn probe(
	core: &pw::core::CoreRc,
	mainloop: &pw::main_loop::MainLoopRc,
	id: u32,
	stop: &async_shutdown::ShutdownManager<crate::session::manager::SessionShutdownReason>,
) -> bool {
	let Ok(registry) = core.get_registry_rc() else {
		return false;
	};
	let answer = Rc::new(Cell::new(None));
	let bound = Rc::new(RefCell::new(None));
	let _globals = registry
		.add_listener_local()
		.global({
			let registry = registry.clone();
			let answer = answer.clone();
			let bound = bound.clone();
			move |global| {
				if global.id != id || global.type_ != pw::types::ObjectType::Node {
					return;
				}
				let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
					answer.set(Some(false));
					return;
				};
				let listener = node
					.add_listener_local()
					.info({
						let answer = answer.clone();
						move |info| {
							answer.set(Some(supports_request(
								info.props().and_then(|p| p.get("node.supports-request")),
							)));
						}
					})
					.register();
				*bound.borrow_mut() = Some((listener, node));
			}
		})
		.register();
	let deadline = Instant::now() + Duration::from_secs(1);
	while answer.get().is_none() && !stop.is_shutdown_triggered() && Instant::now() < deadline {
		mainloop
			.loop_()
			.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(5)));
	}
	answer.get().unwrap_or(false)
}

#[derive(Debug)]
pub(super) struct Pacer {
	interval: Duration,
	live: bool,
	pending: bool,
	in_flight: Option<Instant>,
	last_paint: Option<Instant>,
	last_trigger: Option<Instant>,
	retry_at: Option<Instant>,
	pub requests: u64,
	pub triggers: u64,
}
impl Pacer {
	pub fn new(interval: Duration) -> Self {
		Self {
			interval,
			live: false,
			pending: false,
			in_flight: None,
			last_paint: None,
			last_trigger: None,
			retry_at: None,
			requests: 0,
			triggers: 0,
		}
	}
	pub fn live(&mut self, live: bool) {
		if self.live != live {
			self.live = live;
			self.in_flight = None;
			self.pending = live;
			self.last_paint = None;
			self.last_trigger = None;
			self.retry_at = None;
		}
	}
	pub fn request(&mut self) {
		self.requests += 1;
		self.pending = true;
	}
	pub fn done(&mut self) {
		self.in_flight = None;
	}
	pub fn paint(&mut self, now: Instant) {
		self.last_paint = Some(self.in_flight.unwrap_or(now));
	}
	pub fn due(&self, now: Instant) -> Duration {
		if !self.live {
			return HEARTBEAT;
		}
		if let Some(start) = self.in_flight {
			return (start + LOST_CYCLE)
				.saturating_duration_since(now)
				.max(Duration::from_millis(1));
		}
		if self.retry_at.is_some_and(|retry| now < retry) {
			return self.retry_at.unwrap().saturating_duration_since(now);
		}
		if self.pending {
			return self.last_paint.map_or(Duration::ZERO, |paint| {
				(paint + self.interval).saturating_duration_since(now)
			});
		}
		HEARTBEAT
	}
	pub fn trigger(&mut self, now: Instant, credit: bool) -> bool {
		if !self.live {
			return false;
		}
		if self
			.in_flight
			.is_some_and(|start| now.duration_since(start) >= LOST_CYCLE)
		{
			self.in_flight = None;
			self.pending = true;
		}
		if !self.pending
			&& self
				.last_trigger
				.is_none_or(|last| now.duration_since(last) >= HEARTBEAT)
		{
			self.pending = true;
		}
		if !credit || self.in_flight.is_some() || !self.pending || self.due(now) != Duration::ZERO {
			return false;
		}
		// Record before calling PipeWire: trigger_done may be synchronous.
		self.pending = false;
		self.in_flight = Some(now);
		self.last_trigger = Some(now);
		self.triggers += 1;
		true
	}
	pub fn failed(&mut self) {
		self.in_flight = None;
		self.pending = true;
		self.retry_at = self.last_trigger.map(|last| last + HEARTBEAT);
	}
}

/// pipewire-rs exposes these C events but currently has no builder setters.
/// The hook is removed before either its callback data or stream is destroyed.
pub(super) struct Listener {
	hook: Box<pw::spa::sys::spa_hook>,
	_events: Box<pw::sys::pw_stream_events>,
	_pacer: Rc<RefCell<Pacer>>,
}
impl Listener {
	pub fn attach(stream: &pw::stream::Stream, pacer: Rc<RefCell<Pacer>>) -> Self {
		unsafe extern "C" fn command(data: *mut std::ffi::c_void, command: *const pw::spa::sys::spa_command) {
			if command.is_null() {
				return;
			}
			// SAFETY: PipeWire supplies a live command for this callback; data
			// points to the Rc held until the hook has been removed.
			unsafe {
				let body = (*command).body.body;
				if body.type_ == pw::spa::sys::SPA_TYPE_COMMAND_Node
					&& body.id == pw::spa::sys::SPA_NODE_COMMAND_RequestProcess
				{
					(*(data as *const RefCell<Pacer>)).borrow_mut().request();
				}
			}
		}
		unsafe extern "C" fn done(data: *mut std::ffi::c_void) {
			// SAFETY: same owned callback data as command.
			unsafe {
				(*(data as *const RefCell<Pacer>)).borrow_mut().done();
			}
		}
		// SAFETY: C hooks/events use zero initialization, with absent events null.
		let mut hook: Box<pw::spa::sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
		let mut events: Box<pw::sys::pw_stream_events> = Box::new(unsafe { std::mem::zeroed() });
		events.version = pw::sys::PW_VERSION_STREAM_EVENTS;
		events.command = Some(command);
		events.trigger_done = Some(done);
		// SAFETY: stable boxes and Rc allocation outlive registration.
		unsafe {
			pw::sys::pw_stream_add_listener(
				stream.as_raw_ptr(),
				&mut *hook,
				&*events,
				Rc::as_ptr(&pacer).cast_mut().cast(),
			);
		}
		Self {
			hook,
			_events: events,
			_pacer: pacer,
		}
	}
}
impl Drop for Listener {
	fn drop(&mut self) {
		// SAFETY: registered once above, removed once before freeing its boxes.
		unsafe {
			pw::spa::sys::spa_hook_remove(&mut *self.hook);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn unknown_producers_never_enable_driving() {
		for value in [None, Some("true"), Some("0"), Some("-1"), Some("garbage")] {
			assert!(!supports_request(value));
		}
		assert!(supports_request(Some(" 1 ")));
	}
	#[test]
	fn reentrant_done_bursts_no_pixel_and_credit_obey_paint_cadence() {
		let now = Instant::now();
		let interval = Duration::from_millis(10);
		let mut p = Pacer::new(interval);
		assert!(!p.trigger(now, true));
		p.live(true);
		assert!(p.trigger(now, true));
		p.done(); // synchronous completion during trigger, without pixels
		p.request();
		p.request();
		assert!(!p.trigger(now, false));
		assert!(p.trigger(now, true));
		p.paint(now);
		p.done();
		p.request();
		assert!(!p.trigger(now, true));
		assert_eq!(p.due(now), interval);
		assert!(p.trigger(now + interval, true));
		assert!(!p.trigger(now + interval, true));
	}
	#[test]
	fn failed_trigger_backs_off_and_credit_does_not_lose_a_request() {
		let now = Instant::now();
		let mut p = Pacer::new(Duration::from_millis(10));
		p.live(true);
		assert!(p.trigger(now, true));
		p.failed();
		assert_eq!(p.due(now), HEARTBEAT);
		assert!(!p.trigger(now + Duration::from_millis(10), true));
		assert!(!p.trigger(now + HEARTBEAT, false));
		assert!(p.trigger(now + HEARTBEAT, true));
	}

	#[test]
	fn lost_cycle_heartbeat_and_stop_are_bounded_and_new_mode_starts_fresh() {
		let now = Instant::now();
		let mut p = Pacer::new(Duration::from_millis(8));
		p.live(true);
		assert!(p.trigger(now, true));
		assert!(!p.trigger(now + LOST_CYCLE / 2, true));
		assert!(p.trigger(now + LOST_CYCLE, true));
		p.live(false);
		p.done();
		p.request();
		assert!(!p.trigger(now + HEARTBEAT, true));
		let mut replacement = Pacer::new(Duration::from_millis(16));
		replacement.live(true);
		assert!(replacement.trigger(now + HEARTBEAT, true));
	}
}
