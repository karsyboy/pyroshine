//! Optional, game-scoped frame limiting through in-process limiters.
//!
//! Pyroshine's capture pacing stays authoritative: the compositor already
//! paces frame callbacks to the stream rate, which limits games that wait for
//! them (FIFO/vsync). Games presenting without waiting (mailbox, immediate,
//! "vsync off") render as fast as the GPU allows and the surplus frames are
//! coalesced at capture, wasting GPU time that encoding competes for. This
//! module asks such games to limit themselves.
//!
//! Limits are environment variables on the application's transient systemd
//! unit only: they reach the launched command and its children (including
//! games started by a Steam client that Pyroshine launched), never the user's
//! desktop Steam or games launched outside Pyroshine, and no helper process
//! exists. Supported limiters:
//!
//! - `proton`: DXVK (`DXVK_FRAME_RATE`, Direct3D 8–11) and vkd3d-proton
//!   (`VKD3D_FRAME_RATE`, Direct3D 12), the limiters built into Proton and
//!   Wine's Vulkan translation layers. Native Linux games are not limited.
//! - `mangohud`: MangoHud's Vulkan layer with a hidden overlay
//!   (`MANGOHUD=1`, `MANGOHUD_CONFIG=no_display,fps_limit=N`), for every Vulkan
//!   game including Proton's. Requires MangoHud's implicit layer to be
//!   installed; OpenGL games are not limited.
//!
//! One method applies per launch, so two limiters never fight. Values the user
//! already set in the user manager's environment win, and the game's own
//! launch options (Steam's `%command%`, `env` in the command) apply after
//! ours.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Frame limit policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameLimitMode {
	/// Launch applications unchanged.
	#[default]
	Off,
	/// Limit to the frame rate of the stream that launches the application.
	Stream,
	/// Limit to a fixed frame rate.
	Custom,
}

/// Limiter that enforces the limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameLimitMethod {
	/// DXVK and vkd3d-proton limiters (Proton/Wine Direct3D games).
	#[default]
	Proton,
	/// MangoHud's Vulkan layer, overlay hidden (every Vulkan game).
	Mangohud,
}

pub const MIN_FPS: u32 = 10;
pub const MAX_FPS: u32 = 1_000;

fn default_fps() -> u32 {
	60
}

/// `[frame_limit]`: the default for every application.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameLimitConfig {
	pub mode: FrameLimitMode,
	/// Frame rate for `mode = "custom"`.
	pub fps: u32,
	pub method: FrameLimitMethod,
}

impl Default for FrameLimitConfig {
	fn default() -> Self {
		Self {
			mode: FrameLimitMode::Off,
			fps: default_fps(),
			method: FrameLimitMethod::Proton,
		}
	}
}

/// Per-application (or per-scanner) overrides; unset fields use `[frame_limit]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameLimitOverride {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_limit_mode: Option<FrameLimitMode>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_limit_fps: Option<u32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_limit_method: Option<FrameLimitMethod>,
}

fn validate_fps(name: &str, fps: u32) -> Result<(), String> {
	if (MIN_FPS..=MAX_FPS).contains(&fps) {
		Ok(())
	} else {
		Err(format!("{name} must be {MIN_FPS}–{MAX_FPS}"))
	}
}

impl FrameLimitConfig {
	pub fn validate(&self) -> Result<(), String> {
		validate_fps("frame_limit.fps", self.fps)
	}
}

impl FrameLimitOverride {
	pub fn validate(&self, owner: &str) -> Result<(), String> {
		match self.frame_limit_fps {
			Some(fps) => validate_fps(&format!("frame_limit_fps of {owner}"), fps),
			None => Ok(()),
		}
	}

	pub fn is_empty(&self) -> bool {
		*self == Self::default()
	}
}

/// The limit for one launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLimit {
	pub fps: u32,
	pub method: FrameLimitMethod,
}

/// Combine the global policy, an application's overrides and the launching
/// stream's frame rate. `None` means the application launches unchanged.
pub fn resolve(global: &FrameLimitConfig, application: &FrameLimitOverride, stream_fps: u32) -> Option<FrameLimit> {
	let mode = application.frame_limit_mode.unwrap_or(global.mode);
	let fps = match mode {
		FrameLimitMode::Off => return None,
		FrameLimitMode::Stream => stream_fps,
		FrameLimitMode::Custom => application.frame_limit_fps.unwrap_or(global.fps),
	};
	(fps > 0).then_some(FrameLimit {
		fps: fps.clamp(MIN_FPS, MAX_FPS),
		method: application.frame_limit_method.unwrap_or(global.method),
	})
}

/// Environment for a launch, and what was decided (for the log).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LimitEnvironment {
	pub variables: Vec<(String, String)>,
	/// The method actually used, after fallbacks.
	pub method: Option<FrameLimitMethod>,
	pub notes: Vec<String>,
}

/// Variables that would conflict with a limit the user configured themselves.
const PROTON_KEYS: [&str; 2] = ["DXVK_FRAME_RATE", "VKD3D_FRAME_RATE"];
const MANGOHUD_USER_KEYS: [&str; 4] = [
	"MANGOHUD_CONFIG",
	"MANGOHUD_CONFIGFILE",
	"DISABLE_MANGOHUD",
	"MANGOHUD_DLSYM",
];

/// `inherited`: variable names the application unit would otherwise inherit
/// from the user manager. `mangohud_layer`: whether MangoHud's implicit Vulkan
/// layer is installed.
pub fn environment(limit: FrameLimit, inherited: &HashSet<String>, mangohud_layer: bool) -> LimitEnvironment {
	let mut result = LimitEnvironment::default();
	let fps = limit.fps.to_string();
	let mut method = limit.method;
	if method == FrameLimitMethod::Mangohud {
		if !mangohud_layer {
			result
				.notes
				.push("MangoHud's Vulkan layer is not installed; using the Proton limiters instead".into());
			method = FrameLimitMethod::Proton;
		} else if let Some(key) = MANGOHUD_USER_KEYS.iter().find(|key| inherited.contains(**key)) {
			// The user's own MangoHud setup decides; adding ours would replace it.
			result.notes.push(format!(
				"{key} is set in the user environment; leaving MangoHud to it and using the Proton limiters"
			));
			method = FrameLimitMethod::Proton;
		}
	}
	match method {
		FrameLimitMethod::Proton => {
			for key in PROTON_KEYS {
				if inherited.contains(key) {
					result
						.notes
						.push(format!("{key} is set in the user environment; keeping it"));
				} else {
					result.variables.push((key.into(), fps.clone()));
				}
			}
			if !result.variables.is_empty() {
				result.method = Some(FrameLimitMethod::Proton);
			}
		},
		FrameLimitMethod::Mangohud => {
			result.variables.push(("MANGOHUD".into(), "1".into()));
			result
				.variables
				.push(("MANGOHUD_CONFIG".into(), format!("no_display,fps_limit={fps}")));
			result.method = Some(FrameLimitMethod::Mangohud);
		},
	}
	result
}

/// Implicit Vulkan layer directories, in the loader's search order.
fn implicit_layer_dirs() -> Vec<PathBuf> {
	let home = std::env::var_os("HOME").map(PathBuf::from);
	let config_home = std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.or_else(|| home.as_ref().map(|h| h.join(".config")));
	let data_home = std::env::var_os("XDG_DATA_HOME")
		.map(PathBuf::from)
		.or_else(|| home.as_ref().map(|h| h.join(".local/share")));
	let config_dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into());
	let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
	let mut dirs = Vec::new();
	dirs.extend(config_home);
	dirs.extend(config_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
	dirs.push(PathBuf::from("/etc"));
	dirs.extend(data_home);
	dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
	dirs.into_iter()
		.map(|dir| dir.join("vulkan/implicit_layer.d"))
		.collect()
}

/// Whether a MangoHud implicit layer manifest enabled by `MANGOHUD=1` exists.
pub fn mangohud_layer_installed() -> bool {
	mangohud_layer_in(&implicit_layer_dirs())
}

fn mangohud_layer_in(dirs: &[PathBuf]) -> bool {
	dirs.iter().any(|dir| {
		std::fs::read_dir(dir).is_ok_and(|entries| {
			entries.flatten().any(|entry| {
				let path = entry.path();
				is_mangohud_manifest(&path)
			})
		})
	})
}

fn is_mangohud_manifest(path: &Path) -> bool {
	path.extension().is_some_and(|e| e == "json")
		&& std::fs::read_to_string(path)
			.is_ok_and(|text| text.contains("VK_LAYER_MANGOHUD") && text.contains("\"MANGOHUD\""))
}

/// Human-readable support status for the health check.
pub fn support_summary(config: &FrameLimitConfig, mangohud_layer: bool) -> (bool, String) {
	let mangohud = if mangohud_layer {
		"MangoHud's Vulkan layer is installed"
	} else {
		"MangoHud's Vulkan layer is not installed"
	};
	match config.mode {
		FrameLimitMode::Off => (
			true,
			format!(
				"Frame limiting is off by default (applications may override it). Proton limiters are always \
				 available; {mangohud}."
			),
		),
		_ => {
			let target = match config.mode {
				FrameLimitMode::Stream => "the stream's frame rate".to_string(),
				_ => format!("{} FPS", config.fps),
			};
			match config.method {
				FrameLimitMethod::Proton => (
					true,
					format!(
						"Limiting games to {target} with Proton's DXVK/vkd3d-proton limiters: Direct3D games under \
						 Proton or Wine. Native Vulkan and OpenGL games are not limited ({mangohud})."
					),
				),
				FrameLimitMethod::Mangohud if mangohud_layer => (
					true,
					format!(
						"Limiting games to {target} with MangoHud's Vulkan layer (overlay hidden): Vulkan games, \
						 including Proton's. OpenGL games are not limited."
					),
				),
				FrameLimitMethod::Mangohud => (
					false,
					format!(
						"frame_limit.method = \"mangohud\", but {mangohud}: launches fall back to the Proton \
						 limiters. Install MangoHud (including its 32-bit layer for 32-bit games) or choose \
						 \"proton\"."
					),
				),
			}
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn global(mode: FrameLimitMode, fps: u32, method: FrameLimitMethod) -> FrameLimitConfig {
		FrameLimitConfig { mode, fps, method }
	}

	#[test]
	fn off_by_default_and_overrides_win() {
		let none = FrameLimitOverride::default();
		assert_eq!(resolve(&FrameLimitConfig::default(), &none, 120), None);
		let stream = global(FrameLimitMode::Stream, 60, FrameLimitMethod::Proton);
		assert_eq!(
			resolve(&stream, &none, 120),
			Some(FrameLimit {
				fps: 120,
				method: FrameLimitMethod::Proton
			})
		);
		// An application can opt out of a global limit, or into one.
		let off = FrameLimitOverride {
			frame_limit_mode: Some(FrameLimitMode::Off),
			..Default::default()
		};
		assert_eq!(resolve(&stream, &off, 120), None);
		let custom = FrameLimitOverride {
			frame_limit_mode: Some(FrameLimitMode::Custom),
			frame_limit_fps: Some(90),
			frame_limit_method: Some(FrameLimitMethod::Mangohud),
		};
		assert_eq!(
			resolve(&FrameLimitConfig::default(), &custom, 120),
			Some(FrameLimit {
				fps: 90,
				method: FrameLimitMethod::Mangohud
			})
		);
		// Custom without an application rate uses the global one.
		let global_custom = global(FrameLimitMode::Custom, 48, FrameLimitMethod::Proton);
		assert_eq!(resolve(&global_custom, &none, 120).unwrap().fps, 48);
	}

	#[test]
	fn proton_environment_respects_user_values() {
		let limit = FrameLimit {
			fps: 120,
			method: FrameLimitMethod::Proton,
		};
		let env = environment(limit, &HashSet::new(), false);
		assert_eq!(
			env.variables,
			vec![
				("DXVK_FRAME_RATE".into(), "120".into()),
				("VKD3D_FRAME_RATE".into(), "120".into())
			]
		);
		let inherited: HashSet<String> = ["DXVK_FRAME_RATE".to_string()].into();
		let env = environment(limit, &inherited, false);
		assert_eq!(env.variables, vec![("VKD3D_FRAME_RATE".into(), "120".into())]);
		assert!(env.notes[0].contains("keeping"));
		let both: HashSet<String> = PROTON_KEYS.iter().map(|k| k.to_string()).collect();
		assert_eq!(environment(limit, &both, false).method, None);
	}

	#[test]
	fn mangohud_is_used_alone_and_falls_back_gracefully() {
		let limit = FrameLimit {
			fps: 60,
			method: FrameLimitMethod::Mangohud,
		};
		let env = environment(limit, &HashSet::new(), true);
		assert_eq!(env.method, Some(FrameLimitMethod::Mangohud));
		assert_eq!(
			env.variables,
			vec![
				("MANGOHUD".into(), "1".into()),
				("MANGOHUD_CONFIG".into(), "no_display,fps_limit=60".into())
			]
		);
		// Never both limiters at once.
		assert!(!env.variables.iter().any(|(k, _)| PROTON_KEYS.contains(&k.as_str())));
		let missing = environment(limit, &HashSet::new(), false);
		assert_eq!(missing.method, Some(FrameLimitMethod::Proton));
		assert!(missing.notes[0].contains("not installed"));
		let user: HashSet<String> = ["MANGOHUD_CONFIG".to_string()].into();
		let kept = environment(limit, &user, true);
		assert_eq!(kept.method, Some(FrameLimitMethod::Proton));
		assert!(!kept.variables.iter().any(|(k, _)| k.starts_with("MANGOHUD")));
	}

	#[test]
	fn finds_mangohud_manifests_only() {
		let dir = tempfile::tempdir().unwrap();
		let layers = dir.path().join("vulkan/implicit_layer.d");
		std::fs::create_dir_all(&layers).unwrap();
		std::fs::write(
			layers.join("VkLayer_MESA_device_select.json"),
			r#"{"layer":{"name":"VK_LAYER_MESA_device_select"}}"#,
		)
		.unwrap();
		assert!(!mangohud_layer_in(std::slice::from_ref(&layers)));
		std::fs::write(
			layers.join("MangoHud.x86_64.json"),
			r#"{"layer":{"name":"VK_LAYER_MANGOHUD_overlay_x86_64","enable_environment":{"MANGOHUD":"1"}}}"#,
		)
		.unwrap();
		assert!(mangohud_layer_in(&[dir.path().join("missing"), layers]));
	}

	#[test]
	fn configuration_round_trips_and_validates() {
		let config: FrameLimitConfig = toml::from_str("mode = \"stream\"\nmethod = \"mangohud\"").unwrap();
		assert_eq!(config.mode, FrameLimitMode::Stream);
		assert_eq!(config.fps, 60);
		assert!(config.validate().is_ok());
		assert!(
			FrameLimitConfig {
				fps: 5,
				..config.clone()
			}
			.validate()
			.is_err()
		);
		let application = FrameLimitOverride {
			frame_limit_fps: Some(2_000),
			..Default::default()
		};
		assert!(application.validate("Game").unwrap_err().contains("Game"));
		assert!(FrameLimitOverride::default().is_empty());
		assert!(toml::from_str::<FrameLimitConfig>("mode = \"fast\"").is_err());
	}

	#[test]
	fn support_summary_reports_unsupported_combinations() {
		let off = FrameLimitConfig::default();
		assert!(support_summary(&off, false).0);
		let mangohud = global(FrameLimitMode::Stream, 60, FrameLimitMethod::Mangohud);
		let (ok, message) = support_summary(&mangohud, false);
		assert!(!ok && message.contains("fall back"));
		assert!(support_summary(&mangohud, true).0);
	}
}
