//! Redacted support bundles.
//!
//! A bundle is a gzip-compressed tar archive of everything needed to
//! diagnose a host, safe to attach to a public issue:
//!
//! | File | Contents |
//! | --- | --- |
//! | `README.txt` | What was collected, how, and what was redacted |
//! | `report.json` | The running daemon's diagnostic report: server, health, session, latest statistics, events |
//! | `stats-history.json` | Recent one-second statistics windows (the last two minutes of streaming) |
//! | `health.json` | A health check run for the bundle (only without a reachable daemon) |
//! | `host.json` | Kernel, OS, CPU, memory, GPUs and the service unit |
//! | `gpu-sensors.json` | One reading of the capture GPU's sensors |
//! | `config.toml` | The configuration, redacted setting by setting |
//! | `service-status.txt`, `journal.txt` | `systemctl status` and the recent service log |
//! | `vulkaninfo.txt` | `vulkaninfo --summary`, when installed |
//! | `collection.txt` | Steps that could not be completed |
//!
//! The daemon builds bundles for `GetSupportBundle` (the web interface and
//! `pyroshine diag`); `pyroshine diag` builds one itself when no daemon is
//! reachable. Collection runs on a blocking thread and never touches a
//! session: external commands have a deadline, and the health check runs
//! only when no Pyroshine server is running.

mod host;
mod redact;
mod tar;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use moonshine_management::dto::{DiagnosticReport, HealthReport, StreamStats};
use serde::Serialize;

pub(crate) use self::host::{ServiceUnit, host_info};
pub(crate) use self::redact::Redactor;
#[cfg(test)]
pub(crate) use self::tar::tests::entries as archive_entries;
use crate::gpu_sensors::{self, GpuSampler};

/// Service log lines included.
const JOURNAL_LINES: usize = 3000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// What a bundle is built from besides what it collects itself.
pub(crate) struct BundleSources {
	/// The running daemon's report; `None` for a bundle collected without it.
	pub report: Option<DiagnosticReport>,
	pub stats_history: Vec<StreamStats>,
	/// A health check run for this bundle.
	pub health: Option<HealthReport>,
	pub config_path: PathBuf,
	pub state_path: Option<PathBuf>,
	pub capture_node: Option<PathBuf>,
	pub service: Option<ServiceUnit>,
	/// Steps the caller could not complete, for `collection.txt`.
	pub notes: Vec<String>,
}

/// A finished bundle.
pub struct Bundle {
	/// The archive's top-level directory, also a good file name stem.
	pub name: String,
	pub archive: Vec<u8>,
	/// Steps that could not be completed.
	pub notes: Vec<String>,
}

pub(crate) fn build(mut sources: BundleSources, now: SystemTime) -> Bundle {
	let name = bundle_name(now);
	let mut redactor = Redactor::for_host();
	if let Some(state) = &sources.state_path {
		for (value, placeholder) in crate::state::identifying_values(state) {
			redactor.word(&value, &placeholder);
		}
	}
	let mut files: Vec<(&str, Vec<u8>)> = Vec::new();
	let mut notes = std::mem::take(&mut sources.notes);

	// First: the configuration names the server, which later files repeat.
	match std::fs::read_to_string(&sources.config_path) {
		Ok(text) => files.push(("config.toml", redactor.config(&text).into_bytes())),
		Err(error) => notes.push(redactor.text(&format!(
			"config.toml: cannot read {}: {error}",
			sources.config_path.display()
		))),
	}

	if let Some(report) = &sources.report {
		files.push(("report.json", redacted_json(&mut redactor, report)));
	}
	if !sources.stats_history.is_empty() {
		files.push((
			"stats-history.json",
			redacted_json(&mut redactor, &sources.stats_history),
		));
	}
	if let Some(health) = &sources.health {
		files.push(("health.json", redacted_json(&mut redactor, health)));
	}
	let host = host_info(sources.capture_node.as_deref(), sources.service.as_ref());
	files.push(("host.json", redacted_json(&mut redactor, &host)));
	match sources.capture_node.as_deref().and_then(GpuSampler::open) {
		Some(mut sampler) => files.push(("gpu-sensors.json", redacted_json(&mut redactor, &sampler.sample(&[])))),
		None => notes.push("gpu-sensors.json: the capture GPU could not be identified".into()),
	}

	match &sources.service {
		Some(service) => {
			for (file, program, args) in [
				(
					"service-status.txt",
					"systemctl",
					service.args(&["status", "--no-pager", "--lines=0", &service.name]),
				),
				(
					"journal.txt",
					"journalctl",
					service.args(&[
						"--unit",
						&service.name,
						"--no-pager",
						"--output=short-iso-precise",
						&format!("--lines={JOURNAL_LINES}"),
					]),
				),
			] {
				match run(program, &args, COMMAND_TIMEOUT) {
					Ok(output) => files.push((file, redactor.text(&output).into_bytes())),
					Err(error) => notes.push(redactor.text(&format!("{file}: {program} {}: {error}", args.join(" ")))),
				}
			}
		},
		None => notes.push(
			"journal.txt: Pyroshine does not run as a systemd service here; attach its terminal output instead.".into(),
		),
	}
	match run("vulkaninfo", &["--summary".to_string()], COMMAND_TIMEOUT) {
		Ok(output) => files.push(("vulkaninfo.txt", redactor.text(&output).into_bytes())),
		Err(error) => notes.push(format!("vulkaninfo.txt: {error}")),
	}

	// Notes may quote paths, addresses or the server name.
	let notes: Vec<String> = notes.iter().map(|note| redactor.text(note)).collect();
	let mut archive = tar::Archive::new(&name, unix_seconds(now));
	archive.file("README.txt", readme(&sources, now, &files).as_bytes());
	for (file, contents) in &files {
		archive.file(file, contents);
	}
	if !notes.is_empty() {
		archive.file("collection.txt", format!("{}\n", notes.join("\n")).as_bytes());
	}
	Bundle {
		name,
		archive: archive.finish(),
		notes,
	}
}

/// A bundle collected without a reachable daemon (`pyroshine diag` on a host
/// whose server is stopped, or that the caller cannot reach). The health
/// check runs only when `health_check` is set and no Pyroshine server is
/// running, so its GPU probes and port checks cannot disturb a stream.
pub fn collect_offline(config_path: &Path, health_check: bool, notes: Vec<String>) -> Bundle {
	let mut notes = notes;
	let config = crate::config::Config::read_from_file(config_path).ok();
	if config.is_none() {
		notes.push("The configuration could not be loaded; defaults were assumed for the GPU.".into());
	}
	let running = running_servers();
	let service = running
		.iter()
		.find(|server| server.own_user)
		.and_then(|server| server.service.clone())
		.or_else(installed_service);
	let health = match (health_check, running.is_empty()) {
		(true, true) => {
			let report = crate::healthcheck::run_healthcheck(config.as_ref());
			Some(crate::management::health_report(&report, false))
		},
		(true, false) => {
			notes.push(
				"health.json: skipped because a Pyroshine server is running; its checks would disturb it.".into(),
			);
			None
		},
		(false, _) => None,
	};
	let gpu = config.as_ref().and_then(|config| config.compositor.gpu.clone());
	let sources = BundleSources {
		report: None,
		stats_history: Vec::new(),
		health,
		config_path: config_path.to_path_buf(),
		state_path: crate::state::state_path(),
		capture_node: crate::healthcheck::find_render_node(&gpu).ok(),
		service,
		notes,
	};
	build(sources, SystemTime::now())
}

/// `pyroshine-diag-YYYYMMDD-HHMMSS` in UTC.
pub fn bundle_name(now: SystemTime) -> String {
	moonshine_management::support_bundle_name(unix_seconds(now))
}

fn unix_seconds(time: SystemTime) -> u64 {
	time.duration_since(SystemTime::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs())
}

fn redacted_json(redactor: &mut Redactor, value: &impl Serialize) -> Vec<u8> {
	let mut value = serde_json::to_value(value).expect("support documents serialize");
	redactor.json(&mut value);
	let mut text = serde_json::to_vec_pretty(&value).expect("support documents serialize");
	text.push(b'\n');
	text
}

fn readme(sources: &BundleSources, now: SystemTime, files: &[(&str, Vec<u8>)]) -> String {
	let mode = if sources.report.is_some() {
		"from the running Pyroshine server"
	} else {
		"by pyroshine diag, without the server's live report"
	};
	let mut text = format!(
		"Pyroshine {} support bundle, collected {mode} at {} (Unix time).\n\n\
		 Identifying values were replaced before the bundle was written: the home directory\n\
		 (~), user and host names (<user>, <host>), the server name (<server-name>), the\n\
		 server ID, client IDs and names, certificate fingerprints, UUIDs and IP addresses\n\
		 (<lan-ip-N> for private ranges, <ip-N> otherwise; one number per address).\n\
		 Review the files before sharing them.\n\nFiles:\n",
		env!("CARGO_PKG_VERSION"),
		unix_seconds(now)
	);
	for (file, contents) in files {
		text.push_str(&format!("  {file} ({} bytes)\n", contents.len()));
	}
	text
}

/// A Pyroshine server process found in `/proc`.
#[derive(Debug)]
struct RunningServer {
	own_user: bool,
	service: Option<ServiceUnit>,
}

/// Pyroshine servers other than this process. A `pyroshine` command is a
/// server unless its arguments name a subcommand.
fn running_servers() -> Vec<RunningServer> {
	use std::os::unix::fs::MetadataExt;
	let own = std::process::id().to_string();
	// SAFETY: getuid has no preconditions.
	let uid = unsafe { libc::getuid() };
	let Ok(processes) = std::fs::read_dir("/proc") else {
		return Vec::new();
	};
	processes
		.filter_map(Result::ok)
		.filter_map(|entry| {
			let pid = entry.file_name().into_string().ok()?;
			if pid == own || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
				return None;
			}
			let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
			if !matches!(comm.trim(), "pyroshine" | "moonshine") {
				return None;
			}
			let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
			let subcommand = cmdline
				.split(|byte| *byte == 0)
				.skip(1)
				.any(|arg| matches!(arg, b"diag" | b"pair" | b"healthcheck"));
			if subcommand {
				return None;
			}
			Some(RunningServer {
				own_user: entry.metadata().is_ok_and(|metadata| metadata.uid() == uid),
				service: gpu_sensors::process_cgroup(&pid).and_then(|cgroup| ServiceUnit::from_cgroup(&cgroup)),
			})
		})
		.collect()
}

/// The installed service for this user when none runs: the packaged
/// `pyroshine@<user>` system unit, upstream's `moonshine@<user>`, the Nix
/// module's `moonshine`, or a user unit.
fn installed_service() -> Option<ServiceUnit> {
	let user = std::env::var("USER").unwrap_or_default();
	let candidates = [
		(format!("pyroshine@{user}.service"), false),
		(format!("moonshine@{user}.service"), false),
		("moonshine.service".to_string(), false),
		("pyroshine.service".to_string(), true),
		("moonshine.service".to_string(), true),
	];
	candidates.into_iter().find_map(|(name, user)| {
		let service = ServiceUnit { name, user };
		let args = service.args(&["show", "--property=LoadState", "--value", &service.name]);
		let state = run("systemctl", &args, Duration::from_secs(5)).ok()?;
		(state.trim() == "loaded").then_some(service)
	})
}

/// Run `program` and return its standard output (with standard error when it
/// fails), or why it could not run. Output is read while waiting, so a large
/// log cannot fill the pipe and stall the command.
fn run(program: &str, args: &[String], timeout: Duration) -> Result<String, String> {
	let mut child = Command::new(program)
		.args(args)
		.env("SYSTEMD_COLORS", "0")
		.env("SYSTEMD_PAGER", "")
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|error| format!("cannot run {program}: {error}"))?;
	let reader = |stream: Option<Box<dyn Read + Send>>| {
		std::thread::spawn(move || {
			let mut bytes = Vec::new();
			if let Some(mut stream) = stream {
				let _ = stream.read_to_end(&mut bytes);
			}
			String::from_utf8_lossy(&bytes).into_owned()
		})
	};
	let stdout = reader(
		child
			.stdout
			.take()
			.map(|stream| Box::new(stream) as Box<dyn Read + Send>),
	);
	let stderr = reader(
		child
			.stderr
			.take()
			.map(|stream| Box::new(stream) as Box<dyn Read + Send>),
	);
	let deadline = Instant::now() + timeout;
	let status = loop {
		match child.try_wait() {
			Ok(Some(status)) => break status,
			Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
			Ok(None) => {
				let _ = child.kill();
				let _ = child.wait();
				return Err(format!("{program} did not finish within {} s", timeout.as_secs()));
			},
			Err(error) => return Err(format!("{program}: {error}")),
		}
	};
	let stdout = stdout.join().unwrap_or_default();
	let stderr = stderr.join().unwrap_or_default();
	if status.success() || !stdout.trim().is_empty() {
		// `systemctl status` exits 3 for a stopped unit; its output still counts.
		if stderr.trim().is_empty() {
			Ok(stdout)
		} else {
			Ok(format!("{stdout}\n# stderr:\n{stderr}"))
		}
	} else {
		Err(format!("{program} failed ({status}): {}", stderr.trim()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `/bin/sh` with built-ins only: another test temporarily replaces `PATH`.
	fn shell(script: &str, timeout: Duration) -> Result<String, String> {
		run("/bin/sh", &["-c".into(), script.into()], timeout)
	}

	#[test]
	fn commands_return_output_errors_and_time_out() {
		assert_eq!(shell("echo out", COMMAND_TIMEOUT).unwrap(), "out\n");
		let exited = shell("echo bad >&2; exit 3", COMMAND_TIMEOUT).unwrap_err();
		assert!(exited.contains("bad"), "{exited}");
		let large = shell(
			"i=0; while [ $i -lt 1000 ]; do printf '%01000d' 0; i=$((i + 1)); done",
			COMMAND_TIMEOUT,
		)
		.unwrap();
		assert_eq!(large.len(), 1_000_000, "a full pipe does not stall");
		let slow = shell("while :; do :; done", Duration::from_millis(100)).unwrap_err();
		assert!(slow.contains("did not finish"), "{slow}");
		assert!(run("/nonexistent/pyroshine-no-such-program", &[], COMMAND_TIMEOUT).is_err());
	}

	#[test]
	fn a_bundle_contains_redacted_files() {
		let directory = tempfile::tempdir().unwrap();
		let config_path = directory.path().join("config.toml");
		std::fs::write(&config_path, "name = \"Alice's Tower\"\naddress = \"192.168.4.2\"\n").unwrap();
		let state_path = directory.path().join("state.toml");
		std::fs::write(
			&state_path,
			"unique_id = \"server-id-1234\"\nclients = [\"0123456789ABCDEF\"]\n\n[client_metadata.ab]\nlabel = \"Living room TV\"\n",
		)
		.unwrap();
		let history = vec![
			serde_json::from_value::<StreamStats>(serde_json::json!({
				"epoch": 1, "at_ms": 0, "window_ms": 1000, "frames": 60, "fps": 60.0, "key_frames": 0,
				"encoded_bitrate_bps": 1.0, "wire_bitrate_bps": 1.0, "transport_overhead_percent": null,
				"packets": 1, "failed_packets": 0, "discarded_packets": 0, "stale_frames_dropped": 0,
				"samples_dropped": 0, "stages": []
			}))
			.unwrap(),
		];
		let sources = BundleSources {
			report: None,
			stats_history: history,
			health: None,
			config_path,
			state_path: Some(state_path),
			capture_node: None,
			service: None,
			notes: vec!["report.json: Alice's Tower at 192.168.4.2 is not running".into()],
		};
		let bundle = build(sources, SystemTime::UNIX_EPOCH);
		assert_eq!(bundle.name, "pyroshine-diag-19700101-000000");
		let entries = tar::tests::entries(&bundle.archive);
		let file = |name: &str| {
			entries
				.iter()
				.find(|(path, ..)| path == &format!("{}/{name}", bundle.name))
				.map(|(.., data)| String::from_utf8(data.clone()).unwrap())
		};
		let config = file("config.toml").unwrap();
		assert!(
			config.contains("<server-name>") && config.contains("<lan-ip-1>"),
			"{config}"
		);
		assert!(file("stats-history.json").unwrap().contains("\"fps\": 60.0"));
		assert!(file("host.json").unwrap().contains("\"kernel\""));
		assert!(file("README.txt").unwrap().contains("config.toml"));
		let collection = file("collection.txt").unwrap();
		assert!(
			collection.starts_with("report.json: <server-name> at <lan-ip-1> is not running\n"),
			"{collection}"
		);
		assert!(collection.contains("journal.txt"), "{collection}");
		for (_, _, data) in &entries {
			let text = String::from_utf8_lossy(data);
			for secret in [
				"Alice's Tower",
				"192.168.4.2",
				"server-id-1234",
				"0123456789ABCDEF",
				"Living room TV",
			] {
				assert!(!text.contains(secret), "{secret} leaked");
			}
		}
	}
}
