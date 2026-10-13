//! The daemon's reading of a statistics window.
//!
//! [`analyze`] turns one [`StreamStats`] window into [`Finding`]s: which part
//! of the host limits the stream, and what the operator can change. It runs
//! once per published window on the management task, never on a media path,
//! and sees only what the host measures. Problems on the client (decoder
//! speed, display pacing) or on the network after the host's socket are
//! outside it.
//!
//! Rules are deliberately conservative: a finding names a cause only when the
//! window's numbers show it, and a window that looks healthy has none.

use moonshine_management::dto::{Finding, StageStats, StreamStats};

/// What the stream was negotiated to deliver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StreamTarget {
	pub fps: u32,
	pub bitrate_bps: u64,
}

/// A stage this much slower than the frame budget limits the frame rate.
const BOTTLENECK_SHARE: f64 = 0.8;
/// Delivered frames below this share of what was available count as a shortfall.
const SHORTFALL: f64 = 0.9;
/// Share of skipped captures worth reporting.
const SKIPPED_SHARE: f64 = 0.05;
/// Share of composited frames worth reporting.
const COMPOSITED_SHARE: f64 = 0.5;
/// Share of uneven frame times worth reporting under fixed pacing.
const UNEVEN_PERCENT: f64 = 25.0;

/// Findings for one window, warnings first.
pub(crate) fn analyze(stats: &StreamStats, target: &StreamTarget) -> Vec<Finding> {
	let mut findings = Vec::new();
	if stats.frames == 0 || target.fps == 0 {
		return findings;
	}
	let budget_us = 1e6 / f64::from(target.fps);
	let target_fps = f64::from(target.fps);
	let stage = |id: &str| stats.stages.iter().find(|stage| stage.id == id);
	let source_fps = stats.capture.as_ref().map_or(0.0, |capture| capture.source_fps);
	// What the pipeline could have delivered: the game's new frames, at most
	// the stream rate. Static content (no new frames) offers nothing to miss.
	let available_fps = if source_fps > 0.0 {
		source_fps.min(target_fps)
	} else {
		target_fps
	};
	let short = stats.fps < available_fps * SHORTFALL;

	if let Some(encode) = stage("encode").filter(|encode| short && encode.p50_us > budget_us * BOTTLENECK_SHARE) {
		findings.push(Finding {
			id: "encoder_bound".into(),
			severity: "warning".into(),
			title: format!("The encoder cannot keep up with {} fps", target.fps),
			detail: format!(
				"Encoding takes {} per frame (p95 {}), against a budget of {} at {} fps; {:.0} fps were delivered.",
				millis(encode.p50_us),
				millis(encode.p95_us),
				millis(budget_us),
				target.fps,
				stats.fps
			),
			hint: Some(
				"Lower the resolution, frame rate or bitrate in the client, or choose a codec this GPU encodes \
				 faster. A GPU busy rendering the game also slows encoding."
					.into(),
			),
		});
	}

	let skipped = stats.stale_frames_dropped as f64;
	if skipped > 0.0 && skipped >= stats.frames as f64 * SKIPPED_SHARE {
		findings.push(Finding {
			id: "frames_skipped".into(),
			severity: "warning".into(),
			title: "Captured frames were skipped".into(),
			detail: format!(
				"{} captured frames were replaced by newer ones before the encoder could take them ({} encoded).",
				stats.stale_frames_dropped, stats.frames
			),
			hint: Some(slowest_stage_hint(&stats.stages)),
		});
	}

	if stats.failed_packets > 0 {
		findings.push(Finding {
			id: "send_failures".into(),
			severity: "warning".into(),
			title: "Video packets failed to send".into(),
			detail: format!(
				"The host's network stack rejected {} of {} packets.",
				stats.failed_packets,
				stats.packets + stats.failed_packets
			),
			hint: Some(
				"Check the host's network interface and route to the client; the service log names the socket error."
					.into(),
			),
		});
	}

	if stats.discarded_packets > 0 {
		findings.push(Finding {
			id: "send_backlog".into(),
			severity: "warning".into(),
			title: "The network could not keep up with the bitrate".into(),
			detail: format!(
				"{} packets were discarded before sending at {} (requested {}).",
				stats.discarded_packets,
				megabits(stats.wire_bitrate_bps),
				megabits(target.bitrate_bps as f64)
			),
			hint: Some("Lower the bitrate in the client, or use a wired connection.".into()),
		});
	}

	let encoder_found = findings.iter().any(|finding| finding.id == "encoder_bound");
	if !encoder_found && let Some(total) = stage("total").filter(|total| total.p95_us > 2.0 * budget_us) {
		findings.push(Finding {
			id: "pipeline_latency".into(),
			severity: "info".into(),
			title: "Frames spend several frame times in the host pipeline".into(),
			detail: format!(
				"Capture to send takes {} (p95 {}), against a frame time of {}.",
				millis(total.p50_us),
				millis(total.p95_us),
				millis(budget_us)
			),
			hint: Some(slowest_stage_hint(&stats.stages)),
		});
	}

	if let Some(capture) = &stats.capture {
		// The game, not the host, sets the rate: new frames arrive slower than
		// the stream runs, and the pipeline delivers what arrives.
		if capture.source_fps > 1.0 && capture.source_fps < target_fps * SHORTFALL && !short {
			findings.push(Finding {
				id: "game_bound".into(),
				severity: "info".into(),
				title: format!("The game renders {:.0} fps", capture.source_fps),
				detail: format!(
					"The stream runs at {} fps; the game presented new frames at {:.0} fps and every one was delivered.",
					target.fps, capture.source_fps
				),
				hint: Some(
					"The game limits the frame rate: check its frame limiter, V-Sync and graphics settings.".into(),
				),
			});
		}

		if let Some(path) = &capture.path {
			let frames = path.direct_frames + path.direct_late_frames + path.composited_frames;
			if frames > 0 && path.composited_frames as f64 >= frames as f64 * COMPOSITED_SHARE {
				let reason = path.composition_reasons.first();
				findings.push(Finding {
					id: "composited".into(),
					severity: "info".into(),
					title: match reason {
						Some(reason) => format!("Frames are composited: {}", lowercase_first(&reason.label)),
						None => "Frames are composited".into(),
					},
					detail: format!(
						"{:.0} % of frames were composited rather than exported directly, which costs GPU time and \
						 latency.",
						path.composited_frames as f64 * 100.0 / frames as f64
					),
					hint: reason.and_then(|reason| reason.hint.clone()),
				});
			}
		}

		if capture.pacing == "fixed" && capture.source_fps > 1.0 && capture.uneven_percent > UNEVEN_PERCENT {
			findings.push(Finding {
				id: "uneven_pacing".into(),
				severity: "info".into(),
				title: "The game's frame times are uneven".into(),
				detail: format!(
					"{:.0} % of consecutive frame times differ by more than 2 ms (p95 interval {}).",
					capture.uneven_percent,
					millis(capture.interval_p95_us)
				),
				hint: Some(
					"Cap the game's frame rate at or below the stream's, or enable VRR in Pyrolight so the host \
					 captures frames as the game presents them."
						.into(),
				),
			});
		}
	}

	findings.sort_by_key(|finding| finding.severity != "warning");
	findings
}

/// Name the stage that takes longest, excluding the end-to-end total.
fn slowest_stage_hint(stages: &[StageStats]) -> String {
	match stages
		.iter()
		.filter(|stage| stage.id != "total")
		.max_by(|a, b| a.p95_us.total_cmp(&b.p95_us))
	{
		Some(stage) => format!(
			"The slowest stage is {} (p95 {}); see the pipeline table.",
			lowercase_first(&stage.label),
			millis(stage.p95_us)
		),
		None => "See the pipeline table for the slowest stage.".into(),
	}
}

fn lowercase_first(text: &str) -> String {
	let mut chars = text.chars();
	let Some(first) = chars.next() else {
		return String::new();
	};
	// Keep acronyms ("DMA-BUF import") intact.
	if chars.clone().next().is_some_and(char::is_lowercase) {
		first.to_lowercase().chain(chars).collect()
	} else {
		text.to_string()
	}
}

fn millis(micros: f64) -> String {
	format!("{:.1} ms", micros / 1000.0)
}

fn megabits(bps: f64) -> String {
	format!("{:.0} Mbit/s", bps / 1e6)
}

#[cfg(test)]
mod tests {
	use moonshine_management::dto::{CapturePathStats, CaptureStats, CompositionReason};

	use super::*;

	const TARGET: StreamTarget = StreamTarget {
		fps: 60,
		bitrate_bps: 50_000_000,
	};

	fn stage(id: &str, p50_ms: f64, p95_ms: f64) -> StageStats {
		StageStats {
			id: id.into(),
			label: match id {
				"encode" => "Encode",
				"import" => "DMA-BUF import",
				"total" => "Total pipeline",
				_ => "Other",
			}
			.into(),
			avg_us: p50_ms * 1000.0,
			p50_us: p50_ms * 1000.0,
			p95_us: p95_ms * 1000.0,
			max_us: p95_ms * 1000.0,
		}
	}

	fn capture(source_fps: f64) -> CaptureStats {
		CaptureStats {
			pacing: "fixed".into(),
			source_fps,
			interval_p50_us: 16_667.0,
			interval_p95_us: 17_000.0,
			interval_p99_us: 17_000.0,
			interval_max_us: 17_000.0,
			interval_stddev_us: 100.0,
			uneven_percent: 0.0,
			content_age_p50_us: 1_000.0,
			content_age_p95_us: 2_000.0,
			path: Some(CapturePathStats {
				current: "direct".into(),
				direct_frames: 60,
				direct_late_frames: 0,
				composited_frames: 0,
				composition_reasons: Vec::new(),
			}),
		}
	}

	/// A healthy 60 fps window.
	fn healthy() -> StreamStats {
		StreamStats {
			epoch: 1,
			at_ms: 0,
			window_ms: 1000,
			frames: 60,
			fps: 60.0,
			key_frames: 0,
			encoded_bitrate_bps: 40e6,
			wire_bitrate_bps: 44e6,
			transport_overhead_percent: Some(10.0),
			packets: 4000,
			failed_packets: 0,
			discarded_packets: 0,
			stale_frames_dropped: 0,
			samples_dropped: 0,
			stages: vec![
				stage("import", 0.2, 0.3),
				stage("encode", 4.0, 5.0),
				stage("total", 6.0, 8.0),
			],
			capture: Some(capture(60.0)),
			findings: Vec::new(),
		}
	}

	fn ids(stats: &StreamStats) -> Vec<String> {
		analyze(stats, &TARGET).into_iter().map(|finding| finding.id).collect()
	}

	#[test]
	fn a_healthy_window_has_no_findings() {
		assert!(ids(&healthy()).is_empty());
		// Static content (no new frames) is not a shortfall either.
		let mut idle = healthy();
		idle.capture = Some(capture(0.0));
		assert!(ids(&idle).is_empty());
	}

	#[test]
	fn a_slow_encoder_that_misses_frames_is_named() {
		let mut stats = healthy();
		stats.fps = 41.0;
		stats.stages = vec![stage("encode", 22.0, 26.0), stage("total", 25.0, 30.0)];
		assert_eq!(ids(&stats), ["encoder_bound"], "latency is explained by the encoder");
		// The same encode time without a shortfall (pipelined) is no finding.
		stats.fps = 60.0;
		assert!(!ids(&stats).contains(&"encoder_bound".to_string()));
	}

	#[test]
	fn a_game_below_the_stream_rate_is_game_bound_when_every_frame_is_delivered() {
		let mut stats = healthy();
		stats.fps = 38.0;
		stats.capture = Some(capture(38.0));
		assert_eq!(ids(&stats), ["game_bound"]);
		// When the host also misses the game's frames, it is not the game.
		stats.fps = 20.0;
		assert!(!ids(&stats).contains(&"game_bound".to_string()));
	}

	#[test]
	fn transport_failures_are_warnings_listed_first() {
		let mut stats = healthy();
		stats.discarded_packets = 12;
		stats.failed_packets = 3;
		stats.capture.as_mut().unwrap().uneven_percent = 40.0;
		let findings = analyze(&stats, &TARGET);
		let ids: Vec<_> = findings.iter().map(|finding| finding.id.as_str()).collect();
		assert_eq!(ids, ["send_failures", "send_backlog", "uneven_pacing"]);
		assert!(findings[1].detail.contains("50 Mbit/s"), "{}", findings[1].detail);
	}

	#[test]
	fn mostly_composited_windows_name_the_main_reason_and_its_hint() {
		let mut stats = healthy();
		stats.capture.as_mut().unwrap().path = Some(CapturePathStats {
			current: "mixed".into(),
			direct_frames: 10,
			direct_late_frames: 0,
			composited_frames: 50,
			composition_reasons: vec![CompositionReason {
				id: "scaling".into(),
				label: "The game renders at a different resolution than the stream".into(),
				hint: Some("Run the game fullscreen.".into()),
				frames: 50,
			}],
		});
		let findings = analyze(&stats, &TARGET);
		assert_eq!(findings.len(), 1);
		assert_eq!(
			findings[0].title,
			"Frames are composited: the game renders at a different resolution than the stream"
		);
		assert_eq!(findings[0].hint.as_deref(), Some("Run the game fullscreen."));
		assert!(findings[0].detail.starts_with("83 %"));
	}

	#[test]
	fn skipped_frames_point_at_the_slowest_stage() {
		let mut stats = healthy();
		stats.stale_frames_dropped = 10;
		let findings = analyze(&stats, &TARGET);
		assert_eq!(findings[0].id, "frames_skipped");
		assert_eq!(
			findings[0].hint.as_deref(),
			Some("The slowest stage is encode (p95 5.0 ms); see the pipeline table.")
		);
	}

	#[test]
	fn high_latency_without_a_slow_encoder_is_reported_once() {
		let mut stats = healthy();
		stats.stages = vec![
			stage("import", 30.0, 40.0),
			stage("encode", 4.0, 5.0),
			stage("total", 36.0, 45.0),
		];
		let findings = analyze(&stats, &TARGET);
		assert_eq!(findings.len(), 1);
		assert_eq!(findings[0].id, "pipeline_latency");
		assert!(findings[0].hint.as_deref().unwrap().contains("DMA-BUF import"));
	}
}
