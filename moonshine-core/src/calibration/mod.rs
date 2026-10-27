//! Authenticated UDP bandwidth calibration.
//!
//! A paired client asks the HTTPS API for a run (`/pyroshine-udp-probe`) and
//! receives a run identifier, a secret key and the schedule. It then sends an
//! authenticated hello from its UDP socket to the host's video port; the host
//! sends frame-shaped datagrams through the production video sender to that
//! endpoint while the client keeps sending authenticated reports. Finally the
//! client submits what it received (`/pyroshine-udp-probe-result`) and the host
//! returns per-step measurements and a conservative bitrate suggestion.
//!
//! The probe uses the video UDP port, so it follows the stream's firewall and
//! port-forwarding path. It is refused while a session exists, at most one run
//! exists at a time, and a session start preempts it before binding the port
//! (see `VideoPortArbiter::release_for_session`).
pub mod client;
pub mod plan;
pub mod recommend;
mod sender;
#[cfg(test)]
mod tests;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

use crate::session::stream::video::{FecMode, UdpGsoSocket, VideoStreamConfig};
pub use plan::{ProbePlan, ProbeRequest, Shape};
pub use recommend::{ClientStep, Confidence, Recommendation, Verdict};
pub use sender::{HostRun, HostStep, Outcome};
use wire::{KEY_BYTES, RUN_ID_BYTES};

/// Runs one client may start per [`RATE_WINDOW`]. A Pyrolight calibration is
/// one run; this leaves room for retries without allowing sustained use.
const RUNS_PER_WINDOW: usize = 12;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Finished runs stay available to their client this long.
const RESULT_RETENTION: Duration = Duration::from_secs(60);
/// Registry bound; entries are small and pruned by age first.
const MAX_REGISTERED_RUNS: usize = 16;
/// A session start waits at most this long for a run to release the port.
const PREEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Coordinates the video UDP port between calibration runs and sessions.
///
/// One permit exists; a run holds it for as long as its socket is bound.
#[derive(Clone)]
pub struct VideoPortArbiter {
	slot: Arc<Semaphore>,
	preempt: Arc<watch::Sender<u64>>,
	/// Session initializations between [`Self::release_for_session`] and the
	/// drop of its claim; new runs are refused meanwhile.
	starting: Arc<AtomicUsize>,
}

impl Default for VideoPortArbiter {
	fn default() -> Self {
		Self {
			slot: Arc::new(Semaphore::new(1)),
			preempt: Arc::new(watch::channel(0).0),
			starting: Arc::default(),
		}
	}
}

/// Held by session initialization while it binds the stream sockets. Once it
/// is dropped the session is visible to the HTTPS API (the manager publishes
/// the session before initializing it), which refuses runs from then on.
pub(crate) struct SessionPortClaim(Arc<AtomicUsize>);

impl Drop for SessionPortClaim {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

impl VideoPortArbiter {
	/// Called by session initialization before it binds the stream sockets:
	/// refuse new runs, stop the running one and wait until its socket closed.
	pub(crate) async fn release_for_session(&self) -> SessionPortClaim {
		// Order matters: block first, then preempt. A run that acquired the
		// slot before the block is visible is preempted below; one acquiring it
		// afterward sees the block and releases it (see `Calibration::start`).
		self.starting.fetch_add(1, Ordering::SeqCst);
		let claim = SessionPortClaim(self.starting.clone());
		if self.slot.available_permits() > 0 {
			return claim;
		}
		tracing::info!("A session is starting; stopping the bandwidth calibration run");
		self.preempt.send_modify(|generation| *generation += 1);
		match tokio::time::timeout(PREEMPT_TIMEOUT, self.slot.acquire()).await {
			Ok(Ok(permit)) => drop(permit),
			_ => tracing::warn!("Bandwidth calibration did not release the video port in time"),
		}
		claim
	}

	fn session_starting(&self) -> bool {
		self.starting.load(Ordering::SeqCst) > 0
	}
}

#[derive(Debug, PartialEq, Eq)]
pub struct CalibrationError {
	pub status: u16,
	pub message: String,
}

impl CalibrationError {
	fn new(status: u16, message: impl Into<String>) -> Self {
		Self {
			status,
			message: message.into(),
		}
	}
}

/// Stream settings the calibration mirrors.
#[derive(Clone, Debug)]
pub(crate) struct StreamSettings {
	pub address: String,
	pub video: VideoStreamConfig,
}

impl StreamSettings {
	/// FEC parity the stream may add, as a percentage of data shards. Automatic
	/// FEC can rise to its maximum, so the suggestion budgets for that.
	pub fn fec_percent(&self) -> u32 {
		u32::from(match self.video.fec_mode {
			FecMode::Off => 0,
			FecMode::Fixed => self.video.fec_percentage,
			FecMode::Auto => self.video.fec_max_percentage.max(self.video.fec_min_percentage),
		})
	}
}

struct RunEntry {
	fingerprint: String,
	plan: ProbePlan,
	request: ProbeRequest,
	created: Instant,
	result: watch::Receiver<Option<Arc<HostRun>>>,
	host_link_mbps: u64,
}

#[derive(Default)]
struct Registry {
	runs: HashMap<[u8; RUN_ID_BYTES], RunEntry>,
	starts: HashMap<String, VecDeque<Instant>>,
}

impl Registry {
	fn prune(&mut self) {
		self.runs.retain(|_, entry| {
			entry.created.elapsed() < RESULT_RETENTION + Duration::from_millis(u64::from(plan::MAX_RUN_MS))
		});
		self.starts.retain(|_, times| {
			while times.front().is_some_and(|at| at.elapsed() > RATE_WINDOW) {
				times.pop_front();
			}
			!times.is_empty()
		});
		while self.runs.len() >= MAX_REGISTERED_RUNS {
			let Some(oldest) = self.runs.iter().min_by_key(|(_, e)| e.created).map(|(id, _)| *id) else {
				break;
			};
			self.runs.remove(&oldest);
		}
	}
}

/// What a client needs to receive a run.
#[derive(Clone, Debug)]
pub struct StartReply {
	pub run_id: [u8; RUN_ID_BYTES],
	pub key: [u8; KEY_BYTES],
	pub udp_port: u16,
	pub plan: ProbePlan,
	pub packet_size: usize,
	pub fec_percent: u32,
	/// Datagrams the host's sender submits together (one GSO send). Paced
	/// frames advance chunk by chunk, so the client schedules delays per chunk.
	pub chunk_packets: usize,
}

impl StartReply {
	pub fn to_xml(&self) -> String {
		let mut xml = String::from("<root status_code=\"200\">");
		let _ = write!(
			xml,
			"<Version>{}</Version><RunId>{}</RunId><Key>{}</Key><UdpPort>{}</UdpPort><PacketSize>{}</PacketSize>\
			 <ShardBytes>{}</ShardBytes><WirePacketBytes>{}</WirePacketBytes><Fps>{}</Fps><StepMs>{}</StepMs>\
			 <Shape>{}</Shape><FecPercent>{}</FecPercent><ChunkPackets>{}</ChunkPackets>\
			 <HelloTimeoutMs>{}</HelloTimeoutMs><ConsentTimeoutMs>{}</ConsentTimeoutMs>",
			plan::PROTOCOL_VERSION,
			hex::encode(self.run_id),
			hex::encode(self.key),
			self.udp_port,
			self.packet_size,
			self.plan.shard_bytes,
			self.plan.wire_packet_bytes,
			self.plan.fps,
			self.plan.step_ms,
			self.plan.shape.as_str(),
			self.fec_percent,
			self.chunk_packets,
			sender::HELLO_TIMEOUT.as_millis(),
			sender::CONSENT_TIMEOUT.as_millis(),
		);
		for (index, step) in self.plan.steps.iter().enumerate() {
			let _ = write!(
				xml,
				"<Step><Index>{index}</Index><OfferedKbps>{}</OfferedKbps><PacketsPerFrame>{}</PacketsPerFrame>\
				 <Frames>{}</Frames></Step>",
				step.offered_kbps, step.packets_per_frame, step.frames
			);
		}
		xml.push_str("</root>");
		xml
	}
}

/// The calibration service shared by the HTTPS API and session startup.
#[derive(Clone)]
pub struct Calibration {
	settings: Arc<StreamSettings>,
	arbiter: VideoPortArbiter,
	registry: Arc<Mutex<Registry>>,
}

impl Calibration {
	pub(crate) fn new(settings: StreamSettings, arbiter: VideoPortArbiter) -> Self {
		Self {
			settings: Arc::new(settings),
			arbiter,
			registry: Arc::default(),
		}
	}

	/// Start a run for an authenticated, paired client. The caller has
	/// verified the TLS client certificate (`fingerprint`) and that no session
	/// exists; `peer` is the TLS connection's remote address.
	pub(crate) async fn start(
		&self,
		params: &HashMap<String, String>,
		fingerprint: &str,
		peer: SocketAddr,
		host_link_mbps: u64,
	) -> Result<StartReply, CalibrationError> {
		let video = &self.settings.video;
		// Video shards carry the encryption prefix only when the client asks
		// for it and the host enables it, as in RTSP negotiation.
		let mut params = params.clone();
		if !video.encrypt {
			params.remove("encrypted");
		}
		let request = ProbeRequest::parse(&params, |size, encrypted| video.clamp_packet_size(size, encrypted))
			.map_err(|message| CalibrationError::new(400, message))?;
		{
			let mut registry = self.registry.lock().unwrap();
			registry.prune();
			let starts = registry.starts.entry(fingerprint.to_string()).or_default();
			if starts.len() >= RUNS_PER_WINDOW {
				return Err(CalibrationError::new(429, "Too many calibration runs; wait a minute"));
			}
			starts.push_back(Instant::now());
		}

		// Subscribe before acquiring the slot, so a preemption issued between the
		// acquisition and the run's first wait is still observed.
		let preempt = self.arbiter.preempt.subscribe();
		let permit = self
			.arbiter
			.slot
			.clone()
			.try_acquire_owned()
			.map_err(|_| CalibrationError::new(429, "Another calibration run is in progress"))?;
		if self.arbiter.session_starting() {
			return Err(CalibrationError::new(
				409,
				"End the stream before bandwidth calibration",
			));
		}

		let peer_ip = peer.ip().to_canonical();
		let mut run_id = [0u8; RUN_ID_BYTES];
		let mut key = [0u8; KEY_BYTES];
		aws_lc_rs::rand::fill(&mut run_id).map_err(|_| CalibrationError::new(500, "No randomness available"))?;
		aws_lc_rs::rand::fill(&mut key).map_err(|_| CalibrationError::new(500, "No randomness available"))?;
		let plan = request.plan(peer_ip.is_ipv6());
		let (result_tx, result_rx) = watch::channel(None);
		let (bound_tx, bound_rx) = oneshot::channel();
		let context = sender::RunContext {
			plan: plan.clone(),
			run_id,
			key,
			peer_ip,
			qos: request.qos,
			preempt,
		};
		spawn_run(
			self.settings.address.clone(),
			video.port,
			context,
			permit,
			bound_tx,
			result_tx,
		)
		.map_err(|error| CalibrationError::new(500, format!("Cannot start the calibration worker: {error}")))?;
		let chunk_packets = match bound_rx.await {
			Ok(Ok(chunk_packets)) => chunk_packets,
			Ok(Err(())) => {
				return Err(CalibrationError::new(
					503,
					"The video port is in use; end other streams and try again",
				));
			},
			Err(_) => return Err(CalibrationError::new(500, "The calibration worker failed to start")),
		};

		tracing::info!(
			client = %peer_ip,
			max_kbps = request.max_kbps,
			steps = request.steps,
			step_ms = request.step_ms,
			fps = request.fps,
			packet_size = request.packet_size,
			shape = request.shape.as_str(),
			"Bandwidth calibration run started"
		);
		let reply = StartReply {
			run_id,
			key,
			udp_port: video.port,
			plan: plan.clone(),
			packet_size: request.packet_size,
			fec_percent: self.settings.fec_percent(),
			chunk_packets,
		};
		self.registry.lock().unwrap().runs.insert(
			run_id,
			RunEntry {
				fingerprint: fingerprint.to_string(),
				plan,
				request,
				created: Instant::now(),
				result: result_rx,
				host_link_mbps,
			},
		);
		Ok(reply)
	}

	/// Wait for a run of this client to end, merge the client's statistics and
	/// return the measurements and suggestion as XML.
	pub(crate) async fn result(
		&self,
		params: &HashMap<String, String>,
		fingerprint: &str,
	) -> Result<String, CalibrationError> {
		let run_id: [u8; RUN_ID_BYTES] = params
			.get("run")
			.and_then(|value| hex::decode(value).ok())
			.and_then(|bytes| bytes.try_into().ok())
			.ok_or_else(|| CalibrationError::new(400, "run is required"))?;
		let (mut receiver, plan, request, host_link_mbps) = {
			let registry = self.registry.lock().unwrap();
			// Another client's run is indistinguishable from an unknown one.
			let entry = registry
				.runs
				.get(&run_id)
				.filter(|entry| entry.fingerprint == fingerprint)
				.ok_or_else(|| CalibrationError::new(404, "Unknown calibration run"))?;
			(
				entry.result.clone(),
				entry.plan.clone(),
				entry.request.clone(),
				entry.host_link_mbps,
			)
		};
		let client = parse_client_steps(params, &plan).map_err(|message| CalibrationError::new(400, message))?;
		let client_link_mbps = params
			.get("clientlinkmbps")
			.and_then(|value| value.parse::<u64>().ok())
			.filter(|speed| *speed <= 1_000_000)
			.unwrap_or(0);
		let wait = plan.duration() + sender::HELLO_TIMEOUT + Duration::from_secs(5);
		let host = tokio::time::timeout(wait, receiver.wait_for(Option::is_some))
			.await
			.map_err(|_| CalibrationError::new(504, "The calibration run did not finish"))?
			.map_err(|_| CalibrationError::new(500, "The calibration worker ended unexpectedly"))?
			.clone()
			.expect("waited for a result");
		let report = Report::new(
			&plan,
			&request,
			&host,
			&client,
			self.settings.fec_percent(),
			host_link_mbps,
			client_link_mbps,
		);
		tracing::info!(
			outcome = host.outcome.as_str(),
			usable_wire_kbps = report.recommendation.usable_wire_kbps,
			recommended_kbps = report.recommendation.recommended_kbps,
			confidence = report.recommendation.confidence.as_str(),
			verdicts = ?report.recommendation.verdicts.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
			"Bandwidth calibration finished"
		);
		Ok(report.to_xml())
	}
}

/// Run a probe on its own thread and runtime: a multi-gigabit burst must not
/// occupy the HTTPS runtime's workers, and its sockets belong to that runtime.
fn spawn_run(
	address: String,
	port: u16,
	context: sender::RunContext,
	permit: OwnedSemaphorePermit,
	bound: oneshot::Sender<Result<usize, ()>>,
	result: watch::Sender<Option<Arc<HostRun>>>,
) -> std::io::Result<()> {
	std::thread::Builder::new()
		.name("udp-calibration".into())
		.spawn(move || {
			let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
				Ok(runtime) => runtime,
				Err(error) => {
					tracing::warn!(%error, "Cannot create the calibration runtime");
					let _ = bound.send(Err(()));
					return;
				},
			};
			runtime.block_on(async move {
				let socket = match UdpGsoSocket::new(&address, port).await {
					Ok(socket) => socket,
					Err(()) => {
						let _ = bound.send(Err(()));
						return;
					},
				};
				let _ = bound.send(Ok(socket.segments_per_send(context.plan.shard_bytes)));
				let run = sender::run(socket, context).await;
				// `run` consumed and dropped the socket; only now may a session bind the port.
				drop(permit);
				tracing::debug!(outcome = run.outcome.as_str(), elapsed = ?run.elapsed, "Calibration run ended");
				let _ = result.send(Some(Arc::new(run)));
			});
		})?;
	Ok(())
}

/// `s<i>=` holds the [`ClientStep`] fields of step `i`, comma separated.
fn parse_client_steps(params: &HashMap<String, String>, plan: &ProbePlan) -> Result<Vec<Option<ClientStep>>, String> {
	plan.steps
		.iter()
		.enumerate()
		.map(|(index, step)| {
			let Some(value) = params.get(&format!("s{index}")) else {
				return Ok(None);
			};
			let fields: Vec<u64> = value
				.split(',')
				.map(|field| {
					(field.len() <= 12 && !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()))
						.then(|| field.parse().ok())
						.flatten()
				})
				.collect::<Option<_>>()
				.ok_or_else(|| format!("s{index} must hold decimal numbers"))?;
			let fields: [u64; ClientStep::FIELDS] = fields
				.try_into()
				.map_err(|_| format!("s{index} must hold {} fields", ClientStep::FIELDS))?;
			let client = ClientStep::from_fields(fields);
			if client.received > step.packets() || client.frames_complete > u64::from(step.frames) {
				return Err(format!("s{index} reports more than the step contained"));
			}
			Ok(Some(client))
		})
		.collect()
}

/// Final measurements of a run.
pub struct Report {
	pub outcome: Outcome,
	pub steps: Vec<(plan::StepPlan, HostStep, recommend::StepEvidence)>,
	pub recommendation: Recommendation,
	pub overhead: recommend::StreamOverhead,
	pub host_link_mbps: u64,
	pub client_link_mbps: u64,
}

impl Report {
	pub fn new(
		plan: &ProbePlan,
		request: &ProbeRequest,
		host: &HostRun,
		client: &[Option<ClientStep>],
		fec_percent: u32,
		host_link_mbps: u64,
		client_link_mbps: u64,
	) -> Self {
		let steps: Vec<_> = plan
			.steps
			.iter()
			.enumerate()
			.map(|(index, step)| {
				let sent = host.steps[index];
				let evidence = recommend::StepEvidence {
					offered_kbps: step.offered_kbps,
					planned_packets: step.packets(),
					planned_frames: step.frames,
					sent_frames: sent.sent_frames,
					submitted: sent.submitted,
					failed: sent.failed,
					late_frames: sent.late_frames,
					client: client.get(index).copied().flatten(),
					reported_received: (host.reports > 0).then(|| u64::from(host.reported[index])),
				};
				(*step, sent, evidence)
			})
			.collect();
		let overhead = recommend::StreamOverhead {
			packet_size: request.packet_size,
			wire_packet_bytes: plan.wire_packet_bytes,
			fec_percent,
			frame_interval_us: plan.frame_interval().as_micros() as u64,
		};
		let evidence: Vec<_> = steps.iter().map(|(_, _, evidence)| *evidence).collect();
		let recommendation = recommend::recommend(&evidence, overhead, host_link_mbps, client_link_mbps);
		Self {
			outcome: host.outcome.clone(),
			steps,
			recommendation,
			overhead,
			host_link_mbps,
			client_link_mbps,
		}
	}

	pub fn to_xml(&self) -> String {
		let r = &self.recommendation;
		let mut xml = String::from("<root status_code=\"200\">");
		let _ = write!(
			xml,
			"<Outcome>{}</Outcome><RecommendedKbps>{}</RecommendedKbps><UsableWireKbps>{}</UsableWireKbps>\
			 <Confidence>{}</Confidence><CeilingReached>{}</CeilingReached><Stable>{}</Stable>\
			 <LinkLimited>{}</LinkLimited><MarginPercent>{}</MarginPercent><FecPercent>{}</FecPercent>\
			 <OverheadPpm>{}</OverheadPpm><HostLinkMbps>{}</HostLinkMbps><Reason>{}</Reason>",
			self.outcome.as_str(),
			r.recommended_kbps,
			r.usable_wire_kbps,
			r.confidence.as_str(),
			u8::from(r.ceiling_reached),
			u8::from(r.stable),
			u8::from(r.link_limited),
			recommend::MARGIN_PERCENT,
			self.overhead.fec_percent,
			self.overhead.overhead_ppm(),
			self.host_link_mbps,
			crate::webserver::escape_xml(&r.reason),
		);
		if let Outcome::SendFailed(error) = &self.outcome {
			let _ = write!(xml, "<SendError>{}</SendError>", crate::webserver::escape_xml(error));
		}
		for (index, ((plan, host, evidence), verdict)) in self.steps.iter().zip(&r.verdicts).enumerate() {
			let client = evidence.client.unwrap_or_default();
			let _ = write!(
				xml,
				"<Step><Index>{index}</Index><Verdict>{}</Verdict><OfferedKbps>{}</OfferedKbps>\
				 <Planned>{}</Planned><SentFrames>{}</SentFrames><Submitted>{}</Submitted><SendFailed>{}</SendFailed>\
				 <WouldBlock>{}</WouldBlock><LateFrames>{}</LateFrames><MaxStartLatenessUs>{}</MaxStartLatenessUs>\
				 <Received>{}</Received><NetworkLossPpm>{}</NetworkLossPpm><GapEvents>{}</GapEvents>\
				 <MaxGap>{}</MaxGap><FramesUnrecoverable>{}</FramesUnrecoverable><QueueP50Us>{}</QueueP50Us>\
				 <QueueP95Us>{}</QueueP95Us><QueueMaxUs>{}</QueueMaxUs><FrameJitterUs>{}</FrameJitterUs></Step>",
				verdict.as_str(),
				plan.offered_kbps,
				plan.packets(),
				host.sent_frames,
				host.submitted,
				host.failed,
				host.would_block,
				host.late_frames,
				host.max_start_lateness_us,
				evidence
					.client
					.map(|c| c.received)
					.or(evidence.reported_received)
					.map_or_else(String::new, |r| r.to_string()),
				evidence.network_loss_ppm().map_or_else(String::new, |l| l.to_string()),
				client.gap_events,
				client.max_gap,
				client.frames_unrecoverable,
				client.queue_p50_us,
				client.queue_p95_us,
				client.queue_max_us,
				client.frame_jitter_us,
			);
		}
		xml.push_str("</root>");
		xml
	}
}

/// Physical link speed at the host end of the route to `peer`, 0 when unknown.
pub(crate) fn host_link_mbps(local: Option<SocketAddr>, peer: IpAddr) -> u64 {
	local
		.map(|local| crate::webserver::routed_link_mbps(local.ip(), peer))
		.unwrap_or(0)
}
