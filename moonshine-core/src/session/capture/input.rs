//! Keyboard, pointer, touch and pen input for the captured scene.
//!
//! The control stream decodes Moonlight input into [`CaptureInputEvent`]s and
//! sends them over a calloop channel to the backend serving the session.
//! Controllers are not part of this: they are Inputtino devices owned by the
//! control stream for every backend.

/// Input events the control stream sends to the capture backend.
///
/// Transport-level events crossing the tokio→calloop boundary: input data
/// and completion acknowledgments, `Send` and free of backend objects. The
/// isolated compositor injects them into its seat; Desktop Mode forwards them
/// to the desktop through the portal's input devices.
#[derive(Debug)]
pub(crate) enum CaptureInputEvent {
	Reset {
		keys: Vec<u32>,
		buttons: Vec<u32>,
		ready: tokio::sync::oneshot::Sender<()>,
	},
	/// A key was pressed. `keycode` is a Linux evdev keycode.
	KeyDown {
		keycode: u32,
	},
	/// A key was released. `keycode` is a Linux evdev keycode.
	KeyUp {
		keycode: u32,
	},
	/// Absolute pointer motion. Coordinates are in pixels relative to the
	/// Moonlight client's screen dimensions.
	MouseMoveAbsolute {
		x: i16,
		y: i16,
		screen_width: i16,
		screen_height: i16,
	},
	/// Relative pointer motion in pixels.
	MouseMoveRelative {
		dx: i16,
		dy: i16,
	},
	/// A mouse button was pressed. `button` is a Linux button code (e.g. BTN_LEFT = 0x110).
	MouseButtonDown {
		button: u32,
	},
	/// A mouse button was released.
	MouseButtonUp {
		button: u32,
	},
	/// Vertical scroll. Positive = scroll up, negative = scroll down.
	ScrollVertical {
		amount: i16,
	},
	/// Horizontal scroll.
	ScrollHorizontal {
		amount: i16,
	},
	/// Native touchscreen contact in normalized video coordinates.
	TouchDown {
		slot: u32,
		x: f32,
		y: f32,
	},
	TouchMove {
		slot: u32,
		x: f32,
		y: f32,
	},
	TouchUp {
		slot: u32,
	},
	TouchCancelAll,
	/// Native pen input in normalized video coordinates.
	Pen {
		event_kind: u8,
		tool_kind: u8,
		buttons: u8,
		x: f32,
		y: f32,
		pressure_or_distance: f32,
		rotation: u16,
		tilt: u8,
	},
	/// Text received from the Moonlight clipboard ("type clipboard text").
	/// Committed to the focused client via the text-input protocol.
	TypeText {
		text: String,
	},
}
