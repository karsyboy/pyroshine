//! Probe Desktop Mode capture on the running desktop without encoding:
//! authorize a portal session, receive frames and report their delivery and
//! CPU cost, optionally injecting a scripted input sequence (`--input`).
//!
//! The desktop may ask for consent; accept it on the host.

use std::time::Duration;

use clap::Parser;
use moonshine_core::session::desktop_probe::{ProbeOptions, run};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(about = "Probe Desktop Mode capture (XDG Desktop Portal + PipeWire) without encoding")]
struct Args {
	/// Seconds of frames to receive.
	#[arg(long, default_value_t = 10)]
	duration: u64,
	/// Frame rate asked of the compositor.
	#[arg(long, default_value_t = 60)]
	fps: u32,
	/// Ask for remote input and inject: pointer to the center, a click,
	/// typing "pyroshine" and Enter, a wheel notch, then a release.
	#[arg(long)]
	input: bool,
	/// Keep the portal's restore token in this file between runs.
	#[arg(long)]
	restore_token: Option<std::path::PathBuf>,
	/// Write the report as JSON to this file as well.
	#[arg(long)]
	json: Option<std::path::PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer())
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
		.init();
	let args = Args::parse();
	let report = run(ProbeOptions {
		duration: Duration::from_secs(args.duration),
		refresh_rate: args.fps,
		input: args.input,
		restore_token: args.restore_token,
	})?;
	let json = serde_json::to_string_pretty(&report)?;
	println!("{json}");
	if let Some(path) = args.json {
		std::fs::write(path, json)?;
	}
	Ok(())
}
