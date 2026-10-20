use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use crate::app_scanner::ApplicationScannerConfig;
pub use crate::app_scanner::LibraryConfig;
pub use crate::app_scanner::lutris::LutrisApplicationScannerConfig;
pub use crate::app_scanner::steam::SteamApplicationScannerConfig;
pub use crate::management::history::HistoryConfig;
pub use crate::management::web::ManagementConfig;
pub use crate::session::application::{ApplicationConfig, SessionMode};
pub use crate::session::capture::portal::DesktopConfig;
use crate::session::compositor::CompositorConfig;
pub use crate::session::frame_limit::FrameLimitConfig;
use crate::session::stream::StreamConfig;
pub use crate::session::stream::control::input::gamepad::GamepadConfig;
use crate::webserver::WebserverConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
	/// Name of the Moonshine host.
	pub name: String,

	/// Address to bind to.
	///
	/// Use IPv4 (eg. `0.0.0.0`) for IPv4-only, or IPv6 (eg. `::`) for dual-stack (IPv4 + IPv6).
	pub address: String,

	/// Configuration for the webserver.
	pub webserver: WebserverConfig,

	/// The web management interface (separate from the GameStream webserver).
	pub management: ManagementConfig,

	/// The persistent session history.
	pub history: HistoryConfig,

	/// Configuration for the streams.
	pub stream: StreamConfig,

	/// List of applications to expose to clients.
	#[serde(rename = "application")]
	pub applications: Vec<ApplicationConfig>,

	/// List of scanners that dynamically adds applications when started.
	#[serde(rename = "application_scanner")]
	#[serde(skip_serializing_if = "Vec::is_empty", default)]
	pub application_scanners: Vec<ApplicationScannerConfig>,

	/// How the application catalog follows launcher changes.
	pub library: LibraryConfig,

	/// Configuration for the compositor.
	pub compositor: CompositorConfig,

	/// Desktop Mode: streaming the running desktop (off by default).
	pub desktop: DesktopConfig,

	/// Optional frame limiting of launched applications (off by default).
	pub frame_limit: FrameLimitConfig,

	/// Inhibit system sleep (suspend) while a session is active.
	///
	/// Moonshine asks logind to block sleep for the duration of a stream. This
	/// requires the user to be in the `moonshine` group and the polkit rule
	/// shipped with Moonshine to be installed (handled by the packages). Set to
	/// `false` to disable.
	#[serde(default = "default_inhibit_sleep")]
	pub inhibit_sleep: bool,
}

fn default_inhibit_sleep() -> bool {
	true
}

impl Config {
	#[allow(clippy::result_unit_err)]
	pub fn read_from_file<P: AsRef<Path>>(file: P) -> Result<Config, ()> {
		let config =
			std::fs::read_to_string(file).map_err(|e| tracing::warn!("Failed to open configuration file: {e}"))?;
		let config: Config =
			toml::from_str(&config).map_err(|e| tracing::warn!("Failed to parse configuration file: {e}"))?;

		Ok(config)
	}

	pub fn load_or_create(path: &PathBuf) -> Result<Config, ()> {
		let mut config = if path.exists() {
			Self::read_from_file(path)?
		} else {
			tracing::info!(
				"No config file found at {}, creating a default config file.",
				path.display()
			);
			let config = Self::default();

			let serialized =
				toml::to_string_pretty(&config).map_err(|e| tracing::error!("Failed to serialize config: {e}"))?;

			let dir = path
				.parent()
				.ok_or_else(|| tracing::error!("Failed to get parent directory of config file."))?;
			std::fs::create_dir_all(dir).map_err(|e| tracing::error!("Failed to create config directory: {e}"))?;
			std::fs::write(path, serialized).map_err(|e| tracing::error!("Failed to save config file: {e}"))?;

			config
		};

		config.resolve_paths()?;
		config
			.validate()
			.map_err(|reason| tracing::error!("Invalid configuration: {reason}"))?;
		Ok(config)
	}

	/// Reject settings that would otherwise fail only once a client connects:
	/// listeners that cannot bind together or that clients cannot be told
	/// about, a bind address the stream listeners cannot parse, or a client
	/// timeout outside its safe domain. Values with a documented fallback (for
	/// example an undersized `max_packet_size`, which is ignored) are left to it.
	///
	/// Startup and the management interface both apply this check, so a setting
	/// is rejected with the same reason wherever it is entered.
	pub fn validate(&self) -> Result<(), String> {
		self.address
			.parse::<std::net::IpAddr>()
			.map_err(|error| format!("address '{}' is not an IP address: {error}", self.address))?;
		self.management.validate()?;
		self.history.validate()?;
		let tcp = [
			("webserver.port", self.webserver.port),
			("webserver.port_https", self.webserver.port_https),
			("stream.port", self.stream.port),
		];
		if self.management.enabled
			&& let Some((name, _)) = tcp.iter().find(|(_, port)| *port == self.management.port)
		{
			return Err(format!(
				"management.port and {name} both use TCP port {}",
				self.management.port
			));
		}
		let udp = [
			("stream.video.port", self.stream.video.port),
			("stream.audio.port", self.stream.audio.port),
			("stream.control.port", self.stream.control.port),
		];
		for (protocol, ports) in [("TCP", tcp), ("UDP", udp)] {
			for (index, (name, port)) in ports.iter().enumerate() {
				// Clients are told these ports (RTSP SETUP, serverinfo); an
				// ephemeral port would be advertised as 0 and be unreachable.
				if *port == 0 {
					return Err(format!("{name} must be a fixed port between 1 and 65535, not 0"));
				}
				if let Some((other, _)) = ports[index + 1..].iter().find(|(_, other)| other == port) {
					return Err(format!("{name} and {other} both use {protocol} port {port}"));
				}
			}
		}
		let poll = crate::app_scanner::service::POLL_INTERVAL_SECS;
		if !poll.contains(&self.library.poll_interval_secs) {
			return Err(format!(
				"library.poll_interval_secs must be between {} and {} seconds, not {}",
				poll.start(),
				poll.end(),
				self.library.poll_interval_secs
			));
		}
		self.frame_limit.validate()?;
		for application in &self.applications {
			application
				.frame_limit
				.validate(&format!("application '{}'", application.title))?;
		}
		for scanner in &self.application_scanners {
			scanner
				.frame_limit()
				.validate(&format!("the {} scanner", scanner.kind().id()))?;
		}
		if !STREAM_TIMEOUT_SECS.contains(&self.stream.timeout) {
			return Err(format!(
				"stream.timeout must be between {} and {} seconds, not {}",
				STREAM_TIMEOUT_SECS.start(),
				STREAM_TIMEOUT_SECS.end(),
				self.stream.timeout
			));
		}
		Ok(())
	}

	/// The configured entries clients are offered: `[[application]]`, then
	/// the Desktop Mode entry when `[desktop]` enables it and no application
	/// entry already streams the desktop.
	pub fn offered_applications(&self) -> Vec<ApplicationConfig> {
		let mut applications = self.applications.clone();
		if self.desktop.enabled && !applications.iter().any(|a| a.mode == SessionMode::Desktop) {
			applications.push(ApplicationConfig {
				title: self.desktop.title.clone(),
				mode: SessionMode::Desktop,
				..Default::default()
			});
		}
		applications
	}

	fn resolve_paths(&mut self) -> Result<(), ()> {
		let cert_path = self.webserver.certificate.to_string_lossy().to_string();
		let cert_path =
			shellexpand::full(&cert_path).map_err(|e| tracing::warn!("Failed to expand certificate path: {e}"))?;
		self.webserver.certificate = cert_path.to_string().into();

		let private_key_path = self.webserver.private_key.to_string_lossy().to_string();
		let private_key_path = shellexpand::full(&private_key_path)
			.map_err(|e| tracing::warn!("Failed to expand private key path: {e}"))?;
		self.webserver.private_key = private_key_path.to_string().into();

		Ok(())
	}
}

/// Safe domain of `[stream].timeout`, the active client's liveness deadline.
/// Zero would retire every authenticated client immediately; the upper bound
/// (one day) keeps deadline arithmetic far from overflow and matches the
/// management editor's range.
pub const STREAM_TIMEOUT_SECS: std::ops::RangeInclusive<u64> = 1..=86_400;

impl Default for Config {
	fn default() -> Self {
		Self {
			name: "Pyroshine".to_string(),
			// IPv4-only by default; set to `::` to bind dual-stack (the webserver
			// disables IPV6_V6ONLY, so that single address covers both).
			address: "0.0.0.0".to_string(),
			webserver: Default::default(),
			management: Default::default(),
			history: Default::default(),
			stream: Default::default(),
			applications: vec![ApplicationConfig {
				title: "Steam".to_string(),
				command: vec!["/usr/bin/steam".to_string(), "steam://open/bigpicture".to_string()],
				boxart: None,
				..Default::default()
			}],
			application_scanners: vec![ApplicationScannerConfig::Steam(SteamApplicationScannerConfig {
				library: "$HOME/.local/share/Steam".into(),
				command: vec![
					"/usr/bin/steam".to_string(),
					"-bigpicture".to_string(),
					"steam://rungameid/{game_id}".to_string(),
				],
				pre_command: Vec::new(),
				post_command: Vec::new(),
				stdout: None,
				stderr: None,
				launch_timeout_secs: 2,
				frame_limit: Default::default(),
			})],
			library: LibraryConfig::default(),
			compositor: CompositorConfig::default(),
			desktop: DesktopConfig::default(),
			frame_limit: FrameLimitConfig::default(),
			inhibit_sleep: true,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_configuration_is_valid() {
		assert_eq!(Config::default().validate(), Ok(()));
	}

	#[test]
	fn frame_limits_are_off_by_default_validated_and_omitted_when_unset() {
		let config = Config::default();
		assert_eq!(
			config.frame_limit.mode,
			crate::session::frame_limit::FrameLimitMode::Off
		);
		// Existing configurations serialize without any frame limit override.
		let application = toml::to_string(&config.applications[0]).unwrap();
		assert!(!application.contains("frame_limit"), "{application}");
		let parse = |toml: &str| toml::from_str::<Config>(toml).unwrap().validate();
		assert!(
			parse("[frame_limit]\nmode = \"custom\"\nfps = 5")
				.unwrap_err()
				.contains("frame_limit.fps")
		);
		let application = "[[application]]\ntitle = \"Game\"\ncommand = [\"game\"]\nframe_limit_mode = \"custom\"\nframe_limit_fps = 2000";
		assert!(parse(application).unwrap_err().contains("application 'Game'"));
		let scanner =
			"[[application_scanner]]\ntype = \"steam\"\nlibrary = \"/tmp\"\ncommand = [\"steam\"]\nframe_limit_fps = 1";
		assert!(parse(scanner).unwrap_err().contains("steam scanner"));
		let valid = "[frame_limit]\nmode = \"stream\"\nmethod = \"mangohud\"\n\n[[application]]\ntitle = \"Game\"\ncommand = [\"game\"]\nframe_limit_mode = \"off\"";
		let config: Config = toml::from_str(valid).unwrap();
		assert_eq!(config.validate(), Ok(()));
		assert_eq!(
			config.applications[0].frame_limit.frame_limit_mode,
			Some(crate::session::frame_limit::FrameLimitMode::Off)
		);
	}

	/// Review 2026-10-05 CFG-001: a hand-written file is held to the same
	/// safety domains as the management editor: advertised listener ports are
	/// fixed and nonzero, and the client timeout is within 1..=86400 seconds.
	#[test]
	fn unsafe_file_values_fail_with_the_setting_name() {
		let parse = |toml: &str| toml::from_str::<Config>(toml).unwrap().validate();
		for (toml, reason) in [
			("[stream]\ntimeout = 0", "stream.timeout"),
			("[stream]\ntimeout = 86401", "stream.timeout"),
			("[stream]\ntimeout = 9223372036854775807", "stream.timeout"),
			("[library]\npoll_interval_secs = 0", "library.poll_interval_secs"),
			("[library]\npoll_interval_secs = 86401", "library.poll_interval_secs"),
			("[stream]\nport = 0", "stream.port"),
			("[stream.video]\nport = 0", "stream.video.port"),
			("[stream.audio]\nport = 0", "stream.audio.port"),
			("[stream.control]\nport = 0", "stream.control.port"),
			(
				"[webserver]\nport = 0\nport_https = 47984\ncertificate = \"c\"\nprivate_key = \"k\"",
				"webserver.port",
			),
		] {
			let error = parse(toml).unwrap_err();
			assert!(error.starts_with(reason), "{toml}: {error}");
		}
		for timeout in [1, 60, 86_400] {
			assert_eq!(parse(&format!("[stream]\ntimeout = {timeout}")), Ok(()), "{timeout}");
		}
	}

	/// The management editor's range for the timeout is the runtime domain.
	#[test]
	fn timeout_domain_matches_the_editor_schema() {
		let schema = crate::management::schema::config_schema();
		let field = schema
			.fields
			.iter()
			.find(|field| field.path == "stream.timeout")
			.unwrap();
		let moonshine_management::dto::FieldKind::Integer { min, max, .. } = field.kind else {
			panic!("stream.timeout is an integer field");
		};
		assert_eq!(
			(min as u64, max as u64),
			(*STREAM_TIMEOUT_SECS.start(), *STREAM_TIMEOUT_SECS.end())
		);
	}

	#[test]
	fn conflicting_listeners_and_bad_addresses_are_rejected() {
		let mut config = Config::default();
		config.stream.port = config.webserver.port_https;
		assert!(config.validate().unwrap_err().contains("TCP port"));

		let mut config = Config::default();
		config.stream.audio.port = config.stream.control.port;
		assert!(config.validate().unwrap_err().contains("UDP port"));

		// TCP and UDP listeners may share a number.
		let mut config = Config::default();
		config.stream.video.port = config.stream.port;
		assert_eq!(config.validate(), Ok(()));

		for address in ["", "localhost", "0.0.0.0:47989", "300.1.1.1"] {
			let config = Config {
				address: address.to_string(),
				..Config::default()
			};
			assert!(config.validate().is_err(), "{address}");
		}
		for address in ["::", "127.0.0.1", "::ffff:192.168.1.2"] {
			let config = Config {
				address: address.to_string(),
				..Config::default()
			};
			assert_eq!(config.validate(), Ok(()), "{address}");
		}
	}
}
