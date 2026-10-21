//! Per-certificate Desktop Mode preferences, separate from recovery state.
use super::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAX_BYTES: u64 = 256 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
	#[serde(default)]
	scales: BTreeMap<String, f64>,
}

pub(super) fn valid_scale(scale: f64) -> bool {
	scale.is_finite() && (0.25..=8.0).contains(&scale)
}

/// The TLS certificate fingerprint is stable across address/name changes.
/// Caller-supplied Moonlight uniqueid and IP addresses are not profile keys.
pub(super) fn client_key(key: &str) -> Result<String> {
	if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
		return Err("Desktop client identity must be a SHA-256 certificate fingerprint".into());
	}
	Ok(key.to_ascii_lowercase())
}

fn load(path: &Path) -> Result<Preferences> {
	let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
		Ok(file) => file,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Preferences::default()),
		Err(error) => return Err(error.to_string()),
	};
	let mut bytes = Vec::new();
	file.take(MAX_BYTES + 1)
		.read_to_end(&mut bytes)
		.map_err(|e| e.to_string())?;
	if bytes.len() as u64 > MAX_BYTES {
		return Err("Desktop preferences exceed the size limit".into());
	}
	let preferences: Preferences = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
	if preferences
		.scales
		.iter()
		.any(|(key, scale)| client_key(key).is_err() || !valid_scale(*scale))
	{
		return Err("Desktop preferences contain an invalid client or scale".into());
	}
	Ok(preferences)
}

pub(super) fn scale(path: &Path, client: &str) -> Result<Option<f64>> {
	let key = client_key(client)?;
	Ok(load(path)?.scales.get(&key).copied())
}

pub(super) fn remember(path: &Path, client: &str, scale: f64) -> Result<()> {
	let key = client_key(client)?;
	if !valid_scale(scale) {
		return Err("Desktop scale must be finite and between 25% and 800%".into());
	}
	let mut preferences = load(path)?;
	if preferences
		.scales
		.get(&key)
		.is_some_and(|saved| (saved - scale).abs() < 0.001)
	{
		return Ok(());
	}
	preferences.scales.insert(key, scale);
	let bytes = serde_json::to_vec(&preferences).map_err(|e| e.to_string())?;
	if bytes.len() as u64 > MAX_BYTES {
		return Err("Desktop preferences exceed the size limit".into());
	}
	crate::durable::replace(path, &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::PermissionsExt;

	#[test]
	fn client_scales_are_independent_durable_and_private() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("preferences.json");
		let phone = "a".repeat(64);
		let desktop = "b".repeat(64);
		assert_eq!(scale(&path, &phone), Ok(None));
		remember(&path, &phone, 1.75).unwrap();
		remember(&path, &desktop, 1.25).unwrap();
		assert_eq!(scale(&path, &phone.to_uppercase()), Ok(Some(1.75)));
		assert_eq!(scale(&path, &desktop), Ok(Some(1.25)));
		remember(&path, &phone, 2.0).unwrap();
		assert_eq!(scale(&path, &phone), Ok(Some(2.0)));
		assert_eq!(scale(&path, &desktop), Ok(Some(1.25)));
		assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
	}

	#[test]
	fn invalid_preferences_and_failed_writes_preserve_previous_values() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("preferences.json");
		let client = "a".repeat(64);
		remember(&path, &client, 1.75).unwrap();
		let saved = std::fs::read(&path).unwrap();
		for invalid in [f64::NAN, f64::INFINITY, 0.0, -1.0, 9.0] {
			assert!(remember(&path, &client, invalid).is_err());
		}
		assert!(remember(&path, "client-supplied-id", 2.0).is_err());
		crate::durable::fail_next("rename", libc::EIO);
		assert!(remember(&path, &client, 2.0).is_err());
		assert_eq!(std::fs::read(&path).unwrap(), saved);
		std::fs::write(&path, b"invalid JSON").unwrap();
		assert!(remember(&path, &client, 2.0).is_err());
		assert_eq!(std::fs::read(&path).unwrap(), b"invalid JSON");
	}
}
