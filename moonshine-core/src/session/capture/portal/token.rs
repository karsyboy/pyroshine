//! The portal restore token of the last authorized Desktop Mode session.
//!
//! A restore token lets the portal start the next session with the same
//! sources and devices without asking again, until the user revokes it in the
//! desktop's settings. It is a capability: stored privately (mode 0600) in
//! Pyroshine's data directory, replaced after every session (portals issue a
//! new one each time) and removed when the portal refuses the session.

use std::path::{Path, PathBuf};

const FILE_NAME: &str = "desktop-restore-token";

/// Where the token lives: `$XDG_DATA_HOME/moonshine/desktop-restore-token`.
pub(crate) fn default_path() -> Option<PathBuf> {
	Some(dirs::data_dir()?.join("moonshine").join(FILE_NAME))
}

/// Portal tokens are short UUID-like strings; anything else is not one.
fn valid(token: &str) -> bool {
	!token.is_empty() && token.len() <= 256 && token.bytes().all(|b| b.is_ascii_graphic())
}

pub(crate) fn load(path: &Path) -> Option<String> {
	let token = std::fs::read_to_string(path).ok()?;
	let token = token.trim();
	valid(token).then(|| token.to_string())
}

pub(crate) fn store(path: &Path, token: &str) {
	if !valid(token) {
		tracing::warn!("Ignoring an invalid portal restore token");
		return;
	}
	let result = crate::durable::create_directories(crate::durable::directory(path))
		.and_then(|()| crate::durable::replace_with_mode(path, token.as_bytes(), Some(0o600)));
	if let Err(error) = result {
		tracing::warn!(%error, path = %path.display(), "Cannot store the portal restore token; the desktop will ask again next time");
	}
}

pub(crate) fn forget(path: &Path) {
	match std::fs::remove_file(path) {
		Ok(()) => tracing::info!("Forgot the portal restore token"),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
		Err(error) => tracing::warn!(%error, "Cannot remove the portal restore token"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::PermissionsExt;

	#[test]
	fn tokens_are_stored_privately_and_validated() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("nested").join(FILE_NAME);
		assert_eq!(load(&path), None);
		store(&path, "2978cc28-e67a-4431-b4c2-c9bd4a478d36");
		assert_eq!(load(&path).as_deref(), Some("2978cc28-e67a-4431-b4c2-c9bd4a478d36"));
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
		store(&path, "not a token\n");
		assert_eq!(load(&path).as_deref(), Some("2978cc28-e67a-4431-b4c2-c9bd4a478d36"));
		forget(&path);
		assert_eq!(load(&path), None);
	}
}
