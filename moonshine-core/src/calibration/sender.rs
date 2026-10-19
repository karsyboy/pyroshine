//! Host side of one run: authenticate the client's UDP endpoint, then send the
//! planned frames through the production video sender while the client keeps
//! granting consent.
//!
//! Safety properties, in order of enforcement:
//! - The destination is the source of an authenticated hello from the TLS
//!   peer's own IP address; no request parameter names an address or port.
//! - Sending continues only while authenticated reports keep arriving
//!   ([`CONSENT_TIMEOUT`]); a vanished receiver stops the run.
//! - A run is bounded by its validated plan and an absolute deadline, and it
//!   ends at once on a client stop, a session start or service shutdown.
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::plan::{ProbePlan, Shape};
use super::wire::{
	CONTROL_BYTES, Control, ControlKind, FILLER, FLAG_END_OF_FRAME, FLAG_END_OF_RUN, FLAG_END_OF_STEP, KEY_BYTES,
	MAX_STEPS, PROBE_HEADER_BYTES, ProbeHeader, RUN_ID_BYTES, run_tag,
};
use crate::session::stream::video::{FramePacing, PacingTimer, ShardBuf, UdpGsoSocket};

/// Time for the client's first authenticated hello after the HTTPS reply.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// The run stops when no authenticated control datagram arrived for this long.
pub const CONSENT_TIMEOUT: Duration = Duration::from_millis(1_000);
/// Delay between the hello and the first frame, so the client is ready.
const LEAD_IN: Duration = Duration::from_millis(20);
/// Time after a step ends before a report must reflect it.
const REPORT_GRACE: Duration = Duration::from_millis(150);
/// A report showing more loss than this for a finished step ends the ladder:
/// higher steps would only push more traffic into a saturated path.
const SATURATION_LOSS_PERCENT: u64 = 10;
/// Time to wait for a final report after the last frame.
const FINAL_REPORT_WAIT: Duration = Duration::from_millis(600);
/// Slack beyond the plan before the host is considered unable to keep up.
const OVERRUN_ALLOWANCE: Duration = Duration::from_secs(2);
/// DSCP CS5 (as the video stream marks it when the client asks for QoS).
const VIDEO_TOS: u32 = 160;

/// Why a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
	Completed,
	/// A finished step lost more than 10 % of its datagrams; the ladder stopped there.
	Saturated {
		step: u8,
	},
	/// The client sent an authenticated stop.
	StoppedByClient,
	/// Reports stopped arriving.
	ReceiverSilent,
	/// No authenticated hello arrived.
	HandshakeTimeout,
	/// A session started (the probe yields the video port) or the service stopped.
	Preempted,
	/// The host could not keep the schedule.
	HostOverloaded,
	/// Every datagram of a frame failed to submit.
	SendFailed(String),
}

impl Outcome {
	pub fn as_str(&self) -> &'static str {
		match self {
			Outcome::Completed => "completed",
			Outcome::Saturated { .. } => "saturated",
			Outcome::StoppedByClient => "stopped_by_client",
			Outcome::ReceiverSilent => "receiver_silent",
			Outcome::HandshakeTimeout => "handshake_timeout",
			Outcome::Preempted => "preempted",
			Outcome::HostOverloaded => "host_overloaded",
			Outcome::SendFailed(_) => "send_failed",
		}
	}
}

/// Host-side accounting of one step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostStep {
	pub sent_frames: u32,
	/// Datagrams handed to the socket (each counted once).
	pub attempted: u64,
	/// Datagrams the kernel accepted. Not delivery.
	pub submitted: u64,
	pub failed: u64,
	pub would_block: u64,
	/// Frames whose send started more than one frame interval late.
	pub late_frames: u32,
	pub max_start_lateness_us: u64,
	pub max_pacing_lateness_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRun {
	pub outcome: Outcome,
	pub steps: Vec<HostStep>,
	/// Received counts of the last authenticated report.
	pub reported: [u32; MAX_STEPS],
	pub reports: u32,
	pub client: Option<SocketAddr>,
	pub elapsed: Duration,
}

pub(super) struct RunContext {
	pub plan: ProbePlan,
	pub run_id: [u8; RUN_ID_BYTES],
	pub key: [u8; KEY_BYTES],
	pub peer_ip: IpAddr,
	pub qos: bool,
	/// Changes when the run must yield (session start, service shutdown).
	pub preempt: watch::Receiver<u64>,
}

struct Consent {
	destination: Option<SocketAddr>,
	last_seen: Instant,
	counter: Option<u32>,
	reported: [u32; MAX_STEPS],
	last_report_at: Option<Instant>,
	reports: u32,
	stop: bool,
}

impl Consent {
	/// Accept an authenticated datagram from the peer's IP (and, once locked,
	/// from the locked endpoint only), with a strictly increasing counter.
	fn accept(&mut self, context: &RunContext, datagram: &[u8], source: SocketAddr) -> Option<ControlKind> {
		if datagram.len() != CONTROL_BYTES || source.ip().to_canonical() != context.peer_ip.to_canonical() {
			return None;
		}
		if self.destination.is_some_and(|locked| locked != source) {
			return None;
		}
		let control = Control::decode(datagram, &context.run_id, &context.key)?;
		if self.counter.is_some_and(|last| control.counter <= last) {
			return None;
		}
		self.counter = Some(control.counter);
		let now = Instant::now();
		self.last_seen = now;
		match control.kind {
			ControlKind::Hello => {},
			ControlKind::Report => {
				self.reported = control.received;
				self.last_report_at = Some(now);
				self.reports += 1;
			},
			ControlKind::Stop => self.stop = true,
		}
		Some(control.kind)
	}
}

/// Drain queued control datagrams without waiting.
fn drain(socket: &UdpGsoSocket, consent: &mut Consent, context: &RunContext, buffer: &mut [u8]) {
	// Bounded so a flood of junk datagrams cannot starve the schedule.
	for _ in 0..64 {
		match socket.try_recv_from(buffer) {
			Ok((length, source)) => {
				consent.accept(context, &buffer[..length], source);
			},
			Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
			Err(_) => continue,
		}
	}
}

pub(super) async fn run(mut socket: UdpGsoSocket, mut context: RunContext) -> HostRun {
	let started = Instant::now();
	let plan = context.plan.clone();
	let mut result = HostRun {
		outcome: Outcome::Completed,
		steps: vec![HostStep::default(); plan.steps.len()],
		reported: [0; MAX_STEPS],
		reports: 0,
		client: None,
		elapsed: Duration::ZERO,
	};
	let mut consent = Consent {
		destination: None,
		last_seen: started,
		counter: None,
		reported: [0; MAX_STEPS],
		last_report_at: None,
		reports: 0,
		stop: false,
	};
	let mut buffer = [0u8; 2_048];
	let outcome = send(
		&mut socket,
		&mut context,
		&plan,
		&mut consent,
		&mut result.steps,
		&mut buffer,
	)
	.await;
	result.outcome = outcome;
	result.reported = consent.reported;
	result.reports = consent.reports;
	result.client = consent.destination;
	result.elapsed = started.elapsed();
	result
}

async fn send(
	socket: &mut UdpGsoSocket,
	context: &mut RunContext,
	plan: &ProbePlan,
	consent: &mut Consent,
	steps: &mut [HostStep],
	buffer: &mut [u8],
) -> Outcome {
	// Handshake: the first authenticated hello fixes the destination.
	let hello_deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
	while consent.destination.is_none() {
		tokio::select! {
			biased;
			_ = context.preempt.changed() => return Outcome::Preempted,
			_ = tokio::time::sleep_until(hello_deadline) => return Outcome::HandshakeTimeout,
			received = socket.recv_from(buffer) => match received {
				Ok((length, source)) => {
					if consent.accept(context, &buffer[..length], source) == Some(ControlKind::Hello) {
						consent.destination = Some(source);
					}
				},
				// Errors on an unconnected UDP socket are transient (for example a
				// queued ICMP report); do not spin on them.
				Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
			},
		}
	}
	let destination = consent.destination.expect("handshake completed");
	if context.qos && destination.is_ipv4() {
		let _ = socket.set_tos_v4(VIDEO_TOS);
	}

	let mut timer = PacingTimer::new().ok();
	let interval = plan.frame_interval();
	let start = Instant::now() + LEAD_IN;
	let deadline = start + plan.duration() + OVERRUN_ALLOWANCE;
	let tag = run_tag(&context.run_id);
	let last_step = plan.steps.len() - 1;
	let mut sequence = 0u32;
	let mut frame = 0u32;
	let mut step_ends: Vec<Option<Instant>> = vec![None; plan.steps.len()];

	for (step_index, step) in plan.steps.iter().enumerate() {
		let packets = step.packets_per_frame as usize;
		let mut batch = ShardBuf::new(packets, plan.shard_bytes, 0).into_batch();
		batch.as_bytes_mut().fill(FILLER);
		for frame_in_step in 0..step.frames {
			let due = start + plan.frame_offset(frame);
			// Wait for the frame's start while servicing consent.
			loop {
				drain(socket, consent, context, buffer);
				if let Some(outcome) = check(consent, steps, &step_ends, deadline, &context.preempt) {
					return outcome;
				}
				if Instant::now() >= due {
					break;
				}
				tokio::select! {
					biased;
					_ = context.preempt.changed() => return Outcome::Preempted,
					_ = wait_until(&mut timer, due) => break,
					received = socket.recv_from(buffer) => {
						if let Ok((length, source)) = received {
							consent.accept(context, &buffer[..length], source);
						}
					},
				}
			}
			let accounting = &mut steps[step_index];

			let frame_offset_us = plan.frame_offset(frame).as_micros().min(u128::from(u32::MAX)) as u32;
			let bytes = batch.as_bytes_mut();
			for index in 0..packets {
				let mut flags = 0;
				if index + 1 == packets {
					flags |= FLAG_END_OF_FRAME;
					if frame_in_step + 1 == step.frames {
						flags |= FLAG_END_OF_STEP;
						if step_index == last_step {
							flags |= FLAG_END_OF_RUN;
						}
					}
				}
				ProbeHeader {
					run_tag: tag,
					sequence: sequence.wrapping_add(index as u32),
					step: step_index as u8,
					flags,
					index: index as u16,
					frame,
					frame_offset_us,
				}
				.write(&mut bytes[index * plan.shard_bytes..index * plan.shard_bytes + PROBE_HEADER_BYTES]);
			}

			let lateness = Instant::now().saturating_duration_since(due);
			if lateness > interval {
				accounting.late_frames += 1;
			}
			accounting.max_start_lateness_us = accounting.max_start_lateness_us.max(lateness.as_micros() as u64);
			let pacing = match plan.shape {
				Shape::Burst => None,
				Shape::Paced => {
					// The production pacer spreads a frame across the time its encoded
					// bytes represent at the negotiated bitrate. Declaring the whole
					// frame as encoded bytes at exactly one frame interval's rate
					// spreads it across its interval, as a CBR PyroWave frame is.
					let encoded = packets * plan.shard_bytes;
					batch.set_frame_metadata(frame, encoded, packets, 0, 1);
					batch.set_pacing_origin(due);
					Some(FramePacing {
						bitrate_bps: encoded as u64 * 8 * u64::from(plan.fps),
						frame_interval: interval,
					})
				},
			};
			let stats = socket.send_batch(&mut batch, destination, pacing).await;
			accounting.sent_frames += 1;
			accounting.attempted += stats.outcome.attempted_datagrams as u64;
			accounting.submitted += stats.outcome.submitted_datagrams as u64;
			accounting.failed += stats.outcome.failed_datagrams as u64;
			accounting.would_block += u64::from(stats.would_block_events);
			accounting.max_pacing_lateness_us = accounting
				.max_pacing_lateness_us
				.max(stats.max_pacing_lateness.as_micros() as u64);
			if stats.outcome.submitted_datagrams == 0 {
				let error = stats
					.outcome
					.last_error
					.map(|kind| kind.to_string())
					.unwrap_or_else(|| "no datagram was accepted".into());
				return Outcome::SendFailed(error);
			}
			sequence = sequence.wrapping_add(packets as u32);
			frame += 1;
		}
		step_ends[step_index] = Some(Instant::now());
	}

	// Wait briefly for a report that reflects the final step.
	let final_deadline = Instant::now() + FINAL_REPORT_WAIT;
	let last_end = step_ends[last_step].expect("all steps ran");
	while Instant::now() < final_deadline && !consent.stop {
		if consent.last_report_at.is_some_and(|at| at >= last_end + REPORT_GRACE) {
			break;
		}
		tokio::select! {
			biased;
			_ = context.preempt.changed() => break,
			_ = tokio::time::sleep_until(final_deadline.into()) => break,
			received = socket.recv_from(buffer) => {
				if let Ok((length, source)) = received {
					consent.accept(context, &buffer[..length], source);
				}
			},
		}
	}
	saturation(consent, steps, &step_ends).map_or(Outcome::Completed, |step| Outcome::Saturated { step })
}

/// Conditions that end a run between frames.
fn check(
	consent: &Consent,
	steps: &[HostStep],
	step_ends: &[Option<Instant>],
	deadline: Instant,
	preempt: &watch::Receiver<u64>,
) -> Option<Outcome> {
	if preempt.has_changed().unwrap_or(true) {
		return Some(Outcome::Preempted);
	}
	if consent.stop {
		return Some(Outcome::StoppedByClient);
	}
	let now = Instant::now();
	if now.saturating_duration_since(consent.last_seen) > CONSENT_TIMEOUT {
		return Some(Outcome::ReceiverSilent);
	}
	if now > deadline {
		return Some(Outcome::HostOverloaded);
	}
	saturation(consent, steps, step_ends).map(|step| Outcome::Saturated { step })
}

/// The first finished step whose report shows the path saturated.
fn saturation(consent: &Consent, steps: &[HostStep], step_ends: &[Option<Instant>]) -> Option<u8> {
	let reported_at = consent.last_report_at?;
	steps
		.iter()
		.zip(step_ends)
		.enumerate()
		.find_map(|(index, (step, end))| {
			let end = (*end)?;
			let reflected = reported_at >= end + REPORT_GRACE;
			let received = u64::from(consent.reported[index]);
			(reflected && step.submitted >= 100 && received * 100 < step.submitted * (100 - SATURATION_LOSS_PERCENT))
				.then_some(index as u8)
		})
}

async fn wait_until(timer: &mut Option<PacingTimer>, due: Instant) {
	if let Some(active) = timer {
		if active.sleep_until(due).await.is_ok() {
			return;
		}
		*timer = None;
	}
	tokio::time::sleep_until(due.into()).await;
}
