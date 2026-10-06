//! Description of every user-editable `config.toml` setting for the desktop
//! configuration editor.
//!
//! The schema lives next to the typed configuration it describes. Choice
//! values are serialized from the Rust enums themselves, and the tests require
//! the schema to cover exactly the settings that `Config` serializes, so a
//! new setting cannot be added without deciding how the editor presents it.
//! Labels and help text follow `docs/CONFIGURATION.md`.

use moonshine_management::dto::{ChoiceOption, ConfigSchema, FieldKind, FieldSpec, ScannerVariant, SchemaSection};
use serde::Serialize;

use crate::session::compositor::{CaptureMode, VirtualConnectorStrategy};
use crate::session::stream::control::input::gamepad::{GamepadEmulation, HomeTrigger};
use crate::session::stream::video::{ConversionQueueMode, FecMode, PyroWaveQueueMode};

/// Paths of the list settings edited as structured entries.
pub(crate) const APPLICATIONS: &str = "application";
pub(crate) const SCANNERS: &str = "application_scanner";

fn section(id: &str, title: &str, description: &str) -> SchemaSection {
	SchemaSection {
		id: id.into(),
		title: title.into(),
		description: description.into(),
	}
}

struct Field(FieldSpec);

impl Field {
	fn new(path: &str, section: &str, label: &str, help: &str, kind: FieldKind) -> Self {
		Self(FieldSpec {
			path: path.into(),
			section: section.into(),
			label: label.into(),
			help: help.into(),
			kind,
			advanced: false,
			caution: None,
			required: false,
		})
	}

	fn advanced(mut self) -> Self {
		self.0.advanced = true;
		self
	}

	fn caution(mut self, caution: &str) -> Self {
		self.0.caution = Some(caution.into());
		self
	}

	fn required(mut self) -> Self {
		self.0.required = true;
		self
	}
}

fn text(optional: bool, placeholder: Option<&str>, suggestions: &[&str]) -> FieldKind {
	FieldKind::Text {
		optional,
		placeholder: placeholder.map(Into::into),
		suggestions: suggestions.iter().map(|value| value.to_string()).collect(),
	}
}

fn path(optional: bool, expands: bool, directory: bool) -> FieldKind {
	FieldKind::Path {
		optional,
		expands,
		directory,
	}
}

fn integer(min: f64, max: f64, unit: Option<&str>) -> FieldKind {
	FieldKind::Integer {
		min,
		max,
		unit: unit.map(Into::into),
		optional: false,
	}
}

/// Choice options whose values are the enum's serde names.
fn choices<T: Serialize + Copy>(variants: &[T], describe: impl Fn(T) -> (&'static str, &'static str)) -> FieldKind {
	FieldKind::Choice {
		options: variants
			.iter()
			.map(|variant| {
				let (label, description) = describe(*variant);
				ChoiceOption {
					value: serde_json::to_value(variant)
						.ok()
						.and_then(|value| value.as_str().map(str::to_string))
						.expect("choice enums serialize as strings"),
					label: label.into(),
					description: description.into(),
				}
			})
			.collect(),
		unset_label: None,
	}
}

fn fec_mode(mode: FecMode) -> (&'static str, &'static str) {
	match mode {
		FecMode::Off => ("Off", "Never send parity, even when the client requests a minimum."),
		FecMode::Fixed => ("Fixed", "Always use the configured percentage."),
		FecMode::Auto => (
			"Automatic",
			"Adjust between the minimum and maximum from the client's FEC feedback.",
		),
	}
}

fn queue_mode(mode: PyroWaveQueueMode) -> (&'static str, &'static str) {
	match mode {
		PyroWaveQueueMode::Auto => ("Automatic", "Graphics queue (recommended)."),
		PyroWaveQueueMode::Graphics => ("Graphics", "Graphics queue."),
		PyroWaveQueueMode::Compute => (
			"Compute",
			"Request a compute queue; the library falls back to graphics.",
		),
	}
}

fn conversion_mode(mode: ConversionQueueMode) -> (&'static str, &'static str) {
	match mode {
		ConversionQueueMode::Auto => (
			"Automatic",
			"Dedicated compute family when the GPU has one, otherwise graphics.",
		),
		ConversionQueueMode::Graphics => ("Graphics", "The graphics-capable queue family."),
		ConversionQueueMode::Compute => (
			"Compute",
			"The dedicated compute family; graphics when the GPU has none.",
		),
	}
}

fn emulation(mode: GamepadEmulation) -> (&'static str, &'static str) {
	match mode {
		GamepadEmulation::Auto => (
			"Automatic",
			"Preserve supported native controller models; Xbox for unknown or older model metadata.",
		),
		GamepadEmulation::Xbox => ("Xbox", "Always emulate Xbox controllers."),
		GamepadEmulation::Playstation => (
			"PlayStation",
			"Always emulate PlayStation controllers (motion and touchpad).",
		),
		GamepadEmulation::Nintendo => ("Nintendo", "Always emulate Nintendo controllers."),
	}
}

fn home_trigger(trigger: HomeTrigger) -> (&'static str, &'static str) {
	match trigger {
		HomeTrigger::Disabled => ("Disabled", "No synthetic Home button."),
		HomeTrigger::HoldBack => (
			"Hold Back (legacy)",
			"Holding Back sends Home; a short press sends a 100 ms Back tap.",
		),
		HomeTrigger::BackStart => (
			"Hold Back + Start (recommended)",
			"Holding both buttons sends Home; single presses pass through.",
		),
	}
}

fn capture_mode(mode: CaptureMode) -> (&'static str, &'static str) {
	match mode {
		CaptureMode::Auto => (
			"Automatic",
			"Export the application's buffer directly when it is the whole scene.",
		),
		CaptureMode::Composited => (
			"Always composite",
			"Render every frame through the compositor. For diagnosing capture problems.",
		),
	}
}

fn connector_strategy(strategy: VirtualConnectorStrategy) -> (&'static str, &'static str) {
	match strategy {
		VirtualConnectorStrategy::SingleApplication => (
			"Single application",
			"One focus across the output; the highest-priority window wins.",
		),
		VirtualConnectorStrategy::SteamControlled => ("Steam controlled", "Steam chooses the focused windows."),
		VirtualConnectorStrategy::PerAppId => ("Per app ID", "Separate focus per application ID."),
		VirtualConnectorStrategy::PerWindow => ("Per window", "Separate focus per window."),
	}
}

const OUTPUT_SUGGESTIONS: &[&str] = &["null", "journal", "inherit", "file:/path/to/log", "append:/path/to/log"];

/// Settings shared by applications and every scanner type.
fn launch_fields(section: &str, scanner: bool) -> Vec<FieldSpec> {
	let of = if scanner {
		"each discovered application"
	} else {
		"the application"
	};
	vec![
		Field::new(
			"pre_command",
			section,
			"Before launch",
			&format!(
				"Commands run in order before launching {of} (systemd ExecStartPre). A failing command can prevent launch."
			),
			FieldKind::CommandList,
		)
		.advanced()
		.0,
		Field::new(
			"post_command",
			section,
			"After session",
			&format!("Commands run after {of} stops (systemd ExecStopPost). Useful for cleanup."),
			FieldKind::CommandList,
		)
		.advanced()
		.0,
		Field::new(
			"stdout",
			section,
			"Standard output",
			"systemd StandardOutput destination. Unset discards output (\"null\").",
			text(true, Some("null"), OUTPUT_SUGGESTIONS),
		)
		.advanced()
		.0,
		Field::new(
			"stderr",
			section,
			"Standard error",
			"systemd StandardError destination. Use \"journal\" to capture errors.",
			text(true, Some("null"), OUTPUT_SUGGESTIONS),
		)
		.advanced()
		.0,
		Field::new(
			"launch_timeout_secs",
			section,
			"Launch timeout",
			"Time allowed to reach an active state after launch, separate from the wait for pre-commands. Increase for slow launchers.",
			integer(0.0, 3600.0, Some("s")),
		)
		.advanced()
		.0,
	]
}

fn application_fields() -> Vec<FieldSpec> {
	let section = "applications";
	let mut fields = vec![
		Field::new(
			"title",
			section,
			"Title",
			"Name shown to clients. It also derives the application ID, so keep titles distinct and stable.",
			text(false, Some("Steam"), &[]),
		)
		.required()
		.0,
		Field::new(
			"command",
			section,
			"Command",
			"Executable followed by its arguments. Commands are argument lists, not shell scripts; run a shell explicitly for shell syntax.",
			FieldKind::Command {
				placeholders: Vec::new(),
			},
		)
		.required()
		.0,
		Field::new(
			"boxart",
			section,
			"Cover image",
			"Local image shown by clients. Missing art is resolved automatically when possible.",
			path(true, false, false),
		)
		.0,
		Field::new(
			"output_scale",
			section,
			"Output scale",
			"Wayland output scale for this application. The stream keeps the client's physical resolution.",
			FieldKind::Number {
				min: 0.25,
				max: 8.0,
				step: 0.25,
				unit: None,
				optional: true,
			},
		)
		.0,
	];
	fields.extend(launch_fields(section, false));
	fields
}

fn scanner_command(placeholders: &[&str]) -> FieldSpec {
	Field::new(
		"command",
		"scanners",
		"Launch command",
		"Command used for each discovered application; placeholders are replaced in every argument.",
		FieldKind::Command {
			placeholders: placeholders.iter().map(|value| value.to_string()).collect(),
		},
	)
	.required()
	.0
}

fn scanner_variants() -> Vec<ScannerVariant> {
	let section = "scanners";
	let variant = |id: &str, label: &str, description: &str, mut fields: Vec<FieldSpec>| {
		fields.extend(launch_fields(section, true));
		ScannerVariant {
			id: id.into(),
			label: label.into(),
			description: description.into(),
			fields,
		}
	};
	vec![
		variant(
			"steam",
			"Steam",
			"Installed games in the Steam installation and its libraries.",
			vec![
				Field::new(
					"library",
					section,
					"Steam directory",
					"Path to the Steam installation, for example $HOME/.local/share/Steam.",
					path(false, true, true),
				)
				.required()
				.0,
				scanner_command(&["{game_id}"]),
			],
		),
		variant(
			"lutris",
			"Lutris",
			"Installed entries from the Lutris database.",
			vec![
				Field::new(
					"pga_db",
					section,
					"Lutris database",
					"Path to pga.db. Defaults to the user's data directory (usually ~/.local/share/lutris/pga.db).",
					path(false, true, false),
				)
				.0,
				scanner_command(&["{slug}"]),
			],
		),
		variant(
			"heroic",
			"Heroic Games Launcher",
			"Installed games across Heroic stores and sideloaded apps, excluding DLC.",
			vec![
				Field::new(
					"config_dir",
					section,
					"Heroic configuration",
					"Heroic's configuration directory. Defaults to the native config when present, otherwise the Flatpak config.",
					path(false, true, true),
				)
				.0,
				scanner_command(&["{app_name}", "{runner}"]),
			],
		),
		variant(
			"desktop",
			"Desktop entries",
			"Recursively scan .desktop launchers; each entry's Exec line is its command.",
			vec![
				Field::new(
					"directories",
					section,
					"Directories",
					"Directories scanned recursively for .desktop files.",
					FieldKind::PathList { expands: true },
				)
				.required()
				.0,
				Field::new(
					"include_terminal",
					section,
					"Include terminal applications",
					"Include entries marked Terminal=true.",
					FieldKind::Bool,
				)
				.0,
				Field::new(
					"resolve_icons",
					section,
					"Resolve icons",
					"Resolve desktop entry icons into cover images.",
					FieldKind::Bool,
				)
				.0,
			],
		),
	]
}

/// The complete editor schema.
pub(crate) fn config_schema() -> ConfigSchema {
	let sections = vec![
		section("general", "General", "Host identity and power management."),
		section("network", "Network", "Listener address, ports and client timeout."),
		section(
			"security",
			"Pairing & security",
			"Client pairing, TLS identity and stream encryption.",
		),
		section(
			"video",
			"Video",
			"Forward error correction, packet size and GPU queues.",
		),
		section(
			"input",
			"Input & controllers",
			"Virtual controllers and the Home button shortcut.",
		),
		section(
			"display",
			"Compositor & display",
			"GPU selection, capture, HDR and window focus.",
		),
		section("keyboard", "Keyboard", "XKB layout of the streaming session."),
		section("applications", "Applications", "Applications offered to clients."),
		section(
			"scanners",
			"Application discovery",
			"Scanners that add installed games when Pyroshine starts.",
		),
		section("diagnostics", "Diagnostics", "Logging for performance investigation."),
	];

	let port =
		|path: &str, section: &str, label: &str, help: &str| Field::new(path, section, label, help, FieldKind::Port);
	let fields = vec![
		Field::new(
			"name",
			"general",
			"Host name",
			"Name shown to clients and advertised on the network.",
			text(false, Some("Pyroshine"), &[]),
		),
		Field::new(
			"inhibit_sleep",
			"general",
			"Prevent sleep while streaming",
			"Ask logind to block suspend during a session. Needs the shipped polkit rule and pyroshine group access.",
			FieldKind::Bool,
		),
		Field::new(
			"address",
			"network",
			"Bind address",
			"IP address for the web and stream listeners. 0.0.0.0 is IPv4 only; :: listens on IPv4 and IPv6.",
			text(false, Some("0.0.0.0"), &["0.0.0.0", "::"]),
		)
		.caution("Clients must be able to reach this address."),
		port(
			"webserver.port",
			"network",
			"HTTP port",
			"GameStream HTTP API and host-local pairing page (TCP).",
		)
		.caution("Clients and firewalls expect the default ports unless reconfigured."),
		port(
			"webserver.port_https",
			"network",
			"HTTPS port",
			"GameStream HTTPS API (TCP).",
		)
		.caution("Clients and firewalls expect the default ports unless reconfigured."),
		port("stream.port", "network", "RTSP port", "Stream negotiation (TCP)."),
		port("stream.video.port", "network", "Video port", "Video stream (UDP)."),
		port("stream.audio.port", "network", "Audio port", "Audio stream (UDP)."),
		port("stream.control.port", "network", "Control port", "Input and control stream (UDP)."),
		Field::new(
			"stream.timeout",
			"network",
			"Client timeout",
			"Seconds a streaming client may go without a ping before it is treated as disconnected. The session and application keep running for a resume.",
			integer(1.0, 86_400.0, Some("s")),
		),
		Field::new(
			"webserver.enable_pairing",
			"security",
			"Allow new pairings",
			"Allow new clients to pair. Disable after pairing your devices.",
			FieldKind::Bool,
		),
		Field::new(
			"stream.video.encrypt",
			"security",
			"Encrypt video",
			"Encrypt the video stream with AES-128-GCM when the client supports it.",
			FieldKind::Bool,
		),
		Field::new(
			"webserver.certificate",
			"security",
			"TLS certificate",
			"Server certificate, created if missing. ~ and environment variables are expanded.",
			path(false, true, false),
		)
		.advanced()
		.caution("Changing the server identity requires pairing every client again."),
		Field::new(
			"webserver.private_key",
			"security",
			"TLS private key",
			"Private key of the server certificate, created if missing. Keep it private.",
			path(false, true, false),
		)
		.advanced()
		.caution("Changing the server identity requires pairing every client again."),
		Field::new(
			"stream.video.fec_mode",
			"video",
			"Error correction",
			"How video parity (forward error correction) is chosen.",
			choices(&[FecMode::Off, FecMode::Fixed, FecMode::Auto], fec_mode),
		),
		Field::new(
			"stream.video.fec_percentage",
			"video",
			"Parity",
			"Parity as a percentage of data packets in fixed mode; the starting value in automatic mode.",
			integer(0.0, 255.0, Some("%")),
		),
		Field::new(
			"stream.video.fec_min_percentage",
			"video",
			"Automatic minimum",
			"Lower bound for automatic error correction.",
			integer(0.0, 255.0, Some("%")),
		),
		Field::new(
			"stream.video.fec_max_percentage",
			"video",
			"Automatic maximum",
			"Upper bound for automatic error correction; keep it at least the minimum.",
			integer(0.0, 255.0, Some("%")),
		),
		Field::new(
			"stream.video.max_packet_size",
			"video",
			"Packet size cap",
			"Upper bound for the client's video packet size in bytes; 0 disables the cap and values below 200 are ignored. Lower it for VPNs and tunnels with a small MTU.",
			integer(0.0, 65_000.0, Some("bytes")),
		),
		Field::new(
			"stream.video.pyrowave_queue",
			"video",
			"PyroWave GPU queue",
			"GPU queue preference for PyroWave encoding.",
			choices(
				&[
					PyroWaveQueueMode::Auto,
					PyroWaveQueueMode::Graphics,
					PyroWaveQueueMode::Compute,
				],
				queue_mode,
			),
		)
		.advanced(),
		Field::new(
			"stream.video.conversion_queue",
			"video",
			"Color conversion queue",
			"GPU queue for H.264/HEVC/AV1 color conversion.",
			choices(
				&[
					ConversionQueueMode::Auto,
					ConversionQueueMode::Graphics,
					ConversionQueueMode::Compute,
				],
				conversion_mode,
			),
		)
		.advanced(),
		Field::new(
			"stream.control.gamepad.emulation",
			"input",
			"Controller type",
			"Virtual controller family presented to games.",
			choices(
				&[
					GamepadEmulation::Auto,
					GamepadEmulation::Xbox,
					GamepadEmulation::Playstation,
					GamepadEmulation::Nintendo,
				],
				emulation,
			),
		),
		Field::new(
			"stream.control.gamepad.home_button.trigger",
			"input",
			"Home shortcut",
			"Button combination that sends Home/Guide. Unset keeps older configurations working: a nonzero hold time selects Hold Back.",
			match choices(
				&[HomeTrigger::Disabled, HomeTrigger::HoldBack, HomeTrigger::BackStart],
				home_trigger,
			) {
				FieldKind::Choice { options, .. } => FieldKind::Choice {
					options,
					unset_label: Some("Legacy (from hold time)".into()),
				},
				kind => kind,
			},
		),
		Field::new(
			"stream.control.gamepad.home_button.hold_ms",
			"input",
			"Hold time",
			"How long the shortcut must be held. 0 disables the shortcut for every trigger.",
			integer(0.0, 10_000.0, Some("ms")),
		),
		Field::new(
			"stream.control.gamepad.home_button.rumble_duration_ms",
			"input",
			"Activation rumble",
			"Length of the rumble pulse when the shortcut fires; 0 disables it.",
			integer(0.0, 5_000.0, Some("ms")),
		),
		Field::new(
			"stream.control.gamepad.home_button.rumble_intensity",
			"input",
			"Rumble strength",
			"Strength of the activation pulse.",
			FieldKind::Number {
				min: 0.0,
				max: 1.0,
				step: 0.05,
				unit: None,
				optional: false,
			},
		),
		Field::new(
			"stream.control.gamepad.home_button.suppress_home",
			"input",
			"Ignore the physical Home button",
			"Drop the client's physical Home/Guide button. The shortcut still works.",
			FieldKind::Bool,
		),
		Field::new(
			"compositor.gpu",
			"display",
			"GPU",
			"Render node path (/dev/dri/renderD128), node name, or part of the device's uevent (such as a PCI ID). Unset selects automatically.",
			text(true, Some("Automatic"), &["/dev/dri/renderD128", "/dev/dri/renderD129"]),
		)
		.caution("The capture GPU must be the GPU Vulkan encodes on, or startup fails."),
		Field::new(
			"compositor.hdr",
			"display",
			"HDR",
			"Allow HDR when the GPU and client support it. Disabling it stops advertising HDR.",
			FieldKind::Bool,
		),
		Field::new(
			"compositor.steam_mode",
			"display",
			"Steam mode",
			"Steam window filtering and Steam-controlled focus, like Gamescope's -e.",
			FieldKind::Bool,
		),
		Field::new(
			"compositor.virtual_connector_strategy",
			"display",
			"Window focus",
			"Focus policy when Steam mode is off.",
			choices(
				&[
					VirtualConnectorStrategy::SingleApplication,
					VirtualConnectorStrategy::SteamControlled,
					VirtualConnectorStrategy::PerAppId,
					VirtualConnectorStrategy::PerWindow,
				],
				connector_strategy,
			),
		)
		.advanced(),
		Field::new(
			"compositor.capture_mode",
			"display",
			"Capture",
			"How frames are captured.",
			choices(&[CaptureMode::Auto, CaptureMode::Composited], capture_mode),
		)
		.advanced()
		.caution("Always compositing costs GPU time; use it to diagnose capture problems."),
		Field::new(
			"compositor.keyboard.layout",
			"keyboard",
			"Layout",
			"XKB layout, for example us or de.",
			text(false, Some("us"), &["us", "gb", "de", "fr", "es", "it", "jp"]),
		),
		Field::new(
			"compositor.keyboard.variant",
			"keyboard",
			"Variant",
			"XKB layout variant; empty uses the layout's default.",
			text(false, None, &[]),
		),
		Field::new(
			"compositor.keyboard.model",
			"keyboard",
			"Model",
			"XKB keyboard model; empty uses XKB's default.",
			text(false, None, &[]),
		),
		Field::new(
			"compositor.keyboard.options",
			"keyboard",
			"Options",
			"XKB options such as caps:escape. Unset uses none.",
			text(true, None, &["caps:escape", "compose:ralt"]),
		),
		Field::new(
			APPLICATIONS,
			"applications",
			"Applications",
			"Static applications. Discovered applications are added to this list when Pyroshine starts.",
			FieldKind::Applications {
				item: application_fields(),
			},
		),
		Field::new(
			SCANNERS,
			"scanners",
			"Scanners",
			"Scanners run at startup; restart Pyroshine after installing games.",
			FieldKind::Scanners {
				variants: scanner_variants(),
			},
		),
		Field::new(
			"stream.video.log_stats",
			"diagnostics",
			"Periodic statistics",
			"Log five-second capture, pipeline, transport and runtime summaries. Disabling also skips their collection.",
			FieldKind::Bool,
		),
		Field::new(
			"stream.video.log_frame_spikes",
			"diagnostics",
			"Frame spike warnings",
			"Warn when one frame's encoding and packetization exceeds the frame budget.",
			FieldKind::Bool,
		)
		.advanced(),
	];

	ConfigSchema {
		sections,
		fields: fields.into_iter().map(|field| field.0).collect(),
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use serde_json::{Value, json};

	use super::*;
	use crate::config::Config;

	/// A configuration in which every optional setting is set and every list
	/// and scanner type is present, so its serialization names every setting.
	pub(crate) fn complete_config() -> Value {
		let mut config = serde_json::to_value(Config::default()).unwrap();
		config["compositor"]["gpu"] = json!("/dev/dri/renderD128");
		config["compositor"]["keyboard"]["options"] = json!("caps:escape");
		config["stream"]["control"]["gamepad"]["home_button"]["trigger"] = json!("back_start");
		let launch = json!({
			"pre_command": [["/usr/bin/true"]],
			"post_command": [["/usr/bin/true"]],
			"stdout": "journal",
			"stderr": "journal",
			"launch_timeout_secs": 5,
		});
		let mut application = json!({
			"title": "Game",
			"command": ["/usr/bin/game"],
			"boxart": "/tmp/art.png",
			"output_scale": 1.5,
		});
		merge(&mut application, &launch);
		config["application"] = json!([application]);
		let scanners = [
			json!({"type": "steam", "library": "~/.steam", "command": ["steam"]}),
			json!({"type": "lutris", "pga_db": "/tmp/pga.db", "command": ["lutris"]}),
			json!({"type": "heroic", "config_dir": "/tmp/heroic", "command": ["heroic"]}),
			json!({"type": "desktop", "directories": ["/tmp"], "include_terminal": true, "resolve_icons": false}),
		];
		config["application_scanner"] = Value::Array(
			scanners
				.into_iter()
				.map(|mut scanner| {
					merge(&mut scanner, &launch);
					scanner
				})
				.collect(),
		);
		// Every setting survives a typed round trip, so none was misspelled.
		let typed: Config = serde_json::from_value(config.clone()).unwrap();
		assert_eq!(serde_json::to_value(typed).unwrap(), config);
		config
	}

	fn merge(target: &mut Value, extra: &Value) {
		for (key, value) in extra.as_object().unwrap() {
			target[key] = value.clone();
		}
	}

	fn leaves(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
		match value {
			Value::Object(map) if prefix != APPLICATIONS && prefix != SCANNERS => {
				for (key, value) in map {
					let path = if prefix.is_empty() {
						key.clone()
					} else {
						format!("{prefix}.{key}")
					};
					leaves(value, &path, out);
				}
			},
			_ => {
				out.insert(prefix.to_string());
			},
		}
	}

	fn keys(value: &Value) -> BTreeSet<String> {
		value.as_object().unwrap().keys().cloned().collect()
	}

	fn paths(fields: &[FieldSpec]) -> BTreeSet<String> {
		fields.iter().map(|field| field.path.clone()).collect()
	}

	#[test]
	fn schema_covers_exactly_the_serialized_settings() {
		let config = complete_config();
		let schema = config_schema();

		let mut expected = BTreeSet::new();
		leaves(&config, "", &mut expected);
		assert_eq!(paths(&schema.fields), expected, "top-level settings");

		let field = |path: &str| schema.fields.iter().find(|field| field.path == path).unwrap();
		let FieldKind::Applications { item } = &field(APPLICATIONS).kind else {
			panic!("applications are structured");
		};
		assert_eq!(paths(item), keys(&config[APPLICATIONS][0]), "application settings");

		let FieldKind::Scanners { variants } = &field(SCANNERS).kind else {
			panic!("scanners are structured");
		};
		let scanners = config[SCANNERS].as_array().unwrap();
		assert_eq!(variants.len(), scanners.len());
		for (variant, scanner) in variants.iter().zip(scanners) {
			assert_eq!(scanner["type"], variant.id.as_str());
			let mut settings = keys(scanner);
			settings.remove("type");
			assert_eq!(paths(&variant.fields), settings, "{} scanner settings", variant.id);
		}

		let sections: BTreeSet<_> = schema.sections.iter().map(|section| section.id.clone()).collect();
		for field in &schema.fields {
			assert!(sections.contains(&field.section), "{} has a known section", field.path);
			assert!(
				!field.label.is_empty() && !field.help.is_empty(),
				"{} is documented",
				field.path
			);
		}
	}

	#[test]
	fn every_choice_value_is_accepted_by_the_typed_configuration() {
		let schema = config_schema();
		for field in &schema.fields {
			let FieldKind::Choice { options, .. } = &field.kind else {
				continue;
			};
			assert!(options.len() >= 2, "{}", field.path);
			for option in options {
				let mut config = complete_config();
				let segments: Vec<_> = field.path.split('.').collect();
				let (last, parents) = segments.split_last().unwrap();
				let mut target = &mut config;
				for segment in parents {
					target = &mut target[*segment];
				}
				target[*last] = json!(option.value);
				let typed: Config = serde_json::from_value(config)
					.unwrap_or_else(|e| panic!("{} = {} is rejected: {e}", field.path, option.value));
				let value = serde_json::to_value(typed).unwrap();
				let mut target = &value;
				for segment in &segments {
					target = &target[*segment];
				}
				assert_eq!(target, &json!(option.value), "{}", field.path);
			}
		}
	}

	/// The desktop app renders forms from this schema. Its tests and browser
	/// preview read a committed copy, which must match the daemon's schema.
	/// Regenerate it with `MOONSHINE_UPDATE_UI_FIXTURES=1 cargo test -p
	/// moonshine-core ui_schema_fixture_is_current`.
	#[test]
	fn ui_schema_fixture_is_current() {
		let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pyroshine-ui/src/api/schema.fixture.json");
		let fixture = serde_json::to_string_pretty(&json!({
			"schema": config_schema(),
			"defaults": Config::default(),
		}))
		.unwrap() + "\n";
		if std::env::var_os("MOONSHINE_UPDATE_UI_FIXTURES").is_some() {
			std::fs::write(&path, &fixture).unwrap();
		}
		let committed = std::fs::read_to_string(&path).unwrap_or_default();
		assert!(
			committed == fixture,
			"{} is out of date; regenerate it with MOONSHINE_UPDATE_UI_FIXTURES=1 and check the desktop app renders the change",
			path.display()
		);
	}
}
