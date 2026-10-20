//! XDG Desktop Portal client: a RemoteDesktop session with a ScreenCast
//! source, or a view-only ScreenCast session where remote input is absent.
//!
//! Every step is a portal request: the reply names a `Request` object whose
//! `Response` signal carries the result. The signal is subscribed before the
//! call, on the path the portal derives from the caller's unique name and the
//! handle token, so a fast response cannot be missed.
//!
//! Consent belongs to the portal: `Start` may show the desktop's dialog. A
//! restore token from an earlier session (`persist_mode = 2`) lets the portal
//! skip it; KDE can also grant it permanently in its permission store. Nothing
//! here bypasses either: a session the user did not authorize fails.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REGISTRY: &str = "org.freedesktop.host.portal.Registry";

/// ScreenCast source type: a monitor.
pub(crate) const SOURCE_MONITOR: u32 = 1;
/// RemoteDesktop device types.
pub(crate) const DEVICE_KEYBOARD: u32 = 1;
pub(crate) const DEVICE_POINTER: u32 = 2;
pub(crate) const DEVICE_TOUCHSCREEN: u32 = 4;

/// ScreenCast cursor modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorMode {
	/// Drawn into the frames by the compositor.
	Embedded = 2,
}

/// Remember the consent until the user revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;

static TOKEN: AtomicU32 = AtomicU32::new(0);

fn handle_token() -> String {
	format!(
		"pyroshine{}_{}",
		std::process::id(),
		TOKEN.fetch_add(1, Ordering::Relaxed)
	)
}

/// What a portal step failed with, for the operator and the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PortalError {
	/// The desktop has no ScreenCast portal (no portal backend installed, or
	/// none for this desktop).
	Unavailable(String),
	/// The user declined the request, or closed the dialog.
	Denied,
	/// The portal reported a failure, or replied with something unusable.
	Failed(String),
	/// No answer before the deadline (an unattended consent dialog).
	TimedOut,
}

impl std::fmt::Display for PortalError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unavailable(reason) => write!(f, "the screen-capture portal is not available: {reason}"),
			Self::Denied => write!(f, "screen capture was not allowed on the desktop"),
			Self::Failed(reason) => write!(f, "the screen-capture portal failed: {reason}"),
			Self::TimedOut => write!(f, "nobody answered the desktop's screen-sharing request in time"),
		}
	}
}

fn failed(context: &str, error: impl std::fmt::Display) -> PortalError {
	PortalError::Failed(format!("{context}: {error}"))
}

/// Interface versions the portal frontend implements (0 when absent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PortalVersions {
	pub screen_cast: u32,
	pub remote_desktop: u32,
}

impl PortalVersions {
	/// `ConnectToEIS` (libei input) needs RemoteDesktop version 2.
	pub(crate) fn eis(&self) -> bool {
		self.remote_desktop >= 2
	}
	/// Restore tokens on a RemoteDesktop session need version 2; on a
	/// ScreenCast session, ScreenCast version 4.
	pub(crate) fn restore(&self, remote_desktop: bool) -> bool {
		if remote_desktop {
			self.remote_desktop >= 2
		} else {
			self.screen_cast >= 4
		}
	}
}

/// One captured source of a started session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortalStream {
	/// PipeWire node of the stream.
	pub node_id: u32,
	/// Position in the compositor's logical coordinate space, when reported.
	pub position: Option<(i32, i32)>,
	/// Logical size, when reported.
	pub size: Option<(i32, i32)>,
	pub source_type: u32,
	/// Matches the libei region of the same output (RemoteDesktop 2).
	pub mapping_id: Option<String>,
}

/// What Desktop Mode asks the portal for.
#[derive(Clone, Debug)]
pub(crate) struct SessionRequest {
	pub cursor: CursorMode,
	/// Ask for keyboard and pointer control (a RemoteDesktop session).
	pub input: bool,
	/// Restore token of an earlier session.
	pub restore_token: Option<String>,
}

/// A started portal session.
pub(crate) struct PortalSession {
	connection: zbus::Connection,
	handle: OwnedObjectPath,
	pub versions: PortalVersions,
	/// A RemoteDesktop session (input can be authorized), not ScreenCast only.
	pub remote_desktop: bool,
	/// Device types the user allowed (`DEVICE_*`); zero without input.
	pub devices: u32,
	pub streams: Vec<PortalStream>,
	/// Token to restore this authorization next time, when the portal gave one.
	pub restore_token: Option<String>,
}

async fn portal_proxy<'a>(connection: &zbus::Connection, interface: &'a str) -> Result<zbus::Proxy<'a>, PortalError> {
	zbus::Proxy::new(connection, PORTAL_SERVICE, PORTAL_PATH, interface)
		.await
		.map_err(|e| failed(interface, e))
}

async fn version(connection: &zbus::Connection, interface: &str) -> u32 {
	let Ok(proxy) = zbus::Proxy::new(connection, PORTAL_SERVICE, PORTAL_PATH, interface.to_string()).await else {
		return 0;
	};
	proxy.get_property::<u32>("version").await.unwrap_or(0)
}

/// Which portal interfaces exist, without starting anything.
pub(crate) async fn versions(connection: &zbus::Connection) -> PortalVersions {
	PortalVersions {
		screen_cast: version(connection, SCREEN_CAST).await,
		remote_desktop: version(connection, REMOTE_DESKTOP).await,
	}
}

/// Associate this connection with the `app_id` desktop entry, so the portal
/// names Pyroshine in its dialog and keys stored permissions by it. Optional:
/// older portals lack the registry, and a missing desktop file is refused.
pub(crate) async fn register(connection: &zbus::Connection, app_id: &str) {
	let result = connection
		.call_method(
			Some(PORTAL_SERVICE),
			PORTAL_PATH,
			Some(REGISTRY),
			"Register",
			&(app_id, HashMap::<&str, Value<'_>>::new()),
		)
		.await;
	match result {
		Ok(_) => tracing::debug!(app_id, "Registered with the portal"),
		Err(error) => {
			tracing::debug!(app_id, %error, "Portal registration unavailable; the portal identifies the service by its process")
		},
	}
}

/// Wait for the `Response` of the request created by `call`.
async fn request(
	connection: &zbus::Connection,
	call: impl AsyncFnOnce(&str) -> zbus::Result<OwnedObjectPath>,
) -> Result<HashMap<String, OwnedValue>, PortalError> {
	let token = handle_token();
	let sender = connection
		.unique_name()
		.ok_or_else(|| PortalError::Failed("the session bus connection has no name".into()))?
		.trim_start_matches(':')
		.replace('.', "_");
	let path = format!("{PORTAL_PATH}/request/{sender}/{token}");
	let request = zbus::Proxy::new(
		connection,
		PORTAL_SERVICE,
		path.clone(),
		"org.freedesktop.portal.Request",
	)
	.await
	.map_err(|e| failed("cannot watch the portal request", e))?;
	let mut responses = request
		.receive_signal("Response")
		.await
		.map_err(|e| failed("cannot watch the portal request", e))?;
	let returned = call(&token).await.map_err(|error| match &error {
		zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" => {
			PortalError::Unavailable(error.to_string())
		},
		_ => failed("portal request", error),
	})?;
	if returned.as_str() != path {
		// Portals older than 0.9 chose the path themselves.
		return Err(PortalError::Failed(format!(
			"the portal answered on {} instead of {path}",
			returned.as_str()
		)));
	}
	let message = responses
		.next()
		.await
		.ok_or_else(|| PortalError::Failed("the portal closed the request".into()))?;
	let (code, results): (u32, HashMap<String, OwnedValue>) = message
		.body()
		.deserialize()
		.map_err(|e| failed("unreadable portal response", e))?;
	match code {
		0 => Ok(results),
		1 => Err(PortalError::Denied),
		_ => Err(PortalError::Failed("the portal could not complete the request".into())),
	}
}

fn value<T: TryFrom<OwnedValue>>(results: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
	results
		.get(key)
		.and_then(|value| value.try_clone().ok())
		.and_then(|value| T::try_from(value).ok())
}

fn parse_streams(results: &HashMap<String, OwnedValue>) -> Result<Vec<PortalStream>, PortalError> {
	let streams = results
		.get("streams")
		.ok_or_else(|| PortalError::Failed("the portal started no stream".into()))?;
	let streams: Vec<(u32, HashMap<String, OwnedValue>)> = streams
		.try_clone()
		.map_err(|e| failed("unreadable stream list", e))?
		.try_into()
		.map_err(|e| failed("unreadable stream list", e))?;
	Ok(streams
		.into_iter()
		.map(|(node_id, properties)| PortalStream {
			node_id,
			position: value::<(i32, i32)>(&properties, "position"),
			size: value::<(i32, i32)>(&properties, "size"),
			source_type: value::<u32>(&properties, "source_type").unwrap_or(SOURCE_MONITOR),
			mapping_id: value::<String>(&properties, "mapping_id"),
		})
		.collect())
}

impl PortalSession {
	/// Create, configure and start a session, waiting for the user's consent
	/// where the portal asks for it. The caller bounds the wait.
	pub(crate) async fn start(connection: &zbus::Connection, wanted: SessionRequest) -> Result<Self, PortalError> {
		let versions = versions(connection).await;
		if versions.screen_cast == 0 {
			return Err(PortalError::Unavailable(
				"the desktop's portal implements no ScreenCast interface".into(),
			));
		}
		let remote_desktop = wanted.input && versions.remote_desktop > 0;
		if wanted.input && !remote_desktop {
			tracing::warn!("The portal has no RemoteDesktop interface: Desktop Mode is view-only");
		}
		let screen_cast = portal_proxy(connection, SCREEN_CAST).await?;
		let remote = portal_proxy(connection, REMOTE_DESKTOP).await?;
		let session_interface = if remote_desktop { &remote } else { &screen_cast };

		// CreateSession: the session handle comes back in the response.
		let session_token = handle_token();
		let created = request(connection, async |token| {
			let options = HashMap::from([
				("handle_token", Value::from(token)),
				("session_handle_token", Value::from(session_token.as_str())),
			]);
			session_interface.call("CreateSession", &(options,)).await
		})
		.await?;
		let handle: String = value::<String>(&created, "session_handle")
			.ok_or_else(|| PortalError::Failed("CreateSession returned no session".into()))?;
		let handle = OwnedObjectPath::try_from(handle).map_err(|e| failed("invalid session handle", e))?;
		let mut session = Self {
			connection: connection.clone(),
			handle,
			versions,
			remote_desktop,
			devices: 0,
			streams: Vec::new(),
			restore_token: None,
		};
		// From here on a failure closes the session (Drop).
		let restore = versions.restore(remote_desktop);
		let restore_token = wanted.restore_token.filter(|_| restore);
		let handle = session.handle.clone();
		if remote_desktop {
			request(connection, async |token| {
				let mut options = HashMap::from([
					("handle_token", Value::from(token)),
					(
						"types",
						Value::from(DEVICE_KEYBOARD | DEVICE_POINTER | DEVICE_TOUCHSCREEN),
					),
				]);
				if restore {
					options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
					if let Some(token) = &restore_token {
						options.insert("restore_token", Value::from(token.as_str()));
					}
				}
				remote.call("SelectDevices", &(&handle, options)).await
			})
			.await?;
		}
		request(connection, async |token| {
			let mut options = HashMap::from([
				("handle_token", Value::from(token)),
				("types", Value::from(SOURCE_MONITOR)),
				("multiple", Value::from(false)),
				("cursor_mode", Value::from(wanted.cursor as u32)),
			]);
			// A RemoteDesktop session persists through SelectDevices.
			if restore && !remote_desktop {
				options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
				if let Some(token) = &restore_token {
					options.insert("restore_token", Value::from(token.as_str()));
				}
			}
			screen_cast.call("SelectSources", &(&handle, options)).await
		})
		.await?;
		let started = request(connection, async |token| {
			let options = HashMap::from([("handle_token", Value::from(token))]);
			session_interface.call("Start", &(&handle, "", options)).await
		})
		.await?;
		session.streams = parse_streams(&started)?;
		session.devices = value::<u32>(&started, "devices").unwrap_or(0);
		session.restore_token = value::<String>(&started, "restore_token");
		if session.streams.is_empty() {
			return Err(PortalError::Failed("the portal started no stream".into()));
		}
		Ok(session)
	}

	pub(crate) fn handle(&self) -> ObjectPath<'_> {
		self.handle.as_ref()
	}

	pub(crate) fn connection(&self) -> &zbus::Connection {
		&self.connection
	}

	/// A PipeWire connection that sees only this session's streams.
	pub(crate) async fn open_pipewire_remote(&self) -> Result<OwnedFd, PortalError> {
		let proxy = portal_proxy(&self.connection, SCREEN_CAST).await?;
		let fd: zbus::zvariant::OwnedFd = proxy
			.call(
				"OpenPipeWireRemote",
				&(self.handle(), HashMap::<&str, Value<'_>>::new()),
			)
			.await
			.map_err(|e| failed("OpenPipeWireRemote", e))?;
		Ok(fd.into())
	}

	/// The libei socket for this session's input devices (RemoteDesktop 2).
	pub(crate) async fn connect_to_eis(&self) -> Result<OwnedFd, PortalError> {
		let proxy = portal_proxy(&self.connection, REMOTE_DESKTOP).await?;
		let fd: zbus::zvariant::OwnedFd = proxy
			.call("ConnectToEIS", &(self.handle(), HashMap::<&str, Value<'_>>::new()))
			.await
			.map_err(|e| failed("ConnectToEIS", e))?;
		Ok(fd.into())
	}

	/// Close the session; the portal ends its streams and input devices.
	pub(crate) async fn close(&self) {
		let result = self
			.connection
			.call_method(
				Some(PORTAL_SERVICE),
				self.handle(),
				Some("org.freedesktop.portal.Session"),
				"Close",
				&(),
			)
			.await;
		if let Err(error) = result {
			tracing::debug!(%error, "Closing the portal session failed; it ends with the connection");
		}
	}
}

/// Wait until the portal closes `session` (the user stopped sharing, the
/// desktop ended, or the portal restarted).
pub(crate) async fn closed(connection: &zbus::Connection, session: ObjectPath<'_>) -> zbus::Result<()> {
	let proxy = zbus::Proxy::new(connection, PORTAL_SERVICE, session, "org.freedesktop.portal.Session").await?;
	let mut closed = proxy.receive_signal("Closed").await?;
	let dbus = zbus::fdo::DBusProxy::new(connection).await?;
	let mut owner = dbus
		.receive_name_owner_changed_with_args(&[(0, PORTAL_SERVICE)])
		.await?;
	tokio::select! {
		_ = closed.next() => {},
		_ = owner.next() => tracing::warn!("The desktop portal restarted"),
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn eis_and_restore_tokens_follow_interface_versions() {
		let ubuntu_jammy = PortalVersions {
			screen_cast: 4,
			remote_desktop: 1,
		};
		assert!(!ubuntu_jammy.eis());
		assert!(!ubuntu_jammy.restore(true));
		assert!(ubuntu_jammy.restore(false));
		let current = PortalVersions {
			screen_cast: 5,
			remote_desktop: 2,
		};
		assert!(current.eis());
		assert!(current.restore(true));
	}

	#[test]
	fn handle_tokens_are_unique_object_path_elements() {
		let (a, b) = (handle_token(), handle_token());
		assert_ne!(a, b);
		assert!(a.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'));
	}
}
