//! What Steam reports about the game a session launched.
//!
//! A Steam game's application is `steam … steam://rungameid/<id>`: the unit
//! is the Steam client, which may update the game first, then launches it,
//! and keeps running after the game exits. The unit's state therefore says
//! nothing about the game. Steam records the game's progress in its own logs:
//!
//! - `content_log.txt`: `AppID 892970 state changed : Fully Installed,App Running,`
//!   (also `Update Running`, `Update Started`, …);
//! - `gameprocess_log.txt`: `AppID 892970 adding PID 91461 as a tracked process`
//!   and `AppID 892970 no longer tracking PID 91461, exit code 0`.
//!
//! One observer per Steam session follows the lines appended to both after
//! the launch and publishes [`SteamGameState`] for the management interface
//! and `/serverinfo`. It stats two files once a second on a blocking thread
//! and reads at most [`MAX_READ_BYTES`] per file per check: Steam's log
//! directory is written constantly by other logs, so a file watch would wake
//! as often for nothing. It owns no session resources, holds no shutdown
//! token and ends with the session; nothing here touches a media path.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_shutdown::ShutdownManager;
use moonshine_management::dto::{SteamGameDetails, SteamGameState};
use tokio::sync::watch;

use super::application::ApplicationConfig;
use super::manager::SessionShutdownReason;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Bound of one check per log file; the rest is read on the next check.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// A longer unterminated line is not a Steam log line and is dropped.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// The Steam app a command launches, from `steam://rungameid/<id>`.
pub(crate) fn steam_app_id(command: &[String]) -> Option<u32> {
	let target = crate::app_scanner::launcher_target(command)?;
	let id = target.strip_prefix("steam:")?.parse::<u64>().ok()?;
	// Non-Steam shortcuts use a packed 64-bit rungameid, but Steam's logs
	// report their saved 32-bit app ID just like a store game's app ID.
	let app_id = if id & 0xffff_ffff == 0x0200_0000 && id >> 32 >= 0x8000_0000 {
		(id >> 32) as u32
	} else {
		u32::try_from(id).ok()?
	};
	(app_id > 0).then_some(app_id)
}

/// Where the Steam client that `command` starts writes its logs.
pub(crate) fn logs_dir(command: &[String], home: &Path) -> PathBuf {
	let flatpak = command
		.first()
		.is_some_and(|program| Path::new(program).file_name().is_some_and(|name| name == "flatpak"))
		&& command.iter().any(|argument| argument == "com.valvesoftware.Steam");
	if flatpak {
		return home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/logs");
	}
	// `~/.steam/root` points at the client's installation on every native
	// install (distribution packages, SteamOS, the Valve tarball).
	std::fs::canonicalize(home.join(".steam/root"))
		.unwrap_or_else(|_| home.join(".local/share/Steam"))
		.join("logs")
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
	/// The states in a `state changed` line.
	States(Vec<String>),
	ProcessStarted,
	ProcessEnded,
}

/// The event a log line reports about `app_id`, if any.
fn parse_line(line: &str, app_id: u32) -> Option<Event> {
	let text = line.split_once("] ").map_or(line, |(_, text)| text);
	let (id, rest) = text.strip_prefix("AppID ")?.split_once(' ')?;
	if id.parse::<u32>().ok()? != app_id {
		return None;
	}
	if let Some(states) = rest.strip_prefix("state changed : ") {
		// A note may follow: `Update Required,Fully Installed, (Update delayed …)`.
		let states = states.split(" (").next().unwrap_or_default();
		return Some(Event::States(
			states
				.split(',')
				.map(str::trim)
				.filter(|state| !state.is_empty())
				.map(String::from)
				.collect(),
		));
	}
	if rest.starts_with("adding PID ") {
		return Some(Event::ProcessStarted);
	}
	if rest.starts_with("no longer tracking PID ") {
		return Some(Event::ProcessEnded);
	}
	None
}

/// The game's state from the events seen so far.
#[derive(Debug)]
struct Tracker {
	state: SteamGameState,
	/// Processes Steam tracks for the game.
	processes: usize,
}

impl Tracker {
	fn new() -> Self {
		Self {
			state: SteamGameState::Launching,
			processes: 0,
		}
	}

	/// Apply `event`; returns whether the state changed.
	fn apply(&mut self, event: Event) -> bool {
		let next = match event {
			Event::States(states) => {
				let has = |name: &str| states.iter().any(|state| state == name);
				if has("Update Running") || has("Update Started") {
					SteamGameState::Updating
				} else if has("App Running") {
					SteamGameState::Running
				} else {
					match self.state {
						// Steam launches the game once the update is applied.
						SteamGameState::Updating => SteamGameState::Launching,
						SteamGameState::Running if self.processes == 0 => SteamGameState::Exited,
						state => state,
					}
				}
			},
			Event::ProcessStarted => {
				self.processes += 1;
				SteamGameState::Running
			},
			Event::ProcessEnded => {
				self.processes = self.processes.saturating_sub(1);
				if self.processes == 0 && self.state == SteamGameState::Running {
					SteamGameState::Exited
				} else {
					self.state
				}
			},
		};
		let changed = next != self.state;
		self.state = next;
		changed
	}
}

/// Lines appended to a log file since the observer started.
#[derive(Debug)]
struct LogTail {
	path: PathBuf,
	offset: u64,
	/// Device and inode of the file read, to notice Steam's log rotation.
	identity: Option<(u64, u64)>,
	partial: Vec<u8>,
}

impl LogTail {
	/// Start after the current end: earlier lines belong to earlier runs.
	fn at_end(path: PathBuf) -> Self {
		let metadata = std::fs::metadata(&path).ok();
		Self {
			offset: metadata.as_ref().map_or(0, |metadata| metadata.len()),
			identity: metadata.map(|metadata| (metadata.dev(), metadata.ino())),
			path,
			partial: Vec::new(),
		}
	}

	fn read_lines(&mut self) -> Vec<String> {
		let Ok(metadata) = std::fs::metadata(&self.path) else {
			return Vec::new();
		};
		let identity = Some((metadata.dev(), metadata.ino()));
		if identity != self.identity || metadata.len() < self.offset {
			// Rotated (renamed to `.previous`) or truncated: a new file to read
			// from its start.
			self.identity = identity;
			self.offset = 0;
			self.partial.clear();
		}
		if metadata.len() == self.offset {
			return Vec::new();
		}
		let mut chunk = Vec::new();
		let read = File::open(&self.path).and_then(|mut file| {
			file.seek(SeekFrom::Start(self.offset))?;
			file.take(MAX_READ_BYTES).read_to_end(&mut chunk)
		});
		let Ok(read) = read else {
			return Vec::new();
		};
		self.offset += read as u64;
		self.partial.extend_from_slice(&chunk);

		let mut lines = Vec::new();
		while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
			let line: Vec<u8> = self.partial.drain(..=end).collect();
			lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
		}
		if self.partial.len() > MAX_LINE_BYTES {
			self.partial.clear();
		}
		lines
	}
}

fn details(app_id: u32, state: SteamGameState) -> SteamGameDetails {
	SteamGameDetails {
		app_id,
		state,
		since_ms: SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.map_or(0, |elapsed| elapsed.as_millis() as u64),
	}
}

/// Follow Steam's state of the game `application` launches until `stop`.
/// `None` for applications that do not launch a Steam game.
///
/// Call it before the application starts: the current ends of Steam's logs
/// are taken here (two `stat` calls), and only later lines are followed.
pub(crate) fn observe(
	application: &ApplicationConfig,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Option<watch::Receiver<Option<SteamGameDetails>>> {
	if !application.mode.is_game() {
		return None;
	}
	let app_id = steam_app_id(&application.command)?;
	let home = std::env::var_os("HOME")?;
	let logs = logs_dir(&application.command, Path::new(&home));
	Some(observe_logs(
		app_id,
		application.title.clone(),
		logs,
		stop,
		POLL_INTERVAL,
	))
}

fn observe_logs(
	app_id: u32,
	title: String,
	logs: PathBuf,
	stop: ShutdownManager<SessionShutdownReason>,
	interval: Duration,
) -> watch::Receiver<Option<SteamGameDetails>> {
	let (tx, rx) = watch::channel(Some(details(app_id, SteamGameState::Launching)));
	tracing::debug!(app_id, logs = %logs.display(), "Following Steam's state of the launched game");
	let mut tails = [
		LogTail::at_end(logs.join("content_log.txt")),
		LogTail::at_end(logs.join("gameprocess_log.txt")),
	];
	tokio::spawn(async move {
		let follow = async {
			let mut tracker = Tracker::new();
			let mut ticks = tokio::time::interval(interval);
			ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				ticks.tick().await;
				let Ok((returned, lines)) = tokio::task::spawn_blocking(move || {
					let mut lines = tails[0].read_lines();
					lines.extend(tails[1].read_lines());
					(tails, lines)
				})
				.await
				else {
					return;
				};
				tails = returned;
				for event in lines.iter().filter_map(|line| parse_line(line, app_id)) {
					if tracker.apply(event) {
						tracing::info!(
							app_id,
							state = tracker.state.as_str(),
							"Steam reports {title} {}",
							tracker.state.as_str()
						);
						tx.send_replace(Some(details(app_id, tracker.state)));
					}
				}
			}
		};
		tokio::select! {
			_ = stop.wait_shutdown_triggered() => {},
			() = follow => {},
		}
	});
	rx
}

#[cfg(test)]
mod tests {
	use std::io::Write;

	use super::*;

	// Lines as Steam writes them (from a real client's logs).
	const RUNNING: &str = "[2026-10-21 13:45:13] AppID 892970 state changed : Fully Installed,App Running,";
	const STOPPED: &str = "[2026-10-21 13:46:54] AppID 892970 state changed : Fully Installed,";
	const UPDATING: &str =
		"[2026-10-21 13:44:02] AppID 892970 state changed : Update Required,Update Running,Fully Installed,";
	const OTHER: &str = "[2026-10-21 13:46:54] AppID 2871520 state changed : Update Required,Fully Installed,Prefetching Info, (Update delayed for 48029 secs)";
	const ADDED: &str = "[2026-10-21 13:45:13] AppID 892970 adding PID 91461 as a tracked process \"/home/u/.local/share/Steam/ubuntu12_32/reaper SteamLaunch AppId=892970\"";
	const REMOVED: &str = "[2026-10-21 13:46:54] AppID 892970 no longer tracking PID 91461, exit code 0";

	fn command(arguments: &[&str]) -> Vec<String> {
		arguments.iter().map(|argument| argument.to_string()).collect()
	}

	#[test]
	fn steam_games_are_recognized_by_their_launch_url() {
		let shortcut = (0xf123_4567_u64 << 32) | 0x0200_0000;
		assert_eq!(
			steam_app_id(&command(&["steam", &format!("steam://rungameid/{shortcut}")])),
			Some(0xf123_4567)
		);
		assert_eq!(
			steam_app_id(&command(&["steam", "steam://rungameid/18446744073709551615"])),
			None
		);
		assert_eq!(
			steam_app_id(&command(&["/usr/bin/steam", "-bigpicture", "steam://rungameid/892970"])),
			Some(892970)
		);
		assert_eq!(
			steam_app_id(&command(&[
				"flatpak",
				"run",
				"com.valvesoftware.Steam",
				"steam://rungameid/620"
			])),
			Some(620)
		);
		assert_eq!(
			steam_app_id(&command(&["/usr/bin/steam", "steam://open/bigpicture"])),
			None
		);
		assert_eq!(
			steam_app_id(&command(&["/usr/bin/lutris", "lutris:rungameid/42"])),
			None
		);
	}

	#[test]
	fn logs_are_found_for_native_and_flatpak_steam() {
		let home = tempfile::tempdir().unwrap();
		let native = command(&["/usr/bin/steam", "steam://rungameid/620"]);
		assert_eq!(
			logs_dir(&native, home.path()),
			home.path().join(".local/share/Steam/logs")
		);

		let root = home.path().join("games/Steam");
		std::fs::create_dir_all(&root).unwrap();
		std::fs::create_dir_all(home.path().join(".steam")).unwrap();
		std::os::unix::fs::symlink(&root, home.path().join(".steam/root")).unwrap();
		assert_eq!(
			logs_dir(&native, home.path()),
			root.canonicalize().unwrap().join("logs")
		);

		let flatpak = command(&[
			"/usr/bin/flatpak",
			"run",
			"com.valvesoftware.Steam",
			"steam://rungameid/620",
		]);
		assert_eq!(
			logs_dir(&flatpak, home.path()),
			home.path()
				.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/logs")
		);
	}

	#[test]
	fn parses_only_the_launched_games_lines() {
		assert_eq!(
			parse_line(RUNNING, 892970),
			Some(Event::States(vec!["Fully Installed".into(), "App Running".into()]))
		);
		assert_eq!(parse_line(ADDED, 892970), Some(Event::ProcessStarted));
		assert_eq!(parse_line(REMOVED, 892970), Some(Event::ProcessEnded));
		assert_eq!(parse_line(OTHER, 892970), None);
		assert_eq!(
			parse_line(OTHER, 2871520),
			Some(Event::States(vec![
				"Update Required".into(),
				"Fully Installed".into(),
				"Prefetching Info".into()
			]))
		);
		assert_eq!(parse_line("[2026-10-21 13:45:13] Loaded 12 apps", 892970), None);
		assert_eq!(parse_line("", 892970), None);
	}

	#[test]
	fn follows_update_launch_run_and_exit() {
		let mut tracker = Tracker::new();
		let mut states = Vec::new();
		for line in [UPDATING, STOPPED, RUNNING, ADDED, REMOVED, STOPPED, RUNNING] {
			if tracker.apply(parse_line(line, 892970).unwrap()) {
				states.push(tracker.state);
			}
		}
		use SteamGameState::*;
		assert_eq!(states, vec![Updating, Launching, Running, Exited, Running]);
	}

	#[test]
	fn the_last_tracked_process_ends_the_game() {
		let mut tracker = Tracker::new();
		tracker.apply(Event::ProcessStarted);
		tracker.apply(Event::ProcessStarted);
		// While a process remains, a stopped content state is not an exit.
		assert!(!tracker.apply(Event::States(vec!["Fully Installed".into()])));
		assert!(!tracker.apply(Event::ProcessEnded));
		assert_eq!(tracker.state, SteamGameState::Running);
		assert!(tracker.apply(Event::ProcessEnded));
		assert_eq!(tracker.state, SteamGameState::Exited);
	}

	#[test]
	fn tails_appended_lines_across_partial_writes_and_rotation() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("content_log.txt");
		std::fs::write(&path, format!("{STOPPED}\n")).unwrap();
		let mut tail = LogTail::at_end(path.clone());
		assert!(tail.read_lines().is_empty(), "earlier runs are not replayed");

		let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
		write!(file, "{RUNNING}\n[2026-10-21 13:45:14] AppID 892970 ad").unwrap();
		assert_eq!(tail.read_lines(), vec![RUNNING.to_string()]);
		writeln!(file, "ding PID 7 as a tracked process").unwrap();
		assert_eq!(
			tail.read_lines(),
			vec!["[2026-10-21 13:45:14] AppID 892970 adding PID 7 as a tracked process".to_string()]
		);

		// Steam renames the full log to `.previous` and starts a new one.
		std::fs::rename(&path, directory.path().join("content_log.previous.txt")).unwrap();
		std::fs::write(&path, format!("{REMOVED}\n")).unwrap();
		assert_eq!(tail.read_lines(), vec![REMOVED.to_string()]);
	}

	#[test]
	fn reads_are_bounded_per_check() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("gameprocess_log.txt");
		let mut tail = LogTail::at_end(path.clone());
		let line = "x".repeat(99);
		let lines = (MAX_READ_BYTES as usize / 100) * 3;
		std::fs::write(&path, format!("{line}\n").repeat(lines)).unwrap();
		let mut seen = 0;
		for _ in 0..3 {
			let read = tail.read_lines().len();
			assert!(read <= MAX_READ_BYTES as usize / 100, "{read}");
			seen += read;
		}
		assert_eq!(seen, lines);
		// An unterminated run of garbage is dropped rather than kept growing.
		std::fs::write(&path, vec![b'y'; MAX_LINE_BYTES * 2]).unwrap();
		assert!(tail.read_lines().is_empty());
		assert!(tail.partial.len() <= MAX_LINE_BYTES);
	}

	#[tokio::test]
	async fn publishes_states_until_the_session_stops() {
		let directory = tempfile::tempdir().unwrap();
		let logs = directory.path().to_path_buf();
		let content = logs.join("content_log.txt");
		let process = logs.join("gameprocess_log.txt");
		std::fs::write(&content, format!("{RUNNING}\n")).unwrap();
		let stop = ShutdownManager::new();
		let mut rx = observe_logs(892970, "Valheim".into(), logs, stop.clone(), Duration::from_millis(20));
		assert_eq!(rx.borrow().as_ref().unwrap().state, SteamGameState::Launching);

		let append = |path: &Path, line: &str| {
			let mut file = std::fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(path)
				.unwrap();
			writeln!(file, "{line}").unwrap();
		};
		let next = async |rx: &mut watch::Receiver<Option<SteamGameDetails>>| {
			tokio::time::timeout(Duration::from_secs(5), rx.changed())
				.await
				.unwrap()
				.unwrap();
			rx.borrow_and_update().as_ref().unwrap().state
		};
		// Lines written right after the call (an update at Steam's start) count.
		append(&content, UPDATING);
		assert_eq!(next(&mut rx).await, SteamGameState::Updating);
		append(&content, RUNNING);
		append(&process, ADDED);
		assert_eq!(next(&mut rx).await, SteamGameState::Running);
		append(&process, REMOVED);
		assert_eq!(next(&mut rx).await, SteamGameState::Exited);

		stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		tokio::time::timeout(Duration::from_secs(5), async { while rx.changed().await.is_ok() {} })
			.await
			.expect("the observer ends with the session");
	}
}
