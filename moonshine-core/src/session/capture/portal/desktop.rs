//! The user's graphical session that Desktop Mode streams.
//!
//! Pyroshine runs as a service of the desktop user, outside the graphical
//! session. The session's variables come from the user's systemd manager (the
//! desktop imports them at login), like the tray's; the service's own
//! environment is the fallback for a server started from a terminal.
//!
//! Everything desktop-specific stays here as a [`DesktopKind`] quirk: the
//! portal protocol is the same on every desktop, but consent handling and
//! known limitations differ.

use std::path::{Path, PathBuf};

/// Session variables Desktop Mode reads from the user's systemd manager.
const SESSION_VARIABLES: &[&str] = &[
	"WAYLAND_DISPLAY",
	"DISPLAY",
	"XDG_CURRENT_DESKTOP",
	"XDG_SESSION_TYPE",
	"XDG_SESSION_ID",
];

/// Desktop families. Native output management currently supports KDE and GNOME;
/// input authorization is handled separately through the portal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopKind {
	/// KDE Plasma (`xdg-desktop-portal-kde`, KWin). Consent can be granted
	/// permanently in the portal permission store (`kde-authorized`).
	Kde,
	/// GNOME and Ubuntu's GNOME session (`xdg-desktop-portal-gnome`, Mutter).
	/// Consent is remembered only through restore tokens.
	Gnome,
	/// Another portal implementation.
	Other,
}

impl DesktopKind {
	/// Classify `XDG_CURRENT_DESKTOP`, a colon-separated list such as `KDE`,
	/// `GNOME`, `ubuntu:GNOME` or `GNOME-Classic:GNOME`.
	pub(crate) fn from_current_desktop(value: &str) -> Self {
		let mut names = value.split(':').map(str::trim).filter(|name| !name.is_empty());
		if names.clone().any(|name| name.eq_ignore_ascii_case("KDE")) {
			Self::Kde
		} else if names.any(|name| name.eq_ignore_ascii_case("GNOME") || name.eq_ignore_ascii_case("ubuntu")) {
			Self::Gnome
		} else {
			Self::Other
		}
	}

	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Kde => "kde",
			Self::Gnome => "gnome",
			Self::Other => "other",
		}
	}
}

/// A running Wayland desktop session of the service's user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopSession {
	pub kind: DesktopKind,
	/// `XDG_CURRENT_DESKTOP` as the session set it.
	pub current_desktop: String,
	/// Wayland socket of the desktop compositor (absolute path).
	pub wayland_socket: PathBuf,
	/// Variables the desktop exported, for an application launched into it.
	pub variables: Vec<(String, String)>,
	/// logind session ID, when the desktop exported one.
	pub session_id: Option<String>,
}

/// Why no desktop session can be streamed. The text is for the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DesktopUnavailable {
	/// No graphical session of this user is running.
	NoSession,
	/// The session is an X11 session; Desktop Mode needs Wayland.
	NotWayland(String),
	Ambiguous(String),
}

impl std::fmt::Display for DesktopUnavailable {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Ambiguous(reason) => f.write_str(reason),
			Self::NoSession => write!(f, "no Wayland desktop session of this user is running"),
			Self::NotWayland(kind) => write!(
				f,
				"the desktop session is a {kind} session; Desktop Mode streams Wayland sessions only"
			),
		}
	}
}

#[zbus::proxy(
	interface = "org.freedesktop.systemd1.Manager",
	default_service = "org.freedesktop.systemd1",
	default_path = "/org/freedesktop/systemd1"
)]
trait UserManager {
	#[zbus(property)]
	fn environment(&self) -> zbus::Result<Vec<String>>;
}

/// The session variables from `entries` (`NAME=value`); empty values are unset.
fn session_variables<'a>(entries: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
	let mut variables: Vec<(String, String)> = entries
		.into_iter()
		.filter_map(|entry| entry.split_once('='))
		.filter(|(name, value)| SESSION_VARIABLES.contains(name) && !value.is_empty())
		.map(|(name, value)| (name.to_string(), value.to_string()))
		.collect();
	variables.sort();
	variables.dedup_by(|a, b| a.0 == b.0);
	variables
}

fn variable<'a>(variables: &'a [(String, String)], name: &str) -> Option<&'a str> {
	variables
		.iter()
		.find(|(candidate, _)| candidate == name)
		.map(|(_, value)| value.as_str())
}

/// Resolve the desktop session from its exported variables. `runtime_dir` is
/// where relative Wayland socket names live; `connectable` checks a socket.
pub(crate) fn resolve(
	variables: Vec<(String, String)>,
	runtime_dir: Option<&Path>,
	connectable: impl Fn(&Path) -> bool,
) -> Result<DesktopSession, DesktopUnavailable> {
	let Some(display) = variable(&variables, "WAYLAND_DISPLAY") else {
		return Err(match variable(&variables, "DISPLAY") {
			Some(_) => DesktopUnavailable::NotWayland("X11".into()),
			None => DesktopUnavailable::NoSession,
		});
	};
	if let Some(kind) = variable(&variables, "XDG_SESSION_TYPE").filter(|kind| *kind != "wayland") {
		return Err(DesktopUnavailable::NotWayland(kind.to_string()));
	}
	let socket = if Path::new(display).is_absolute() {
		PathBuf::from(display)
	} else {
		runtime_dir.ok_or(DesktopUnavailable::NoSession)?.join(display)
	};
	if !connectable(&socket) {
		return Err(DesktopUnavailable::NoSession);
	}
	let current_desktop = variable(&variables, "XDG_CURRENT_DESKTOP")
		.unwrap_or_default()
		.to_string();
	Ok(DesktopSession {
		kind: DesktopKind::from_current_desktop(&current_desktop),
		current_desktop,
		wayland_socket: socket,
		session_id: variable(&variables, "XDG_SESSION_ID").map(Into::into),
		variables,
	})
}

/// Find the user's running Wayland desktop session over `connection` (the
/// user's session bus).
pub(crate) async fn discover(connection: &zbus::Connection) -> Result<DesktopSession, DesktopUnavailable> {
	let from_manager = match UserManagerProxy::builder(connection)
		.cache_properties(zbus::proxy::CacheProperties::No)
		.build()
		.await
	{
		Ok(manager) => manager.environment().await.ok(),
		Err(_) => None,
	}
	.map(|entries| session_variables(entries.iter().map(String::as_str)))
	.filter(|variables| variable(variables, "WAYLAND_DISPLAY").is_some() || variable(variables, "DISPLAY").is_some());
	let variables = from_manager.unwrap_or_else(|| {
		let own: Vec<String> = SESSION_VARIABLES
			.iter()
			.filter_map(|name| std::env::var(name).ok().map(|value| format!("{name}={value}")))
			.collect();
		session_variables(own.iter().map(String::as_str))
	});
	let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
	let mut desktop = resolve(variables, runtime_dir.as_deref(), |socket| {
		std::os::unix::net::UnixStream::connect(socket).is_ok()
	})?;
	let sessions = tokio::time::timeout(std::time::Duration::from_secs(3), wayland_logins())
		.await
		.map_err(|_| DesktopUnavailable::Ambiguous("Timed out identifying the graphical login".into()))?
		.map_err(|e| DesktopUnavailable::Ambiguous(format!("Cannot identify the graphical login: {e}")))?;
	desktop.session_id = Some(select_login(&sessions, desktop.session_id.as_deref())?);
	Ok(desktop)
}

fn select_login(sessions: &[String], exported: Option<&str>) -> Result<String, DesktopUnavailable> {
	if sessions.len() != 1 || exported.is_some_and(|id| id != sessions[0]) {
		return Err(DesktopUnavailable::Ambiguous(
			"Desktop Mode requires exactly one Wayland login for this user, matching the exported desktop environment"
				.into(),
		));
	}
	Ok(sessions[0].clone())
}

async fn wayland_logins() -> zbus::Result<Vec<String>> {
	let bus = zbus::Connection::system().await?;
	let manager = zbus::Proxy::new(
		&bus,
		"org.freedesktop.login1",
		"/org/freedesktop/login1",
		"org.freedesktop.login1.Manager",
	)
	.await?;
	let sessions: Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)> =
		manager.call("ListSessions", &()).await?;
	// SAFETY: geteuid has no preconditions.
	let uid = unsafe { libc::geteuid() };
	let mut matching = Vec::new();
	for (id, user, _, _, path) in sessions {
		if user != uid {
			continue;
		}
		let session = zbus::Proxy::new(&bus, "org.freedesktop.login1", path, "org.freedesktop.login1.Session").await?;
		if session.get_property::<String>("Type").await? == "wayland"
			&& session.get_property::<String>("State").await? != "closing"
		{
			if session.get_property::<bool>("LockedHint").await? {
				return Err(zbus::Error::Failure(
					"The host desktop is locked; unlock it before Desktop Mode".into(),
				));
			}
			matching.push(id);
		}
	}
	Ok(matching)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn vars(entries: &[&str]) -> Vec<(String, String)> {
		session_variables(entries.iter().copied())
	}

	#[test]
	fn graphical_login_must_be_unambiguous_and_match_exported_environment() {
		assert_eq!(select_login(&["4".into()], Some("4")).unwrap(), "4");
		assert_eq!(select_login(&["4".into()], None).unwrap(), "4");
		assert!(select_login(&[], None).is_err());
		assert!(select_login(&["4".into(), "5".into()], Some("4")).is_err());
		assert!(select_login(&["4".into()], Some("5")).is_err());
	}

	#[test]
	fn desktop_families_are_recognized_from_current_desktop() {
		assert_eq!(DesktopKind::from_current_desktop("KDE"), DesktopKind::Kde);
		assert_eq!(DesktopKind::from_current_desktop("GNOME"), DesktopKind::Gnome);
		assert_eq!(DesktopKind::from_current_desktop("ubuntu:GNOME"), DesktopKind::Gnome);
		assert_eq!(
			DesktopKind::from_current_desktop("GNOME-Classic:GNOME"),
			DesktopKind::Gnome
		);
		assert_eq!(DesktopKind::from_current_desktop("Hyprland"), DesktopKind::Other);
		assert_eq!(DesktopKind::from_current_desktop(""), DesktopKind::Other);
	}

	#[test]
	fn a_wayland_session_resolves_its_socket_in_the_runtime_dir() {
		let session = resolve(
			vars(&[
				"WAYLAND_DISPLAY=wayland-0",
				"XDG_CURRENT_DESKTOP=ubuntu:GNOME",
				"XDG_SESSION_ID=4",
				"PATH=/bin",
			]),
			Some(Path::new("/run/user/1000")),
			|socket| socket == Path::new("/run/user/1000/wayland-0"),
		)
		.unwrap();
		assert_eq!(session.kind, DesktopKind::Gnome);
		assert_eq!(session.session_id.as_deref(), Some("4"));
		assert!(session.variables.iter().all(|(name, _)| name != "PATH"));
	}

	#[test]
	fn x11_missing_and_stale_sessions_are_refused_with_a_reason() {
		assert_eq!(
			resolve(vars(&["DISPLAY=:0"]), Some(Path::new("/run")), |_| true),
			Err(DesktopUnavailable::NotWayland("X11".into()))
		);
		assert_eq!(
			resolve(
				vars(&["WAYLAND_DISPLAY=wayland-0", "XDG_SESSION_TYPE=x11"]),
				Some(Path::new("/run")),
				|_| true
			),
			Err(DesktopUnavailable::NotWayland("x11".into()))
		);
		assert_eq!(
			resolve(vars(&[]), Some(Path::new("/run")), |_| true),
			Err(DesktopUnavailable::NoSession)
		);
		// A logged-out session leaves its variables in the manager.
		assert_eq!(
			resolve(vars(&["WAYLAND_DISPLAY=wayland-0"]), Some(Path::new("/run")), |_| false),
			Err(DesktopUnavailable::NoSession)
		);
	}
}
