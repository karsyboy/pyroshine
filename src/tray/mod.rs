//! Desktop tray: a status icon, pairing notifications and shortcuts into the
//! web interface.
//!
//! The server starts the tray itself while the user's desktop session runs
//! (see [`launcher`]), as a child process of the same executable
//! (`pyroshine tray --attached`); on a headless host nothing starts. The tray
//! owns no streaming resources and talks to the server only through its
//! management interface on the session bus, so hiding it, a crash or a
//! desktop logout leaves the server, its web interface and any stream
//! untouched. Every management task opens in the browser.
//!
//! `pyroshine open [page]` (the application launcher) opens a page of the web
//! interface, through the running tray when there is one.

mod appearance;
mod attach;
mod browser;
mod icons;
mod instance;
mod item;
mod launcher;
mod notifications;
mod service;

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

pub use launcher::Launcher;

/// The XDG activation token the launcher passed for the user's click.
fn launch_activation_token() -> Option<String> {
	["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
		.iter()
		.find_map(|name| std::env::var(name).ok())
		.and_then(|token| browser::activation_token(&token))
}

/// Whether a display server answers: the Wayland socket or the X11 socket
/// named by the given variables accepts a connection. A variable alone proves
/// nothing (it may be left over from an ended session).
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

/// Open `page` of the web interface in the default browser.
pub async fn open(page: &str) -> Result<(), String> {
	let activation = launch_activation_token();
	let page = browser::page(page);
	let connection = instance::session_bus().await.ok();
	if let Some(connection) = &connection
		&& instance::forward(connection, &page, activation.as_deref()).await
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
		&browser::address(base.as_deref(), &page),
		activation,
	)
	.await
}

/// Run the tray until it is hidden, the session ends or, when `attached`,
/// the server that started it exits (its end of standard input closes).
pub async fn run(attached: bool) -> Result<(), String> {
	// The server starts the tray as /proc/self/exe, which process lists would
	// show as "exe".
	let _ = std::fs::write("/proc/self/comm", "pyroshine-tray");
	let session = display_server(
		std::env::var_os("WAYLAND_DISPLAY"),
		std::env::var_os("XDG_RUNTIME_DIR"),
		std::env::var("DISPLAY").ok(),
	);
	let Some(session) = session else {
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
	let hide = Arc::new(Notify::new());
	let tray = item::spawn(service.clone(), notifier.clone(), hide.clone()).await;
	let appearance = tokio::spawn(appearance::follow(connection.clone(), tray.clone()));
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

	let parent = async {
		if attached {
			// The server keeps the write end open and never writes; end of
			// file means it exited, however it exited. A detached thread, not
			// tokio's stdin, so the blocked read never holds up exiting.
			let (exited, wait) = tokio::sync::oneshot::channel::<()>();
			std::thread::spawn(move || {
				let mut byte = [0u8; 1];
				while matches!(std::io::stdin().read(&mut byte), Ok(1..)) {}
				let _ = exited.send(());
			});
			let _ = wait.await;
		} else {
			std::future::pending::<()>().await;
		}
	};

	let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
	let mut hangup = signal(SignalKind::hangup()).map_err(|error| error.to_string())?;
	tokio::select! {
		() = hide.notified() => tracing::info!("Tray icon hidden from its menu; Pyroshine keeps running"),
		() = parent => tracing::info!("The server exited; the tray exits"),
		_ = tokio::signal::ctrl_c() => {},
		_ = terminate.recv() => {},
		_ = hangup.recv() => {},
		// The session bus closed (logout): this session's tray ends with it.
		_ = following => tracing::info!("The session bus closed; the tray exits"),
	}
	opener.abort();
	appearance.abort();
	Ok(())
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
