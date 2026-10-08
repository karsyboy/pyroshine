//! Aggregated stream statistics for the management interface.
//!
//! Both encoder backends already publish one [`FrameStats`] per completed
//! frame on a `broadcast` channel (the benchmark's source). The aggregator
//! subscribes to it only while the desktop UI runs and a client is streaming,
//! so a headless server, or one without a streaming client, has no receiver
//! and `send` stays the no-op it always was.
//!
//! While subscribed, the aggregator never waits on the pipeline and the
//! pipeline never waits on it: a timer drains the channel four times a second
//! (no per-frame wakeups), and a receiver that falls behind loses the oldest
//! samples, which are counted. Once per second the window is summarized into a
//! [`StreamStats`] for the UI.

use std::time::{Duration, Instant, SystemTime};

use moonshine_management::dto::{CaptureStats, StageStats, StreamStats};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::watch;

use crate::session::stream::video::FrameStats;

/// How often buffered samples are read. The channel holds 256 frames, enough
/// for 1000 FPS at this period.
pub(crate) const DRAIN_PERIOD: Duration = Duration::from_millis(250);
/// Length of one published summary.
pub(crate) const WINDOW: Duration = Duration::from_secs(1);
/// A frame time differing from the previous one by more than this is uneven.
const UNEVEN_STEP_US: f64 = 2_000.0;

struct Stage {
	id: &'static str,
	label: &'static str,
	read: fn(&FrameStats) -> Duration,
}

/// Pipeline stages in processing order. A stage that is zero for every frame
/// of a window is not performed by the active encoder and is not reported.
const STAGES: &[Stage] = &[
	Stage {
		id: "capture_queue",
		label: "Capture queue",
		read: |stats| stats.channel_wait,
	},
	Stage {
		id: "import",
		label: "DMA-BUF import",
		read: |stats| stats.import,
	},
	Stage {
		id: "convert",
		label: "Color conversion",
		read: |stats| stats.convert,
	},
	Stage {
		id: "submit",
		label: "Encoder submit",
		read: |stats| stats.submit,
	},
	Stage {
		id: "encode",
		label: "Encode",
		read: |stats| stats.encode_wait,
	},
	Stage {
		id: "output_queue",
		label: "Encoder output queue",
		read: |stats| stats.consumer_queue,
	},
	Stage {
		id: "packetize",
		label: "Packetization",
		read: |stats| stats.packetize,
	},
	Stage {
		id: "send_queue",
		label: "Send queue",
		read: |stats| stats.enqueue,
	},
	Stage {
		id: "send",
		label: "Socket send",
		read: |stats| stats.send,
	},
	Stage {
		id: "total",
		label: "Total pipeline",
		read: |stats| stats.total,
	},
];

/// Accumulates one window of frame samples. Buffers are reused across
/// windows, so steady-state aggregation does not allocate.
pub(crate) struct StatsWindow {
	started: Instant,
	frames: u64,
	key_frames: u64,
	encoded_bytes: u64,
	wire_bytes: u64,
	packets: u64,
	failed_packets: u64,
	discarded_packets: u64,
	stale_frames_dropped: u64,
	samples_dropped: u64,
	stage_micros: Vec<Vec<f64>>,
	vrr_frames: u64,
	captured_frames: u64,
	intervals: Vec<f64>,
	content_ages: Vec<f64>,
	uneven_steps: u64,
	interval_steps: u64,
	/// Frame time of the previous new frame; carried across windows.
	last_interval: Option<f64>,
}

impl StatsWindow {
	pub(crate) fn new(now: Instant) -> Self {
		Self {
			started: now,
			frames: 0,
			key_frames: 0,
			encoded_bytes: 0,
			wire_bytes: 0,
			packets: 0,
			failed_packets: 0,
			discarded_packets: 0,
			stale_frames_dropped: 0,
			samples_dropped: 0,
			stage_micros: STAGES.iter().map(|_| Vec::with_capacity(256)).collect(),
			vrr_frames: 0,
			captured_frames: 0,
			intervals: Vec::with_capacity(256),
			content_ages: Vec::with_capacity(256),
			uneven_steps: 0,
			interval_steps: 0,
			last_interval: None,
		}
	}

	pub(crate) fn record(&mut self, stats: &FrameStats) {
		self.frames += 1;
		self.key_frames += u64::from(stats.is_key_frame);
		self.encoded_bytes += stats.encoded_bytes as u64;
		self.wire_bytes += stats.wire_bytes as u64;
		self.packets += stats.packet_count as u64;
		self.failed_packets += stats.failed_packet_count as u64;
		self.discarded_packets += stats.discarded_packet_count as u64;
		self.stale_frames_dropped += u64::from(stats.stale_frames_dropped);
		for (stage, samples) in STAGES.iter().zip(&mut self.stage_micros) {
			samples.push((stage.read)(stats).as_secs_f64() * 1e6);
		}
		self.captured_frames += 1;
		self.vrr_frames += u64::from(stats.vrr_capture);
		self.content_ages.push(stats.content_age.as_secs_f64() * 1e6);
		// Replays and the first frame of an epoch have no frame time.
		if !stats.source_interval.is_zero() {
			let interval = stats.source_interval.as_secs_f64() * 1e6;
			if let Some(previous) = self.last_interval {
				self.interval_steps += 1;
				self.uneven_steps += u64::from((interval - previous).abs() > UNEVEN_STEP_US);
			}
			self.last_interval = Some(interval);
			self.intervals.push(interval);
		}
	}

	/// Summarize the window's frame pacing and clear its samples.
	fn finish_capture(&mut self) -> Option<CaptureStats> {
		if self.captured_frames == 0 {
			return None;
		}
		let pacing = match self.vrr_frames {
			0 => "fixed",
			vrr if vrr == self.captured_frames => "vrr",
			_ => "mixed",
		};
		let percentile = |samples: &[f64], p: f64| {
			if samples.is_empty() {
				0.0
			} else {
				samples[((samples.len() - 1) as f64 * p).round() as usize]
			}
		};
		let total: f64 = self.intervals.iter().sum();
		let mean = if self.intervals.is_empty() {
			0.0
		} else {
			total / self.intervals.len() as f64
		};
		let variance = if self.intervals.is_empty() {
			0.0
		} else {
			self.intervals
				.iter()
				.map(|interval| (interval - mean).powi(2))
				.sum::<f64>()
				/ self.intervals.len() as f64
		};
		self.intervals.sort_unstable_by(f64::total_cmp);
		self.content_ages.sort_unstable_by(f64::total_cmp);
		let stats = CaptureStats {
			pacing: pacing.into(),
			source_fps: if total > 0.0 {
				self.intervals.len() as f64 * 1e6 / total
			} else {
				0.0
			},
			interval_p50_us: percentile(&self.intervals, 0.5),
			interval_p95_us: percentile(&self.intervals, 0.95),
			interval_p99_us: percentile(&self.intervals, 0.99),
			interval_max_us: self.intervals.last().copied().unwrap_or(0.0),
			interval_stddev_us: variance.sqrt(),
			uneven_percent: if self.interval_steps == 0 {
				0.0
			} else {
				self.uneven_steps as f64 * 100.0 / self.interval_steps as f64
			},
			content_age_p50_us: percentile(&self.content_ages, 0.5),
			content_age_p95_us: percentile(&self.content_ages, 0.95),
		};
		self.intervals.clear();
		self.content_ages.clear();
		Some(stats)
	}

	/// Samples overwritten before they were read. They are still frames the
	/// pipeline produced, so they count towards the frame rate.
	pub(crate) fn lagged(&mut self, samples: u64) {
		self.samples_dropped += samples;
		self.frames += samples;
	}

	pub(crate) fn elapsed(&self, now: Instant) -> Duration {
		now.saturating_duration_since(self.started)
	}

	/// Summarize the window and start the next one.
	pub(crate) fn finish(&mut self, now: Instant, epoch: u64) -> StreamStats {
		let window = self.elapsed(now).max(Duration::from_millis(1));
		let seconds = window.as_secs_f64();
		let sampled = self.frames - self.samples_dropped;
		// Byte counts come from read samples only; scale them to the window's
		// frame count so dropped samples do not understate the bitrate.
		let scale = if sampled == 0 {
			0.0
		} else {
			self.frames as f64 / sampled as f64
		};
		let stages = STAGES
			.iter()
			.zip(&mut self.stage_micros)
			.filter_map(|(stage, samples)| {
				if samples.iter().all(|sample| *sample == 0.0) {
					samples.clear();
					return None;
				}
				samples.sort_unstable_by(f64::total_cmp);
				let percentile = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
				let stats = StageStats {
					id: stage.id.into(),
					label: stage.label.into(),
					avg_us: samples.iter().sum::<f64>() / samples.len() as f64,
					p50_us: percentile(0.5),
					p95_us: percentile(0.95),
					max_us: samples[samples.len() - 1],
				};
				samples.clear();
				Some(stats)
			})
			.collect();
		let capture = self.finish_capture();
		let stats = StreamStats {
			epoch,
			at_ms: unix_millis(SystemTime::now()),
			window_ms: window.as_millis() as u64,
			frames: self.frames,
			fps: self.frames as f64 / seconds,
			key_frames: self.key_frames,
			encoded_bitrate_bps: self.encoded_bytes as f64 * 8.0 * scale / seconds,
			wire_bitrate_bps: self.wire_bytes as f64 * 8.0 * scale / seconds,
			transport_overhead_percent: (self.encoded_bytes > 0)
				.then(|| (self.wire_bytes as f64 / self.encoded_bytes as f64 - 1.0) * 100.0),
			packets: self.packets,
			failed_packets: self.failed_packets,
			discarded_packets: self.discarded_packets,
			stale_frames_dropped: self.stale_frames_dropped,
			samples_dropped: self.samples_dropped,
			stages,
			capture,
		};
		let buffers = std::mem::take(&mut self.stage_micros);
		let intervals = std::mem::take(&mut self.intervals);
		let content_ages = std::mem::take(&mut self.content_ages);
		let last_interval = self.last_interval;
		*self = Self {
			stage_micros: buffers,
			intervals,
			content_ages,
			last_interval,
			..Self::new(now)
		};
		stats
	}
}

pub(crate) fn unix_millis(time: SystemTime) -> u64 {
	time.duration_since(SystemTime::UNIX_EPOCH)
		.map(|elapsed| elapsed.as_millis() as u64)
		.unwrap_or(0)
}

/// Aggregate `receiver` until `active` turns false, publishing summaries to
/// `latest` and `publish`. Returns when aggregation is no longer wanted or
/// the channel closed.
pub(crate) async fn aggregate(
	mut receiver: broadcast::Receiver<FrameStats>,
	epoch: u64,
	mut active: watch::Receiver<bool>,
	latest: &watch::Sender<Option<StreamStats>>,
	mut publish: impl FnMut(&StreamStats),
) {
	let mut window = StatsWindow::new(Instant::now());
	let mut drain = tokio::time::interval(DRAIN_PERIOD);
	drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		tokio::select! {
			_ = drain.tick() => {},
			changed = active.wait_for(|active| !*active) => {
				let _ = changed;
				break;
			},
		}
		loop {
			match receiver.try_recv() {
				Ok(stats) => window.record(&stats),
				Err(TryRecvError::Lagged(samples)) => window.lagged(samples),
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Closed) => {
					latest.send_replace(None);
					return;
				},
			}
		}
		let now = Instant::now();
		if window.elapsed(now) >= WINDOW {
			let stats = window.finish(now, epoch);
			publish(&stats);
			latest.send_replace(Some(stats));
		}
	}
	latest.send_replace(None);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn frame(encoded: usize, wire: usize, encode_us: u64, key: bool) -> FrameStats {
		FrameStats {
			channel_wait: Duration::from_micros(10),
			import: Duration::from_micros(100),
			convert: Duration::ZERO,
			submit: Duration::from_micros(20),
			consumer_queue: Duration::ZERO,
			encode_wait: Duration::from_micros(encode_us),
			packetize: Duration::from_micros(50),
			enqueue: Duration::from_micros(5),
			send: Duration::from_micros(200),
			total: Duration::from_micros(encode_us + 400),
			encoded_bytes: encoded,
			wire_bytes: wire,
			packet_count: 10,
			attempted_packet_count: 10,
			failed_packet_count: 0,
			discarded_packet_count: 0,
			stale_frames_dropped: 1,
			is_key_frame: key,
			source_interval: Duration::from_micros(8_333),
			content_age: Duration::from_micros(40),
			vrr_capture: false,
		}
	}

	#[test]
	fn summarizes_frame_pacing() {
		let start = Instant::now();
		let mut window = StatsWindow::new(start);
		// A VRR capture of a game alternating 7 and 13 ms frames.
		for index in 0..100u64 {
			let mut stats = frame(10_000, 12_000, 1_000, false);
			stats.vrr_capture = true;
			stats.source_interval = Duration::from_millis(if index % 2 == 0 { 7 } else { 13 });
			stats.content_age = Duration::from_micros(10 + index);
			window.record(&stats);
		}
		// A replay repeats old content: no frame time.
		let mut replay = frame(10_000, 12_000, 1_000, false);
		replay.vrr_capture = true;
		replay.source_interval = Duration::ZERO;
		replay.content_age = Duration::ZERO;
		window.record(&replay);
		let capture = window.finish(start + Duration::from_secs(1), 1).capture.unwrap();
		assert_eq!(capture.pacing, "vrr");
		assert!((capture.source_fps - 100.0).abs() < 1e-6, "{}", capture.source_fps);
		assert!((capture.interval_stddev_us - 3_000.0).abs() < 1e-6);
		assert_eq!(capture.interval_max_us, 13_000.0);
		assert!((capture.uneven_percent - 100.0).abs() < 1e-9, "every step is 6 ms");
		assert!(capture.content_age_p95_us < 120.0);

		// Steady frames in the next window, half captured on the fixed clock.
		for index in 0..10u64 {
			let mut stats = frame(10_000, 12_000, 1_000, false);
			stats.vrr_capture = index < 5;
			stats.source_interval = Duration::from_millis(13);
			window.record(&stats);
		}
		let capture = window.finish(start + Duration::from_secs(2), 1).capture.unwrap();
		assert_eq!(capture.pacing, "mixed");
		// Only the step from the previous window's last 13 ms frame is compared.
		assert_eq!(capture.uneven_percent, 0.0);
		assert_eq!(capture.interval_p50_us, 13_000.0);

		assert!(window.finish(start + Duration::from_secs(3), 1).capture.is_none());
	}

	#[test]
	fn summarizes_rates_percentiles_and_skips_unused_stages() {
		let start = Instant::now();
		let mut window = StatsWindow::new(start);
		for index in 0..100u64 {
			window.record(&frame(10_000, 12_000, 1_000 + index * 10, index == 0));
		}
		let stats = window.finish(start + Duration::from_secs(1), 7);
		assert_eq!(stats.epoch, 7);
		assert_eq!(stats.frames, 100);
		assert!((stats.fps - 100.0).abs() < 1e-9);
		assert_eq!(stats.key_frames, 1);
		assert!((stats.encoded_bitrate_bps - 8_000_000.0).abs() < 1e-3);
		assert!((stats.wire_bitrate_bps - 9_600_000.0).abs() < 1e-3);
		assert!((stats.transport_overhead_percent.unwrap() - 20.0).abs() < 1e-9);
		assert_eq!(stats.packets, 1000);
		assert_eq!(stats.stale_frames_dropped, 100);

		let ids: Vec<_> = stats.stages.iter().map(|stage| stage.id.as_str()).collect();
		assert!(!ids.contains(&"convert"), "zero stages are not performed: {ids:?}");
		assert!(!ids.contains(&"output_queue"));
		let encode = stats.stages.iter().find(|stage| stage.id == "encode").unwrap();
		assert!((encode.avg_us - 1_495.0).abs() < 1e-6);
		assert!((encode.p50_us - 1_500.0).abs() < 11.0);
		assert!((encode.p95_us - 1_940.0).abs() < 11.0);
		assert!((encode.max_us - 1_990.0).abs() < 1e-6);

		// The next window starts empty and reuses its buffers.
		let next = window.finish(start + Duration::from_secs(2), 7);
		assert_eq!(next.frames, 0);
		assert!(next.stages.is_empty());
		assert!(next.transport_overhead_percent.is_none());
	}

	#[test]
	fn dropped_samples_count_as_frames_and_scale_bitrates() {
		let start = Instant::now();
		let mut window = StatsWindow::new(start);
		for _ in 0..50 {
			window.record(&frame(10_000, 10_000, 1_000, false));
		}
		window.lagged(50);
		let stats = window.finish(start + Duration::from_secs(1), 1);
		assert_eq!(stats.frames, 100);
		assert_eq!(stats.samples_dropped, 50);
		assert!((stats.encoded_bitrate_bps - 8_000_000.0).abs() < 1e-3);
	}

	/// A consumer that never reads cannot slow the producer: `send` never
	/// blocks, the oldest samples are overwritten, and the aggregator counts
	/// them when it catches up.
	#[tokio::test]
	async fn a_stalled_consumer_never_blocks_the_pipeline() {
		let (sender, receiver) = broadcast::channel(256);
		let started = std::time::Instant::now();
		for _ in 0..10_000 {
			let _ = sender.send(frame(1_000, 1_100, 500, false));
		}
		assert!(
			started.elapsed() < Duration::from_secs(1),
			"sending never waits for the consumer"
		);

		let (active_tx, active) = watch::channel(true);
		let (latest, mut latest_rx) = watch::channel(None);
		let task = tokio::spawn(async move {
			aggregate(receiver, 3, active, &latest, |_| {}).await;
		});
		tokio::time::timeout(Duration::from_secs(5), latest_rx.wait_for(Option::is_some))
			.await
			.unwrap()
			.unwrap();
		let stats = latest_rx.borrow().clone().unwrap();
		assert_eq!(stats.frames, 10_000);
		assert_eq!(stats.samples_dropped, 10_000 - 256);
		active_tx.send_replace(false);
		tokio::time::timeout(Duration::from_secs(5), task)
			.await
			.unwrap()
			.unwrap();
		assert!(latest_rx.borrow().is_none(), "stopping clears the last summary");
	}
}
