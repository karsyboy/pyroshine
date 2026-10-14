//! Commands the window invokes. Each one is a thin call to the daemon's
//! management interface; the window never touches processes, and the only
//! file it writes is a support bundle the operator asked to save.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, State};

use crate::daemon::{Daemon, Overview, UiError};

type Result<T> = std::result::Result<T, UiError>;

fn parse(text: &str) -> Result<Value> {
	serde_json::from_str(text).map_err(|error| UiError {
		kind: "failed".into(),
		message: format!("Unexpected reply from Pyroshine: {error}"),
	})
}

#[tauri::command]
pub fn overview(daemon: State<'_, Arc<Daemon>>) -> Overview {
	daemon.overview()
}

#[tauri::command]
pub async fn refresh(app: AppHandle, daemon: State<'_, Arc<Daemon>>) -> Result<Overview> {
	daemon.refresh(&app).await
}

#[tauri::command]
pub async fn end_session(daemon: State<'_, Arc<Daemon>>) -> Result<()> {
	Ok(daemon.proxy().await?.end_session().await?)
}

#[tauri::command]
pub async fn approve_pairing(
	daemon: State<'_, Arc<Daemon>>,
	client_id: String,
	request: String,
	pin: String,
	label: String,
) -> Result<()> {
	Ok(daemon
		.proxy()
		.await?
		.approve_pairing(&client_id, &request, &pin, &label)
		.await?)
}

#[tauri::command]
pub async fn reject_pairing(daemon: State<'_, Arc<Daemon>>, client_id: String, request: String) -> Result<()> {
	Ok(daemon.proxy().await?.reject_pairing(&client_id, &request).await?)
}

#[tauri::command]
pub async fn revoke_client(daemon: State<'_, Arc<Daemon>>, fingerprint: String) -> Result<()> {
	Ok(daemon.proxy().await?.revoke_client(&fingerprint).await?)
}

#[tauri::command]
pub async fn rename_client(daemon: State<'_, Arc<Daemon>>, fingerprint: String, label: String) -> Result<()> {
	Ok(daemon.proxy().await?.rename_client(&fingerprint, &label).await?)
}

/// The configuration document and the schema describing it.
#[tauri::command]
pub async fn load_config(daemon: State<'_, Arc<Daemon>>) -> Result<Value> {
	let proxy = daemon.proxy().await?;
	let (document, schema) = tokio::try_join!(proxy.get_config(), proxy.get_config_schema())?;
	Ok(serde_json::json!({ "document": parse(&document)?, "schema": parse(&schema)? }))
}

#[tauri::command]
pub async fn validate_config(daemon: State<'_, Arc<Daemon>>, values: Value) -> Result<Value> {
	parse(&daemon.proxy().await?.validate_config(&values.to_string()).await?)
}

#[tauri::command]
pub async fn save_config(daemon: State<'_, Arc<Daemon>>, values: Value, revision: String) -> Result<Value> {
	parse(
		&daemon
			.proxy()
			.await?
			.save_config(&values.to_string(), &revision)
			.await?,
	)
}

/// Recent events (oldest first). Empty from a daemon that does not keep them.
#[tauri::command]
pub async fn events(daemon: State<'_, Arc<Daemon>>) -> Result<Value> {
	match daemon.proxy().await?.get_events().await {
		Ok(events) => parse(&events),
		Err(error) if UiError::from(error.clone()).kind == "incompatible" => Ok(Value::Array(Vec::new())),
		Err(error) => Err(error.into()),
	}
}

/// Run the host checks again; the daemon also announces the new report.
#[tauri::command]
pub async fn run_health_check(app: AppHandle, daemon: State<'_, Arc<Daemon>>) -> Result<Value> {
	let report = parse(&daemon.proxy().await?.run_health_check().await?)?;
	daemon.health_changed(&app, report.clone());
	Ok(report)
}

/// Everything needed to diagnose the host, as indented JSON for the clipboard.
#[tauri::command]
pub async fn diagnostic_report(daemon: State<'_, Arc<Daemon>>) -> Result<String> {
	let report = parse(&daemon.proxy().await?.get_diagnostic_report().await?)?;
	serde_json::to_string_pretty(&report).map_err(|error| UiError {
		kind: "failed".into(),
		message: error.to_string(),
	})
}

/// Save a redacted support bundle from the daemon in the user's download
/// directory and return its path.
#[tauri::command]
pub async fn save_support_bundle(daemon: State<'_, Arc<Daemon>>) -> Result<String> {
	let archive = daemon.proxy().await?.get_support_bundle().await?;
	let seconds = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs());
	let name = format!("{}.tar.gz", moonshine_management::support_bundle_name(seconds));
	let path = download_dir().join(name);
	write_private(&path, &archive).map_err(|error| UiError {
		kind: "failed".into(),
		message: format!("Cannot save {}: {error}", path.display()),
	})?;
	Ok(path.display().to_string())
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads` when it
/// exists, else the home directory.
fn download_dir() -> PathBuf {
	let home = std::env::var_os("HOME")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("/tmp"));
	let config = std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.unwrap_or_else(|| home.join(".config"));
	let configured = std::fs::read_to_string(config.join("user-dirs.dirs"))
		.ok()
		.and_then(|text| user_dir(&text, "XDG_DOWNLOAD_DIR", &home));
	[configured, Some(home.join("Downloads"))]
		.into_iter()
		.flatten()
		.find(|directory| directory.is_dir())
		.unwrap_or(home)
}

/// One `KEY="$HOME/..."` entry of `user-dirs.dirs`.
fn user_dir(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
	let value = text
		.lines()
		.find_map(|line| line.trim().strip_prefix(key)?.trim_start().strip_prefix('='))?
		.trim()
		.trim_matches('"');
	match value.strip_prefix("$HOME") {
		Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
		None if value.starts_with('/') => Some(PathBuf::from(value)),
		None => None,
	}
}

/// The bundle holds logs and host details: readable by the owner only.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
	use std::io::Write;
	use std::os::unix::fs::OpenOptionsExt;
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.mode(0o600)
		.open(path)?;
	file.write_all(contents)?;
	file.sync_all()
}

/// Quit the desktop app. Pyroshine and any stream keep running.
#[tauri::command]
pub fn quit(app: AppHandle) {
	app.exit(0);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_the_download_directory_from_user_dirs() {
		let home = Path::new("/home/alice");
		let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Fichiers\"\n";
		assert_eq!(
			user_dir(text, "XDG_DOWNLOAD_DIR", home),
			Some(PathBuf::from("/home/alice/Fichiers"))
		);
		assert_eq!(
			user_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", "XDG_DOWNLOAD_DIR", home),
			Some(PathBuf::from("/data/dl"))
		);
		assert_eq!(user_dir(text, "XDG_MUSIC_DIR", home), None);
	}
}
