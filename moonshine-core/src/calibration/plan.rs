//! Validated request parameters and the deterministic schedule of a run.
//!
//! The host and the client derive the same schedule from the same numbers,
//! so the client knows how many datagrams each step should deliver even if the
//! final HTTPS exchange fails.
use std::collections::HashMap;
use std::time::Duration;

use super::wire::{MAX_STEPS, PROBE_HEADER_BYTES};

/// Protocol version this host implements.
pub const PROTOCOL_VERSION: u32 = 1;
/// Highest offered rate a client may request, in kbit/s (IP-level).
pub const MAX_RATE_KBPS: u32 = 3_000_000;
pub const MIN_RATE_KBPS: u32 = 1_000;
pub const MIN_STEP_MS: u32 = 250;
pub const MAX_STEP_MS: u32 = 2_000;
/// Upper bound of the transmitting part of one run.
pub const MAX_RUN_MS: u32 = 12_000;
pub const MIN_FPS: u32 = 10;
pub const MAX_FPS: u32 = 480;
/// Smallest probe `packetSize`; smaller packets cannot carry the probe header.
pub const MIN_PACKET_SIZE: usize = 256;
/// Jumbo-frame bound; real paths are bounded further by their MTU.
pub const MAX_PACKET_SIZE: usize = 9_000;
/// Datagrams one frame may hold (the probe header indexes them with 16 bits).
pub const MAX_PACKETS_PER_FRAME: u32 = 65_535;
/// Bytes one frame may hold: the host keeps one frame buffer per step. 3 Gbit/s
/// at 10 FPS (37.5 MB per frame) fits.
pub const MAX_FRAME_BYTES: usize = 48 * 1024 * 1024;

/// How each frame's datagrams leave the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
	/// H.264/HEVC/AV1: the whole frame is submitted at its start, in GSO
	/// chunks, as the conventional sender does.
	Burst,
	/// PyroWave: each frame is spread across its interval by the production
	/// frame pacer.
	Paced,
}

impl Shape {
	pub fn as_str(self) -> &'static str {
		match self {
			Shape::Burst => "burst",
			Shape::Paced => "paced",
		}
	}
}

/// A client's request, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRequest {
	pub max_kbps: u32,
	pub steps: u8,
	pub step_ms: u32,
	pub fps: u32,
	/// Moonlight `packetSize` (NV header plus payload), after the host's cap.
	pub packet_size: usize,
	pub encrypted: bool,
	pub shape: Shape,
	/// Mark probe datagrams with the stream's video DSCP class, as a client
	/// that requests QoS would have its video marked.
	pub qos: bool,
}

impl ProbeRequest {
	/// Validate query parameters. `clamp_packet_size` applies the host's
	/// `max_packet_size`, exactly as RTSP negotiation would for the stream.
	pub fn parse(
		params: &HashMap<String, String>,
		clamp_packet_size: impl Fn(usize, bool) -> usize,
	) -> Result<Self, String> {
		fn number<T: std::str::FromStr>(
			params: &HashMap<String, String>,
			key: &str,
			default: Option<T>,
		) -> Result<T, String> {
			match params.get(key) {
				// Bounded digits only: no signs, whitespace or exponents.
				Some(value) if !value.is_empty() && value.len() <= 9 && value.bytes().all(|b| b.is_ascii_digit()) => {
					value.parse().map_err(|_| format!("{key} is out of range"))
				},
				Some(_) => Err(format!("{key} must be a decimal number")),
				None => default.ok_or_else(|| format!("{key} is required")),
			}
		}
		fn flag(params: &HashMap<String, String>, key: &str) -> Result<bool, String> {
			match params.get(key).map(String::as_str) {
				None | Some("0") => Ok(false),
				Some("1") => Ok(true),
				Some(_) => Err(format!("{key} must be 0 or 1")),
			}
		}

		let version: u32 = number(params, "version", None)?;
		if version != PROTOCOL_VERSION {
			return Err(format!(
				"probe version {version} is not supported (this host implements {PROTOCOL_VERSION})"
			));
		}
		let max_kbps: u32 = number(params, "maxkbps", None)?;
		let steps: u8 = number(params, "steps", Some(5))?;
		let step_ms: u32 = number(params, "stepms", Some(1_000))?;
		let fps: u32 = number(params, "fps", None)?;
		let requested_packet_size: usize = number(params, "packetsize", Some(1_392))?;
		let encrypted = flag(params, "encrypted")?;
		let qos = flag(params, "qos")?;
		let shape = match params.get("shape").map(String::as_str) {
			Some("burst") => Shape::Burst,
			Some("paced") => Shape::Paced,
			_ => return Err("shape must be burst or paced".into()),
		};

		if !(MIN_RATE_KBPS..=MAX_RATE_KBPS).contains(&max_kbps) {
			return Err(format!("maxkbps must be {MIN_RATE_KBPS}–{MAX_RATE_KBPS}"));
		}
		if steps == 0 || usize::from(steps) > MAX_STEPS {
			return Err(format!("steps must be 1–{MAX_STEPS}"));
		}
		if !(MIN_STEP_MS..=MAX_STEP_MS).contains(&step_ms) {
			return Err(format!("stepms must be {MIN_STEP_MS}–{MAX_STEP_MS}"));
		}
		if u32::from(steps) * step_ms > MAX_RUN_MS {
			return Err(format!("steps × stepms must not exceed {MAX_RUN_MS} ms"));
		}
		if !(MIN_FPS..=MAX_FPS).contains(&fps) {
			return Err(format!("fps must be {MIN_FPS}–{MAX_FPS}"));
		}
		if !(MIN_PACKET_SIZE..=MAX_PACKET_SIZE).contains(&requested_packet_size) {
			return Err(format!("packetsize must be {MIN_PACKET_SIZE}–{MAX_PACKET_SIZE}"));
		}
		let packet_size = clamp_packet_size(requested_packet_size, encrypted).max(MIN_PACKET_SIZE);
		let request = Self {
			max_kbps,
			steps,
			step_ms,
			fps,
			packet_size,
			encrypted,
			shape,
			qos,
		};
		// Reject requests whose top step would need oversized frames.
		let top = request.plan(false);
		if top.steps.last().is_some_and(|s| {
			s.packets_per_frame > MAX_PACKETS_PER_FRAME
				|| s.packets_per_frame as usize * top.shard_bytes > MAX_FRAME_BYTES
		}) {
			return Err("maxkbps is too high for this fps and packet size".into());
		}
		Ok(request)
	}

	/// The schedule for a peer of the given address family.
	pub fn plan(&self, ipv6: bool) -> ProbePlan {
		let shard_bytes = crate::session::stream::video::wire_shard_size(self.packet_size, self.encrypted)
			.expect("packet size is bounded");
		debug_assert!(shard_bytes >= PROBE_HEADER_BYTES);
		let wire_packet_bytes = shard_bytes + ip_udp_header_bytes(ipv6);
		let frames_per_step = (u64::from(self.step_ms) * u64::from(self.fps)).div_ceil(1_000).max(1) as u32;
		let steps = (1..=u32::from(self.steps))
			.map(|step| {
				let requested_kbps = (u64::from(self.max_kbps) * u64::from(step)).div_ceil(u64::from(self.steps));
				// Bytes per frame at the requested IP-level rate, rounded to whole datagrams.
				let frame_bits = requested_kbps * 1_000 / u64::from(self.fps);
				let packets = (frame_bits as f64 / (wire_packet_bytes as f64 * 8.0)).round().max(1.0) as u64;
				let packets_per_frame = packets.min(u64::from(u32::MAX)) as u32;
				StepPlan {
					requested_kbps: requested_kbps as u32,
					offered_kbps: (u64::from(packets_per_frame) * wire_packet_bytes as u64 * 8 * u64::from(self.fps)
						/ 1_000) as u32,
					packets_per_frame,
					frames: frames_per_step,
				}
			})
			.collect();
		ProbePlan {
			shard_bytes,
			wire_packet_bytes,
			fps: self.fps,
			step_ms: self.step_ms,
			shape: self.shape,
			steps,
		}
	}
}

/// IP and UDP header bytes, as the video sender accounts its wire bytes.
pub fn ip_udp_header_bytes(ipv6: bool) -> usize {
	if ipv6 { 48 } else { 28 }
}

/// Ethernet bytes around every IP packet: preamble and start delimiter (8),
/// MAC header (14), FCS (4) and the inter-packet gap (12).
pub const ETHERNET_FRAMING_BYTES: usize = 38;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepPlan {
	pub requested_kbps: u32,
	/// IP-level rate the whole datagrams of this step actually offer.
	pub offered_kbps: u32,
	pub packets_per_frame: u32,
	pub frames: u32,
}

impl StepPlan {
	pub fn packets(&self) -> u64 {
		u64::from(self.packets_per_frame) * u64::from(self.frames)
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbePlan {
	/// UDP payload of each probe datagram (equal to a video shard).
	pub shard_bytes: usize,
	/// Shard plus IP and UDP headers.
	pub wire_packet_bytes: usize,
	pub fps: u32,
	pub step_ms: u32,
	pub shape: Shape,
	pub steps: Vec<StepPlan>,
}

impl ProbePlan {
	pub fn frame_interval(&self) -> Duration {
		Duration::from_secs(1) / self.fps
	}

	/// Scheduled start of a run-wide frame number relative to the run start.
	pub fn frame_offset(&self, frame: u32) -> Duration {
		Duration::from_nanos(u64::from(frame) * 1_000_000_000 / u64::from(self.fps))
	}

	pub fn total_frames(&self) -> u32 {
		self.steps.iter().map(|s| s.frames).sum()
	}

	/// Transmitting time of the whole run.
	pub fn duration(&self) -> Duration {
		self.frame_offset(self.total_frames())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
		pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
	}

	fn base() -> Vec<(&'static str, &'static str)> {
		vec![
			("version", "1"),
			("maxkbps", "500000"),
			("fps", "60"),
			("shape", "burst"),
		]
	}

	#[test]
	fn defaults_and_schedule() {
		let request = ProbeRequest::parse(&params(&base()), |size, _| size).unwrap();
		assert_eq!(request.steps, 5);
		assert_eq!(request.step_ms, 1_000);
		assert_eq!(request.packet_size, 1_392);
		let plan = request.plan(false);
		assert_eq!(plan.shard_bytes, 1_408);
		assert_eq!(plan.wire_packet_bytes, 1_436);
		assert_eq!(plan.steps.len(), 5);
		assert_eq!(plan.total_frames(), 300);
		assert_eq!(plan.duration(), Duration::from_secs(5));
		// Each step offers its requested rate within one datagram per frame.
		for (index, step) in plan.steps.iter().enumerate() {
			assert_eq!(step.requested_kbps, 100_000 * (index as u32 + 1));
			let tolerance = (1_436 * 8 * 60 / 1_000) as i64;
			assert!((i64::from(step.offered_kbps) - i64::from(step.requested_kbps)).abs() <= tolerance);
		}
		// IPv6 headers cost more per datagram, so fewer fit the same rate.
		assert!(request.plan(true).steps[4].packets_per_frame < plan.steps[4].packets_per_frame);
	}

	#[test]
	fn encrypted_and_capped_packet_sizes_match_rtsp_negotiation() {
		let mut pairs = base();
		pairs.extend([("packetsize", "1392"), ("encrypted", "1")]);
		// A host `max_packet_size` of 1200 caps an encrypted stream at 1168.
		let request = ProbeRequest::parse(&params(&pairs), |size, encrypted| {
			size.min(if encrypted { 1_200 - 32 } else { 1_200 })
		})
		.unwrap();
		assert_eq!(request.packet_size, 1_168);
		assert_eq!(request.plan(false).shard_bytes, 1_168 + 16 + 32);
	}

	#[test]
	fn rejects_out_of_domain_requests() {
		let rejected = |extra: &[(&'static str, &'static str)]| {
			let mut pairs = base();
			pairs.retain(|(k, _)| !extra.iter().any(|(e, _)| e == k));
			pairs.extend_from_slice(extra);
			ProbeRequest::parse(&params(&pairs), |size, _| size).unwrap_err()
		};
		assert!(rejected(&[("version", "2")]).contains("not supported"));
		assert!(rejected(&[("maxkbps", "999")]).contains("maxkbps"));
		assert!(rejected(&[("maxkbps", "3000001")]).contains("maxkbps"));
		assert!(rejected(&[("maxkbps", "-5")]).contains("decimal"));
		assert!(rejected(&[("maxkbps", "1e6")]).contains("decimal"));
		assert!(rejected(&[("maxkbps", "")]).contains("decimal"));
		assert!(rejected(&[("steps", "0")]).contains("steps"));
		assert!(rejected(&[("steps", "9")]).contains("steps"));
		assert!(rejected(&[("stepms", "100")]).contains("stepms"));
		assert!(rejected(&[("steps", "8"), ("stepms", "2000")]).contains("exceed"));
		assert!(rejected(&[("fps", "5")]).contains("fps"));
		assert!(rejected(&[("fps", "481")]).contains("fps"));
		assert!(rejected(&[("packetsize", "100")]).contains("packetsize"));
		assert!(rejected(&[("packetsize", "9001")]).contains("packetsize"));
		assert!(rejected(&[("shape", "flood")]).contains("shape"));
		assert!(rejected(&[("encrypted", "yes")]).contains("encrypted"));
		assert!(rejected(&[("qos", "2")]).contains("qos"));
		// 3 Gbit/s at 10 FPS with minimum-size datagrams needs oversized frames.
		assert!(rejected(&[("maxkbps", "3000000"), ("fps", "10"), ("packetsize", "256")]).contains("too high"));
		let mut missing = base();
		missing.retain(|(k, _)| *k != "fps");
		assert!(
			ProbeRequest::parse(&params(&missing), |s, _| s)
				.unwrap_err()
				.contains("required")
		);
	}
}
