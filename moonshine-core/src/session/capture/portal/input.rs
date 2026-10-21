//! Remote keyboard, pointer and touch input for Desktop Mode.
//!
//! Input reaches the desktop only through devices the user authorized in the
//! portal's RemoteDesktop session:
//!
//! - **libei** (`ConnectToEIS`, RemoteDesktop 2: Plasma 6.1+, GNOME 46+): an
//!   emulated-input connection whose devices the compositor creates for this
//!   session. Events are framed per device, as libei requires.
//!
//! No virtual kernel input device is created for keyboard or pointer: that
//! would bypass the compositor's authorization. Controllers stay Inputtino
//! devices of the control stream, as in Game Mode; they are owned by the
//! service, not authorized by the portal, and need `/dev/uinput` access.
//!
//! Every pressed key and button is tracked and released when the client
//! disconnects (`Reset`), when the session stops, and before the devices go
//! away, so the desktop is never left with a stuck key.

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use enumflags2::BitFlags;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

use crate::session::capture::input::CaptureInputEvent;

/// Linux `BTN_TOUCH`-free key state values shared by both transports.
const RELEASED: u32 = 0;
const PRESSED: u32 = 1;

/// The captured output, for mapping client coordinates to the desktop.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OutputGeometry {
	/// Stream size in pixels (the captured buffer).
	pub width: u32,
	pub height: u32,
	/// Logical position and size the portal reported, if any.
	pub logical_position: Option<(i32, i32)>,
	pub logical_size: Option<(i32, i32)>,
	pub mapping_id: Option<String>,
	/// Managed outputs keep their pixel mode but allow UI scale changes.
	pub follow_scale: bool,
}

/// How the encoders present the captured output in the stream, so client
/// positions can be mapped back onto the output. Updated by the session when
/// a stream is (re)negotiated; read by the input path.
#[derive(Debug, Default)]
pub(crate) struct StreamView {
	/// Stream width and height packed (`width << 32 | height`); 0 before PLAY.
	size: std::sync::atomic::AtomicU64,
	/// The encoder preserves the aspect ratio (letterbox) instead of
	/// stretching the output over the frame.
	fit: std::sync::atomic::AtomicBool,
}

impl StreamView {
	pub(crate) fn set(&self, width: u32, height: u32, fit: bool) {
		use std::sync::atomic::Ordering;
		self.size
			.store(u64::from(width) << 32 | u64::from(height), Ordering::Release);
		self.fit.store(fit, Ordering::Release);
	}

	pub(crate) fn set_fit(&self, fit: bool) {
		self.fit.store(fit, std::sync::atomic::Ordering::Release);
	}

	/// A normalized stream position as a normalized output position.
	fn to_output(&self, source: (u32, u32), x: f64, y: f64) -> (f64, f64) {
		use std::sync::atomic::Ordering;
		let size = self.size.load(Ordering::Acquire);
		let stream = ((size >> 32) as u32, size as u32);
		if !self.fit.load(Ordering::Acquire) || stream.0 == 0 || stream.1 == 0 {
			return (x, y);
		}
		let (vx, vy, vw, vh) = crate::session::capture::frame::fit_viewport(source, stream);
		(
			(x * f64::from(stream.0) - f64::from(vx)) / f64::from(vw),
			(y * f64::from(stream.1) - f64::from(vy)) / f64::from(vh),
		)
	}
}

/// A point on the output in normalized `[0, 1]` coordinates.
fn normalized(x: f64, y: f64) -> (f64, f64) {
	(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

/// A libei region: where an output lies in the device's logical space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Region {
	pub x: f64,
	pub y: f64,
	pub width: f64,
	pub height: f64,
	pub physical_scale: f64,
}

/// The region of `regions` that shows `output`: the one with its mapping ID,
/// else the one at its logical position and size, else the only region.
/// Managed outputs also match their pixel mode through EIS's physical scale,
/// so changing UI scale never changes pointer targeting or stream resolution.
pub(crate) fn output_region(output: &OutputGeometry, regions: &[(Region, Option<String>)]) -> Option<Region> {
	if let Some(id) = &output.mapping_id
		&& let Some((region, _)) = regions.iter().find(|(_, mapping)| mapping.as_ref() == Some(id))
	{
		return Some(*region);
	}
	if let (Some((x, y)), Some((w, h))) = (output.logical_position, output.logical_size) {
		return regions.iter().find_map(|(region, _)| {
			let size_matches = if output.follow_scale {
				region.physical_scale.is_finite()
					&& region.physical_scale > 0.0
					&& region.width == (f64::from(output.width) / region.physical_scale).round()
					&& region.height == (f64::from(output.height) / region.physical_scale).round()
			} else {
				region.width == f64::from(w) && region.height == f64::from(h)
			};
			(region.x == f64::from(x) && region.y == f64::from(y) && size_matches).then_some(*region)
		});
	}
	match regions {
		[(region, _)] => Some(*region),
		_ => None,
	}
}

/// Pressed keys and buttons, to release them when control ends.
#[derive(Debug, Default)]
pub(crate) struct Pressed {
	pub keys: BTreeSet<u32>,
	pub buttons: BTreeSet<u32>,
}

/// How client pointer and touch positions reach the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputTransport {
	/// libei devices from `ConnectToEIS`.
	Eis,
	/// The user allowed no input, or the portal offers none: view-only.
	None,
}

impl InputTransport {
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Eis => "libei",
			Self::None => "none",
		}
	}
}

/// EIS devices can be replaced/paused as output regions change. This is not
/// portal revocation. Bound the unavailable interval without extending it for
/// repeated partial updates; the socket and portal still fail immediately.
#[derive(Default)]
pub(crate) struct Readiness {
	deadline: Option<Instant>,
}
impl Readiness {
	pub(crate) fn expired(&mut self, ready: bool, now: Instant) -> bool {
		if ready {
			self.deadline = None;
			false
		} else {
			now >= *self.deadline.get_or_insert(now + Duration::from_secs(3))
		}
	}
}

#[derive(Clone, Copy, Default)]
struct DeviceReset {
	keys: bool,
	buttons: bool,
	touches: bool,
}
impl DeviceReset {
	fn merge(&mut self, other: Self) {
		self.keys |= other.keys;
		self.buttons |= other.buttons;
		self.touches |= other.touches;
	}
	fn include(&mut self, device: &reis::event::Device) {
		self.keys |= device.has_capability(DeviceCapability::Keyboard);
		self.buttons |= device.has_capability(DeviceCapability::Button);
		self.touches |= device.has_capability(DeviceCapability::Touch);
	}
}

/// A libei device and what it can do.
struct EiDevice {
	device: reis::event::Device,
	// reis clears a removed device's interfaces before delivering DeviceRemoved.
	// Retain its capabilities so the matching held state can still be cleared.
	reset: DeviceReset,
	emulating: bool,
}

/// libei sender connection.
struct Eis {
	context: ei::Context,
	handshake: Option<reis::handshake::EiHandshaker<'static>>,
	converter: Option<reis::event::EiEventConverter>,
	devices: Vec<EiDevice>,
	sequence: u32,
	/// Pointer position, kept for devices that offer only relative motion.
	disconnected: bool,
	reset: DeviceReset,
}

/// The desktop input sink of a Desktop Mode session.
pub(crate) struct DesktopInput {
	transport: InputTransport,
	eis: Option<Eis>,
	output: OutputGeometry,
	pressed: Pressed,
	touches: BTreeSet<u32>,
	warned: BTreeSet<&'static str>,
	view: std::sync::Arc<StreamView>,
	suspended: bool,
}

fn now_micros() -> u64 {
	let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
	// SAFETY: valid out pointer.
	unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
	ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

impl Eis {
	fn new(socket: OwnedFd) -> std::io::Result<Self> {
		let stream = UnixStream::from(socket);
		stream.set_nonblocking(true)?;
		let context = ei::Context::new(stream)?;
		Ok(Self {
			context,
			handshake: Some(reis::handshake::EiHandshaker::new(
				"pyroshine",
				ei::handshake::ContextType::Sender,
			)),
			converter: None,
			devices: Vec::new(),
			sequence: 0,
			disconnected: false,
			reset: DeviceReset::default(),
		})
	}

	fn serial(&self) -> u32 {
		self.converter.as_ref().map_or(0, |c| c.connection().serial())
	}

	/// Read and handle everything the compositor sent.
	fn dispatch(&mut self) -> Result<(), String> {
		match self.context.read() {
			Ok(0) => {
				self.disconnected = true;
				return Err("the compositor closed the input connection".into());
			},
			Ok(_) => {},
			Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
			Err(error) => {
				self.disconnected = true;
				return Err(format!("input connection failed: {error}"));
			},
		}
		while let Some(result) = self.context.pending_event() {
			let event = match result {
				reis::PendingRequestResult::Request(event) => event,
				reis::PendingRequestResult::ParseError(error) => return Err(format!("libei protocol error: {error}")),
				reis::PendingRequestResult::InvalidObject(_) => continue,
			};
			if let Some(handshake) = &mut self.handshake {
				match handshake.handle_event(event) {
					Ok(Some(response)) => {
						self.converter = Some(reis::event::EiEventConverter::new(&self.context, response));
						self.handshake = None;
					},
					Ok(None) => {},
					Err(error) => return Err(format!("libei handshake failed: {error}")),
				}
				continue;
			}
			let converter = self.converter.as_mut().expect("handshake finished");
			converter
				.handle_event(event)
				.map_err(|error| format!("libei protocol error: {error}"))?;
		}
		self.drain_events()?;
		let _ = self.context.flush();
		Ok(())
	}

	fn drain_events(&mut self) -> Result<(), String> {
		let Some(converter) = self.converter.as_mut() else {
			return Ok(());
		};
		let mut events = Vec::new();
		while let Some(event) = converter.next_event() {
			events.push(event);
		}
		for event in events {
			match event {
				EiEvent::SeatAdded(seat) => {
					let wanted: BitFlags<DeviceCapability> = DeviceCapability::Pointer
						| DeviceCapability::PointerAbsolute
						| DeviceCapability::Keyboard
						| DeviceCapability::Button
						| DeviceCapability::Scroll
						| DeviceCapability::Touch;
					seat.seat.bind_capabilities(wanted);
				},
				EiEvent::DeviceAdded(added) => {
					tracing::info!(
						name = added.device.name().unwrap_or("unnamed"),
						kind = ?added.device.device_type(),
						"Desktop input device added"
					);
					let mut reset = DeviceReset::default();
					reset.include(&added.device);
					self.devices.push(EiDevice {
						device: added.device,
						reset,
						emulating: false,
					});
				},
				EiEvent::DeviceRemoved(removed) => {
					if let Some(device) = self.devices.iter().find(|d| d.device == removed.device) {
						self.reset.merge(device.reset);
					}
					self.devices.retain(|d| d.device != removed.device);
					tracing::debug!(name = removed.device.name(), "Desktop input device removed");
				},
				EiEvent::DeviceResumed(resumed) => {
					tracing::debug!(name = resumed.device.name(), "Desktop input device resumed");
					let serial = resumed.serial;
					if let Some(device) = self.devices.iter_mut().find(|d| d.device == resumed.device) {
						self.sequence += 1;
						device.device.device().start_emulating(serial, self.sequence);
						device.emulating = true;
					}
				},
				EiEvent::DevicePaused(paused) => {
					// The EIS protocol resets paused devices to neutral. Do not
					// retain those held states for a later resumed/replaced device.
					self.reset.include(&paused.device);
					tracing::debug!(name = paused.device.name(), "Desktop input device paused");
					if let Some(device) = self.devices.iter_mut().find(|d| d.device == paused.device) {
						device.emulating = false;
					}
				},
				EiEvent::Disconnected(disconnected) => {
					self.disconnected = true;
					return Err(format!(
						"the compositor ended remote input: {}",
						disconnected.explanation.unwrap_or_default()
					));
				},
				_ => {},
			}
		}
		Ok(())
	}

	/// The emulating device with `capability`.
	fn device(&self, capability: DeviceCapability) -> Option<&reis::event::Device> {
		self.devices
			.iter()
			.find(|d| d.emulating && d.device.has_capability(capability))
			.map(|d| &d.device)
	}

	fn frame(&self, device: &reis::event::Device) {
		device.device().frame(self.serial(), now_micros());
		let _ = self.context.flush();
	}

	/// Stop emulating on every device, ending the session's input cleanly.
	fn stop(&mut self) {
		let serial = self.serial();
		for device in self.devices.iter_mut().filter(|d| d.emulating) {
			device.device.device().stop_emulating(serial);
			device.emulating = false;
		}
		let _ = self.context.flush();
	}
}

impl DesktopInput {
	/// Input through libei on `socket`.
	pub(crate) fn eis(socket: OwnedFd, output: OutputGeometry) -> std::io::Result<Self> {
		Ok(Self::with(InputTransport::Eis, Some(Eis::new(socket)?), output))
	}

	/// No remote input: events are dropped (and logged once).
	pub(crate) fn none(output: OutputGeometry) -> Self {
		Self::with(InputTransport::None, None, output)
	}

	fn with(transport: InputTransport, eis: Option<Eis>, output: OutputGeometry) -> Self {
		Self {
			transport,
			eis,
			output,
			pressed: Pressed::default(),
			touches: BTreeSet::new(),
			warned: BTreeSet::new(),
			view: std::sync::Arc::default(),
			suspended: transport == InputTransport::Eis,
		}
	}

	/// Follow how the stream presents the output (see [`StreamView`]).
	pub(crate) fn set_view(&mut self, view: std::sync::Arc<StreamView>) {
		self.view = view;
	}

	pub(crate) fn transport(&self) -> InputTransport {
		self.transport
	}

	/// The libei socket, for registering with the event loop.
	pub(crate) fn eis_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
		use std::os::fd::AsFd;
		self.eis.as_ref().map(|eis| eis.context.as_fd())
	}

	pub(crate) fn authorized_devices(&self) -> bool {
		self.transport == InputTransport::None
			|| self.eis.as_ref().is_some_and(|eis| {
				eis.device(DeviceCapability::Keyboard).is_some()
					&& eis.device(DeviceCapability::PointerAbsolute).is_some()
					&& eis.device(DeviceCapability::Button).is_some()
			})
	}
	pub(crate) fn ready(&self) -> bool {
		self.authorized_devices()
			&& (self.transport == InputTransport::None
				|| self.eis.as_ref().is_some_and(|eis| {
					eis.device(DeviceCapability::PointerAbsolute)
						.is_some_and(|device| output_region(&self.output, &region_list(device)).is_some())
				}))
	}

	pub(crate) fn set_output(&mut self, output: OutputGeometry) {
		self.output = output;
	}

	/// Handle libei traffic; an error ends remote input for this session.
	pub(crate) fn dispatch(&mut self) -> Result<(), String> {
		let Some(eis) = &mut self.eis else {
			return Ok(());
		};
		let result = eis.dispatch();
		let reset = std::mem::take(&mut eis.reset);
		if reset.keys {
			self.pressed.keys.clear();
		}
		if reset.buttons {
			self.pressed.buttons.clear();
		}
		if reset.touches {
			self.touches.clear();
		}
		if result.is_err() {
			self.pressed = Pressed::default();
			self.touches.clear();
			self.suspended = true;
		} else {
			// Cache availability on administrative EIS updates. The per-input
			// path only reads a boolean, rather than locking device interfaces.
			self.set_suspended(!self.ready());
		}
		result
	}

	pub(crate) fn set_suspended(&mut self, suspended: bool) {
		if suspended && !self.suspended {
			self.release_all();
		}
		self.suspended = suspended;
	}

	fn warn_once(&mut self, what: &'static str, message: &str) {
		if self.warned.insert(what) {
			tracing::warn!(transport = self.transport.as_str(), "{message}");
		}
	}

	/// Logical desktop position of a normalized output point for libei, or
	/// stream-logical coordinates for the portal methods.
	fn absolute(&self, x: f64, y: f64, regions: Option<&[(Region, Option<String>)]>) -> Option<(f64, f64)> {
		let (x, y) = self.view.to_output((self.output.width, self.output.height), x, y);
		let (x, y) = normalized(x, y);
		match regions {
			Some(regions) => {
				let region = output_region(&self.output, regions)?;
				Some((region.x + x * region.width, region.y + y * region.height))
			},
			None => {
				let (w, h) = self
					.output
					.logical_size
					.map(|(w, h)| (f64::from(w), f64::from(h)))
					.unwrap_or((f64::from(self.output.width), f64::from(self.output.height)));
				Some((x * w, y * h))
			},
		}
	}

	fn key(&mut self, keycode: u32, state: u32) {
		match state {
			PRESSED => {
				self.pressed.keys.insert(keycode);
			},
			_ => {
				if !self.pressed.keys.remove(&keycode) {
					return;
				}
			},
		}
		if let Some(eis) = &self.eis {
			let Some(device) = eis.device(DeviceCapability::Keyboard) else {
				return;
			};
			let Some(keyboard) = device.interface::<ei::Keyboard>() else {
				return;
			};
			let state = if state == PRESSED {
				ei::keyboard::KeyState::Press
			} else {
				ei::keyboard::KeyState::Released
			};
			keyboard.key(keycode, state);
			eis.frame(device);
		}
	}

	fn button(&mut self, button: u32, state: u32) {
		match state {
			PRESSED => {
				self.pressed.buttons.insert(button);
			},
			_ => {
				if !self.pressed.buttons.remove(&button) {
					return;
				}
			},
		}
		if let Some(eis) = &self.eis {
			let Some(device) = eis.device(DeviceCapability::Button) else {
				return;
			};
			let Some(buttons) = device.interface::<ei::Button>() else {
				return;
			};
			let state = if state == PRESSED {
				ei::button::ButtonState::Press
			} else {
				ei::button::ButtonState::Released
			};
			buttons.button(button, state);
			eis.frame(device);
		}
	}

	/// Release every pressed key, button and touch point.
	pub(crate) fn release_all(&mut self) {
		for keycode in std::mem::take(&mut self.pressed.keys) {
			self.pressed.keys.insert(keycode);
			self.key(keycode, RELEASED);
		}
		for button in std::mem::take(&mut self.pressed.buttons) {
			self.pressed.buttons.insert(button);
			self.button(button, RELEASED);
		}
		for slot in std::mem::take(&mut self.touches) {
			self.touch_up(slot);
		}
	}

	fn touch_up(&mut self, slot: u32) {
		if let Some(eis) = &self.eis
			&& let Some(device) = eis.device(DeviceCapability::Touch)
			&& let Some(touch) = device.interface::<ei::Touchscreen>()
		{
			touch.up(slot);
			eis.frame(device);
		}
	}

	fn touch(&mut self, slot: u32, x: f32, y: f32, down: bool) {
		if let Some(eis) = &self.eis {
			let Some(device) = eis.device(DeviceCapability::Touch) else {
				self.warn_once(
					"touch",
					"The desktop offers no touchscreen device; touch input is ignored",
				);
				return;
			};
			let regions = region_list(device);
			let Some((x, y)) = self.absolute(f64::from(x), f64::from(y), Some(&regions)) else {
				return;
			};
			let Some(touch) = device.interface::<ei::Touchscreen>() else {
				return;
			};
			if down {
				touch.down(slot, x as f32, y as f32);
			} else {
				touch.motion(slot, x as f32, y as f32);
			}
			eis.frame(device);
		}
		if down {
			self.touches.insert(slot);
		}
	}

	/// Forward one client input event to the desktop.
	pub(crate) fn handle(&mut self, event: CaptureInputEvent) {
		if self.transport == InputTransport::None {
			if let CaptureInputEvent::Reset { ready, .. } = event {
				let _ = ready.send(());
			} else {
				self.warn_once(
					"view-only",
					"Remote input was not authorized for this desktop session; it is view-only",
				);
			}
			return;
		}
		if self.suspended {
			// Do not queue presses to replay on a new device. Reset barriers
			// still complete while devices are being replaced.
			self.release_all();
			if let CaptureInputEvent::Reset { ready, .. } = event {
				let _ = ready.send(());
			}
			return;
		}
		match event {
			CaptureInputEvent::Reset { keys, buttons, ready } => {
				// The control stream reports what the client held; release
				// those and anything else still pressed here.
				for keycode in keys {
					self.key(keycode, RELEASED);
				}
				for button in buttons {
					self.button(button, RELEASED);
				}
				self.release_all();
				let _ = ready.send(());
			},
			CaptureInputEvent::KeyDown { keycode } => self.key(keycode, PRESSED),
			CaptureInputEvent::KeyUp { keycode } => self.key(keycode, RELEASED),
			CaptureInputEvent::MouseButtonDown { button } => self.button(button, PRESSED),
			CaptureInputEvent::MouseButtonUp { button } => self.button(button, RELEASED),
			CaptureInputEvent::MouseMoveAbsolute {
				x,
				y,
				screen_width,
				screen_height,
			} => {
				let nx = if screen_width > 0 {
					f64::from(x) / f64::from(screen_width)
				} else {
					0.0
				};
				let ny = if screen_height > 0 {
					f64::from(y) / f64::from(screen_height)
				} else {
					0.0
				};
				if let Some(eis) = &self.eis {
					let Some(device) = eis.device(DeviceCapability::PointerAbsolute) else {
						self.warn_once(
							"absolute",
							"The desktop offers no absolute pointer device; absolute motion is ignored",
						);
						return;
					};
					let regions = region_list(device);
					let Some((x, y)) = self.absolute(nx, ny, Some(&regions)) else {
						self.warn_once(
							"region",
							"The captured output is not among the absolute pointer's regions; absolute motion is ignored",
						);
						return;
					};
					if let Some(pointer) = device.interface::<ei::PointerAbsolute>() {
						pointer.motion_absolute(x as f32, y as f32);
						eis.frame(device);
					}
				}
			},
			CaptureInputEvent::MouseMoveRelative { dx, dy } => {
				if let Some(eis) = &self.eis
					&& let Some(device) = eis.device(DeviceCapability::Pointer)
					&& let Some(pointer) = device.interface::<ei::Pointer>()
				{
					pointer.motion_relative(f32::from(dx), f32::from(dy));
					eis.frame(device);
				}
			},
			CaptureInputEvent::ScrollVertical { amount } => self.scroll(0, -i32::from(amount)),
			CaptureInputEvent::ScrollHorizontal { amount } => self.scroll(i32::from(amount), 0),
			CaptureInputEvent::TouchDown { slot, x, y } => self.touch(slot, x, y, true),
			CaptureInputEvent::TouchMove { slot, x, y } => self.touch(slot, x, y, false),
			CaptureInputEvent::TouchUp { slot } => {
				if self.touches.remove(&slot) {
					self.touch_up(slot);
				}
			},
			CaptureInputEvent::TouchCancelAll => {
				for slot in std::mem::take(&mut self.touches) {
					self.touch_up(slot);
				}
			},
			CaptureInputEvent::Pen { .. } => {
				self.warn_once("pen", "Pen input is not supported in Desktop Mode; it is ignored");
			},
			CaptureInputEvent::TypeText { text } => self.type_text(&text),
		}
	}

	/// Scroll in 1/120 wheel steps (positive: right, down).
	fn scroll(&mut self, x120: i32, y120: i32) {
		if let Some(eis) = &self.eis
			&& let Some(device) = eis.device(DeviceCapability::Scroll)
			&& let Some(scroll) = device.interface::<ei::Scroll>()
		{
			scroll.scroll_discrete(x120, y120);
			eis.frame(device);
		}
	}

	fn type_text(&mut self, text: &str) {
		if let Some(eis) = &self.eis
			&& let Some(device) = eis.device(DeviceCapability::Text)
			&& let Some(input) = device.interface::<ei::Text>()
		{
			input.utf8(text);
			eis.frame(device);
			return;
		}
		self.warn_once(
			"text",
			"The desktop offers no text input device; clipboard typing is ignored",
		);
	}

	/// Release everything and stop emulating before the session closes.
	pub(crate) fn shutdown(&mut self) {
		self.release_all();
		if let Some(eis) = &mut self.eis {
			eis.stop();
		}
	}
}

fn region_list(device: &reis::event::Device) -> Vec<(Region, Option<String>)> {
	device
		.regions()
		.iter()
		.map(|region| {
			(
				Region {
					x: f64::from(region.x),
					y: f64::from(region.y),
					width: f64::from(region.width),
					height: f64::from(region.height),
					physical_scale: f64::from(region.scale),
				},
				region.mapping_id.clone(),
			)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn output() -> OutputGeometry {
		OutputGeometry {
			width: 3840,
			height: 2160,
			logical_position: Some((1920, 0)),
			logical_size: Some((2560, 1440)),
			mapping_id: None,
			follow_scale: false,
		}
	}

	fn region(x: f64, y: f64, width: f64, height: f64) -> Region {
		Region {
			x,
			y,
			width,
			height,
			physical_scale: 1.0,
		}
	}

	struct MockEis {
		server: reis::eis::Context,
		seat: reis::eis::Seat,
		device: reis::eis::Device,
		input: DesktopInput,
	}
	impl MockEis {
		fn new() -> Self {
			use reis::{PendingRequestResult, eis};
			let (server_socket, client_socket) = UnixStream::pair().unwrap();
			let server = eis::Context::new(server_socket).unwrap();
			let mut input = DesktopInput::eis(client_socket.into(), output()).unwrap();
			let handshake = server.handshake();
			handshake.handshake_version(1);
			server.flush().unwrap();
			input.dispatch().unwrap();
			server.read().unwrap();
			let mut finished = false;
			while let Some(request) = server.pending_request() {
				match request {
					PendingRequestResult::Request(eis::Request::Handshake(
						_,
						eis::handshake::Request::InterfaceVersion { name, version },
					)) => handshake.interface_version(&name, version.min(1)),
					PendingRequestResult::Request(eis::Request::Handshake(_, eis::handshake::Request::Finish)) => {
						finished = true
					},
					PendingRequestResult::Request(_) => {},
					other => panic!("mock handshake: {other:?}"),
				}
			}
			assert!(finished);
			let connection = handshake.connection(1, 1);
			let seat = connection.seat(1);
			seat.name("test seat");
			for (i, interface) in ["ei_keyboard", "ei_pointer_absolute", "ei_button"]
				.into_iter()
				.enumerate()
			{
				seat.capability(1 << i, interface);
			}
			seat.done();
			server.flush().unwrap();
			input.dispatch().unwrap();
			server.read().unwrap();
			while let Some(request) = server.pending_request() {
				assert!(matches!(request, PendingRequestResult::Request(_)));
			}
			let device = Self::add_device(&seat, 2);
			server.flush().unwrap();
			input.dispatch().unwrap();
			assert!(input.ready());
			Self {
				server,
				seat,
				device,
				input,
			}
		}
		fn add_device(seat: &reis::eis::Seat, serial: u32) -> reis::eis::Device {
			Self::add_scaled_device(seat, serial, 2560, 1440, 1.5)
		}
		fn add_scaled_device(
			seat: &reis::eis::Seat,
			serial: u32,
			width: u32,
			height: u32,
			scale: f32,
		) -> reis::eis::Device {
			use reis::eis;
			let device = seat.device(1);
			device.name("test input");
			device.device_type(eis::device::DeviceType::Virtual);
			device.interface::<eis::Keyboard>(1);
			device.interface::<eis::PointerAbsolute>(1);
			device.interface::<eis::Button>(1);
			device.region(1920, 0, width, height, scale);
			device.done();
			device.resumed(serial);
			device
		}
		fn dispatch(&mut self) {
			self.server.flush().unwrap();
			self.input.dispatch().unwrap();
		}
	}

	#[test]
	fn eis_pause_resumes_without_revocation_or_replaying_held_keys() {
		let mut mock = MockEis::new();
		let now = Instant::now();
		let mut readiness = Readiness::default();
		mock.input.handle(CaptureInputEvent::KeyDown { keycode: 30 });
		mock.input.handle(CaptureInputEvent::MouseButtonDown { button: 0x110 });
		assert!(!mock.input.pressed.keys.is_empty());
		mock.device.paused(3);
		mock.dispatch();
		assert!(!mock.input.ready());
		assert!(!readiness.expired(mock.input.ready(), now));
		mock.input.set_suspended(true);
		assert!(mock.input.pressed.keys.is_empty());
		assert!(mock.input.pressed.buttons.is_empty());
		mock.input.handle(CaptureInputEvent::KeyDown { keycode: 31 });
		assert!(mock.input.pressed.keys.is_empty());
		mock.device.resumed(4);
		mock.dispatch();
		mock.input.set_suspended(false);
		assert!(mock.input.ready());
		assert!(!readiness.expired(mock.input.ready(), now + Duration::from_secs(2)));
		assert!(mock.input.pressed.keys.is_empty());
	}

	#[test]
	fn eis_replacement_clears_held_state_even_when_ready_in_the_same_dispatch() {
		let mut mock = MockEis::new();
		mock.input.handle(CaptureInputEvent::KeyDown { keycode: 30 });
		mock.device.destroyed(3);
		mock.device = MockEis::add_device(&mock.seat, 4);
		mock.dispatch();
		assert!(mock.input.ready());
		assert!(mock.input.pressed.keys.is_empty());
	}

	#[test]
	fn eis_scale_changes_keep_input_ready_and_map_into_the_new_logical_size() {
		let mut mock = MockEis::new();
		mock.input.output.follow_scale = true;
		for (serial, width, height, scale) in [(3, 1920, 1080, 2.0), (5, 3072, 1728, 1.25), (7, 3840, 2160, 1.0)] {
			mock.device.destroyed(serial);
			mock.device = MockEis::add_scaled_device(&mock.seat, serial + 1, width, height, scale);
			mock.dispatch();
			assert!(mock.input.ready());
			assert!(!mock.input.suspended);
			let device = mock
				.input
				.eis
				.as_ref()
				.unwrap()
				.device(DeviceCapability::PointerAbsolute)
				.unwrap();
			assert_eq!(
				mock.input.absolute(0.5, 0.5, Some(&region_list(device))),
				Some((1920.0 + f64::from(width) / 2.0, f64::from(height) / 2.0))
			);
			assert_eq!((mock.input.output.width, mock.input.output.height), (3840, 2160));
		}
	}

	#[test]
	fn managed_regions_follow_fractional_scale_but_reject_stale_or_invalid_regions() {
		let output = OutputGeometry {
			width: 2880,
			height: 1920,
			logical_position: Some((0, 0)),
			logical_size: Some((2880, 1920)),
			follow_scale: true,
			mapping_id: None,
		};
		let scaled = Region {
			physical_scale: 1.75,
			..region(0.0, 0.0, 1646.0, 1097.0)
		};
		assert_eq!(output_region(&output, &[(scaled, None)]), Some(scaled));
		let input = DesktopInput::none(output.clone());
		assert_eq!(input.absolute(0.5, 0.5, Some(&[(scaled, None)])), Some((823.0, 548.5)));
		for invalid in [
			Region {
				physical_scale: 1.0,
				..scaled
			},
			Region {
				physical_scale: f64::NAN,
				..scaled
			},
			Region {
				physical_scale: 0.0,
				..scaled
			},
			Region {
				physical_scale: -1.75,
				..scaled
			},
			Region { x: 10.0, ..scaled },
			Region {
				width: 1920.0,
				height: 1080.0,
				..scaled
			},
			Region {
				width: 2880.0,
				height: 1920.0,
				..scaled
			},
		] {
			assert_eq!(output_region(&output, &[(invalid, None)]), None);
		}
	}

	#[test]
	fn missing_input_has_a_fixed_deadline_and_socket_loss_is_immediate() {
		let now = Instant::now();
		let mut readiness = Readiness::default();
		assert!(!readiness.expired(false, now));
		assert!(!readiness.expired(false, now + Duration::from_secs(2)));
		assert!(readiness.expired(false, now + Duration::from_secs(3)));
		assert!(!readiness.expired(true, now + Duration::from_secs(4)));
		assert!(!readiness.expired(false, now + Duration::from_secs(5)));
		assert!(readiness.expired(false, now + Duration::from_secs(8)));
		let mock = MockEis::new();
		let MockEis { server, mut input, .. } = mock;
		drop(server);
		assert!(input.dispatch().is_err());
	}

	#[test]
	fn a_single_stale_region_cannot_match_an_explicit_managed_geometry() {
		assert_eq!(
			output_region(&output(), &[(region(0.0, 0.0, 1920.0, 1080.0), None)]),
			None
		);
	}

	#[test]
	fn the_output_region_is_found_by_mapping_then_geometry() {
		let left = region(0.0, 0.0, 1920.0, 1080.0);
		let right = region(1920.0, 0.0, 2560.0, 1440.0);
		let regions = [(left, Some("a".to_string())), (right, Some("b".to_string()))];
		assert_eq!(output_region(&output(), &regions), Some(right));
		let mapped = OutputGeometry {
			mapping_id: Some("a".into()),
			..output()
		};
		assert_eq!(output_region(&mapped, &regions), Some(left));
		// Ambiguous: two regions, no identifying information.
		let unknown = OutputGeometry {
			logical_position: None,
			..output()
		};
		assert_eq!(output_region(&unknown, &regions), None);
		assert_eq!(output_region(&unknown, &regions[..1]), Some(left));
	}

	#[test]
	fn absolute_positions_scale_into_the_output_region() {
		let input = DesktopInput::none(output());
		let regions = [(region(1920.0, 0.0, 2560.0, 1440.0), None)];
		assert_eq!(input.absolute(0.5, 0.5, Some(&regions)), Some((3200.0, 720.0)));
		assert_eq!(input.absolute(2.0, -1.0, Some(&regions)), Some((4480.0, 0.0)));
		// Portal methods take stream-logical coordinates.
		assert_eq!(input.absolute(0.5, 1.0, None), Some((1280.0, 1440.0)));
	}

	#[test]
	fn letterboxed_streams_map_back_onto_the_output() {
		let mut input = DesktopInput::none(output());
		let view = std::sync::Arc::new(StreamView::default());
		input.set_view(view.clone());
		let regions = [(region(1920.0, 0.0, 2560.0, 1440.0), None)];
		// A 3840x2160 output in a 16:9 stream fills it.
		view.set(1920, 1080, true);
		assert_eq!(input.absolute(0.5, 0.5, Some(&regions)), Some((3200.0, 720.0)));
		// In a 4:3 stream it is letterboxed: 1440x810 at y 135 of 1440x1080.
		view.set(1440, 1080, true);
		assert_eq!(input.absolute(0.5, 0.5, Some(&regions)), Some((3200.0, 720.0)));
		assert_eq!(input.absolute(0.0, 135.0 / 1080.0, Some(&regions)), Some((1920.0, 0.0)));
		// The bars clamp to the output's edge.
		assert_eq!(input.absolute(0.5, 0.01, Some(&regions)), Some((3200.0, 0.0)));
		// PyroWave stretches: positions are not remapped.
		view.set_fit(false);
		assert_eq!(input.absolute(0.0, 135.0 / 1080.0, Some(&regions)).unwrap().1, 180.0);
	}

	#[test]
	fn view_only_sessions_acknowledge_resets() {
		let mut input = DesktopInput::none(output());
		let (ready, waiting) = tokio::sync::oneshot::channel();
		input.handle(CaptureInputEvent::Reset {
			keys: vec![30],
			buttons: vec![],
			ready,
		});
		assert!(waiting.blocking_recv().is_ok());
	}
}
