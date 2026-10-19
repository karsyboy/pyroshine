//! Conservative bitrate suggestion from a calibration run.
//!
//! The ladder offers increasing IP-level rates. A step is *clean* when the
//! host sent it as planned and the client received it with negligible loss,
//! without frames that FEC could not have recovered and without queueing
//! delay that indicates a filling bottleneck buffer. The usable rate is the
//! highest step reached through clean steps only: a clean step above a failed
//! one is reported as instability, not as headroom.
//!
//! The suggestion converts that wire rate into a Moonlight video bitrate by
//! removing the stream's per-datagram headers and FEC parity, then keeps a
//! safety margin. One short run never guarantees bandwidth, so the result is
//! labelled with a confidence and never applied automatically.

/// Kept free for encoder rate variation (conventional key frames and scene
/// changes exceed the average frame), audio/control traffic, other traffic
/// on the path, and the probe's short duration. It matches the reserve of
/// the existing HTTPS calibration.
pub const MARGIN_PERCENT: u32 = 20;
/// Packet loss above which a step is not clean (0.2 %).
pub const CLEAN_LOSS_PPM: u64 = 2_000;
/// Steps whose host submission failed for more than this share are host-limited.
pub const HOST_FAILURE_PPM: u64 = 1_000;
/// Frames that started this late (relative to a frame interval) count as late.
pub const LATE_FRAME_SHARE_PERCENT: u64 = 2;
/// Queueing delay (95th percentile above the run's minimum) that marks a step
/// as delayed: three quarters of a frame interval, but at least this many
/// microseconds. A frame (or GSO chunk) burst always waits for its own
/// serialization at the bottleneck; only a queue that does not drain before
/// the next frame indicates a saturated path.
pub const MIN_DELAY_LIMIT_US: u64 = 2_000;
/// Suggestions are rounded down to this step.
pub const ROUNDING_KBPS: u64 = 500;
/// Bytes of the NV video header inside each `packetSize`.
const NV_HEADER_BYTES: usize = 16;

/// What the client measured for one step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientStep {
	pub received: u64,
	pub duplicates: u64,
	pub reordered: u64,
	/// Runs of consecutive missing datagrams.
	pub gap_events: u64,
	/// Longest run of consecutive missing datagrams.
	pub max_gap: u64,
	pub frames_complete: u64,
	/// Frames missing more datagrams than the host's FEC share could repair.
	pub frames_unrecoverable: u64,
	/// Queueing delay percentiles: one-way delay above the run's minimum.
	pub queue_p50_us: u64,
	pub queue_p95_us: u64,
	pub queue_max_us: u64,
	/// Spread (p95 − p50) of frame delivery times.
	pub frame_jitter_us: u64,
}

impl ClientStep {
	pub const FIELDS: usize = 11;

	pub fn to_fields(&self) -> [u64; Self::FIELDS] {
		[
			self.received,
			self.duplicates,
			self.reordered,
			self.gap_events,
			self.max_gap,
			self.frames_complete,
			self.frames_unrecoverable,
			self.queue_p50_us,
			self.queue_p95_us,
			self.queue_max_us,
			self.frame_jitter_us,
		]
	}

	pub fn from_fields(fields: [u64; Self::FIELDS]) -> Self {
		let [
			received,
			duplicates,
			reordered,
			gap_events,
			max_gap,
			frames_complete,
			frames_unrecoverable,
			queue_p50_us,
			queue_p95_us,
			queue_max_us,
			frame_jitter_us,
		] = fields;
		Self {
			received,
			duplicates,
			reordered,
			gap_events,
			max_gap,
			frames_complete,
			frames_unrecoverable,
			queue_p50_us,
			queue_p95_us,
			queue_max_us,
			frame_jitter_us,
		}
	}
}

/// Everything known about one step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepEvidence {
	pub offered_kbps: u32,
	pub planned_packets: u64,
	pub planned_frames: u32,
	/// Frames the host started sending.
	pub sent_frames: u32,
	pub submitted: u64,
	pub failed: u64,
	pub late_frames: u32,
	/// The client's final statistics, when it submitted them.
	pub client: Option<ClientStep>,
	/// The last authenticated in-run report, when no final statistics exist.
	pub reported_received: Option<u64>,
}

impl StepEvidence {
	fn received(&self) -> Option<u64> {
		self.client.map(|c| c.received).or(self.reported_received)
	}

	/// Datagrams the host submitted that the client did not receive, in ppm of
	/// the submitted ones. Socket submission is not delivery.
	pub fn network_loss_ppm(&self) -> Option<u64> {
		let received = self.received()?;
		(self.submitted > 0).then(|| self.submitted.saturating_sub(received) * 1_000_000 / self.submitted)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
	Clean,
	/// Loss, or frames FEC could not have repaired.
	Lossy,
	/// Received, but with queueing delay that shows a saturated bottleneck.
	Delayed,
	/// The host could not submit the step as planned; says nothing about the network.
	HostLimited,
	/// Sent only in part (the run ended during it) or without client data.
	Incomplete,
	NotRun,
}

impl Verdict {
	pub fn as_str(self) -> &'static str {
		match self {
			Verdict::Clean => "clean",
			Verdict::Lossy => "lossy",
			Verdict::Delayed => "delayed",
			Verdict::HostLimited => "host_limited",
			Verdict::Incomplete => "incomplete",
			Verdict::NotRun => "not_run",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
	None,
	Low,
	Medium,
	High,
}

impl Confidence {
	pub fn as_str(self) -> &'static str {
		match self {
			Confidence::None => "none",
			Confidence::Low => "low",
			Confidence::Medium => "medium",
			Confidence::High => "high",
		}
	}
}

/// Stream parameters needed to convert wire rate into a video bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOverhead {
	pub packet_size: usize,
	/// UDP payload plus IP/UDP headers of one datagram.
	pub wire_packet_bytes: usize,
	/// Parity as a percentage of data shards.
	pub fec_percent: u32,
	pub frame_interval_us: u64,
}

impl StreamOverhead {
	/// Wire bytes per video payload byte, in parts per million.
	pub fn overhead_ppm(&self) -> u64 {
		let payload = self.packet_size.saturating_sub(NV_HEADER_BYTES).max(1) as u64;
		self.wire_packet_bytes as u64 * 1_000_000 / payload
	}

	/// Video bitrate whose datagrams and parity fill `wire_kbps`, before margin.
	pub fn video_kbps(&self, wire_kbps: u64) -> u64 {
		wire_kbps * 1_000_000 / self.overhead_ppm() * 100 / (100 + u64::from(self.fec_percent))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recommendation {
	pub verdicts: Vec<Verdict>,
	/// Highest offered rate reached through clean steps.
	pub usable_wire_kbps: u64,
	/// Suggested Moonlight video bitrate; 0 when there is no suggestion.
	pub recommended_kbps: u64,
	pub confidence: Confidence,
	/// A failing step above the usable rate bounds it from above. Without one
	/// the usable rate is only a lower bound.
	pub ceiling_reached: bool,
	/// No clean step above a failed one.
	pub stable: bool,
	/// A physical link speed lowered the suggestion.
	pub link_limited: bool,
	pub reason: String,
}

pub fn verdict(step: &StepEvidence, frame_interval_us: u64) -> Verdict {
	if step.sent_frames == 0 {
		return Verdict::NotRun;
	}
	let complete = step.sent_frames >= step.planned_frames;
	if step.failed * 1_000_000 > step.planned_packets * HOST_FAILURE_PPM
		|| u64::from(step.late_frames) * 100 > u64::from(step.sent_frames) * LATE_FRAME_SHARE_PERCENT
	{
		return Verdict::HostLimited;
	}
	let Some(loss) = step.network_loss_ppm() else {
		return Verdict::Incomplete;
	};
	let lossy = loss > CLEAN_LOSS_PPM
		|| step
			.client
			.is_some_and(|c| c.frames_unrecoverable * 200 > u64::from(step.sent_frames));
	if lossy {
		// Loss in a partial step is still evidence of a limit.
		return Verdict::Lossy;
	}
	if step
		.client
		.is_some_and(|c| c.queue_p95_us > (frame_interval_us * 3 / 4).max(MIN_DELAY_LIMIT_US))
	{
		return Verdict::Delayed;
	}
	if !complete || step.client.is_none() {
		return Verdict::Incomplete;
	}
	Verdict::Clean
}

/// `host_link_mbps`/`client_link_mbps`: known physical Ethernet speeds of the
/// route at either end, or 0 when unknown.
pub fn recommend(
	steps: &[StepEvidence],
	overhead: StreamOverhead,
	host_link_mbps: u64,
	client_link_mbps: u64,
) -> Recommendation {
	let verdicts: Vec<Verdict> = steps.iter().map(|s| verdict(s, overhead.frame_interval_us)).collect();
	let clean = verdicts.iter().take_while(|v| **v == Verdict::Clean).count();
	let first_failure = verdicts.get(clean).copied();
	let stable = !verdicts[clean..].contains(&Verdict::Clean);
	let ceiling_reached = matches!(first_failure, Some(Verdict::Lossy | Verdict::Delayed));
	let host_ceiling = first_failure == Some(Verdict::HostLimited);

	if clean == 0 {
		let reason = match first_failure {
			Some(Verdict::Lossy) => "the lowest test rate already lost packets; test with a lower maximum rate",
			Some(Verdict::Delayed) => {
				"the lowest test rate already queued in the network; test with a lower maximum rate"
			},
			Some(Verdict::HostLimited) => "the host could not send the lowest test rate on schedule",
			Some(Verdict::Incomplete) | Some(Verdict::NotRun) | None => {
				"the run ended before a step completed with client measurements"
			},
			Some(Verdict::Clean) => unreachable!(),
		};
		return Recommendation {
			verdicts,
			usable_wire_kbps: 0,
			recommended_kbps: 0,
			confidence: Confidence::None,
			ceiling_reached,
			stable,
			link_limited: false,
			reason: reason.into(),
		};
	}

	let usable_wire_kbps = u64::from(steps[clean - 1].offered_kbps);
	let margin = |kbps: u64| kbps * u64::from(100 - MARGIN_PERCENT) / 100;
	let mut recommended = margin(overhead.video_kbps(usable_wire_kbps));
	// A measurement can never exceed the slowest known physical link; cap at
	// its Ethernet-framed capacity in case of measurement error.
	let mut link_limited = false;
	for link in [host_link_mbps, client_link_mbps] {
		if link == 0 {
			continue;
		}
		let wire_kbps = link * 1_000 * overhead.wire_packet_bytes as u64
			/ (overhead.wire_packet_bytes + super::plan::ETHERNET_FRAMING_BYTES) as u64;
		let limit = margin(overhead.video_kbps(wire_kbps));
		if limit < recommended {
			recommended = limit;
			link_limited = true;
		}
	}
	recommended = recommended / ROUNDING_KBPS * ROUNDING_KBPS;

	let confidence = if !stable || clean == 1 || host_ceiling {
		Confidence::Low
	} else if !ceiling_reached {
		Confidence::Medium
	} else {
		Confidence::High
	};
	let reason = if !stable {
		"a higher rate passed after a lower one failed: the path is unstable; repeat the test".to_string()
	} else if host_ceiling {
		"the host could not send faster on schedule; the network limit is unknown".to_string()
	} else if ceiling_reached {
		format!(
			"the next step was {} ; suggestion keeps {MARGIN_PERCENT}% in reserve",
			first_failure.map(Verdict::as_str).unwrap_or_default()
		)
	} else {
		format!(
			"every tested rate was clean: the path carries at least this much; test a higher maximum to find \
			 its limit (suggestion keeps {MARGIN_PERCENT}% in reserve)"
		)
	};
	Recommendation {
		verdicts,
		usable_wire_kbps,
		recommended_kbps: recommended,
		confidence,
		ceiling_reached,
		stable,
		link_limited,
		reason,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const OVERHEAD: StreamOverhead = StreamOverhead {
		packet_size: 1_392,
		wire_packet_bytes: 1_436,
		fec_percent: 20,
		frame_interval_us: 16_667,
	};

	fn step(offered_kbps: u32, loss_ppm: u64, queue_p95_us: u64) -> StepEvidence {
		let planned = 10_000;
		let received = planned - planned * loss_ppm / 1_000_000;
		StepEvidence {
			offered_kbps,
			planned_packets: planned,
			planned_frames: 60,
			sent_frames: 60,
			submitted: planned,
			failed: 0,
			late_frames: 0,
			client: Some(ClientStep {
				received,
				frames_complete: if loss_ppm == 0 { 60 } else { 50 },
				queue_p95_us,
				..ClientStep::default()
			}),
			reported_received: None,
		}
	}

	#[test]
	fn conversion_removes_headers_fec_and_margin() {
		// 1436 wire bytes carry 1376 payload bytes.
		assert_eq!(OVERHEAD.overhead_ppm(), 1_043_604);
		// 120 Mbit/s of wire at 20 % FEC is ~95.8 Mbit/s of video.
		assert_eq!(OVERHEAD.video_kbps(120_000), 95_821);
		let result = recommend(
			&[step(60_000, 0, 500), step(120_000, 0, 600), step(180_000, 50_000, 900)],
			OVERHEAD,
			0,
			0,
		);
		assert_eq!(result.usable_wire_kbps, 120_000);
		// 95,821 × 0.8 = 76,656, rounded down to 76,500.
		assert_eq!(result.recommended_kbps, 76_500);
		assert_eq!(result.confidence, Confidence::High);
		assert!(result.ceiling_reached && result.stable && !result.link_limited);
		assert_eq!(result.verdicts, vec![Verdict::Clean, Verdict::Clean, Verdict::Lossy]);
	}

	#[test]
	fn all_clean_is_only_a_lower_bound() {
		let result = recommend(&[step(50_000, 0, 100), step(100_000, 1_000, 100)], OVERHEAD, 0, 0);
		assert!(!result.ceiling_reached);
		assert_eq!(result.confidence, Confidence::Medium);
		assert_eq!(result.usable_wire_kbps, 100_000);
	}

	#[test]
	fn queueing_delay_bounds_the_rate_before_loss() {
		let result = recommend(
			&[
				step(100_000, 0, 1_000),
				step(200_000, 0, 14_000),
				step(300_000, 0, 30_000),
			],
			OVERHEAD,
			0,
			0,
		);
		assert_eq!(result.verdicts[1], Verdict::Delayed);
		assert_eq!(result.usable_wire_kbps, 100_000);
		assert!(result.ceiling_reached);
	}

	#[test]
	fn a_clean_step_after_a_failure_is_instability_not_headroom() {
		let result = recommend(
			&[step(100_000, 0, 100), step(200_000, 30_000, 100), step(300_000, 0, 100)],
			OVERHEAD,
			0,
			0,
		);
		assert_eq!(result.usable_wire_kbps, 100_000);
		assert!(!result.stable);
		assert_eq!(result.confidence, Confidence::Low);
	}

	#[test]
	fn host_failures_are_not_network_loss() {
		let mut limited = step(200_000, 0, 100);
		limited.submitted = 9_000;
		limited.failed = 1_000;
		limited.client.as_mut().unwrap().received = 9_000;
		let result = recommend(&[step(100_000, 0, 100), limited], OVERHEAD, 0, 0);
		assert_eq!(result.verdicts[1], Verdict::HostLimited);
		assert!(!result.ceiling_reached);
		assert_eq!(result.confidence, Confidence::Low);
		assert!(result.reason.contains("host"));
		// The network loss of the delivered part is zero.
		assert_eq!(limited.network_loss_ppm(), Some(0));
	}

	#[test]
	fn unrecoverable_frames_fail_a_step_with_low_average_loss() {
		let mut bursty = step(100_000, 1_000, 100);
		bursty.client.as_mut().unwrap().frames_unrecoverable = 2;
		assert_eq!(verdict(&bursty, OVERHEAD.frame_interval_us), Verdict::Lossy);
	}

	#[test]
	fn incomplete_and_missing_data_never_count_as_clean() {
		let mut partial = step(100_000, 0, 100);
		partial.sent_frames = 30;
		assert_eq!(verdict(&partial, 16_667), Verdict::Incomplete);
		let mut silent = step(100_000, 0, 100);
		silent.client = None;
		assert_eq!(verdict(&silent, 16_667), Verdict::Incomplete);
		// A report alone shows loss, but cannot make a step clean.
		silent.reported_received = Some(5_000);
		assert_eq!(verdict(&silent, 16_667), Verdict::Lossy);
		let result = recommend(&[partial], OVERHEAD, 0, 0);
		assert_eq!(result.recommended_kbps, 0);
		assert_eq!(result.confidence, Confidence::None);
		let mut not_run = step(100_000, 0, 100);
		not_run.sent_frames = 0;
		assert_eq!(verdict(&not_run, 16_667), Verdict::NotRun);
	}

	#[test]
	fn lossy_first_step_gives_no_suggestion() {
		let result = recommend(&[step(100_000, 10_000, 100)], OVERHEAD, 0, 0);
		assert_eq!(result.recommended_kbps, 0);
		assert!(result.reason.contains("lower maximum"));
	}

	#[test]
	fn known_link_speed_caps_the_suggestion() {
		// A measurement error on a 100 Mbit/s link cannot suggest more than it carries.
		let result = recommend(&[step(500_000, 0, 100)], OVERHEAD, 100, 0);
		assert!(result.link_limited);
		let link_video = OVERHEAD.video_kbps(100_000 * 1_436 / (1_436 + 38)) * 80 / 100;
		assert_eq!(result.recommended_kbps, link_video / 500 * 500);
	}
}
