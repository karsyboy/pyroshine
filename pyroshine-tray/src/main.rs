//! Pyroshine desktop tray: a status icon, pairing notifications and
//! shortcuts into the web interface.
//!
//! The tray is a small, optional companion of a separately running server.
//! It never starts, stops or supervises the server and owns no streaming
//! resources; quitting it (or a crash) leaves the server, its web interface
//! and any stream untouched. All state comes from the server's management
//! interface on the session bus; every management task opens in the browser.
//!
//! - `pyroshine-tray` runs the tray (desktop autostart). It exits at once
//!   outside a graphical session or when the session already has a tray, and
//!   when the session bus goes away (logout).
//! - `pyroshine-tray --open [page]` opens a page of the web interface (the
//!   application launcher), through the running tray when there is one.

mod attach;
mod browser;
mod icons;
mod instance;
mod notifications;
mod service;
mod tray;

use std::path::Path;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "Usage: pyroshine-tray [--open [PAGE]]

Desktop tray for a running Pyroshine server. Without options it shows the
status icon (started at desktop login). --open opens the web interface in the
default browser at PAGE (dashboard, clients, library, history, settings,
diagnostics, updates or maintenance).";

enum Mode {
	Tray,
	Open(String),
}

fn parse_args() -> Mode {
	let mut mode = Mode::Tray;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--open" => mode = Mode::Open(args.next().unwrap_or_else(|| "dashboard".into())),
			// The old desktop app's flags, still used by stale autostart entries.
			"--background" => mode = Mode::Tray,
			"--page" => mode = Mode::Open(args.next().unwrap_or_else(|| "dashboard".into())),
			"--version" => {
				println!("pyroshine-tray {}", env!("CARGO_PKG_VERSION"));
				std::process::exit(0);
			},
			"--help" | "-h" => {
				println!("{USAGE}");
				std::process::exit(0);
			},
			other => eprintln!("Ignoring unknown argument {other}"),
		}
	}
	mode
}

/// The XDG activation token the launcher passed for the user's click. It is
/// removed from the environment so it is used for this launch only.
fn take_launch_activation_token() -> Option<String> {
	let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
		.iter()
		.find_map(|name| std::env::var(name).ok())
		.and_then(|token| browser::activation_token(&token));
	// SAFETY: called before the runtime or any other thread exists.
	unsafe {
		std::env::remove_var("XDG_ACTIVATION_TOKEN");
		std::env::remove_var("DESKTOP_STARTUP_ID");
	}
	token
}

/// Whether a display server answers: the Wayland socket or the X11 socket
/// named by the environment accepts a connection. A variable alone proves
/// nothing (it may be left over from an ended session).
fn graphical_session() -> Option<String> {
	display_server(
		std::env::var_os("WAYLAND_DISPLAY"),
		std::env::var_os("XDG_RUNTIME_DIR"),
		std::env::var("DISPLAY").ok(),
	)
}

fn display_server(
	wayland: Option<std::ffi::OsString>,
	runtime: Option<std::ffi::OsString>,
	x11: Option<String>,
) -> Option<String> {
	if let Some(display) = wayland.filter(|display| !display.is_empty()) {
		let socket = match (Path::new(&display).is_absolute(), &runtime) {
			(true, _) => Path::new(&display).to_path_buf(),
			(false, Some(runtime)) => Path::new(runtime).join(&display),
			(false, None) => return None,
		};
		if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
			return Some(format!("Wayland ({})", display.to_string_lossy()));
		}
	}
	if let Some(display) = x11 {
		// ":0" or ":0.0" on the local machine.
		let number = display
			.strip_prefix(':')
			.and_then(|rest| rest.split('.').next())
			.filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()));
		if let Some(number) = number {
			let socket = format!("/tmp/.X11-unix/X{number}");
			if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
				return Some(format!("X11 ({display})"));
			}
		}
	}
	None
}

async fn open(page: &str, activation: Option<String>) -> Result<(), String> {
	let connection = instance::session_bus().await.ok();
	if let Some(connection) = &connection
		&& instance::forward(connection, page, activation.as_deref()).await
	{
		return Ok(());
	}
	// No tray: ask the server for its address directly.
	let management = match &connection {
		Some(connection) => instance::management_bus(connection).await.ok(),
		None => None,
	};
	let base = match &management {
		Some(management) => match moonshine_management::ManagementProxy::new(management).await {
			Ok(proxy) => proxy.get_web_url().await.ok().filter(|url| !url.is_empty()),
			Err(_) => None,
		},
		None => None,
	};
	if base.is_none() {
		eprintln!(
			"Pyroshine did not report its web address (is it running for this user?); opening {}",
			browser::DEFAULT_URL
		);
	}
	browser::open(
		connection.as_ref(),
		&browser::address(base.as_deref(), page),
		activation,
	)
	.await
}

async fn run_tray() -> Result<(), String> {
	let Some(session) = graphical_session() else {
		println!("No graphical session (no answering Wayland or X11 display); the tray is not needed here.");
		return Ok(());
	};
	let connection = instance::session_bus()
		.await
		.map_err(|error| format!("The session bus is unavailable: {error}"))?;
	let (connection, mut requests) = match instance::acquire(connection)
		.await
		.map_err(|error| format!("Cannot register the tray: {error}"))?
	{
		instance::Instance::Primary { connection, requests } => (connection, requests),
		instance::Instance::Running => {
			println!("The Pyroshine tray already runs in this session.");
			return Ok(());
		},
	};
	tracing::info!(session, "Pyroshine tray started");

	let management = instance::management_bus(&connection)
		.await
		.map_err(|error| format!("The management bus is unavailable: {error}"))?;
	let service = Arc::new(service::Service::new(connection.clone(), management));
	let notifier = notifications::Notifier::new(service.clone()).await;
	let quit = Arc::new(Notify::new());
	let tray = tray::spawn(service.clone(), notifier.clone(), quit.clone()).await;
	let following = service.start(tray, notifier.clone());

	let opener = {
		let service = service.clone();
		let notifier = notifier.clone();
		tokio::spawn(async move {
			while let Some((page, activation)) = requests.recv().await {
				if let Err(error) = service.open(&page, activation).await {
					notifier.error("Could not open Pyroshine in the browser", &error).await;
				}
			}
		})
	};

	let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
	let mut hangup = signal(SignalKind::hangup()).map_err(|error| error.to_string())?;
	tokio::select! {
		() = quit.notified() => tracing::info!("Quit from the tray menu; Pyroshine keeps running"),
		_ = tokio::signal::ctrl_c() => {},
		_ = terminate.recv() => {},
		_ = hangup.recv() => {},
		// The session bus closed (logout): this session's tray ends with it.
		_ = following => tracing::info!("The session bus closed; the tray exits"),
	}
	opener.abort();
	Ok(())
}

fn main() {
	let mode = parse_args();
	let activation = take_launch_activation_token();
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::try_from_env("PYROSHINE_TRAY_LOG").unwrap_or_else(|_| EnvFilter::new("warn")))
		.init();
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(2)
		.enable_all()
		.build()
		.expect("the runtime starts");
	let result = runtime.block_on(async {
		match mode {
			Mode::Tray => run_tray().await,
			Mode::Open(page) => open(&page, activation).await,
		}
	});
	if let Err(error) = result {
		eprintln!("{error}");
		std::process::exit(1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stale_display_variables_are_not_a_session() {
		let directory = std::env::temp_dir().join(format!("pyroshine-tray-test-{}", std::process::id()));
		std::fs::create_dir_all(&directory).unwrap();
		let runtime = Some(directory.clone().into_os_string());
		assert_eq!(
			display_server(Some("wayland-stale".into()), runtime.clone(), Some(":987".into())),
			None,
			"no socket answers"
		);
		assert_eq!(display_server(None, None, None), None);
		assert_eq!(
			display_server(Some("".into()), runtime.clone(), Some("not-a-display".into())),
			None
		);
		let listener = std::os::unix::net::UnixListener::bind(directory.join("wayland-live")).unwrap();
		assert_eq!(
			display_server(Some("wayland-live".into()), runtime, None).as_deref(),
			Some("Wayland (wayland-live)")
		);
		drop(listener);
		std::fs::remove_dir_all(&directory).unwrap();
	}
}
