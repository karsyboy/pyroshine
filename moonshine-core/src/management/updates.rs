//! Release notifications.
//!
//! The server can look up the project's published releases and tell the web
//! interface when a newer one exists, with update steps for the way it was
//! installed. It never downloads or installs anything: packages are updated
//! by the operator with the system's package manager.
//!
//! Automatic checks are off until the operator turns them on (preferences in
//! `updates.json` beside `state.toml`; they take effect at once and are not
//! configuration). "Check now" works either way. A check is one HTTPS
//! request to the GitHub releases API, verified against the system's trust
//! store, bounded in time and size. A failed check (offline host, rate
//! limit) is reported on the page and retried at the next interval; it never
//! affects streaming.
//!
//! The release link is built from the validated version, never taken from
//! the response, so the page only ever links to the project's own releases.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use moonshine_management::ManagementError;
use moonshine_management::dto::{InstallGuidance, ReleaseInfo, UpdateStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::telemetry::unix_millis;

const REPOSITORY: &str = "karsyboy/pyroshine";
const API_HOST: &str = "api.github.com";
const INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// After startup, wait before the first automatic check.
const FIRST_CHECK: Duration = Duration::from_secs(120);
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub(crate) fn updates_path(state_path: &Path) -> PathBuf {
	crate::durable::directory(state_path).join("updates.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Preferences {
	automatic: bool,
	prerelease: bool,
	/// The release version whose notice was dismissed.
	dismissed: Option<String>,
}

/// A semantic version, compared by precedence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
	core: [u64; 3],
	pre: Vec<String>,
}

impl Version {
	/// `1.2.3` or `1.2.3-alpha.1`, with an optional leading `v`.
	pub(crate) fn parse(text: &str) -> Option<Self> {
		let text = text.strip_prefix('v').unwrap_or(text);
		let (core, pre) = match text.split_once('-') {
			Some((core, pre)) => (core, Some(pre)),
			None => (text, None),
		};
		let mut numbers = core.split('.').map(|part| {
			(!part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit()))
				.then(|| part.parse::<u64>().ok())
				.flatten()
		});
		let core = [numbers.next()??, numbers.next()??, numbers.next()??];
		if numbers.next().is_some() {
			return None;
		}
		let pre = match pre {
			None => Vec::new(),
			Some(pre) => {
				let parts: Vec<String> = pre.split('.').map(str::to_string).collect();
				if parts
					.iter()
					.any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric()))
				{
					return None;
				}
				parts
			},
		};
		Some(Self { core, pre })
	}

	pub(crate) fn is_prerelease(&self) -> bool {
		!self.pre.is_empty()
	}
}

impl std::fmt::Display for Version {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}.{}.{}", self.core[0], self.core[1], self.core[2])?;
		if !self.pre.is_empty() {
			write!(f, "-{}", self.pre.join("."))?;
		}
		Ok(())
	}
}

impl PartialOrd for Version {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for Version {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		use std::cmp::Ordering;
		self.core
			.cmp(&other.core)
			.then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
				(true, true) => Ordering::Equal,
				(true, false) => Ordering::Greater,
				(false, true) => Ordering::Less,
				(false, false) => {
					for (a, b) in self.pre.iter().zip(&other.pre) {
						let order = match (a.parse::<u64>(), b.parse::<u64>()) {
							(Ok(a), Ok(b)) => a.cmp(&b),
							(Ok(_), Err(_)) => Ordering::Less,
							(Err(_), Ok(_)) => Ordering::Greater,
							(Err(_), Err(_)) => a.cmp(b),
						};
						if order != Ordering::Equal {
							return order;
						}
					}
					self.pre.len().cmp(&other.pre.len())
				},
			})
	}
}

#[derive(Deserialize)]
struct GithubRelease {
	tag_name: String,
	#[serde(default)]
	draft: bool,
	#[serde(default)]
	prerelease: bool,
	#[serde(default)]
	published_at: Option<String>,
}

/// The newest release on the channel, from the releases API's response.
fn newest(releases: &[GithubRelease], prerelease: bool) -> Option<ReleaseInfo> {
	releases
		.iter()
		.filter(|release| !release.draft)
		.filter_map(|release| Some((Version::parse(&release.tag_name)?, release)))
		.filter(|(version, release)| prerelease || (!version.is_prerelease() && !release.prerelease))
		.max_by(|(a, _), (b, _)| a.cmp(b))
		.map(|(version, release)| ReleaseInfo {
			prerelease: version.is_prerelease() || release.prerelease,
			url: format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
			version: version.to_string(),
			published_at: release
				.published_at
				.clone()
				.filter(|date| date.len() <= 40 && date.bytes().all(|b| b.is_ascii_graphic())),
		})
}

/// How this installation is updated, from the package databases and the
/// executable's location.
pub(crate) fn install_guidance(executable: &Path, root: &Path) -> InstallGuidance {
	let exists = |path: &str| root.join(path.trim_start_matches('/')).exists();
	let pacman = std::fs::read_dir(root.join("var/lib/pacman/local"))
		.map(|entries| {
			entries
				.filter_map(Result::ok)
				.any(|entry| entry.file_name().to_string_lossy().starts_with("pyroshine"))
		})
		.unwrap_or(false);
	let restart = "Restart the service to run the new version: sudo systemctl restart \"pyroshine@$USER\"".to_string();
	let (kind, label, steps) = if executable.starts_with("/nix/store") {
		(
			"nix",
			"NixOS / Nix",
			vec![
				"Update the flake input that provides Pyroshine (nix flake update), then rebuild (sudo nixos-rebuild switch).".to_string(),
			],
		)
	} else if executable.starts_with("/usr") && pacman {
		(
			"arch",
			"Arch Linux / CachyOS package",
			vec![
				"Update from the [pyrowave] repository: sudo pacman -Syu".to_string(),
				restart,
			],
		)
	} else if executable.starts_with("/usr") && exists("/var/lib/dpkg/info/pyroshine.list") {
		(
			"debian",
			"Debian / Ubuntu package",
			vec![
				"Download pyroshine_<version>_amd64.deb (and pyroshine-tray if you use the tray) from the release page and verify it against SHA256SUMS.".to_string(),
				"Install it: sudo apt install ./pyroshine_<version>_amd64.deb".to_string(),
				restart,
			],
		)
	} else if executable.starts_with("/usr") && (exists("/var/lib/rpm") || exists("/usr/lib/sysimage/rpm")) {
		(
			"rpm",
			"Fedora / openSUSE package",
			vec![
				"Download pyroshine-<version>-1.x86_64.rpm (and pyroshine-tray if you use the tray) from the release page and verify it against SHA256SUMS.".to_string(),
				"Install it: sudo dnf install ./pyroshine-<version>-1.x86_64.rpm".to_string(),
				restart,
			],
		)
	} else if executable.starts_with("/usr") {
		(
			"unknown",
			"System installation",
			vec![
				"Install the new release the same way you installed this one.".to_string(),
				restart,
			],
		)
	} else {
		(
			"portable",
			"Portable installation",
			vec![
				"Download pyroshine-install.sh from the release page and run it again; it replaces this installation."
					.to_string(),
				restart,
			],
		)
	};
	InstallGuidance {
		kind: kind.into(),
		label: label.into(),
		steps,
	}
}

struct State {
	preferences: Preferences,
	checking: bool,
	last_checked_ms: Option<u64>,
	last_error: Option<String>,
	latest: Option<ReleaseInfo>,
}

/// Fetches the release list. Replaced in tests.
pub(crate) type Fetch =
	Arc<dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>> + Send + Sync>;

pub(crate) struct Updates {
	path: PathBuf,
	current: Version,
	install: InstallGuidance,
	state: Mutex<State>,
	/// Wakes the automatic checker after a preference change.
	wake: Notify,
	fetch: Fetch,
}

impl Updates {
	pub(crate) fn open(path: PathBuf) -> Self {
		let executable = std::env::current_exe().unwrap_or_default();
		Self::with(
			path,
			env!("CARGO_PKG_VERSION"),
			install_guidance(&executable, Path::new("/")),
			Arc::new(|| Box::pin(fetch_releases())),
		)
	}

	pub(crate) fn with(path: PathBuf, current: &str, install: InstallGuidance, fetch: Fetch) -> Self {
		let preferences = std::fs::read(&path)
			.ok()
			.and_then(|bytes| serde_json::from_slice(&bytes).ok())
			.unwrap_or_default();
		Self {
			path,
			current: Version::parse(current).unwrap_or(Version {
				core: [0, 0, 0],
				pre: Vec::new(),
			}),
			install,
			state: Mutex::new(State {
				preferences,
				checking: false,
				last_checked_ms: None,
				last_error: None,
				latest: None,
			}),
			wake: Notify::new(),
			fetch,
		}
	}

	pub(crate) fn status(&self) -> UpdateStatus {
		let state = self.state.lock().unwrap();
		let newer = state
			.latest
			.as_ref()
			.and_then(|latest| Version::parse(&latest.version))
			.is_some_and(|latest| latest > self.current);
		UpdateStatus {
			current_version: self.current.to_string(),
			automatic: state.preferences.automatic,
			channel: if state.preferences.prerelease {
				"prerelease"
			} else {
				"stable"
			}
			.into(),
			checking: state.checking,
			last_checked_ms: state.last_checked_ms,
			last_error: state.last_error.clone(),
			update_available: newer,
			dismissed: newer
				&& state.latest.as_ref().map(|latest| &latest.version) == state.preferences.dismissed.as_ref(),
			latest: state.latest.clone(),
			install: self.install.clone(),
		}
	}

	fn save(&self, preferences: &Preferences) -> Result<(), ManagementError> {
		let bytes = serde_json::to_vec_pretty(preferences).expect("preferences serialize");
		crate::durable::replace(&self.path, &bytes)
			.map_err(|error| ManagementError::Failed(format!("Cannot save the update preferences: {error}")))
	}

	pub(crate) fn set(&self, automatic: bool, channel: &str) -> Result<UpdateStatus, ManagementError> {
		let prerelease = match channel {
			"stable" => false,
			"prerelease" => true,
			_ => return Err(ManagementError::Invalid("The channel is stable or prerelease.".into())),
		};
		let preferences = {
			let mut state = self.state.lock().unwrap();
			if state.preferences.prerelease != prerelease {
				// Another channel: the previous result no longer applies.
				state.latest = None;
			}
			state.preferences.automatic = automatic;
			state.preferences.prerelease = prerelease;
			state.preferences.clone()
		};
		self.save(&preferences)?;
		self.wake.notify_one();
		Ok(self.status())
	}

	pub(crate) fn dismiss(&self, version: &str) -> Result<UpdateStatus, ManagementError> {
		let version = Version::parse(version)
			.ok_or_else(|| ManagementError::Invalid("A release version is required.".into()))?
			.to_string();
		let preferences = {
			let mut state = self.state.lock().unwrap();
			state.preferences.dismissed = Some(version);
			state.preferences.clone()
		};
		self.save(&preferences)?;
		Ok(self.status())
	}

	/// Look up the releases now.
	pub(crate) async fn check(&self) -> UpdateStatus {
		let prerelease = {
			let mut state = self.state.lock().unwrap();
			if state.checking {
				drop(state);
				return self.status();
			}
			state.checking = true;
			state.preferences.prerelease
		};
		let result = tokio::time::timeout(TIMEOUT, (self.fetch)())
			.await
			.unwrap_or_else(|_| Err("The release server did not answer in time.".into()))
			.and_then(|body| {
				serde_json::from_slice::<Vec<GithubRelease>>(&body)
					.map_err(|error| format!("Unexpected answer from the release server: {error}"))
			});
		{
			let mut state = self.state.lock().unwrap();
			state.checking = false;
			state.last_checked_ms = Some(unix_millis(SystemTime::now()));
			match result {
				Ok(releases) => {
					state.latest = newest(&releases, prerelease);
					state.last_error = None;
				},
				Err(error) => {
					tracing::info!("Update check failed: {error}");
					state.last_error = Some(error);
				},
			}
		}
		self.status()
	}

	/// Check once a day while automatic checks are on. `published` receives
	/// each result.
	pub(crate) async fn run(&self, published: impl Fn(&UpdateStatus)) {
		let mut delay = FIRST_CHECK;
		loop {
			tokio::select! {
				() = tokio::time::sleep(delay) => {},
				() = self.wake.notified() => {
					delay = FIRST_CHECK.min(delay);
					continue;
				},
			}
			if self.state.lock().unwrap().preferences.automatic {
				published(&self.check().await);
			}
			delay = INTERVAL;
		}
	}
}

/// One GET of the releases list over HTTPS, verified against the system's
/// trust store.
async fn fetch_releases() -> Result<Vec<u8>, String> {
	let mut roots = rustls::RootCertStore::empty();
	let native = rustls_native_certs::load_native_certs();
	for certificate in native.certs {
		let _ = roots.add(certificate);
	}
	if roots.is_empty() {
		return Err("No trusted certificates were found on this system (install ca-certificates).".into());
	}
	let config = rustls::ClientConfig::builder()
		.with_root_certificates(roots)
		.with_no_client_auth();
	let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
	let tcp = tokio::net::TcpStream::connect((API_HOST, 443))
		.await
		.map_err(|error| format!("Cannot reach {API_HOST}: {error}"))?;
	let name = rustls::pki_types::ServerName::try_from(API_HOST).expect("valid host name");
	let tls = connector
		.connect(name, tcp)
		.await
		.map_err(|error| format!("Secure connection to {API_HOST} failed: {error}"))?;
	let (mut sender, connection) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
		.await
		.map_err(|error| error.to_string())?;
	tokio::spawn(async move {
		let _ = connection.await;
	});
	let request = hyper::Request::get(format!("/repos/{REPOSITORY}/releases?per_page=30"))
		.header(hyper::header::HOST, API_HOST)
		.header(
			hyper::header::USER_AGENT,
			concat!("Pyroshine/", env!("CARGO_PKG_VERSION")),
		)
		.header(hyper::header::ACCEPT, "application/vnd.github+json")
		.body(Empty::<Bytes>::new())
		.expect("valid request");
	let response = sender.send_request(request).await.map_err(|error| error.to_string())?;
	let status = response.status();
	if !status.is_success() {
		return Err(match status.as_u16() {
			403 | 429 => "The release server limited requests from this address; the next check will try again.".into(),
			code => format!("The release server answered {code}."),
		});
	}
	Limited::new(response.into_body(), MAX_RESPONSE)
		.collect()
		.await
		.map(|body| body.to_bytes().to_vec())
		.map_err(|error| format!("Cannot read the release list: {error}"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn version(text: &str) -> Version {
		Version::parse(text).unwrap()
	}

	#[test]
	fn versions_follow_semver_precedence() {
		assert!(version("0.18.0") > version("0.18.0-alpha.0"));
		assert!(version("0.18.0-alpha.10") > version("0.18.0-alpha.2"));
		assert!(version("0.18.0-beta") > version("0.18.0-alpha.9"));
		assert!(version("0.18.0-alpha.1") > version("0.18.0-alpha"));
		assert!(version("1.0.0") > version("0.99.99"));
		assert_eq!(version("v0.17.6"), version("0.17.6"));
		assert_eq!(version("0.18.0-alpha.0").to_string(), "0.18.0-alpha.0");
		for invalid in [
			"",
			"1.2",
			"1.2.3.4",
			"1.2.x",
			"1.2.3-",
			"1.2.3-a..b",
			"1.2.3-<script>",
			"../1.2.3",
		] {
			assert!(Version::parse(invalid).is_none(), "{invalid}");
		}
	}

	fn releases() -> Vec<GithubRelease> {
		serde_json::from_str(
			r#"[
				{"tag_name": "v0.19.0-alpha.1", "prerelease": true, "html_url": "https://evil.example/"},
				{"tag_name": "v0.18.1", "published_at": "2026-10-20T10:00:00Z"},
				{"tag_name": "v0.20.0", "draft": true},
				{"tag_name": "not-a-version"},
				{"tag_name": "v0.17.6"}
			]"#,
		)
		.unwrap()
	}

	#[test]
	fn the_newest_release_follows_the_channel_and_links_to_the_project() {
		let stable = newest(&releases(), false).unwrap();
		assert_eq!(stable.version, "0.18.1");
		assert_eq!(stable.url, "https://github.com/karsyboy/pyroshine/releases/tag/v0.18.1");
		assert!(!stable.prerelease);
		let pre = newest(&releases(), true).unwrap();
		assert_eq!(pre.version, "0.19.0-alpha.1");
		assert!(pre.prerelease);
		assert!(!pre.url.contains("evil"));
	}

	fn fixed(body: &'static str) -> Fetch {
		Arc::new(move || Box::pin(async move { Ok(body.as_bytes().to_vec()) }))
	}

	fn guidance() -> InstallGuidance {
		install_guidance(Path::new("/opt/pyroshine/bin/pyroshine"), Path::new("/nonexistent"))
	}

	#[tokio::test]
	async fn checks_report_updates_dismissal_and_failures() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("updates.json");
		let body = r#"[{"tag_name":"v0.18.1"},{"tag_name":"v0.19.0-rc.1","prerelease":true}]"#;
		let updates = Updates::with(path.clone(), "0.18.0", guidance(), fixed(body));
		let status = updates.status();
		assert!(!status.automatic && status.latest.is_none() && !status.update_available);
		let status = updates.check().await;
		assert_eq!(status.latest.unwrap().version, "0.18.1");
		assert!(status.update_available && !status.dismissed);
		let status = updates.dismiss("0.18.1").unwrap();
		assert!(status.dismissed);
		let status = updates.set(true, "prerelease").unwrap();
		assert!(status.automatic && status.latest.is_none());
		assert_eq!(updates.check().await.latest.unwrap().version, "0.19.0-rc.1");
		assert!(updates.set(true, "nightly").is_err());
		// Preferences persist.
		let reopened = Updates::with(path, "0.18.0", guidance(), fixed("[]"));
		assert!(reopened.status().automatic);
		assert_eq!(reopened.status().channel, "prerelease");

		let offline = Updates::with(
			directory.path().join("offline.json"),
			"0.18.0",
			guidance(),
			Arc::new(|| Box::pin(async { Err("Cannot reach api.github.com: no route".to_string()) })),
		);
		let status = offline.check().await;
		assert!(status.last_error.unwrap().contains("Cannot reach"));
		assert!(status.last_checked_ms.is_some() && !status.update_available);
		let garbage = Updates::with(directory.path().join("g.json"), "0.18.0", guidance(), fixed("<html>"));
		assert!(garbage.check().await.last_error.unwrap().contains("Unexpected"));
		// The running version is never offered as an update.
		let current = Updates::with(directory.path().join("c.json"), "0.18.1", guidance(), fixed(body));
		assert!(!current.check().await.update_available);
	}

	#[test]
	fn guidance_follows_the_installation() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path();
		assert_eq!(guidance().kind, "portable");
		assert_eq!(
			install_guidance(Path::new("/nix/store/abc-moonshine/bin/moonshine"), root).kind,
			"nix"
		);
		std::fs::create_dir_all(root.join("var/lib/pacman/local/pyroshine-bin-0.18.0-1")).unwrap();
		assert_eq!(install_guidance(Path::new("/usr/bin/pyroshine"), root).kind, "arch");
		std::fs::remove_dir_all(root.join("var/lib/pacman")).unwrap();
		std::fs::create_dir_all(root.join("var/lib/dpkg/info")).unwrap();
		std::fs::write(root.join("var/lib/dpkg/info/pyroshine.list"), "").unwrap();
		assert_eq!(install_guidance(Path::new("/usr/bin/pyroshine"), root).kind, "debian");
	}
}
