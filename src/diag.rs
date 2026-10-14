//! `pyroshine diag`: write a redacted support bundle for headless hosts.
//!
//! The running server builds the bundle (`GetSupportBundle`), so it carries
//! the live report, recent statistics and GPU readings. Without a reachable
//! server (stopped, or another user's), the command collects what it can
//! itself: configuration, host and GPU details, the service log and a health
//! check when no server runs.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use moonshine_management::{BUS_NAME, ManagementError, ManagementProxy};

/// Collection runs journalctl and vulkaninfo; allow for a slow disk.
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Write a bundle. `bus` is the address of the bus serving the management
/// interface (`MOONSHINE_MANAGEMENT_BUS` for a test instance), or `None` for
/// the session bus.
pub async fn run(
	config: PathBuf,
	output: Option<PathBuf>,
	offline: bool,
	health_check: bool,
	bus: Option<String>,
) -> Result<(), String> {
	let mut notes = Vec::new();
	let from_server = if offline {
		None
	} else {
		match from_server(bus).await {
			Ok(archive) => Some(archive),
			Err(reason) => {
				eprintln!("{reason}\nCollecting without the server instead.");
				notes.push(format!("report.json: the server could not be reached: {reason}"));
				None
			},
		}
	};
	let (archive, name) = match from_server {
		Some(archive) => (archive, moonshine_core::support::bundle_name(SystemTime::now())),
		None => {
			let bundle = tokio::task::spawn_blocking(move || {
				moonshine_core::support::collect_offline(&config, health_check, notes)
			})
			.await
			.map_err(|error| format!("Collection failed: {error}"))?;
			for note in &bundle.notes {
				eprintln!("  not collected: {note}");
			}
			(bundle.archive, bundle.name)
		},
	};
	let path = destination(output, &name);
	write(&path, &archive).map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
	println!(
		"Wrote {} ({} KiB).\nIdentifying values are redacted; review the files before sharing them.",
		path.display(),
		archive.len().div_ceil(1024)
	);
	Ok(())
}

/// `output` itself, a file named after the bundle inside an `output`
/// directory, or that file in the current directory.
fn destination(output: Option<PathBuf>, name: &str) -> PathBuf {
	let file = format!("{name}.tar.gz");
	match output {
		Some(directory) if directory.is_dir() => directory.join(file),
		Some(path) => path,
		None => PathBuf::from(file),
	}
}

/// The bundle holds logs and host details: readable by the owner only.
fn write(path: &Path, archive: &[u8]) -> std::io::Result<()> {
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.mode(0o600)
		.open(path)?;
	file.write_all(archive)?;
	file.sync_all()
}

async fn from_server(bus: Option<String>) -> Result<Vec<u8>, String> {
	let builder = match &bus {
		Some(address) => zbus::connection::Builder::address(address.as_str()),
		None => zbus::connection::Builder::session(),
	};
	let connection = builder
		.map(|builder| builder.method_timeout(BUNDLE_TIMEOUT))
		.map_err(|error| format!("Cannot reach your session bus: {error}."))?
		.build()
		.await
		.map_err(|error| format!("Cannot reach your session bus: {error}."))?;
	let proxy = ManagementProxy::builder(&connection)
		.destination(BUS_NAME)
		.map_err(|error| error.to_string())?
		.build()
		.await
		.map_err(|error| error.to_string())?;
	proxy.get_support_bundle().await.map_err(|error| {
		if matches!(&error, zbus::Error::MethodError(name, ..) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod")
		{
			return "The running Pyroshine server is older than this command and cannot build a bundle.".into();
		}
		let error = ManagementError::from(error);
		match error.kind() {
			"unavailable" => format!(
				"Pyroshine is not reachable on this user's session bus ({}). Run this command as the streaming \
				 user, without sudo, while the server runs.",
				error.message()
			),
			_ => format!("Pyroshine could not build a support bundle: {}", error.message()),
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_destination_is_a_file_or_a_directory() {
		let directory = tempfile::tempdir().unwrap();
		let name = "pyroshine-diag-20261014-155902";
		assert_eq!(
			destination(Some(directory.path().to_path_buf()), name),
			directory.path().join(format!("{name}.tar.gz"))
		);
		let file = directory.path().join("bundle.tgz");
		assert_eq!(destination(Some(file.clone()), name), file);
		assert_eq!(destination(None, name), PathBuf::from(format!("{name}.tar.gz")));
	}

	#[test]
	fn bundles_are_private_to_the_owner() {
		use std::os::unix::fs::PermissionsExt;
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("bundle.tar.gz");
		write(&path, b"archive").unwrap();
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
	}

	/// Without a server on the bus, the command collects offline.
	#[tokio::test]
	async fn an_unreachable_server_falls_back_to_offline_collection() {
		let directory = tempfile::tempdir().unwrap();
		let config = directory.path().join("config.toml");
		std::fs::write(&config, "name = \"Bench\"\n").unwrap();
		let output = directory.path().join("bundle.tar.gz");
		// A bus address nobody listens on.
		let bus = format!("unix:path={}", directory.path().join("no-bus").display());
		run(config, Some(output.clone()), false, false, Some(bus))
			.await
			.unwrap();
		let archive = std::fs::read(&output).unwrap();
		let listing = std::process::Command::new("tar").arg("-tzf").arg(&output).output();
		if let Ok(listing) = listing {
			let names = String::from_utf8_lossy(&listing.stdout);
			assert!(
				names.contains("/config.toml") && names.contains("/collection.txt"),
				"{names}"
			);
			assert!(!names.contains("/report.json"), "{names}");
		}
		assert!(archive.len() > 100);
	}
}
