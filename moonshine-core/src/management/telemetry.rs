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

use moonshine_management::dto::{StageStats, StreamStats};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::watch;

use crate::session::stream::video::FrameStats;

/// How often buffered samples are read. The channel holds 256 frames, enough
/// for 1000 FPS at this period.
pub(crate) const DRAIN_PERIOD: Duration = Duration::from_millis(250);
/// Length of one published summary.
pub(crate) const WINDOW: Duration = Duration::from_secs(1);

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
		};
		let buffers = std::mem::take(&mut self.stage_micros);
		*self = Self {
			stage_micros: buffers,
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
		}
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
