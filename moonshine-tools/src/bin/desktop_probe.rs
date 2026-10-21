//! Probe Desktop Mode capture on the running desktop without encoding:
//! authorize a portal session, receive frames and report their delivery and
//! CPU cost, optionally injecting a scripted input sequence (`--input`).
//!
//! This performs a real 1920x1080 desktop takeover: physical displays are
//! disabled during the probe and restored afterward. Input needs host consent.
//! Install/start the desktop guard with the server executable first.

use std::time::Duration;

use clap::Parser;
use moonshine_core::session::desktop_probe::{ProbeOptions, run};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(about = "Probe managed Desktop Mode without encoding; temporarily disables physical displays")]
struct Args {
	/// Read the running guard's status without starting it or changing displays.
	#[arg(long, conflicts_with_all = ["input", "restore_token", "timings", "implicit_sync", "width", "height", "fps", "duration"])]
	status: bool,
	/// Collect constant-space capture timing percentiles (adds timing overhead).
	#[arg(long)]
	timings: bool,
	/// Use implicit DMA-BUF fences even when timeline synchronization is available.
	#[arg(long)]
	implicit_sync: bool,
	/// Width of the virtual display.
	#[arg(long, default_value_t = 1920)]
	width: u32,
	/// Height of the virtual display.
	#[arg(long, default_value_t = 1080)]
	height: u32,
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
	let report = if args.status {
		serde_json::to_value(moonshine_core::desktop::status_read_only()?)?
	} else {
		serde_json::to_value(run(ProbeOptions {
			duration: Duration::from_secs(args.duration),
			refresh_rate: args.fps,
			timings: args.timings,
			implicit_sync: args.implicit_sync,
			resolution: (args.width, args.height),
			input: args.input,
			restore_token: args.restore_token,
		})?)?
	};
	let json = serde_json::to_string_pretty(&report)?;
	println!("{json}");
	if let Some(path) = args.json {
		std::fs::write(path, json)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn status_cannot_accidentally_request_a_capture_or_input_probe() {
		assert!(Args::try_parse_from(["probe", "--status"]).unwrap().status);
		for option in [
			vec!["--input"],
			vec!["--timings"],
			vec!["--implicit-sync"],
			vec!["--fps", "120"],
			vec!["--width", "2880"],
			vec!["--duration", "5"],
		] {
			let mut argv = vec!["probe", "--status"];
			argv.extend(option);
			assert!(Args::try_parse_from(argv).is_err());
		}
	}
}
