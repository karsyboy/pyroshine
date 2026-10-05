//! EIS server for input that X11 clients synthesize with XTest.
//!
//! A rootless XWayland built with libei forwards XTest requests to the EIS
//! server named by `LIBEI_SOCKET` instead of moving its own pointer sprite.
//! Without a server it falls back to server-side XTest: the X pointer moves
//! while the compositor's pointer, cursor position, pointer focus and
//! constraints stay where they were, and the next compositor pointer event
//! snaps XWayland back. Steam Input emits its controller-as-mouse motion,
//! clicks and keys this way, so the cursor of a controller-driven game would
//! otherwise follow two disagreeing positions.
//!
//! The socket is offered to XWayland only. Its events are injected through the
//! same seat paths as Moonlight input ([`super::input::process_emulated_input`]).
//! Gamescope runs the equivalent server (`InputEmulation.cpp`) for the same
//! purpose.

use std::path::PathBuf;

use smithay::backend::libei::{EiInput, EiInputEvent, EiRegion};
use smithay::reexports::calloop::{LoopHandle, PostAction, RegistrationToken};
use smithay::reexports::reis::calloop::EisListenerSource;
use smithay::reexports::reis::eis;
use smithay::utils::Rectangle;

use super::KeyboardConfig;
use super::state::MoonshineCompositor;

/// The listening EIS socket; dropping its source unlinks the socket.
pub(super) struct EmulatedInputServer {
	pub token: RegistrationToken,
	pub socket: PathBuf,
}

/// Listen on `$XDG_RUNTIME_DIR/<wayland_display>-ei`, next to the session's
/// Wayland socket whose name the compositor already holds a lock for.
///
/// Returns `None` (logged) when the socket cannot be created; XWayland then
/// keeps its server-side XTest fallback.
pub(super) fn start(
	handle: &LoopHandle<'static, MoonshineCompositor>,
	wayland_display: &str,
	keyboard: &KeyboardConfig,
) -> Option<EmulatedInputServer> {
	let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
		tracing::warn!("XDG_RUNTIME_DIR is unset; XTest input emulation is unavailable");
		return None;
	};
	let socket = PathBuf::from(runtime_dir).join(format!("{wayland_display}-ei"));
	// A previous session that crashed can leave the path behind; the Wayland
	// display name it derives from is locked by this compositor.
	if socket.exists()
		&& let Err(error) = std::fs::remove_file(&socket)
	{
		tracing::warn!(%error, socket = %socket.display(), "Failed to remove stale EIS socket");
	}
	let listener = match eis::Listener::bind(&socket) {
		Ok(listener) => listener,
		Err(error) => {
			tracing::warn!(%error, socket = %socket.display(), "Failed to create EIS socket; XTest input emulation is unavailable");
			return None;
		},
	};

	let keyboard = keyboard.clone();
	let connections = handle.clone();
	let token = handle
		.insert_source(EisListenerSource::new(listener), move |context, _, _| {
			let keyboard = keyboard.clone();
			if let Err(error) = connections.insert_source(
				EiInput::new(context),
				move |event, connection, state: &mut MoonshineCompositor| {
					match event {
						EiInputEvent::Connected => {
							let seat = connection.add_seat("moonshine");
							if let Err(error) = seat.add_keyboard("Moonshine emulated keyboard", keyboard.xkb_config())
							{
								// XWayland waits for every bound capability before
								// it emulates anything, so this disables XTest.
								tracing::warn!(?error, "Failed to add the EIS keyboard; XTest input stays queued");
							}
							seat.add_pointer("Moonshine emulated pointer");
							// One region spanning all X11 root coordinates, like
							// gamescope: the compositor clamps to its own scene.
							seat.add_pointer_absolute(
								"Moonshine emulated absolute pointer",
								&[EiRegion {
									rect: Rectangle::new((0, 0).into(), (i32::MAX, i32::MAX).into()),
									scale: 1.0,
									mapping_id: None,
								}],
							);
							if let Err(error) = connection.flush() {
								tracing::debug!(%error, "Failed to flush EIS seat");
							}
							tracing::debug!("EIS client connected for XTest input emulation");
						},
						EiInputEvent::Disconnected => tracing::debug!("EIS client disconnected"),
						EiInputEvent::Event(event) => super::input::process_emulated_input(event, state),
						// XWayland emulates keycodes, never keysyms or text.
						EiInputEvent::TextKeysym { .. } | EiInputEvent::TextUtf8 { .. } => {},
					}
				},
			) {
				tracing::warn!(%error, "Failed to register EIS client");
			}
			Ok(PostAction::Continue)
		})
		.inspect_err(|error| tracing::warn!(%error, "Failed to register the EIS socket"))
		.ok()?;
	tracing::debug!(socket = %socket.display(), "Listening for XTest input emulation");
	Some(EmulatedInputServer { token, socket })
}
