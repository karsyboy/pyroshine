//! Starts the tray while the user's desktop session runs.
//!
//! The server usually runs as a system service for the user, started before
//! (or without) any login, so its own environment says nothing about a
//! desktop. The desktop session publishes its display variables in the
//! user's systemd manager environment (Plasma and GNOME do), which is read
//! over the session bus; a process started from a terminal falls back to its
//! own environment. A session counts as running when its display socket
//! answers and the session bus has a tray host or a notification server.
//! On a headless host neither exists and nothing starts.
//!
//! The tray is a child process of this executable (`/proc/self/exe`, so a
//! package upgrade never mixes versions) with the session's variables. It
//! holds the read end of a pipe on standard input and exits when the server
//! does; the service's cgroup ends it too. A desktop logout or a changed
//! session stops it; a crash restarts it with a growing delay; hiding it
//! from its menu (a clean exit) keeps it hidden until the next desktop
//! session or server start. The checks run every few seconds and cost a
//! property read and a socket connect, nothing on the streaming path.

use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

/// How often the desktop session is checked.
const POLL: Duration = Duration::from_secs(10);
/// First delay before restarting a crashed tray; it doubles up to the maximum.
const RESTART_DELAY: Duration = Duration::from_secs(5);
const RESTART_DELAY_MAX: Duration = Duration::from_secs(300);
/// A tray that ran this long before failing restarts with the first delay.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// How long a tray may take to exit after its session ended.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Session variables the tray takes from the desktop.
const SESSION_VARIABLES: &[&str] = &[
	"WAYLAND_DISPLAY",
	"DISPLAY",
	"XAUTHORITY",
	"XDG_CURRENT_DESKTOP",
	"XDG_SESSION_DESKTOP",
	"XDG_SESSION_TYPE",
	"XDG_SESSION_ID",
];

/// Bus names only a running desktop owns: a tray host or a notification
/// server. Owned, not merely activatable.
const DESKTOP_NAMES: &[&str] = &["org.kde.StatusNotifierWatcher", "org.freedesktop.Notifications"];

#[zbus::proxy(
	interface = "org.freedesktop.systemd1.Manager",
	default_service = "org.freedesktop.systemd1",
	default_path = "/org/freedesktop/systemd1"
)]
trait UserManager {
	#[zbus(property)]
	fn environment(&self) -> zbus::Result<Vec<String>>;
}

/// Runs the tray beside the server; dropping it stops the tray.
pub struct Launcher {
	task: JoinHandle<()>,
}

impl Launcher {
	pub fn spawn() -> Self {
		Self {
			task: tokio::spawn(supervise()),
		}
	}
}

impl Drop for Launcher {
	fn drop(&mut self) {
		// Dropping the child (kill_on_drop) and its pipe ends the tray.
		self.task.abort();
	}
}

/// A running desktop session: the variables the tray gets. Two sessions are
/// the same while these are (`XDG_SESSION_ID` changes with every login).
#[derive(Clone, Debug, PartialEq)]
struct Desktop {
	variables: Vec<(String, String)>,
}

/// The session variables from `entries` (`NAME=value`), when they name a display.
fn session_variables<'a>(entries: impl IntoIterator<Item = &'a str>) -> Option<Vec<(String, String)>> {
	let mut variables: Vec<(String, String)> = entries
		.into_iter()
		.filter_map(|entry| entry.split_once('='))
		.filter(|(name, value)| SESSION_VARIABLES.contains(name) && !value.is_empty())
		.map(|(name, value)| (name.to_string(), value.to_string()))
		.collect();
	variables.sort();
	variables.dedup_by(|a, b| a.0 == b.0);
	variables
		.iter()
		.any(|(name, _)| name == "WAYLAND_DISPLAY" || name == "DISPLAY")
		.then_some(variables)
}

fn variable<'a>(variables: &'a [(String, String)], name: &str) -> Option<&'a str> {
	variables
		.iter()
		.find(|(candidate, _)| candidate == name)
		.map(|(_, value)| value.as_str())
}

/// The desktop session, if one runs.
async fn desktop(connection: &zbus::Connection) -> Option<Desktop> {
	let manager = match UserManagerProxy::builder(connection)
		.cache_properties(zbus::proxy::CacheProperties::No)
		.build()
		.await
	{
		Ok(manager) => manager.environment().await.ok(),
		Err(_) => None,
	};
	let from_manager = manager.and_then(|entries| session_variables(entries.iter().map(String::as_str)));
	let variables = match from_manager {
		Some(variables) => variables,
		None => {
			let own: Vec<String> = SESSION_VARIABLES
				.iter()
				.filter_map(|name| std::env::var(name).ok().map(|value| format!("{name}={value}")))
				.collect();
			session_variables(own.iter().map(String::as_str))?
		},
	};
	super::display_server(
		variable(&variables, "WAYLAND_DISPLAY").map(Into::into),
		std::env::var_os("XDG_RUNTIME_DIR"),
		variable(&variables, "DISPLAY").map(Into::into),
	)?;
	let dbus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
	for name in DESKTOP_NAMES {
		let name = zbus::names::BusName::try_from(*name).expect("valid bus name");
		if dbus.name_has_owner(name).await.unwrap_or(false) {
			return Some(Desktop { variables });
		}
	}
	None
}

fn command(desktop: &Desktop) -> tokio::process::Command {
	let mut command = std::process::Command::new("/proc/self/exe");
	command
		.arg0("pyroshine")
		.args(["tray", "--attached"])
		.envs(desktop.variables.iter().map(|(name, value)| (name, value)))
		// The tray is idle almost always; it needs no thread per core.
		.env("TOKIO_WORKER_THREADS", "2")
		.stdin(Stdio::piped());
	// A test instance serves its management interface on its own bus.
	if let Some(bus) = std::env::var_os("MOONSHINE_MANAGEMENT_BUS") {
		command.env("PYROSHINE_MANAGEMENT_BUS", bus);
	}
	let mut command = tokio::process::Command::from(command);
	command.kill_on_drop(true);
	command
}

fn restart_delay(failures: u32) -> Duration {
	RESTART_DELAY
		.saturating_mul(1 << failures.saturating_sub(1).min(16))
		.min(RESTART_DELAY_MAX)
}

async fn supervise() {
	let connection = match zbus::Connection::session().await {
		Ok(connection) => connection,
		Err(error) => {
			tracing::info!("No session bus ({error}); the desktop tray is not started");
			return;
		},
	};
	// The session the tray was hidden in; it stays hidden until another.
	let mut hidden: Option<Desktop> = None;
	let mut failures = 0u32;
	loop {
		let Some(session) = desktop(&connection).await else {
			hidden = None;
			tokio::time::sleep(POLL).await;
			continue;
		};
		if hidden.as_ref() == Some(&session) {
			tokio::time::sleep(POLL).await;
			continue;
		}

		let mut child = match command(&session).spawn() {
			Ok(child) => child,
			Err(error) => {
				failures += 1;
				tracing::warn!("Cannot start the desktop tray: {error}");
				tokio::time::sleep(restart_delay(failures)).await;
				continue;
			},
		};
		let pipe = child.stdin.take();
		let started = Instant::now();
		tracing::info!("Desktop session found; the tray started");

		let exited = loop {
			tokio::select! {
				status = child.wait() => break Some(status),
				() = tokio::time::sleep(POLL) => {
					if desktop(&connection).await.as_ref() != Some(&session) {
						break None;
					}
				},
			}
		};
		drop(pipe);
		match exited {
			None => {
				tracing::info!("The desktop session ended; the tray stops");
				if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
					let _ = child.kill().await;
				}
				failures = 0;
			},
			Some(Ok(status)) if status.success() => {
				tracing::info!("The tray was hidden; it returns with the next desktop session");
				hidden = Some(session);
				failures = 0;
			},
			Some(result) => {
				failures = if started.elapsed() >= HEALTHY_RUN {
					1
				} else {
					failures + 1
				};
				let delay = restart_delay(failures);
				match result {
					Ok(status) => tracing::warn!(%status, ?delay, "The tray stopped unexpectedly; restarting"),
					Err(error) => tracing::warn!(%error, ?delay, "The tray stopped unexpectedly; restarting"),
				}
				tokio::time::sleep(delay).await;
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_display_sessions_count_and_only_their_variables_pass() {
		assert_eq!(session_variables(["HOME=/home/user", "XDG_SESSION_TYPE=tty"]), None);
		assert_eq!(session_variables(["WAYLAND_DISPLAY="]), None);
		let variables = session_variables([
			"PATH=/usr/bin",
			"XDG_SESSION_ID=3",
			"WAYLAND_DISPLAY=wayland-1",
			"LD_PRELOAD=/tmp/evil.so",
			"DISPLAY=:0",
		])
		.unwrap();
		assert_eq!(
			variables,
			[
				("DISPLAY".to_string(), ":0".to_string()),
				("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
				("XDG_SESSION_ID".to_string(), "3".to_string()),
			]
		);
		assert_eq!(variable(&variables, "DISPLAY"), Some(":0"));
	}

	#[test]
	fn a_new_login_is_a_different_session() {
		let first = session_variables(["WAYLAND_DISPLAY=wayland-0", "XDG_SESSION_ID=3"]);
		let again = session_variables(["XDG_SESSION_ID=3", "WAYLAND_DISPLAY=wayland-0"]);
		let next = session_variables(["WAYLAND_DISPLAY=wayland-0", "XDG_SESSION_ID=4"]);
		assert_eq!(first, again);
		assert_ne!(first, next);
	}

	#[test]
	fn restarts_back_off() {
		assert_eq!(restart_delay(1), Duration::from_secs(5));
		assert_eq!(restart_delay(2), Duration::from_secs(10));
		assert_eq!(restart_delay(4), Duration::from_secs(40));
		assert_eq!(restart_delay(30), RESTART_DELAY_MAX);
	}
}
