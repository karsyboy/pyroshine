//! Refuse a Steam launch that another Steam client would take over.
//!
//! Steam is single-instance per user: a second `steam steam://rungameid/...`
//! hands the URL to the client that is already running and exits. When that
//! client belongs to the host's desktop, the game opens there, not in the
//! session's compositor, and the stream ends a moment later with nothing to
//! show. Checking before the unit starts turns that into a clear launch error
//! without touching the running client: Pyroshine never closes, signals or
//! takes over a Steam it did not start.
//!
//! The check runs once per launch, after the previous session's unit has been
//! stopped, so a Steam still running belongs to someone else. It reads `/proc`
//! (one directory listing and a few small files per process of the service's
//! user); it is never on a media path.
//!
//! Applications with `pre_command` hooks are not checked: hooks run inside the
//! unit before its command, and the documented way to close a desktop Steam
//! automatically is such a hook. Without the check, the launch keeps the
//! earlier behavior and the quick exit is explained afterwards.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::application::ApplicationConfig;
use super::failure::SessionFailure;

/// A running Steam client found by [`find_steam_client`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SteamClient {
	pub pid: u32,
}

/// Whether `command` starts the Steam client, natively (`steam`, `steam.sh`,
/// any path to them) or as the Flathub Flatpak.
pub(crate) fn launches_steam(command: &[String]) -> bool {
	let Some(program) = command.first() else {
		return false;
	};
	let name = Path::new(program)
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();
	match name {
		"steam" | "steam.sh" => true,
		"flatpak" => command
			.iter()
			.skip(1)
			.any(|argument| argument == "com.valvesoftware.Steam"),
		_ => false,
	}
}

/// The first live process named `steam` owned by `uid` under `proc_root`.
///
/// `/proc/<pid>` is owned by the process's effective user, so other users'
/// processes are skipped without reading them. The main client process is
/// called `steam` both natively and inside the Flatpak; zombies (already
/// exited, waiting for their parent) are not running clients.
pub(crate) fn find_steam_client(proc_root: &Path, uid: u32) -> Option<SteamClient> {
	let entries = std::fs::read_dir(proc_root).ok()?;
	let mut pids: Vec<u32> = entries
		.flatten()
		.filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
		.collect();
	// Lowest PID first, so a report names the longest-running client.
	pids.sort_unstable();
	pids.into_iter().find_map(|pid| {
		let directory = proc_root.join(pid.to_string());
		if std::fs::metadata(&directory).ok()?.uid() != uid {
			return None;
		}
		let comm = std::fs::read_to_string(directory.join("comm")).ok()?;
		if comm.trim_end_matches('\n') != "steam" {
			return None;
		}
		let stat = std::fs::read_to_string(directory.join("stat")).ok()?;
		// `pid (comm) state ...`: the state follows the last parenthesis.
		let state = stat.rsplit_once(')')?.1.split_whitespace().next()?;
		(state != "Z" && state != "X").then_some(SteamClient { pid })
	})
}

/// The launch failure for `config` when a Steam client of this user is
/// already running, or `None` when the launch may proceed.
pub(crate) fn check(config: &ApplicationConfig, proc_root: &Path, uid: u32) -> Option<SessionFailure> {
	if !config.mode.is_game() || !config.pre_command.is_empty() || !launches_steam(&config.command) {
		return None;
	}
	let client = find_steam_client(proc_root, uid)?;
	tracing::warn!(
		pid = client.pid,
		title = config.title,
		"Steam is already running outside the session; refusing to launch so it does not open on the host's desktop"
	);
	Some(
		SessionFailure::new(
			"steam_already_running",
			format!(
				"Steam is already running on the host (process {}), so it would open {} on the host's desktop \
				 instead of in the stream.",
				client.pid, config.title
			),
		)
		.with_hint(
			"Quit Steam on the host's desktop, then launch again. To close it automatically, see \"Close a \
			 desktop Steam before streaming Steam\" in Pyroshine's TIPS.md.",
		),
	)
}

/// [`check`] against this process's `/proc` and user, off the async runtime.
pub(crate) async fn check_current_user(config: &ApplicationConfig) -> Option<SessionFailure> {
	if !config.mode.is_game() || !config.pre_command.is_empty() || !launches_steam(&config.command) {
		return None;
	}
	let config = config.clone();
	// SAFETY: geteuid has no preconditions and cannot fail.
	let uid = unsafe { libc::geteuid() };
	tokio::task::spawn_blocking(move || check(&config, Path::new("/proc"), uid))
		.await
		.unwrap_or_else(|error| {
			// The check is advisory: a failed scan must not block launches.
			tracing::warn!("Steam instance check did not complete: {error}");
			None
		})
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	fn command(arguments: &[&str]) -> Vec<String> {
		arguments.iter().map(|argument| argument.to_string()).collect()
	}

	fn process(root: &Path, pid: u32, comm: &str, state: &str) {
		let directory = root.join(pid.to_string());
		fs::create_dir_all(&directory).unwrap();
		fs::write(directory.join("comm"), format!("{comm}\n")).unwrap();
		fs::write(
			directory.join("stat"),
			format!("{pid} ({comm}) {state} 1 {pid} {pid} 0 -1 4194560"),
		)
		.unwrap();
	}

	fn uid(root: &Path) -> u32 {
		fs::metadata(root).unwrap().uid()
	}

	fn steam_game() -> ApplicationConfig {
		ApplicationConfig {
			title: "Valheim".into(),
			command: command(&["/usr/bin/steam", "-bigpicture", "steam://rungameid/892970"]),
			..Default::default()
		}
	}

	#[test]
	fn recognizes_native_and_flatpak_steam_commands() {
		assert!(launches_steam(&command(&["/usr/bin/steam", "steam://rungameid/620"])));
		assert!(launches_steam(&command(&["steam", "-bigpicture"])));
		assert!(launches_steam(&command(&[
			"/run/current-system/sw/bin/steam",
			"steam://open/bigpicture"
		])));
		assert!(launches_steam(&command(&["/home/u/.steam/steam.sh", "-silent"])));
		assert!(launches_steam(&command(&[
			"/usr/bin/flatpak",
			"run",
			"com.valvesoftware.Steam",
			"steam://rungameid/620"
		])));
		assert!(!launches_steam(&command(&[
			"/usr/bin/flatpak",
			"run",
			"org.prismlauncher.PrismLauncher"
		])));
		assert!(!launches_steam(&command(&["/usr/bin/lutris", "lutris:rungameid/42"])));
		assert!(!launches_steam(&command(&["/usr/bin/steamtinkerlaunch"])));
		assert!(!launches_steam(&[]));
	}

	#[test]
	fn finds_a_running_steam_client_and_skips_zombies_and_other_names() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		process(root, 40, "steamwebhelper", "S");
		process(root, 41, "steam", "Z");
		fs::create_dir_all(root.join("self")).unwrap();
		assert_eq!(find_steam_client(root, uid(root)), None);

		process(root, 900, "steam", "S");
		process(root, 77, "steam", "R");
		assert_eq!(find_steam_client(root, uid(root)), Some(SteamClient { pid: 77 }));
	}

	#[test]
	fn ignores_other_users_processes() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		process(root, 77, "steam", "S");
		assert_eq!(find_steam_client(root, uid(root).wrapping_add(1)), None);
	}

	#[test]
	fn a_running_client_fails_a_steam_launch_with_a_clear_cause() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		process(root, 1234, "steam", "S");

		let failure = check(&steam_game(), root, uid(root)).expect("conflict");
		assert_eq!(failure.code, "steam_already_running");
		assert!(failure.message.contains("process 1234"), "{}", failure.message);
		assert!(failure.message.contains("Valheim"), "{}", failure.message);
		assert!(failure.client_message().contains("Quit Steam"));
	}

	#[test]
	fn launches_without_a_running_client_or_outside_steam_proceed() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		assert_eq!(check(&steam_game(), root, uid(root)), None);

		process(root, 1234, "steam", "S");
		let lutris = ApplicationConfig {
			title: "Lutris game".into(),
			command: command(&["/usr/bin/lutris", "lutris:rungameid/42"]),
			..Default::default()
		};
		assert_eq!(check(&lutris, root, uid(root)), None);
	}

	/// A hook may be what closes the desktop Steam (TIPS.md), and Desktop
	/// Mode streams the desktop Steam belongs to: neither is checked.
	#[test]
	fn hooks_and_desktop_mode_skip_the_check() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		process(root, 1234, "steam", "S");

		let mut hooked = steam_game();
		hooked.pre_command = vec![command(&["/usr/bin/true"])];
		assert_eq!(check(&hooked, root, uid(root)), None);

		let mut desktop = steam_game();
		desktop.mode = crate::session::application::SessionMode::Desktop;
		assert_eq!(check(&desktop, root, uid(root)), None);
	}
}
