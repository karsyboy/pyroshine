//! Commands the window invokes. Each one is a thin call to the daemon's
//! management interface; the window never touches files or processes.

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

/// Quit the desktop app. Pyroshine and any stream keep running.
#[tauri::command]
pub fn quit(app: AppHandle) {
	app.exit(0);
}
