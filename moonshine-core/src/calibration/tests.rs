//! Loopback runs of the complete probe: the host service with its production
//! sender, the reference receiver, and (for the recommendation) a simulated
//! bottleneck between them.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::watch;

use super::client::{self, ClientPolicy, ClientStop};
use super::*;

fn free_udp_port() -> u16 {
	std::net::UdpSocket::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port()
}

fn service(fec: (FecMode, u8)) -> Calibration {
	let video = VideoStreamConfig {
		port: free_udp_port(),
		fec_mode: fec.0,
		fec_percentage: fec.1,
		..Default::default()
	};
	Calibration::new(
		StreamSettings {
			address: "127.0.0.1".into(),
			video,
		},
		VideoPortArbiter::default(),
	)
}

fn params(pairs: &[(&str, String)]) -> HashMap<String, String> {
	pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn request(max_kbps: u32, steps: u8, step_ms: u32, fps: u32, shape: &str) -> HashMap<String, String> {
	params(&[
		("version", "1".into()),
		("maxkbps", max_kbps.to_string()),
		("steps", steps.to_string()),
		("stepms", step_ms.to_string()),
		("fps", fps.to_string()),
		("shape", shape.into()),
	])
}

fn query_params(query: &str) -> HashMap<String, String> {
	url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

async fn client_socket(ip: &str) -> UdpSocket {
	UdpSocket::bind((ip, 0)).await.unwrap()
}

/// Start, receive and fetch the result from the same loopback peer.
async fn complete_run(service: &Calibration, request: HashMap<String, String>) -> (client::ClientRun, String) {
	let socket = client_socket("127.0.0.1").await;
	let peer = socket.local_addr().unwrap();
	let reply = service.start(&request, "client", peer, 0).await.unwrap();
	let parsed = StartReply::from_xml(&reply.to_xml()).unwrap();
	for (parsed, sent) in parsed.plan.steps.iter().zip(&reply.plan.steps) {
		assert_eq!(
			(parsed.offered_kbps, parsed.packets_per_frame, parsed.frames),
			(sent.offered_kbps, sent.packets_per_frame, sent.frames)
		);
	}
	assert_eq!(
		(parsed.chunk_packets, parsed.plan.shard_bytes),
		(reply.chunk_packets, reply.plan.shard_bytes)
	);
	let host = SocketAddr::from(([127, 0, 0, 1], parsed.udp_port));
	let (_cancel_tx, cancel) = watch::channel(false);
	let run = client::receive(&socket, host, &parsed, ClientPolicy::default(), cancel)
		.await
		.unwrap();
	let xml = service
		.result(&query_params(&client::result_query(&parsed, &run, 0)), "client")
		.await
		.unwrap();
	(run, xml)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_run_delivers_every_planned_datagram_and_suggests_a_bitrate() {
	let service = service((FecMode::Fixed, 20));
	let (run, xml) = complete_run(&service, request(40_000, 4, 250, 60, "burst")).await;
	assert_eq!(run.stop, ClientStop::Completed);
	let plan = request(40_000, 4, 250, 60, "burst");
	let plan = ProbeRequest::parse(&plan, |s, _| s).unwrap().plan(false);
	for (step, summary) in plan.steps.iter().zip(&run.steps) {
		let summary = summary.unwrap();
		assert_eq!(summary.received, step.packets(), "{xml}");
		assert_eq!(summary.duplicates, 0);
		assert_eq!(summary.frames_complete, u64::from(step.frames));
		assert_eq!(summary.gap_events, 0);
	}
	assert_eq!(client::xml_value(&xml, "Outcome"), Some("completed"), "{xml}");
	assert_eq!(xml.matches("<Verdict>clean</Verdict>").count(), 4, "{xml}");
	// Every step clean: a lower bound, never more than the top rate allows.
	assert_eq!(client::xml_value(&xml, "CeilingReached"), Some("0"));
	assert_eq!(client::xml_value(&xml, "Confidence"), Some("medium"));
	let usable: u64 = client::xml_value(&xml, "UsableWireKbps").unwrap().parse().unwrap();
	assert_eq!(usable, u64::from(plan.steps[3].offered_kbps));
	let suggested: u64 = client::xml_value(&xml, "RecommendedKbps").unwrap().parse().unwrap();
	assert!(suggested > 0 && suggested < usable * 80 / 100 * 100 / 120, "{xml}");
	assert_eq!(client::xml_value(&xml, "FecPercent"), Some("20"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paced_run_spreads_frames_across_their_interval() {
	let service = service((FecMode::Off, 0));
	let (run, xml) = complete_run(&service, request(30_000, 2, 500, 30, "paced")).await;
	assert_eq!(run.stop, ClientStop::Completed, "{xml}");
	assert_eq!(xml.matches("<Verdict>clean</Verdict>").count(), 2, "{xml}");
	// Paced frames are spread, so their delivery delay against each packet's
	// own schedule stays small on loopback.
	let summary = run.steps[1].unwrap();
	assert!(summary.queue_p95_us < 5_000, "{summary:?}");
	assert_eq!(client::xml_value(&xml, "FecPercent"), Some("0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_or_foreign_hellos_never_receive_traffic() {
	let service = service((FecMode::Fixed, 20));
	// A forged key from the right address.
	let socket = client_socket("127.0.0.1").await;
	let reply = service
		.start(
			&request(20_000, 1, 250, 60, "burst"),
			"client",
			socket.local_addr().unwrap(),
			0,
		)
		.await
		.unwrap();
	let host = SocketAddr::from(([127, 0, 0, 1], reply.udp_port));
	let forged = wire::Control::new(reply.run_id, wire::ControlKind::Hello, 1).encode(&[0u8; KEY_BYTES]);
	// The right key, but from another address than the TLS peer.
	let stranger = client_socket("127.0.0.2").await;
	let foreign = wire::Control::new(reply.run_id, wire::ControlKind::Hello, 1).encode(&reply.key);
	let deadline = Instant::now() + sender::HELLO_TIMEOUT + Duration::from_millis(300);
	let mut buffer = [0u8; 2_048];
	while Instant::now() < deadline {
		socket.send_to(&forged, host).await.unwrap();
		stranger.send_to(&foreign, host).await.unwrap();
		for receiver in [&socket, &stranger] {
			if let Ok(Ok((length, _))) =
				tokio::time::timeout(Duration::from_millis(50), receiver.recv_from(&mut buffer)).await
			{
				panic!("received {length} bytes without authenticating");
			}
		}
	}
	let xml = service
		.result(&params(&[("run", hex::encode(reply.run_id))]), "client")
		.await
		.unwrap();
	assert_eq!(client::xml_value(&xml, "Outcome"), Some("handshake_timeout"));
	assert_eq!(client::xml_value(&xml, "RecommendedKbps"), Some("0"));
	// The slot is free again.
	assert!(service.arbiter.slot.available_permits() == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sending_stops_when_the_receiver_stops_consenting() {
	let service = service((FecMode::Fixed, 20));
	let socket = client_socket("127.0.0.1").await;
	let reply = service
		.start(
			&request(20_000, 4, 2_000, 60, "burst"),
			"client",
			socket.local_addr().unwrap(),
			0,
		)
		.await
		.unwrap();
	let host = SocketAddr::from(([127, 0, 0, 1], reply.udp_port));
	// One hello, then silence.
	let hello = wire::Control::new(reply.run_id, wire::ControlKind::Hello, 1).encode(&reply.key);
	socket.send_to(&hello, host).await.unwrap();
	let started = Instant::now();
	let mut buffer = [0u8; 2_048];
	let mut last = started;
	while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buffer)).await {
		last = Instant::now();
	}
	// Traffic ends within the consent timeout (plus scheduling slack), far
	// before the planned 8 s.
	let streamed = last.duration_since(started);
	assert!(
		streamed < sender::CONSENT_TIMEOUT + Duration::from_millis(300),
		"{streamed:?}"
	);
	let xml = service
		.result(&params(&[("run", hex::encode(reply.run_id))]), "client")
		.await
		.unwrap();
	assert_eq!(client::xml_value(&xml, "Outcome"), Some("receiver_silent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_cancellation_stops_the_run() {
	let service = service((FecMode::Fixed, 20));
	let socket = client_socket("127.0.0.1").await;
	let reply = service
		.start(
			&request(20_000, 4, 2_000, 60, "burst"),
			"client",
			socket.local_addr().unwrap(),
			0,
		)
		.await
		.unwrap();
	let host = SocketAddr::from(([127, 0, 0, 1], reply.udp_port));
	let (cancel_tx, cancel) = watch::channel(false);
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(400)).await;
		let _ = cancel_tx.send(true);
	});
	let started = Instant::now();
	let run = client::receive(&socket, host, &reply, ClientPolicy::default(), cancel)
		.await
		.unwrap();
	assert_eq!(run.stop, ClientStop::Cancelled);
	let xml = service
		.result(&query_params(&client::result_query(&reply, &run, 0)), "client")
		.await
		.unwrap();
	assert!(started.elapsed() < Duration::from_secs(2));
	assert_eq!(client::xml_value(&xml, "Outcome"), Some("stopped_by_client"));
	// The step in progress is incomplete, never clean.
	assert!(!xml.contains("<Verdict>clean</Verdict>"), "{xml}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_start_preempts_the_run_and_gets_the_video_port() {
	let service = service((FecMode::Fixed, 20));
	let socket = client_socket("127.0.0.1").await;
	let reply = service
		.start(
			&request(20_000, 4, 2_000, 60, "burst"),
			"client",
			socket.local_addr().unwrap(),
			0,
		)
		.await
		.unwrap();
	let host = SocketAddr::from(([127, 0, 0, 1], reply.udp_port));
	let (_cancel_tx, cancel) = watch::channel(false);
	let receiving = tokio::spawn({
		let reply = reply.clone();
		async move { client::receive(&socket, host, &reply, ClientPolicy::default(), cancel).await }
	});
	tokio::time::sleep(Duration::from_millis(300)).await;

	// Admission: one run at a time, none while a session starts.
	let other = SocketAddr::from(([127, 0, 0, 1], 9));
	let busy = service
		.start(&request(20_000, 1, 250, 60, "burst"), "other", other, 0)
		.await;
	assert_eq!(busy.unwrap_err().status, 429);

	let preempted = Instant::now();
	let claim = service.arbiter.release_for_session().await;
	assert!(preempted.elapsed() < PREEMPT_TIMEOUT);
	// The session can bind the video port immediately.
	std::net::UdpSocket::bind(("127.0.0.1", reply.udp_port)).unwrap();
	let refused = service
		.start(&request(20_000, 1, 250, 60, "burst"), "other", other, 0)
		.await;
	assert_eq!(refused.unwrap_err().status, 409);
	assert_eq!(
		service.arbiter.slot.available_permits(),
		1,
		"a refused start keeps no slot"
	);
	drop(claim);

	let run = receiving.await.unwrap().unwrap();
	assert_eq!(run.stop, ClientStop::Silent);
	let xml = service
		.result(&query_params(&client::result_query(&reply, &run, 0)), "client")
		.await
		.unwrap();
	assert_eq!(client::xml_value(&xml, "Outcome"), Some("preempted"));
}

#[tokio::test]
async fn results_belong_to_the_client_that_started_the_run() {
	let service = service((FecMode::Fixed, 20));
	let peer = SocketAddr::from(([127, 0, 0, 1], 9));
	let reply = service
		.start(&request(20_000, 1, 250, 60, "burst"), "owner", peer, 0)
		.await
		.unwrap();
	let run = params(&[("run", hex::encode(reply.run_id))]);
	assert_eq!(service.result(&run, "intruder").await.unwrap_err().status, 404);
	let malformed = params(&[("run", hex::encode(reply.run_id)), ("s0", "1,2,3".into())]);
	assert_eq!(service.result(&malformed, "owner").await.unwrap_err().status, 400);
	let impossible = params(&[
		("run", hex::encode(reply.run_id)),
		("s0", format!("{},0,0,0,0,0,0,0,0,0,0", u64::MAX / 2)),
	]);
	assert_eq!(service.result(&impossible, "owner").await.unwrap_err().status, 400);
	assert_eq!(service.result(&params(&[]), "owner").await.unwrap_err().status, 400);
}

#[test]
fn runs_per_client_are_rate_limited() {
	let runtime = tokio::runtime::Runtime::new().unwrap();
	let service = service((FecMode::Fixed, 20));
	{
		let mut registry = service.registry.lock().unwrap();
		registry
			.starts
			.insert("busy".into(), (0..RUNS_PER_WINDOW).map(|_| Instant::now()).collect());
	}
	let peer = SocketAddr::from(([127, 0, 0, 1], 9));
	let error = runtime
		.block_on(service.start(&request(20_000, 1, 250, 60, "burst"), "busy", peer, 0))
		.unwrap_err();
	assert_eq!(error.status, 429);
	assert!(error.message.contains("minute"));
	// Validation errors are reported before admission.
	let error = runtime
		.block_on(service.start(&request(0, 1, 250, 60, "burst"), "fresh", peer, 0))
		.unwrap_err();
	assert_eq!(error.status, 400);
}

/// A drop-tail bottleneck relaying host → client datagrams at `rate_kbps`
/// (IP-level) with `queue_bytes` of buffering; client → host datagrams pass
/// unshaped. The host only ever sees the relay's address.
struct Bottleneck {
	address: SocketAddr,
	dropped: Arc<AtomicU64>,
}

async fn bottleneck(host: SocketAddr, client: SocketAddr, rate_kbps: u64, queue_bytes: usize) -> Bottleneck {
	let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
	let address = relay.local_addr().unwrap();
	let dropped = Arc::new(AtomicU64::new(0));
	let (queue_tx, mut queue_rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
	let queued = Arc::new(AtomicU64::new(0));
	tokio::spawn({
		let relay = relay.clone();
		let dropped = dropped.clone();
		let queued = queued.clone();
		async move {
			let mut buffer = vec![0u8; 65_536];
			while let Ok((length, from)) = relay.recv_from(&mut buffer).await {
				if from == client {
					let _ = relay.send_to(&buffer[..length], host).await;
				} else if from == host {
					let wire = (length + 28) as u64;
					if queued.load(Ordering::SeqCst) + wire > queue_bytes as u64 {
						dropped.fetch_add(1, Ordering::SeqCst);
					} else {
						queued.fetch_add(wire, Ordering::SeqCst);
						let _ = queue_tx.send((Instant::now(), buffer[..length].to_vec()));
					}
				}
			}
		}
	});
	tokio::spawn({
		let relay = relay.clone();
		async move {
			// Serialize at the bottleneck rate: a link never banks idle time.
			let mut free_at = Instant::now();
			while let Some((arrived, datagram)) = queue_rx.recv().await {
				let wire = (datagram.len() + 28) as u64;
				// Tokio sleeps in whole milliseconds: release everything whose
				// transmission starts within the next millisecond. A late wakeup
				// delays delivery but never wastes link capacity.
				let starts = free_at.max(arrived);
				free_at = starts + Duration::from_nanos(wire * 8 * 1_000_000 / rate_kbps);
				if starts > Instant::now() + Duration::from_millis(1) {
					tokio::time::sleep_until(starts.into()).await;
				}
				let _ = relay.send_to(&datagram, client).await;
				queued.fetch_sub(wire, Ordering::SeqCst);
			}
		}
	});
	Bottleneck { address, dropped }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_simulated_bottleneck_bounds_the_suggestion() {
	// 60 Mbit/s bottleneck with 256 kB of buffering (a small switch port);
	// ladder to 120 Mbit/s.
	let service = service((FecMode::Fixed, 20));
	let socket = client_socket("127.0.0.1").await;
	let client = socket.local_addr().unwrap();
	let host = SocketAddr::from(([127, 0, 0, 1], service.settings.video.port));
	let link = bottleneck(host, client, 60_000, 256 * 1024).await;
	// The TLS peer is the relay's IP (both loopback); its port is unused.
	let reply = service
		.start(&request(120_000, 6, 500, 60, "paced"), "client", link.address, 0)
		.await
		.unwrap();
	let (_cancel_tx, cancel) = watch::channel(false);
	let run = client::receive(&socket, link.address, &reply, ClientPolicy::default(), cancel)
		.await
		.unwrap();
	let xml = service
		.result(&query_params(&client::result_query(&reply, &run, 0)), "client")
		.await
		.unwrap();
	assert!(
		link.dropped.load(Ordering::SeqCst) > 0,
		"the bottleneck never overflowed: {xml}"
	);
	let usable: u64 = client::xml_value(&xml, "UsableWireKbps").unwrap().parse().unwrap();
	// Steps are 20/40/60/80/100/120 Mbit/s: 40 is clean; 60 is at capacity and
	// may pass or queue; 80 and above overflow.
	assert!((40_000..=62_000).contains(&usable), "{xml}");
	assert_eq!(client::xml_value(&xml, "CeilingReached"), Some("1"), "{xml}");
	let suggested: u64 = client::xml_value(&xml, "RecommendedKbps").unwrap().parse().unwrap();
	assert!(suggested <= 60_000 * 80 / 100 * 100 / 120, "{xml}");
	assert!(suggested >= 20_000, "{xml}");
	// The ladder stopped at saturation rather than sending every step.
	assert!(
		matches!(
			client::xml_value(&xml, "Outcome"),
			Some("saturated" | "stopped_by_client")
		),
		"{xml}"
	);
}
