//! Steam's on-screen keyboard through gamescope's private input-method protocol.
//!
//! Steam binds `gamescope_input_method_manager` on `GAMESCOPE_WAYLAND_DISPLAY`
//! and commits each typed character as a string. Without the global it falls
//! back to XTest and remaps a keycode (`XChangeKeyboardMapping`) for every
//! character. Each remap broadcasts `MappingNotify` to every X client, and Wine
//! rebuilds its keyboard tables on it, so the game hitched on every key.
//!
//! A character is typed with the key (and Shift, when needed) that produces it
//! in the seat's own keymap, so no keymap changes. Only a character the layout
//! lacks goes through a temporary keymap, as gamescope's `type_text` does.
//! Version 3's pointer requests are not implemented: the global is advertised
//! at version 2, so Steam Input keeps its existing pointer path.

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{FilterResult, KeyboardHandle, Keycode, xkb};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use smithay::utils::SERIAL_COUNTER;
use std::sync::Mutex;

use super::state::MoonshineCompositor;

#[allow(dead_code, unused_imports, non_upper_case_globals, clippy::all)]
mod protocol {
	use smithay::reexports::wayland_server;
	use smithay::reexports::wayland_server::protocol::*;
	pub mod __interfaces {
		use smithay::reexports::wayland_server::backend as wayland_backend;
		use smithay::reexports::wayland_server::protocol::__interfaces::*;
		wayland_scanner::generate_interfaces!("protocols/gamescope-input-method.xml");
	}
	use self::__interfaces::*;
	wayland_scanner::generate_server_code!("protocols/gamescope-input-method.xml");
}

use protocol::gamescope_input_method::{self, Action, GamescopeInputMethod};
use protocol::gamescope_input_method_manager::{self, GamescopeInputMethodManager};

/// Highest interface version served: 3 adds pointer requests (see module docs).
const VERSION: u32 = 2;

// xkb keycodes (evdev + 8) of the keys actions and Shift use.
const KEY_LEFTSHIFT: u32 = 42 + 8;
const KEY_ENTER: u32 = 28 + 8;
const KEY_BACKSPACE: u32 = 14 + 8;
const KEY_DELETE: u32 = 111 + 8;
const KEY_LEFT: u32 = 105 + 8;
const KEY_RIGHT: u32 = 106 + 8;
const KEY_UP: u32 = 103 + 8;
const KEY_DOWN: u32 = 108 + 8;

/// The seat's one input method; a second one is told it is unavailable.
#[derive(Default)]
pub(crate) struct InputMethodState {
	active: Option<GamescopeInputMethod>,
}

impl InputMethodState {
	pub fn new(display: &DisplayHandle) -> Self {
		display.create_global::<MoonshineCompositor, GamescopeInputMethodManager, _>(VERSION, ());
		Self::default()
	}
}

/// Double-buffered state of one input method, applied on `commit`.
#[derive(Default)]
pub(crate) struct InputMethodData {
	pending: Mutex<Pending>,
}

#[derive(Default)]
struct Pending {
	text: Option<String>,
	action: Option<Action>,
}

/// The protocol's `done` serial. The compositor never changes the text-input
/// state, so it stays at its first value and every commit must carry it.
const SERIAL: u32 = 1;

impl GlobalDispatch<GamescopeInputMethodManager, ()> for MoonshineCompositor {
	fn bind(
		_: &mut Self,
		_: &DisplayHandle,
		_: &Client,
		resource: New<GamescopeInputMethodManager>,
		_: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		data_init.init(resource, ());
	}
}

impl Dispatch<GamescopeInputMethodManager, ()> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_: &Client,
		_: &GamescopeInputMethodManager,
		request: gamescope_input_method_manager::Request,
		_: &(),
		_: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			gamescope_input_method_manager::Request::CreateInputMethod { input_method, .. } => {
				let input_method = data_init.init(input_method, InputMethodData::default());
				if state.input_method.active.as_ref().is_some_and(Resource::is_alive) {
					input_method.unavailable();
					return;
				}
				input_method.done(SERIAL);
				tracing::debug!(target: "input", "Steam input method bound");
				state.input_method.active = Some(input_method);
			},
			gamescope_input_method_manager::Request::Destroy => {},
		}
	}
}

impl Dispatch<GamescopeInputMethod, InputMethodData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_: &Client,
		resource: &GamescopeInputMethod,
		request: gamescope_input_method::Request,
		data: &InputMethodData,
		_: &DisplayHandle,
		_: &mut DataInit<'_, Self>,
	) {
		let mut pending = data.pending.lock().unwrap_or_else(|e| e.into_inner());
		match request {
			gamescope_input_method::Request::SetString { text } => pending.text = Some(text),
			gamescope_input_method::Request::SetAction { action } => pending.action = action.into_result().ok(),
			gamescope_input_method::Request::Commit { serial } => {
				let Pending { text, action } = std::mem::take(&mut *pending);
				drop(pending);
				// Inert: a second input method, or a commit for another state.
				if serial != SERIAL || state.input_method.active.as_ref() != Some(resource) {
					return;
				}
				let Some(keyboard) = state.seat.get_keyboard() else {
					return;
				};
				if let Some(text) = text {
					type_text(state, &keyboard, &text);
				}
				if let Some(keycode) = action.and_then(action_keycode) {
					tap(state, &keyboard, Keycode::new(keycode));
				}
			},
			// Pointer requests (version 3) are never sent at version 2.
			_ => {},
		}
	}

	fn destroyed(
		state: &mut Self,
		_: smithay::reexports::wayland_server::backend::ClientId,
		resource: &GamescopeInputMethod,
		_: &InputMethodData,
	) {
		if state.input_method.active.as_ref() == Some(resource) {
			state.input_method.active = None;
		}
	}
}

fn action_keycode(action: Action) -> Option<u32> {
	match action {
		Action::Submit => Some(KEY_ENTER),
		Action::DeleteLeft => Some(KEY_BACKSPACE),
		Action::DeleteRight => Some(KEY_DELETE),
		Action::MoveLeft => Some(KEY_LEFT),
		Action::MoveRight => Some(KEY_RIGHT),
		Action::MoveUp => Some(KEY_UP),
		Action::MoveDown => Some(KEY_DOWN),
		_ => None,
	}
}

/// Keysym for a typed character. libxkbcommon maps U+20AC to the Unicode
/// keysym rather than `EuroSign`, which layouts bind (gamescope does the same).
fn keysym_for(ch: char) -> xkb::Keysym {
	if ch == '\u{20ac}' {
		return xkb::Keysym::EuroSign;
	}
	xkb::utf32_to_keysym(ch as u32)
}

fn type_text(state: &mut MoonshineCompositor, keyboard: &KeyboardHandle<MoonshineCompositor>, text: &str) {
	for ch in text.chars() {
		let keysym = keysym_for(ch);
		if keysym == xkb::Keysym::NoSymbol {
			tracing::debug!(target: "input", codepoint = ch as u32, "Cannot type character from the input method");
			continue;
		}
		match key_in_seat_keymap(state, keyboard, keysym) {
			Some((keycode, shift)) => {
				if shift {
					press(state, keyboard, Keycode::new(KEY_LEFTSHIFT), KeyState::Pressed);
				}
				tap(state, keyboard, keycode);
				if shift {
					press(state, keyboard, Keycode::new(KEY_LEFTSHIFT), KeyState::Released);
				}
			},
			// Rare: the layout lacks it. A temporary keymap types it; an X
			// client may read the key after the seat keymap is restored.
			None => keyboard.inject_text_keysyms(state, &[keysym]),
		}
	}
}

/// The key that produces `keysym` in the seat's active layout with no
/// modifier or with Shift alone, preferring the unmodified level.
fn key_in_seat_keymap(
	state: &mut MoonshineCompositor,
	keyboard: &KeyboardHandle<MoonshineCompositor>,
	keysym: xkb::Keysym,
) -> Option<(Keycode, bool)> {
	keyboard.with_xkb_state(state, |context| {
		let xkb = context.xkb().lock().unwrap_or_else(|e| e.into_inner());
		// SAFETY: the keymap and state references are used only inside this
		// callback and no reference count escapes the `Xkb` they borrow from.
		let (keymap, layout) = unsafe { (xkb.keymap(), xkb.state().serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE)) };
		let shift = 1 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
		let mut shifted = None;
		for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
			let keycode = Keycode::new(raw);
			for level in 0..keymap.num_levels_for_key(keycode, layout) {
				if keymap.key_get_syms_by_level(keycode, layout, level) != [keysym] {
					continue;
				}
				let mut masks = [0; 4];
				let count = keymap.key_get_mods_for_level(keycode, layout, level, &mut masks);
				let masks = &masks[..count.min(masks.len())];
				if masks.contains(&0) {
					return Some((keycode, false));
				}
				if shifted.is_none() && masks.contains(&shift) {
					shifted = Some((keycode, true));
				}
			}
		}
		shifted
	})
}

fn tap(state: &mut MoonshineCompositor, keyboard: &KeyboardHandle<MoonshineCompositor>, keycode: Keycode) {
	press(state, keyboard, keycode, KeyState::Pressed);
	press(state, keyboard, keycode, KeyState::Released);
}

fn press(
	state: &mut MoonshineCompositor,
	keyboard: &KeyboardHandle<MoonshineCompositor>,
	keycode: Keycode,
	key_state: KeyState,
) {
	keyboard.input::<(), _>(
		state,
		keycode,
		key_state,
		SERIAL_COUNTER.next_serial(),
		smithay::backend::input::InputTime::now(),
		|_, _, _| FilterResult::Forward,
	);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn actions_map_to_their_editing_keys() {
		assert_eq!(action_keycode(Action::Submit), Some(KEY_ENTER));
		assert_eq!(action_keycode(Action::DeleteLeft), Some(KEY_BACKSPACE));
		assert_eq!(action_keycode(Action::MoveDown), Some(KEY_DOWN));
		assert_eq!(action_keycode(Action::None), None);
	}

	#[test]
	fn euro_uses_the_layout_keysym() {
		assert_eq!(keysym_for('€'), xkb::Keysym::EuroSign);
		assert_eq!(keysym_for('a'), xkb::Keysym::a);
	}
}
