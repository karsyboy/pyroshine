use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::icons::IconIndex;
use super::watch::{Filter, WatchTarget};
use super::{ArtworkOrigin, Discovered, SourceScan, Unit, UnitError, expand_path, launcher_target};
use crate::session::application::ApplicationConfig;

fn default_true() -> bool {
	true
}

fn default_false() -> bool {
	false
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopApplicationScannerConfig {
	/// Directories to scan recursively for `.desktop` files.
	pub directories: Vec<PathBuf>,

	/// Whether terminal-based entries should be included.
	#[serde(default = "default_false")]
	pub include_terminal: bool,

	/// Whether to resolve desktop entry icons into Moonshine boxart paths.
	#[serde(default = "default_true")]
	pub resolve_icons: bool,

	/// Commands to run before launching each scanned application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after each scanned application's session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// systemd StandardOutput value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<String>,

	/// systemd StandardError value for launched applications.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,

	/// Seconds to wait for each scanned application to reach an active state after launch.
	#[serde(default = "crate::session::application::default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

const SKIPPED_EXEC_FIELD_CODES: [char; 10] = ['f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'v', 'm'];

/// Subdirectories watched per configured directory; deeper or wider trees
/// are still covered by the periodic check.
const MAX_WATCHED_DIRECTORIES: usize = 256;

/// One parsed, launchable entry before icon resolution.
struct Parsed {
	file_id: String,
	path: PathBuf,
	icon: Option<String>,
	application: ApplicationConfig,
}

/// Scan every configured directory, one unit per directory.
///
/// Entries are identified by their desktop file ID (the path below the
/// scanned directory, `/` replaced by `-`). As in the XDG menu specification,
/// the first directory that provides an ID wins: an entry in a user
/// directory listed first overrides, or with `Hidden`/`NoDisplay` hides, the
/// system entry of the same ID.
pub(crate) fn scan(config: &DesktopApplicationScannerConfig, icons: &mut IconIndex) -> SourceScan {
	let mut claimed = std::collections::HashSet::new();
	let mut dedupe_keys = std::collections::HashSet::new();
	let mut parsed_units = Vec::new();

	for directory in &config.directories {
		let Some(directory) = expand_path(directory) else {
			parsed_units.push(Err(Unit::failed(
				directory.display().to_string(),
				UnitError::Unavailable(format!("Cannot expand {}", directory.display())),
			)));
			continue;
		};
		let key = directory.display().to_string();

		match fs::metadata(&directory) {
			Ok(metadata) if metadata.is_dir() => {},
			Ok(_) => {
				parsed_units.push(Err(Unit::failed(
					key,
					UnitError::Unavailable(format!("{} is not a directory", directory.display())),
				)));
				continue;
			},
			Err(error) => {
				tracing::debug!(
					"Skipping unavailable desktop application directory '{}': {error}",
					directory.display()
				);
				parsed_units.push(Err(Unit::failed(key, UnitError::io(directory.display(), &error))));
				continue;
			},
		}

		let mut files: Vec<PathBuf> = WalkDir::new(&directory)
			.follow_links(true)
			.into_iter()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_type().is_file())
			.map(|entry| entry.into_path())
			.filter(|path| {
				path.extension()
					.and_then(|extension| extension.to_str())
					.is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
			})
			.collect();
		// Directory order is arbitrary; sort for stable identities and order.
		files.sort();

		let mut parsed = Vec::new();
		for path in files {
			let file_id = desktop_file_id(&directory, &path);
			let entry = match DesktopEntry::load(&path) {
				Ok(Some(entry)) => entry,
				Ok(None) | Err(()) => continue,
			};
			// Any entry claims its ID, including one that hides itself.
			if !claimed.insert(file_id.clone()) {
				continue;
			}
			let Some(item) = parse_desktop_application(&entry, &path, file_id, config) else {
				continue;
			};
			let dedupe_key = (
				item.application.title.trim().to_ascii_lowercase(),
				item.application.command.join("\u{1f}"),
			);
			if dedupe_keys.insert(dedupe_key) {
				parsed.push(item);
			}
		}
		parsed_units.push(Ok((key, parsed)));
	}

	if config.resolve_icons {
		icons.prepare(
			parsed_units
				.iter()
				.filter_map(|unit| unit.as_ref().ok())
				.flat_map(|(_, parsed)| parsed.iter())
				.filter_map(|item| item.icon.as_deref()),
		);
	}

	let units = parsed_units
		.into_iter()
		.map(|unit| match unit {
			Err(unit) => unit,
			Ok((key, parsed)) => Unit::ok(
				key,
				parsed
					.into_iter()
					.map(|item| {
						let boxart = item
							.icon
							.as_deref()
							.filter(|_| config.resolve_icons)
							.and_then(|icon| icons.resolve(icon, &item.path));
						let mut discovered = Discovered::new(item.file_id, item.application);
						discovered.launches = launcher_target(&discovered.application.command);
						match boxart {
							Some(boxart) => discovered.with_artwork(vec![boxart], ArtworkOrigin::DesktopIcon),
							None => discovered,
						}
					})
					.collect(),
			),
		})
		.collect();
	SourceScan { units, failure: None }
}

/// Every configured directory and its subdirectories (bounded).
pub(crate) fn watch_targets(config: &DesktopApplicationScannerConfig) -> Vec<WatchTarget> {
	let mut targets = Vec::new();
	for directory in config.directories.iter().filter_map(|directory| expand_path(directory)) {
		let mut subdirectories: Vec<PathBuf> = WalkDir::new(&directory)
			.follow_links(true)
			.into_iter()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_type().is_dir())
			.map(|entry| entry.into_path())
			.take(MAX_WATCHED_DIRECTORIES)
			.collect();
		if subdirectories.is_empty() {
			// Missing: watch the path anyway so the periodic check notices it.
			subdirectories.push(directory);
		}
		targets.extend(
			subdirectories
				.into_iter()
				.map(|path| WatchTarget::directory(path, Filter::Suffix(".desktop")).with_subdirectories()),
		);
	}
	targets
}

/// The desktop file ID of `path` found below `directory`.
fn desktop_file_id(directory: &Path, path: &Path) -> String {
	path.strip_prefix(directory)
		.unwrap_or(path)
		.to_string_lossy()
		.replace('/', "-")
}

fn parse_desktop_application(
	entry: &DesktopEntry,
	path: &Path,
	file_id: String,
	config: &DesktopApplicationScannerConfig,
) -> Option<Parsed> {
	if !entry.is_application() || entry.is_hidden() || entry.is_no_display() {
		return None;
	}

	if entry.is_terminal() && !config.include_terminal {
		return None;
	}

	check_try_exec(entry);

	let Some(title) = entry.name() else {
		tracing::debug!("Skipping desktop entry without a name at '{}'.", path.display());
		return None;
	};

	let Some(command) = parse_exec_command(entry, path) else {
		tracing::debug!("Skipping desktop entry without a usable Exec at '{}'.", path.display());
		return None;
	};

	Some(Parsed {
		file_id,
		path: path.to_path_buf(),
		icon: entry
			.icon()
			.map(str::trim)
			.filter(|icon| !icon.is_empty())
			.map(str::to_string),
		application: ApplicationConfig {
			title,
			boxart: None,
			command,
			output_scale: None,
			pre_command: config.pre_command.clone(),
			post_command: config.post_command.clone(),
			stdout: config.stdout.clone(),
			stderr: config.stderr.clone(),
			launch_timeout_secs: config.launch_timeout_secs,
		},
	})
}

#[derive(Debug, Default)]
struct DesktopEntry {
	fields: HashMap<String, String>,
	localized_names: HashMap<String, String>,
}

impl DesktopEntry {
	fn load(path: &Path) -> Result<Option<Self>, ()> {
		let contents = fs::read_to_string(path)
			.map_err(|e| tracing::warn!("Failed to read desktop entry '{}': {e}", path.display()))?;
		let mut entry = DesktopEntry::default();
		let mut in_desktop_entry = false;
		let mut found_desktop_entry = false;

		for line in contents.lines() {
			let line = line.trim();
			if line.is_empty() || line.starts_with('#') {
				continue;
			}

			if line.starts_with('[') && line.ends_with(']') {
				let section = &line[1..line.len() - 1];
				if section == "Desktop Entry" {
					in_desktop_entry = true;
					found_desktop_entry = true;
					continue;
				}

				if found_desktop_entry {
					break;
				}

				in_desktop_entry = false;
				continue;
			}

			if !in_desktop_entry {
				continue;
			}

			let Some((key, value)) = line.split_once('=') else {
				continue;
			};

			let key = key.trim();
			let value = unescape_desktop_value(value.trim());
			if let Some(locale) = key.strip_prefix("Name[").and_then(|key| key.strip_suffix(']')) {
				entry.localized_names.insert(locale.to_string(), value);
			} else {
				entry.fields.insert(key.to_string(), value);
			}
		}

		if !found_desktop_entry {
			return Ok(None);
		}

		Ok(Some(entry))
	}

	fn is_application(&self) -> bool {
		self.fields.get("Type").is_some_and(|value| value == "Application")
	}

	fn is_hidden(&self) -> bool {
		self.boolean_field("Hidden")
	}

	fn is_no_display(&self) -> bool {
		self.boolean_field("NoDisplay")
	}

	fn is_terminal(&self) -> bool {
		self.boolean_field("Terminal")
	}

	fn boolean_field(&self, key: &str) -> bool {
		self.fields
			.get(key)
			.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
	}

	fn name(&self) -> Option<String> {
		self.fields.get("Name").cloned().or_else(|| {
			let locale_preferences = locale_preferences();
			for locale in locale_preferences {
				if let Some(name) = self.localized_names.get(&locale) {
					return Some(name.clone());
				}
			}

			None
		})
	}

	fn icon(&self) -> Option<&str> {
		self.fields.get("Icon").map(String::as_str)
	}

	fn exec(&self) -> Option<&str> {
		self.fields.get("Exec").map(String::as_str)
	}

	fn try_exec(&self) -> Option<&str> {
		self.fields.get("TryExec").map(String::as_str)
	}
}

fn check_try_exec(entry: &DesktopEntry) {
	let Some(try_exec) = entry.try_exec().map(str::trim).filter(|value| !value.is_empty()) else {
		return;
	};

	match resolve_executable(try_exec) {
		Some(path) if is_executable(&path) => {},
		Some(path) => tracing::debug!(
			"Including desktop entry despite non-executable TryExec '{}', resolved to '{}'.",
			try_exec,
			path.display()
		),
		None => tracing::debug!("Including desktop entry despite unresolved TryExec '{}'.", try_exec),
	}
}

fn parse_exec_command(entry: &DesktopEntry, desktop_file: &Path) -> Option<Vec<String>> {
	let exec = entry.exec()?.trim();
	if exec.is_empty() {
		return None;
	}

	let tokens = shlex::split(exec).or_else(|| split_exec_fallback(exec))?;
	let mut command = Vec::new();

	for token in tokens {
		if token == "%i" {
			if let Some(icon) = entry.icon().map(str::trim).filter(|icon| !icon.is_empty()) {
				command.push("--icon".to_string());
				command.push(icon.to_string());
			}
			continue;
		}

		let expanded = expand_exec_token(&token, entry, desktop_file);
		if !expanded.is_empty() {
			command.push(expanded);
		}
	}

	if command.is_empty() { None } else { Some(command) }
}

fn expand_exec_token(token: &str, entry: &DesktopEntry, desktop_file: &Path) -> String {
	let mut expanded = String::new();
	let mut chars = token.chars();

	while let Some(character) = chars.next() {
		if character != '%' {
			expanded.push(character);
			continue;
		}

		let Some(field_code) = chars.next() else {
			break;
		};

		match field_code {
			'%' => expanded.push('%'),
			'c' => expanded.push_str(entry.name().as_deref().unwrap_or_default()),
			'k' => expanded.push_str(&desktop_file.to_string_lossy()),
			'i' => {
				if let Some(icon) = entry.icon().map(str::trim).filter(|icon| !icon.is_empty()) {
					expanded.push_str(icon);
				}
			},
			field_code if SKIPPED_EXEC_FIELD_CODES.contains(&field_code) => {},
			_ => {},
		}
	}

	expanded
}

fn split_exec_fallback(exec: &str) -> Option<Vec<String>> {
	let mut tokens = Vec::new();
	let mut current = String::new();
	let mut in_single_quotes = false;
	let mut in_double_quotes = false;
	let mut chars = exec.chars().peekable();

	while let Some(character) = chars.next() {
		match character {
			'\'' if !in_double_quotes => in_single_quotes = !in_single_quotes,
			'"' if !in_single_quotes => in_double_quotes = !in_double_quotes,
			'\\' if in_double_quotes || in_single_quotes => {
				if let Some(next) = chars.next() {
					current.push(next);
				}
			},
			character if character.is_whitespace() && !in_single_quotes && !in_double_quotes => {
				if !current.is_empty() {
					tokens.push(std::mem::take(&mut current));
				}
			},
			_ => current.push(character),
		}
	}

	if in_single_quotes || in_double_quotes {
		return None;
	}

	if !current.is_empty() {
		tokens.push(current);
	}

	Some(tokens)
}

fn unescape_desktop_value(value: &str) -> String {
	let mut unescaped = String::new();
	let mut chars = value.chars();

	while let Some(character) = chars.next() {
		if character != '\\' {
			unescaped.push(character);
			continue;
		}

		match chars.next() {
			Some('n') => unescaped.push('\n'),
			Some('t') => unescaped.push('\t'),
			Some('r') => unescaped.push('\r'),
			Some('s') => unescaped.push(' '),
			Some('\\') => unescaped.push('\\'),
			Some(other) => unescaped.push(other),
			None => break,
		}
	}

	unescaped
}

fn locale_preferences() -> Vec<String> {
	let mut locales = Vec::new();
	for key in ["LC_MESSAGES", "LC_ALL", "LANG"] {
		let Ok(locale) = env::var(key) else {
			continue;
		};
		let locale = locale.trim();
		if locale.is_empty() {
			continue;
		}

		let locale = locale
			.split('.')
			.next()
			.unwrap_or(locale)
			.split('@')
			.next()
			.unwrap_or(locale)
			.to_string();

		if !locale.is_empty() {
			locales.push(locale.clone());
			if let Some((language, _)) = locale.split_once('_') {
				locales.push(language.to_string());
			}
		}
	}

	locales.push("C".to_string());
	locales
}

fn resolve_executable(executable: &str) -> Option<PathBuf> {
	if executable.contains('/') {
		return expand_path(Path::new(executable));
	}

	let path = env::var_os("PATH")?;
	for directory in env::split_paths(&path) {
		let candidate = directory.join(executable);
		if candidate.exists() {
			return Some(candidate);
		}
	}

	None
}

fn is_executable(path: &Path) -> bool {
	if !path.is_file() {
		return false;
	}

	match fs::metadata(path) {
		Ok(metadata) => metadata.permissions().mode() & 0o111 != 0,
		Err(_) => false,
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt;
	use std::sync::Mutex;

	use tempfile::tempdir;

	use super::*;

	fn write_file(path: &Path, contents: &str) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	static ENV_MUTEX: Mutex<()> = Mutex::new(());

	fn scan_desktop_applications(config: &DesktopApplicationScannerConfig) -> Result<Vec<ApplicationConfig>, ()> {
		Ok(scan(config, &mut IconIndex::new()).applications())
	}

	const GAME: &str = "[Desktop Entry]\nType=Application\nName=Game\nExec=/usr/bin/game\n";

	/// A user directory listed first overrides the system entry of the same
	/// ID, and hides it with `Hidden=true`, as in the XDG menu specification.
	#[test]
	fn earlier_directories_override_and_hide_entries_with_the_same_id() {
		let tempdir = tempdir().unwrap();
		let user = tempdir.path().join("user");
		let system = tempdir.path().join("system");
		write_file(&system.join("game.desktop"), GAME);
		write_file(
			&system.join("vendor/tool.desktop"),
			"[Desktop Entry]\nType=Application\nName=Tool\nExec=/usr/bin/tool\n",
		);
		write_file(
			&user.join("game.desktop"),
			"[Desktop Entry]\nType=Application\nName=Game (custom)\nExec=/usr/bin/game --custom\n",
		);
		write_file(
			&user.join("vendor-tool.desktop"),
			"[Desktop Entry]\nType=Application\nName=Tool\nExec=/usr/bin/tool\nHidden=true\n",
		);
		let result = scan(&scanner_config(vec![user, system]), &mut IconIndex::new());
		let titles: Vec<_> = result.applications().into_iter().map(|app| app.title).collect();
		assert_eq!(titles, vec!["Game (custom)"]);
		let ids: Vec<_> = result
			.units
			.iter()
			.flat_map(|unit| unit.result.as_ref().unwrap())
			.map(|entry| entry.native_id.clone())
			.collect();
		assert_eq!(ids, vec!["game.desktop"]);
	}

	#[test]
	fn launcher_shortcuts_name_the_application_they_start() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("Portal 2.desktop"),
			"[Desktop Entry]\nType=Application\nName=Portal 2\nExec=steam steam://rungameid/620\n",
		);
		let result = scan(&scanner_config(vec![app_dir]), &mut IconIndex::new());
		let entry = &result.units[0].result.as_ref().unwrap()[0];
		assert_eq!(entry.launches.as_deref(), Some("steam:620"));
	}

	/// A configured directory that cannot be read is an error of its unit,
	/// not an empty directory, so its previous entries are kept.
	#[test]
	fn missing_directories_are_unavailable_units() {
		let tempdir = tempdir().unwrap();
		let present = tempdir.path().join("present");
		write_file(&present.join("game.desktop"), GAME);
		let missing = tempdir.path().join("missing");
		let result = scan(&scanner_config(vec![missing, present]), &mut IconIndex::new());
		assert!(matches!(result.units[0].result, Err(UnitError::Unavailable(_))));
		assert_eq!(result.units[1].result.as_ref().unwrap().len(), 1);
	}

	fn scanner_config(directories: Vec<PathBuf>) -> DesktopApplicationScannerConfig {
		DesktopApplicationScannerConfig {
			directories,
			include_terminal: false,
			resolve_icons: true,
			pre_command: vec![],
			post_command: vec![],
			stdout: None,
			stderr: None,
			launch_timeout_secs: 10,
		}
	}

	#[test]
	fn parses_launchable_desktop_entry() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		let desktop_file = app_dir.join("moonshine-game.desktop");
		write_file(
			&desktop_file,
			r#"
[Desktop Entry]
Type=Application
Name=Moonshine Game
Exec=/usr/bin/example --flag %U %%done
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Moonshine Game");
		assert_eq!(applications[0].command, vec!["/usr/bin/example", "--flag", "%done"]);
	}

	#[test]
	fn filters_hidden_terminal_and_invalid_entries() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("hidden.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Hidden
Exec=/usr/bin/hidden
Hidden=true
"#,
		);
		write_file(
			&app_dir.join("terminal.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Terminal
Exec=/usr/bin/terminal
Terminal=true
"#,
		);
		write_file(
			&app_dir.join("link.desktop"),
			r#"
[Desktop Entry]
Type=Link
Name=Link
Exec=/usr/bin/link
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert!(applications.is_empty());
	}

	#[test]
	fn includes_terminal_entries_when_configured() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("terminal.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Terminal
Exec=/usr/bin/terminal
Terminal=true
"#,
		);

		let mut config = scanner_config(vec![app_dir]);
		config.include_terminal = true;

		let applications = scan_desktop_applications(&config).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Terminal");
	}

	#[test]
	fn includes_entries_with_failing_try_exec() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("broken.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Broken
Exec=/usr/bin/broken
TryExec=/definitely/missing
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);
		assert_eq!(applications[0].title, "Broken");
	}

	#[test]
	fn keeps_first_duplicate_entry() {
		let tempdir = tempdir().unwrap();
		let first_dir = tempdir.path().join("first");
		let second_dir = tempdir.path().join("second");
		let contents = r#"
[Desktop Entry]
Type=Application
Name=Duplicate
Exec=/usr/bin/duplicate --run
"#;
		write_file(&first_dir.join("duplicate.desktop"), contents);
		write_file(&second_dir.join("duplicate.desktop"), contents);

		let applications = scan_desktop_applications(&scanner_config(vec![first_dir, second_dir])).unwrap();
		assert_eq!(applications.len(), 1);
	}

	#[test]
	fn resolves_absolute_and_named_icons() {
		let tempdir = tempdir().unwrap();
		let app_dir = tempdir.path().join("applications");
		let icon_root = tempdir.path().join("data/icons/hicolor/128x128/apps");
		let explicit_icon = tempdir.path().join("explicit.png");
		let named_icon = icon_root.join("moonshine.png");
		write_file(&explicit_icon, "png");
		write_file(&named_icon, "png");

		let _env_lock = ENV_MUTEX.lock().unwrap();
		let previous_data_home = env::var_os("XDG_DATA_HOME");
		unsafe { env::set_var("XDG_DATA_HOME", tempdir.path().join("data")) };

		write_file(
			&app_dir.join("explicit.desktop"),
			&format!(
				r#"
[Desktop Entry]
Type=Application
Name=Explicit Icon
Exec=/usr/bin/explicit
Icon={}
"#,
				explicit_icon.display()
			),
		);
		write_file(
			&app_dir.join("named.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Named Icon
Exec=/usr/bin/named
Icon=moonshine
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 2);
		let explicit_application = applications
			.iter()
			.find(|application| application.title == "Explicit Icon")
			.unwrap();
		assert_eq!(explicit_application.boxart.as_deref(), Some(explicit_icon.as_path()));

		let named_application = applications
			.iter()
			.find(|application| application.title == "Named Icon")
			.unwrap();
		assert_eq!(named_application.boxart.as_deref(), Some(named_icon.as_path()));

		match previous_data_home {
			Some(value) => unsafe { env::set_var("XDG_DATA_HOME", value) },
			None => unsafe { env::remove_var("XDG_DATA_HOME") },
		}
	}

	#[test]
	fn try_exec_uses_path_lookup() {
		let tempdir = tempdir().unwrap();
		let bin_dir = tempdir.path().join("bin");
		let executable = bin_dir.join("moonshine-test");
		write_file(&executable, "#!/bin/sh\n");

		#[cfg(unix)]
		{
			let mut permissions = fs::metadata(&executable).unwrap().permissions();
			permissions.set_mode(0o755);
			fs::set_permissions(&executable, permissions).unwrap();
		}

		let _env_lock = ENV_MUTEX.lock().unwrap();
		let previous_path = env::var_os("PATH");
		unsafe { env::set_var("PATH", &bin_dir) };

		let app_dir = tempdir.path().join("applications");
		write_file(
			&app_dir.join("path.desktop"),
			r#"
[Desktop Entry]
Type=Application
Name=Path Lookup
Exec=/usr/bin/ok
TryExec=moonshine-test
"#,
		);

		let applications = scan_desktop_applications(&scanner_config(vec![app_dir])).unwrap();
		assert_eq!(applications.len(), 1);

		match previous_path {
			Some(value) => unsafe { env::set_var("PATH", value) },
			None => unsafe { env::remove_var("PATH") },
		}
	}
}
