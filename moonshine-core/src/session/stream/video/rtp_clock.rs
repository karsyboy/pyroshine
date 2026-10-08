//! The 90 kHz RTP timeline of one client epoch.
//!
//! A frame's RTP timestamp is its content time ([`ExportedFrame::source_time`])
//! relative to the epoch origin, as Sunshine and GameStream do. It is not
//! derived from a frame counter and the negotiated rate: that synthetic grid
//! claims one period per *sent* frame, so static screens or a game below the
//! stream rate made RTP time run faster than real time, and a client that
//! learns source cadence from RTP (VRR presentation) could not recover it.
//!
//! Moonlight clients use the value as frame PTS only; loss detection uses
//! sequence and frame numbers. Timestamps are strictly increasing within an
//! epoch, wrap at 32 bits (about 13 hours) like any RTP clock, and restart
//! near zero when a reconnecting client starts a new epoch.
//!
//! [`ExportedFrame::source_time`]: crate::session::compositor::frame::ExportedFrame

use std::time::Instant;

const RTP_CLOCK_HZ: u128 = 90_000;

#[derive(Debug, Default)]
pub(crate) struct RtpClock {
	origin: Option<Instant>,
	last: Option<u64>,
}

impl RtpClock {
	/// A clock whose epoch starts now.
	pub(crate) fn new() -> Self {
		Self {
			origin: Some(Instant::now()),
			last: None,
		}
	}

	/// Start a new epoch (client reconnect/resume) at `origin`.
	pub(crate) fn reset(&mut self, origin: Instant) {
		self.origin = Some(origin);
		self.last = None;
	}

	/// RTP timestamp of a frame whose content became current at `source_time`.
	///
	/// A frame whose content time does not advance (a replayed or repeated
	/// frame, or content older than the epoch) still gets the next tick, so
	/// the timeline stays strictly increasing. Zero is never produced: Moonlight
	/// treats a zero timestamp after the first frame as "no PTS" and
	/// substitutes its own receive time.
	pub(crate) fn timestamp(&mut self, source_time: Instant) -> u32 {
		let origin = *self.origin.get_or_insert(source_time);
		let nanos = source_time.saturating_duration_since(origin).as_nanos();
		// Round to the nearest tick; u64 ticks cover thousands of years.
		let mut ticks = ((nanos * RTP_CLOCK_HZ + 500_000_000) / 1_000_000_000) as u64;
		if let Some(last) = self.last
			&& ticks <= last
		{
			ticks = last + 1;
		}
		if ticks as u32 == 0 {
			ticks += 1;
		}
		self.last = Some(ticks);
		ticks as u32
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn timestamps_follow_content_time_not_frame_count() {
		let origin = Instant::now();
		let mut clock = RtpClock::default();
		clock.reset(origin);
		// A 60 FPS source on a 120 FPS stream: one frame every 16.67 ms.
		let stamps: Vec<u32> = (1..=4)
			.map(|frame| clock.timestamp(origin + Duration::from_nanos(16_666_667 * frame)))
			.collect();
		assert_eq!(stamps, [1500, 3000, 4500, 6000]);
	}

	#[test]
	fn uneven_source_cadence_is_preserved() {
		let origin = Instant::now();
		let mut clock = RtpClock::default();
		clock.reset(origin);
		let a = clock.timestamp(origin + Duration::from_micros(10_000));
		let b = clock.timestamp(origin + Duration::from_micros(22_000));
		let c = clock.timestamp(origin + Duration::from_micros(43_333));
		// 12 ms and 21.333 ms at 90 kHz.
		assert_eq!(b - a, 1080);
		assert_eq!(c - b, 1920);
	}

	#[test]
	fn repeated_or_stale_content_stays_strictly_increasing() {
		let origin = Instant::now();
		let mut clock = RtpClock::default();
		clock.reset(origin);
		let content = origin + Duration::from_millis(5);
		let first = clock.timestamp(content);
		assert_eq!(clock.timestamp(content), first + 1);
		assert_eq!(clock.timestamp(origin), first + 2);
		// Content captured before the epoch began.
		let mut fresh = RtpClock::default();
		fresh.reset(origin + Duration::from_secs(1));
		assert_eq!(fresh.timestamp(origin), 1);
	}

	#[test]
	fn reset_restarts_the_timeline_for_a_new_epoch() {
		let origin = Instant::now();
		let mut clock = RtpClock::default();
		clock.reset(origin);
		assert_eq!(clock.timestamp(origin + Duration::from_secs(100)), 9_000_000);
		let resumed = origin + Duration::from_secs(200);
		clock.reset(resumed);
		assert_eq!(clock.timestamp(resumed + Duration::from_millis(10)), 900);
	}

	#[test]
	fn timeline_wraps_at_32_bits_without_producing_zero() {
		let origin = Instant::now();
		let mut clock = RtpClock::default();
		clock.reset(origin);
		// 2^32 ticks of 90 kHz is 47721.858... seconds.
		let wrap = Duration::from_nanos((1u128 << 32).checked_mul(1_000_000_000).unwrap().div_ceil(90_000) as u64);
		let before = clock.timestamp(origin + wrap - Duration::from_millis(10));
		assert_eq!(before, u32::MAX - 899);
		let at_wrap = clock.timestamp(origin + wrap);
		assert_eq!(at_wrap, 1, "zero is skipped");
		let after = clock.timestamp(origin + wrap + Duration::from_millis(10));
		assert_eq!(after, 900);
		// Wrapping differences, as a receiver computes them, stay positive.
		assert_eq!(at_wrap.wrapping_sub(before), 901);
	}

	#[test]
	fn lazily_anchored_clock_starts_at_the_first_frame() {
		let mut clock = RtpClock::default();
		let first = Instant::now();
		assert_eq!(clock.timestamp(first), 1);
		assert_eq!(clock.timestamp(first + Duration::from_millis(1)), 90);
	}
}
