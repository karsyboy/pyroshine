//! Run Pyroshine's UDP bandwidth calibration from a terminal, as a paired client.
//!
//! `identity` creates a client certificate and prints the fingerprint to add
//! to a (test) server's paired clients; `run` performs calibration runs with
//! the same protocol and receiver Pyrolight uses and prints the measurements.
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use moonshine_core::calibration::StartReply;
use moonshine_core::calibration::client::{self, ClientPolicy, xml_value};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(about = "Pyroshine UDP bandwidth calibration client")]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Create a client certificate in DIR and print its pairing fingerprint.
	Identity { dir: PathBuf },
	/// Calibrate against a host this identity is paired with.
	Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
	/// Host name or address.
	#[arg(long)]
	host: String,
	#[arg(long, default_value_t = 47984)]
	https_port: u16,
	/// Directory holding client.pem and client.key.
	#[arg(long)]
	identity: PathBuf,
	/// Pin the host certificate (PEM). Without it any host certificate is accepted.
	#[arg(long)]
	server_cert: Option<PathBuf>,
	#[arg(long, default_value_t = 200_000)]
	max_kbps: u32,
	#[arg(long, default_value_t = 5)]
	steps: u8,
	#[arg(long, default_value_t = 1_000)]
	step_ms: u32,
	#[arg(long, default_value_t = 60)]
	fps: u32,
	/// burst (H.264/HEVC/AV1) or paced (PyroWave).
	#[arg(long, default_value = "burst")]
	shape: String,
	#[arg(long, default_value_t = 1_392)]
	packet_size: u32,
	#[arg(long)]
	encrypted: bool,
	/// Do not stop the ladder after a saturated step.
	#[arg(long)]
	no_early_stop: bool,
	/// Repeat the run this many times (a pause of one second between runs).
	#[arg(long, default_value_t = 1)]
	repeat: u32,
	/// Print each result as one JSON object per line.
	#[arg(long)]
	json: bool,
	/// Known physical link speed at this end, in Mbit/s.
	#[arg(long, default_value_t = 0)]
	client_link_mbps: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	match Cli::parse().command {
		Command::Identity { dir } => identity(&dir),
		Command::Run(args) => {
			for attempt in 0..args.repeat {
				if attempt > 0 {
					tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				}
				run(&args).await?;
			}
			Ok(())
		},
	}
}

fn identity(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
	std::fs::create_dir_all(dir)?;
	// ECDSA P-256: the host accepts it, and rcgen cannot generate RSA keys here.
	let key = rcgen::KeyPair::generate()?;
	let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
	params
		.distinguished_name
		.push(rcgen::DnType::CommonName, "moonshine-udp-probe");
	let cert = params.self_signed(&key)?;
	std::fs::write(dir.join("client.pem"), cert.pem())?;
	std::fs::write(dir.join("client.key"), key.serialize_pem())?;
	println!("{}", hex::encode(Sha256::digest(cert.der())));
	Ok(())
}

/// Accepts the pinned host certificate, or any when none is pinned (as a
/// Moonlight client does before pairing). Signatures are still verified.
#[derive(Debug)]
struct PinnedHost {
	pin: Option<Vec<u8>>,
	provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedHost {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		_: &[CertificateDer<'_>],
		_: &ServerName<'_>,
		_: &[u8],
		_: rustls::pki_types::UnixTime,
	) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
		match &self.pin {
			Some(pin) if pin.as_slice() != end_entity.as_ref() => Err(rustls::Error::General(
				"host certificate does not match --server-cert".into(),
			)),
			_ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
		}
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &rustls::DigitallySignedStruct,
	) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &rustls::DigitallySignedStruct,
	) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
	}

	fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
		self.provider.signature_verification_algorithms.supported_schemes()
	}
}

struct Https {
	address: SocketAddr,
	host: String,
	config: Arc<rustls::ClientConfig>,
}

impl Https {
	fn new(args: &RunArgs) -> Result<Self, Box<dyn std::error::Error>> {
		let certs: Vec<CertificateDer<'static>> =
			rustls_pemfile::certs(&mut std::fs::read(args.identity.join("client.pem"))?.as_slice())
				.collect::<Result<_, _>>()?;
		let key: PrivateKeyDer<'static> =
			rustls_pemfile::private_key(&mut std::fs::read(args.identity.join("client.key"))?.as_slice())?
				.ok_or("client.key holds no private key")?;
		let pin = match &args.server_cert {
			Some(path) => Some(
				rustls_pemfile::certs(&mut std::fs::read(path)?.as_slice())
					.next()
					.ok_or("--server-cert holds no certificate")??
					.to_vec(),
			),
			None => None,
		};
		let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
		let config = rustls::ClientConfig::builder_with_provider(provider.clone())
			.with_safe_default_protocol_versions()?
			.dangerous()
			.with_custom_certificate_verifier(Arc::new(PinnedHost { pin, provider }))
			.with_client_auth_cert(certs, key)?;
		let address = (args.host.as_str(), args.https_port)
			.to_socket_addrs()?
			.next()
			.ok_or("host did not resolve")?;
		Ok(Self {
			address,
			host: args.host.clone(),
			config: Arc::new(config),
		})
	}

	async fn get(&self, path: &str) -> Result<String, Box<dyn std::error::Error>> {
		let socket = tokio::net::TcpStream::connect(self.address).await?;
		let name = ServerName::try_from(self.host.clone()).unwrap_or(ServerName::try_from("localhost")?);
		let mut stream = tokio_rustls::TlsConnector::from(self.config.clone())
			.connect(name, socket)
			.await?;
		stream
			.write_all(
				format!(
					"GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
					self.host
				)
				.as_bytes(),
			)
			.await?;
		let mut response = Vec::new();
		// A server closing without close_notify after the body is fine here.
		let _ = stream.read_to_end(&mut response).await;
		let response = String::from_utf8_lossy(&response).into_owned();
		let body = response
			.split_once("\r\n\r\n")
			.map(|(_, body)| body.to_string())
			.ok_or("malformed HTTP response")?;
		if !response.starts_with("HTTP/1.1 200") {
			return Err(response.lines().next().unwrap_or_default().to_string().into());
		}
		if !body.contains("status_code=\"200\"") {
			let status = body
				.split("status_message=\"")
				.nth(1)
				.and_then(|rest| rest.split('"').next())
				.unwrap_or(&body);
			return Err(status.to_string().into());
		}
		Ok(body)
	}
}

async fn run(args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
	let https = Https::new(args)?;
	let start = https
		.get(&format!(
			"/pyroshine-udp-probe?version=1&maxkbps={}&steps={}&stepms={}&fps={}&shape={}&packetsize={}&encrypted={}",
			args.max_kbps,
			args.steps,
			args.step_ms,
			args.fps,
			args.shape,
			args.packet_size,
			u8::from(args.encrypted)
		))
		.await?;
	let reply = StartReply::from_xml(&start)?;
	let host = SocketAddr::new(https.address.ip(), reply.udp_port);
	let bind: SocketAddr = if host.is_ipv4() {
		"0.0.0.0:0".parse()?
	} else {
		"[::]:0".parse()?
	};
	// Size the receive buffer as the stream's video socket would be.
	let requested = client::stream_receive_buffer_bytes(reply.packet_size, reply.plan.shape);
	let raw = socket2::Socket::new(
		socket2::Domain::for_address(bind),
		socket2::Type::DGRAM,
		Some(socket2::Protocol::UDP),
	)?;
	let _ = raw.set_recv_buffer_size(requested);
	// Linux reports twice the effective size (kernel bookkeeping).
	let effective = raw.recv_buffer_size().unwrap_or(0) / if cfg!(target_os = "linux") { 2 } else { 1 };
	raw.bind(&bind.into())?;
	raw.set_nonblocking(true)?;
	let socket = tokio::net::UdpSocket::from_std(raw.into())?;
	if effective < requested {
		eprintln!(
			"warning: receive buffer {effective} B is below the stream's {requested} B (raise net.core.rmem_max); \
			 loss may be local"
		);
	}
	let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
	tokio::spawn(async move {
		if tokio::signal::ctrl_c().await.is_ok() {
			let _ = cancel_tx.send(true);
		}
	});
	let policy = ClientPolicy {
		stop_loss_percent: (!args.no_early_stop).then_some(10),
	};
	let started = std::time::Instant::now();
	let run = client::receive(&socket, host, &reply, policy, cancel).await?;
	let result = https
		.get(&format!(
			"/pyroshine-udp-probe-result?{}",
			client::result_query(&reply, &run, args.client_link_mbps)
		))
		.await?;
	if args.json {
		print_json(args, &reply, &run, &result, started.elapsed(), effective);
	} else {
		print_table(&reply, &run, &result);
		println!("receive buffer: {effective} B (stream requests {requested} B)");
	}
	Ok(())
}

fn steps_of(result: &str) -> Vec<&str> {
	result.split("<Step>").skip(1).collect()
}

fn print_table(reply: &StartReply, run: &client::ClientRun, result: &str) {
	println!(
		"shape={} fps={} shard={} B chunk={} fec={}% client stop={:?}",
		reply.plan.shape.as_str(),
		reply.plan.fps,
		reply.plan.shard_bytes,
		reply.chunk_packets,
		reply.fec_percent,
		run.stop
	);
	println!(
		"{:>4} {:>10} {:>9} {:>9} {:>8} {:>9} {:>6} {:>7} {:>9} {:>9} {:>7}  verdict",
		"step", "offered", "planned", "submit", "failed", "received", "loss%", "gaps", "q50 ms", "q95 ms", "unrec"
	);
	for step in steps_of(result) {
		let value = |tag| xml_value(step, tag).unwrap_or("-");
		let loss = value("NetworkLossPpm")
			.parse::<f64>()
			.map(|ppm| format!("{:.2}", ppm / 10_000.0))
			.unwrap_or_else(|_| "-".into());
		let ms = |tag| {
			value(tag)
				.parse::<f64>()
				.map(|us| format!("{:.2}", us / 1_000.0))
				.unwrap_or_else(|_| "-".into())
		};
		println!(
			"{:>4} {:>10} {:>9} {:>9} {:>8} {:>9} {:>6} {:>7} {:>9} {:>9} {:>7}  {}",
			value("Index"),
			value("OfferedKbps"),
			value("Planned"),
			value("Submitted"),
			value("SendFailed"),
			value("Received"),
			loss,
			value("GapEvents"),
			ms("QueueP50Us"),
			ms("QueueP95Us"),
			value("FramesUnrecoverable"),
			value("Verdict")
		);
	}
	let field = |tag| xml_value(result, tag).unwrap_or("-");
	println!(
		"outcome={} usable wire={} kbps suggested={} kbps confidence={} ({})",
		field("Outcome"),
		field("UsableWireKbps"),
		field("RecommendedKbps"),
		field("Confidence"),
		field("Reason")
	);
}

fn print_json(
	args: &RunArgs,
	reply: &StartReply,
	run: &client::ClientRun,
	result: &str,
	elapsed: std::time::Duration,
	receive_buffer: usize,
) {
	let number = |text: Option<&str>| text.and_then(|t| t.parse::<i64>().ok());
	let steps: Vec<serde_json::Value> = steps_of(result)
		.into_iter()
		.map(|step| {
			let mut object = serde_json::Map::new();
			for tag in [
				"Index",
				"OfferedKbps",
				"Planned",
				"SentFrames",
				"Submitted",
				"SendFailed",
				"WouldBlock",
				"LateFrames",
				"MaxStartLatenessUs",
				"Received",
				"NetworkLossPpm",
				"GapEvents",
				"MaxGap",
				"FramesUnrecoverable",
				"QueueP50Us",
				"QueueP95Us",
				"QueueMaxUs",
				"FrameJitterUs",
			] {
				object.insert(tag.into(), number(xml_value(step, tag)).into());
			}
			object.insert("Verdict".into(), xml_value(step, "Verdict").into());
			serde_json::Value::Object(object)
		})
		.collect();
	let summary = serde_json::json!({
		"host": args.host,
		"shape": reply.plan.shape.as_str(),
		"fps": reply.plan.fps,
		"packet_size": reply.packet_size,
		"shard_bytes": reply.plan.shard_bytes,
		"chunk_packets": reply.chunk_packets,
		"fec_percent": reply.fec_percent,
		"client_stop": format!("{:?}", run.stop),
		"elapsed_ms": elapsed.as_millis() as u64,
		"receive_buffer_bytes": receive_buffer,
		"max_kbps": args.max_kbps,
		"outcome": xml_value(result, "Outcome"),
		"usable_wire_kbps": number(xml_value(result, "UsableWireKbps")),
		"recommended_kbps": number(xml_value(result, "RecommendedKbps")),
		"confidence": xml_value(result, "Confidence"),
		"ceiling_reached": xml_value(result, "CeilingReached") == Some("1"),
		"stable": xml_value(result, "Stable") == Some("1"),
		"reason": xml_value(result, "Reason"),
		"steps": steps,
	});
	println!("{summary}");
}
