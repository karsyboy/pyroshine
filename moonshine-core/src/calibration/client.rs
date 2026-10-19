//! Reference receiver for the UDP calibration probe.
//!
//! Pyrolight implements the same receiver in C++; this one drives the
//! integration tests and the `moonshine-udp-probe` tool. It is transport
//! agnostic for the HTTPS half: callers obtain a [`StartReply`] from
//! `/pyroshine-udp-probe` and submit [`result_query`] to
//! `/pyroshine-udp-probe-result`.
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::watch;

use super::StartReply;
use super::plan::{ProbePlan, Shape, StepPlan};
use super::recommend::ClientStep;
use super::wire::{
	Control, ControlKind, FLAG_END_OF_RUN, FLAG_END_OF_STEP, KEY_BYTES, MAX_STEPS, ProbeHeader, RUN_ID_BYTES, run_tag,
};

const HELLO_INTERVAL: Duration = Duration::from_millis(50);
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// The host ended the run (or the path broke) when nothing arrives this long.
const SILENCE_TIMEOUT: Duration = Duration::from_millis(1_000);
/// Late datagrams accepted after the end-of-run marker.
const END_LINGER: Duration = Duration::from_millis(50);
/// Queueing-delay histogram: 50 µs buckets from −200 ms to +400 ms around the
/// first datagram's delay.
const BUCKET_US: i64 = 50;
const BUCKET_OFFSET: i64 = 4_000;
const BUCKETS: usize = 12_000;

/// Receive buffer Moonlight requests for its video socket (moonlight-common-c
/// `LiVideoReceiveBufferSize`): 2048 datagrams, 8192 for PyroWave. A receiver
/// measuring the path should use the buffer the stream would get, or it
/// reports its own buffer overflowing as network loss.
pub fn stream_receive_buffer_bytes(packet_size: usize, shape: Shape) -> usize {
	let datagrams = match shape {
		Shape::Burst => 2_048,
		Shape::Paced => 8_192,
	};
	datagrams * (packet_size + 16)
}

/// Receiver policy.
#[derive(Clone, Copy, Debug)]
pub struct ClientPolicy {
	/// Stop the ladder when a finished step lost more than this percentage.
	pub stop_loss_percent: Option<u64>,
}

impl Default for ClientPolicy {
	fn default() -> Self {
		Self {
			stop_loss_percent: Some(10),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientStop {
	/// The host sent the end-of-run marker.
	Completed,
	/// The receiver stopped the ladder after a saturated step.
	Saturated {
		step: u8,
	},
	Cancelled,
	/// Datagrams stopped before the end-of-run marker.
	Silent,
}

#[derive(Clone, Debug)]
pub struct ClientRun {
	pub steps: Vec<Option<ClientStep>>,
	pub stop: ClientStop,
	pub received: u64,
	/// Address the first probe datagram came from.
	pub source: Option<SocketAddr>,
}

struct StepState {
	plan: StepPlan,
	first_sequence: u64,
	first_frame: u32,
	seen: Vec<u64>,
	received: u64,
	duplicates: u64,
	reordered: u64,
	frame_counts: Vec<u32>,
	/// Latest delay (µs, relative to the first datagram) per frame.
	frame_delivery: Vec<Option<i64>>,
	histogram: Vec<u32>,
	highest_frame: Option<u32>,
	ended: bool,
}

impl StepState {
	fn new(plan: StepPlan, first_sequence: u64, first_frame: u32) -> Self {
		let packets = plan.packets() as usize;
		Self {
			plan,
			first_sequence,
			first_frame,
			seen: vec![0; packets.div_ceil(64)],
			received: 0,
			duplicates: 0,
			reordered: 0,
			frame_counts: vec![0; plan.frames as usize],
			frame_delivery: vec![None; plan.frames as usize],
			histogram: vec![0; BUCKETS],
			highest_frame: None,
			ended: false,
		}
	}

	/// Frames and datagrams the host certainly sent: the whole step once it
	/// ended, otherwise up to the newest frame seen.
	fn evaluated_frames(&self) -> u32 {
		if self.ended {
			self.plan.frames
		} else {
			self.highest_frame.map_or(0, |frame| frame + 1)
		}
	}

	fn summarize(&self, fec_percent: u32, delay_floor: i64, interval_us: i64) -> Option<ClientStep> {
		let frames = self.evaluated_frames();
		if frames == 0 {
			return None;
		}
		let packets = u64::from(frames) * u64::from(self.plan.packets_per_frame);
		let (mut gap_events, mut max_gap, mut run) = (0u64, 0u64, 0u64);
		for index in 0..packets {
			if self.seen[(index / 64) as usize] & (1 << (index % 64)) == 0 {
				if run == 0 {
					gap_events += 1;
				}
				run += 1;
				max_gap = max_gap.max(run);
			} else {
				run = 0;
			}
		}
		let per_frame = self.plan.packets_per_frame;
		let data = (u64::from(per_frame) * 100).div_ceil(100 + u64::from(fec_percent)) as u32;
		let repairable = per_frame - data.min(per_frame);
		let counts = &self.frame_counts[..frames as usize];
		let frames_complete = counts.iter().filter(|c| **c >= per_frame).count() as u64;
		let frames_unrecoverable = counts
			.iter()
			.filter(|c| per_frame.saturating_sub(**c) > repairable)
			.count() as u64;
		let (queue_p50_us, queue_p95_us, queue_max_us) = percentiles(&self.histogram, delay_floor);
		let mut deliveries: Vec<i64> = self.frame_delivery[..frames as usize]
			.iter()
			.flatten()
			.copied()
			.collect();
		deliveries.sort_unstable();
		let frame_jitter_us = if deliveries.is_empty() {
			0
		} else {
			let at = |share: usize| deliveries[(deliveries.len() - 1) * share / 100];
			(at(95) - at(50)).clamp(0, 10 * interval_us) as u64
		};
		Some(ClientStep {
			received: self.received,
			duplicates: self.duplicates,
			reordered: self.reordered,
			gap_events,
			max_gap,
			frames_complete,
			frames_unrecoverable,
			queue_p50_us,
			queue_p95_us,
			queue_max_us,
			frame_jitter_us,
		})
	}
}

/// p50/p95/max of a delay histogram, relative to `floor` (a bucket index).
fn percentiles(histogram: &[u32], floor: i64) -> (u64, u64, u64) {
	let total: u64 = histogram.iter().map(|c| u64::from(*c)).sum();
	if total == 0 {
		return (0, 0, 0);
	}
	let at = |rank: u64| {
		let mut seen = 0u64;
		for (bucket, count) in histogram.iter().enumerate() {
			seen += u64::from(*count);
			if seen > rank {
				return ((bucket as i64 - floor).max(0) * BUCKET_US) as u64;
			}
		}
		0
	};
	let max = histogram.iter().rposition(|c| *c > 0).unwrap_or(0) as i64;
	(
		at(total / 2),
		at(total * 95 / 100),
		((max - floor).max(0) * BUCKET_US) as u64,
	)
}

struct Receiver<'a> {
	reply: &'a StartReply,
	steps: Vec<StepState>,
	start: Option<Instant>,
	first_delay: Option<i64>,
	highest_sequence: Option<u64>,
	received: u64,
	interval_us: i64,
}

impl<'a> Receiver<'a> {
	fn new(reply: &'a StartReply) -> Self {
		let mut steps = Vec::new();
		let (mut sequence, mut frame) = (0u64, 0u32);
		for step in &reply.plan.steps {
			steps.push(StepState::new(*step, sequence, frame));
			sequence += step.packets();
			frame += step.frames;
		}
		Self {
			reply,
			steps,
			start: None,
			first_delay: None,
			highest_sequence: None,
			received: 0,
			interval_us: reply.plan.frame_interval().as_micros() as i64,
		}
	}

	/// Account one datagram. Returns its step index and flags when it belongs to this run.
	fn accept(&mut self, datagram: &[u8], arrival: Instant) -> Option<(usize, u8)> {
		let header = ProbeHeader::parse(datagram)?;
		if header.run_tag != run_tag(&self.reply.run_id) || datagram.len() != self.reply.plan.shard_bytes {
			return None;
		}
		let step_index = usize::from(header.step);
		let step = self.steps.get_mut(step_index)?;
		let sequence = u64::from(header.sequence);
		let index = sequence.checked_sub(step.first_sequence)?;
		let frame = header.frame.checked_sub(step.first_frame)?;
		if index >= step.plan.packets() || frame >= step.plan.frames {
			return None;
		}
		let (word, bit) = ((index / 64) as usize, 1u64 << (index % 64));
		if step.seen[word] & bit != 0 {
			step.duplicates += 1;
			return Some((step_index, header.flags));
		}
		step.seen[word] |= bit;
		step.received += 1;
		self.received += 1;
		if self.highest_sequence.is_some_and(|highest| sequence < highest) {
			step.reordered += 1;
		}
		self.highest_sequence = Some(self.highest_sequence.map_or(sequence, |h| h.max(sequence)));
		step.frame_counts[frame as usize] += 1;
		step.highest_frame = Some(step.highest_frame.map_or(frame, |h| h.max(frame)));

		// One-way delay against the packet's own scheduled send time (offsets
		// cancel out: only the variation is used).
		let start = *self.start.get_or_insert(arrival);
		let arrival_us = arrival.saturating_duration_since(start).as_micros() as i64;
		let mut scheduled_us = i64::from(header.frame_offset_us);
		if self.reply.plan.shape == Shape::Paced {
			// The pacer releases whole chunks at deadlines proportional to the
			// datagrams preceding them (all datagrams have equal size).
			let chunk = self.reply.chunk_packets.max(1) as i64;
			let preceding = i64::from(header.index) / chunk * chunk;
			scheduled_us += preceding * self.interval_us / i64::from(step.plan.packets_per_frame);
		}
		let delay = arrival_us - scheduled_us;
		let first = *self.first_delay.get_or_insert(delay);
		let bucket = ((delay - first) / BUCKET_US + BUCKET_OFFSET).clamp(0, BUCKETS as i64 - 1) as usize;
		step.histogram[bucket] += 1;
		let delivery = &mut step.frame_delivery[frame as usize];
		*delivery = Some(delivery.map_or(delay - first, |d| d.max(delay - first)));
		if header.flags & FLAG_END_OF_STEP != 0 {
			step.ended = true;
		}
		// A datagram of a later step proves the earlier steps were fully sent.
		for earlier in &mut self.steps[..step_index] {
			earlier.ended = true;
		}
		Some((step_index, header.flags))
	}

	fn loss_percent(&self, step: usize) -> u64 {
		let state = &self.steps[step];
		let planned = state.plan.packets();
		planned.saturating_sub(state.received) * 100 / planned.max(1)
	}

	fn report(&self, counter: u32) -> Control {
		let mut control = Control::new(self.reply.run_id, ControlKind::Report, counter);
		for (index, step) in self.steps.iter().enumerate().take(MAX_STEPS) {
			control.received[index] = step.received.min(u64::from(u32::MAX)) as u32;
		}
		control
	}

	fn finish(&self, stop: ClientStop, source: Option<SocketAddr>) -> ClientRun {
		// The lowest delay bucket of the whole run is the uncongested baseline.
		let floor = (0..BUCKETS)
			.find(|bucket| self.steps.iter().any(|s| s.histogram[*bucket] > 0))
			.unwrap_or(0) as i64;
		let steps = self
			.steps
			.iter()
			.map(|step| step.summarize(self.reply.fec_percent, floor, self.interval_us))
			.collect();
		ClientRun {
			steps,
			stop,
			received: self.received,
			source,
		}
	}
}

/// Receive one run on `socket`, sending control datagrams to `host` (the
/// host's address with the reply's UDP port). Ends on the end-of-run marker,
/// silence, a saturated step (per `policy`) or `cancel`.
pub async fn receive(
	socket: &UdpSocket,
	host: SocketAddr,
	reply: &StartReply,
	policy: ClientPolicy,
	mut cancel: watch::Receiver<bool>,
) -> std::io::Result<ClientRun> {
	let mut receiver = Receiver::new(reply);
	let mut counter = 0u32;
	let mut send = |kind: ControlKind, receiver: &Receiver<'_>| {
		counter += 1;
		let control = match kind {
			ControlKind::Report => receiver.report(counter),
			kind => Control::new(reply.run_id, kind, counter),
		};
		control.encode(&reply.key)
	};
	let mut buffer = vec![0u8; 65_536];
	let hello_deadline = Instant::now() + super::sender::HELLO_TIMEOUT;
	let mut next_control = Instant::now();
	let mut last_arrival: Option<Instant> = None;
	let mut end_at: Option<Instant> = None;
	let mut source = None;
	let mut stop_sent: Option<ClientStop> = None;

	loop {
		let now = Instant::now();
		if let Some(end) = end_at
			&& now >= end
		{
			return Ok(receiver.finish(stop_sent.unwrap_or(ClientStop::Completed), source));
		}
		if last_arrival.is_none() && now >= hello_deadline {
			return Err(std::io::Error::new(
				std::io::ErrorKind::TimedOut,
				"no probe datagram arrived (is UDP to the host's video port blocked?)",
			));
		}
		if last_arrival.is_some_and(|at| now.saturating_duration_since(at) > SILENCE_TIMEOUT) {
			return Ok(receiver.finish(stop_sent.unwrap_or(ClientStop::Silent), source));
		}
		if now >= next_control {
			let datagram = if last_arrival.is_none() {
				send(ControlKind::Hello, &receiver)
			} else {
				send(ControlKind::Report, &receiver)
			};
			let _ = socket.send_to(&datagram, host).await;
			next_control = now
				+ if last_arrival.is_none() {
					HELLO_INTERVAL
				} else {
					REPORT_INTERVAL
				};
		}
		let wake = [Some(next_control), end_at].into_iter().flatten().min().unwrap();
		tokio::select! {
			biased;
			_ = cancel.changed() => {
				for _ in 0..3 {
					let _ = socket.send_to(&send(ControlKind::Stop, &receiver), host).await;
				}
				return Ok(receiver.finish(ClientStop::Cancelled, source));
			},
			_ = tokio::time::sleep_until(wake.into()) => {},
			received = socket.recv_from(&mut buffer) => {
				let (length, from) = received?;
				let arrival = Instant::now();
				if from.ip().to_canonical() != host.ip().to_canonical() {
					continue;
				}
				let Some((_, flags)) = receiver.accept(&buffer[..length], arrival) else {
					continue;
				};
				source.get_or_insert(from);
				last_arrival = Some(arrival);
				if flags & FLAG_END_OF_RUN != 0 && end_at.is_none() {
					end_at = Some(arrival + END_LINGER);
				}
				// Stop the ladder after a finished step that lost too much; the
				// last step needs no stop.
				if stop_sent.is_none()
					&& let Some(limit) = policy.stop_loss_percent
				{
					let saturated = (0..receiver.steps.len())
						.find(|s| receiver.steps[*s].ended && *s + 1 < receiver.steps.len() && receiver.loss_percent(*s) > limit);
					if let Some(saturated) = saturated {
						for _ in 0..3 {
							let _ = socket.send_to(&send(ControlKind::Stop, &receiver), host).await;
						}
						stop_sent = Some(ClientStop::Saturated { step: saturated as u8 });
						end_at = Some(arrival + END_LINGER);
					}
				}
			},
		}
	}
}

/// Query string for `/pyroshine-udp-probe-result`.
pub fn result_query(reply: &StartReply, run: &ClientRun, client_link_mbps: u64) -> String {
	let mut query = format!("run={}", hex::encode(reply.run_id));
	for (index, step) in run.steps.iter().enumerate() {
		if let Some(step) = step {
			let fields: Vec<String> = step.to_fields().iter().map(u64::to_string).collect();
			query += &format!("&s{index}={}", fields.join(","));
		}
	}
	if client_link_mbps > 0 {
		query += &format!("&clientlinkmbps={client_link_mbps}");
	}
	query
}

/// Text of the first `<tag>…</tag>` in `xml`.
pub fn xml_value<'x>(xml: &'x str, tag: &str) -> Option<&'x str> {
	let open = format!("<{tag}>");
	let start = xml.find(&open)? + open.len();
	let end = xml[start..].find(&format!("</{tag}>"))? + start;
	Some(&xml[start..end])
}

impl StartReply {
	/// Parse the start response (the inverse of [`StartReply::to_xml`]).
	pub fn from_xml(xml: &str) -> Result<Self, String> {
		let number = |tag: &str| -> Result<u64, String> {
			xml_value(xml, tag)
				.and_then(|v| v.parse().ok())
				.ok_or_else(|| format!("missing {tag}"))
		};
		let bytes = |tag: &str| -> Result<Vec<u8>, String> {
			xml_value(xml, tag)
				.and_then(|v| hex::decode(v).ok())
				.ok_or_else(|| format!("missing {tag}"))
		};
		let run_id: [u8; RUN_ID_BYTES] = bytes("RunId")?.try_into().map_err(|_| "bad RunId")?;
		let key: [u8; KEY_BYTES] = bytes("Key")?.try_into().map_err(|_| "bad Key")?;
		let shape = match xml_value(xml, "Shape") {
			Some("burst") => Shape::Burst,
			Some("paced") => Shape::Paced,
			_ => return Err("bad Shape".into()),
		};
		let mut steps = Vec::new();
		let mut rest = xml;
		while let Some(start) = rest.find("<Step>") {
			let end = rest[start..].find("</Step>").ok_or("unterminated Step")? + start;
			let step = &rest[start..end];
			let field = |tag| {
				xml_value(step, tag)
					.and_then(|v| v.parse::<u32>().ok())
					.ok_or_else(|| format!("missing Step {tag}"))
			};
			let offered = field("OfferedKbps")?;
			steps.push(StepPlan {
				requested_kbps: offered,
				offered_kbps: offered,
				packets_per_frame: field("PacketsPerFrame")?,
				frames: field("Frames")?,
			});
			rest = &rest[end..];
		}
		if steps.is_empty() || steps.len() > MAX_STEPS {
			return Err("bad step count".into());
		}
		Ok(Self {
			run_id,
			key,
			udp_port: number("UdpPort")? as u16,
			packet_size: number("PacketSize")? as usize,
			fec_percent: number("FecPercent")? as u32,
			chunk_packets: number("ChunkPackets")? as usize,
			plan: ProbePlan {
				shard_bytes: number("ShardBytes")? as usize,
				wire_packet_bytes: number("WirePacketBytes")? as usize,
				fps: number("Fps")? as u32,
				step_ms: number("StepMs")? as u32,
				shape,
				steps,
			},
		})
	}
}
