//! Pyroshine desktop app: system tray, pairing notifications, dashboard and
//! settings over the daemon's local management interface.
//!
//! The app is an optional client of a separately running Pyroshine service.
//! It never starts, stops or supervises the daemon; quitting it (or a crash)
//! leaves the server and any stream untouched. All state comes from the
//! daemon over the session bus (see `moonshine_management`).

// A development-mode build loads the Vite dev server (`devUrl`) instead of the
// embedded frontend; never let one ship. See the crate's `custom-protocol` feature.
#[cfg(all(dev, not(debug_assertions)))]
compile_error!("release builds must enable the default `custom-protocol` feature");

mod commands;
mod daemon;
mod icons;
mod instance;
mod notifications;
mod tray;
mod window;

use std::sync::Arc;

use tauri::RunEvent;
use tracing_subscriber::EnvFilter;

/// Command-line options.
struct Options {
	/// Start in the tray without opening the window (autostart).
	background: bool,
	/// Page to open, for example `clients`.
	page: String,
}

fn parse_args() -> Options {
	let mut options = Options {
		background: false,
		page: "dashboard".into(),
	};
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--background" => options.background = true,
			"--page" => {
				if let Some(page) = args.next() {
					options.page = page;
				}
			},
			"--version" => {
				println!("pyroshine-ui {}", env!("CARGO_PKG_VERSION"));
				std::process::exit(0);
			},
			"--help" | "-h" => {
				println!(
					"Usage: pyroshine-ui [--background] [--page dashboard|clients|settings|diagnostics]\n\n\
					 Desktop app for a running Pyroshine server. --background starts in the system tray."
				);
				std::process::exit(0);
			},
			other => eprintln!("Ignoring unknown argument {other}"),
		}
	}
	options
}

fn main() {
	let options = parse_args();
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::try_from_env("PYROSHINE_UI_LOG").unwrap_or_else(|_| EnvFilter::new("warn")))
		.init();
	window::apply_webkit_workarounds();
	let launch_activation = window::take_launch_activation_token();

	// One instance per user: a second launch (desktop entry, notification,
	// autostart) asks the running instance to show the requested page.
	// A background launch (autostart) leaves a running instance as it is.
	let forward = (!options.background).then_some(options.page.as_str());
	let instance = tauri::async_runtime::block_on(instance::acquire(forward, launch_activation.as_deref()));
	let (connection, show_requests) = match instance {
		instance::Instance::Primary { connection, requests } => (Some(connection), Some(requests)),
		instance::Instance::Forwarded => return,
		instance::Instance::NoBus(error) => {
			tracing::error!("The D-Bus session bus is unavailable ({error}); Pyroshine cannot be reached.");
			(None, None)
		},
	};
	let daemon = Arc::new(daemon::Daemon::new(connection));

	let app = tauri::Builder::default()
		.manage(daemon.clone())
		.invoke_handler(tauri::generate_handler![
			commands::overview,
			commands::refresh,
			commands::end_session,
			commands::approve_pairing,
			commands::reject_pairing,
			commands::revoke_client,
			commands::rename_client,
			commands::load_config,
			commands::validate_config,
			commands::save_config,
			commands::quit,
		])
		.setup(move |app| {
			let handle = app.handle().clone();
			if let Some(mut requests) = show_requests {
				let handle = handle.clone();
				tauri::async_runtime::spawn(async move {
					while let Some((page, activation)) = requests.recv().await {
						window::show(&handle, &page, activation);
					}
				});
			}
			let notifier = notifications::Notifier::new(handle.clone(), daemon.connection());
			let tray = tray::spawn(handle.clone(), daemon.clone(), notifier.clone());
			daemon.start(handle.clone(), tray, notifier);
			if !options.background {
				window::show(&handle, &options.page, launch_activation);
			}
			Ok(())
		})
		.build(tauri::generate_context!())
		.expect("the desktop app starts");

	app.run(|_, event| {
		// Closing the window keeps the tray; only "Quit" exits (with a code).
		if let RunEvent::ExitRequested { code: None, api, .. } = event {
			api.prevent_exit();
		}
	});
}

#[cfg(test)]
mod tests {
	/// A development-mode binary loads the Vite dev server (`devUrl`) and shows
	/// "Could not connect to localhost" when installed. Every build that runs
	/// tests must be a production build.
	#[test]
	#[allow(clippy::assertions_on_constants)]
	fn builds_embed_the_frontend() {
		assert!(
			!cfg!(dev),
			"built without the custom-protocol feature; the window would load the dev server"
		);
	}
}
