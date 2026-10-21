//! Opt-in, constant-space capture timings. Percentiles are logarithmic bucket
//! upper bounds in microseconds; the maximum is exact. No per-frame logging.
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, Default, Serialize)]
pub struct TimingReport {
	pub samples: u64,
	pub p50_upper_us: u64,
	pub p95_upper_us: u64,
	pub p99_upper_us: u64,
	pub max_us: u64,
}

#[derive(Default)]
pub(super) struct Timing {
	buckets: [u64; 32],
	count: u64,
	max: u64,
}

impl Timing {
	pub fn record(&mut self, duration: Duration) {
		let us = duration.as_micros().min(u64::MAX as u128) as u64;
		let bucket = (u64::BITS - us.leading_zeros()).min(31) as usize;
		self.buckets[bucket] += 1;
		self.count += 1;
		self.max = self.max.max(us);
	}
	pub fn report(&self) -> TimingReport {
		let percentile = |percent: u64| {
			if self.count == 0 {
				return 0;
			}
			let rank = (self.count * percent).div_ceil(100);
			let mut count = 0;
			for (bucket, samples) in self.buckets.iter().enumerate() {
				count += samples;
				if count >= rank {
					return ((1u64 << bucket) - 1).max(self.max * u64::from(bucket == 31));
				}
			}
			self.max
		};
		TimingReport {
			samples: self.count,
			p50_upper_us: percentile(50),
			p95_upper_us: percentile(95),
			p99_upper_us: percentile(99),
			max_us: self.max,
		}
	}
}

#[derive(Default)]
pub(super) struct Timings {
	pub enabled: bool,
	pub callback: Timing,
	pub producer_ready: Timing,
	pub acquire_to_publish: Timing,
	pub hold: Timing,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureReport {
	pub request_driven: bool,
	pub paint_requests: u64,
	pub paint_triggers: u64,
	pub requested_mode: crate::desktop::Mode,
	/// Actual native output mode, acknowledged by the guard before capture.
	pub output_mode: Option<crate::desktop::Mode>,
	pub pipewire_size: (u32, u32),
	/// Numerator and denominator, without integer-Hz truncation.
	pub pipewire_framerate: Option<(u32, u32)>,
	pub pipewire_max_framerate: Option<(u32, u32)>,
	pub fourcc: u32,
	pub modifier: u64,
	pub planes: usize,
	pub sync_mode: String,
	pub actual_pool: usize,
	pub held: usize,
	pub pending: usize,
	pub no_credit: u64,
	pub sent: u64,
	pub skipped: u64,
	pub first_completed_us: Option<u64>,
	pub timings_enabled: bool,
	pub callback: TimingReport,
	pub producer_ready: TimingReport,
	pub acquire_to_publish: TimingReport,
	pub hold: TimingReport,
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn percentile_bounds_include_zero_and_overflow_bucket() {
		let mut timing = Timing::default();
		assert_eq!(timing.report().samples, 0);
		for us in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
			timing.record(Duration::from_micros(us));
		}
		let report = timing.report();
		assert_eq!(report.samples, 10);
		assert_eq!(report.p50_upper_us, 7);
		assert_eq!(report.p99_upper_us, 15);
		assert_eq!(report.max_us, 9);
		timing.record(Duration::from_secs(1_000_000));
		assert_eq!(timing.report().p99_upper_us, 1_000_000_000_000);
	}
}
