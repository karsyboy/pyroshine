//! Native Valve USB controller interface (classic state 1 / Deck state 9).
//! Report layouts follow Linux hid-steam and SDL's Valve HIDAPI parsers.
//! This backend belongs beside the input adapter: the pinned public Inputtino
//! API has no Valve device. It uses the existing local gamepad runtime for
//! feature requests, so there are no native callback threads or detached tasks.

use std::{
	cell::RefCell,
	fs::OpenOptions,
	io::{self, Read, Write},
	os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
	rc::Rc,
};
use tokio::{io::unix::AsyncFd, task::JoinHandle};

use super::{BatteryState, GamepadBattery, GamepadMotion, GamepadTouch, GamepadUpdate, VirtualIdentity};
use crate::session::stream::control::{FeedbackCommand, feedback::RumbleCommand, input::ownership::FeedbackRoute};
use inputtino::JoypadMotionType;

// Vendor application collection, unnumbered 64-byte input and feature reports.
// hid-steam identifies the controller interface by its feature collection.
const DESCRIPTOR: &[u8] = &[
	0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x40, 0x09, 0x01, 0x81,
	0x02, 0x09, 0x01, 0xb1, 0x02, 0xc0,
];
// linux/uhid.h is a packed, native-endian UAPI. Short writes are zero-filled by
// the kernel. Encode bytes explicitly, avoiding Rust/C packing assumptions.
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
const EVENT_SIZE: usize = 4380;

fn event(kind: u32, payload: &[u8]) -> Vec<u8> {
	let mut bytes = Vec::with_capacity(4 + payload.len());
	bytes.extend_from_slice(&kind.to_ne_bytes());
	bytes.extend_from_slice(payload);
	bytes
}

fn write_event(mut file: &std::fs::File, bytes: &[u8]) -> io::Result<()> {
	// UHID expects one event per write; write_all would split a short write.
	match file.write(bytes) {
		Ok(n) if n == bytes.len() => Ok(()),
		Ok(_) => Err(io::ErrorKind::WriteZero.into()),
		Err(e) => Err(e),
	}
}

fn product(model: VirtualIdentity) -> u16 {
	if model == VirtualIdentity::SteamDeck {
		0x1205
	} else {
		0x1102
	}
}

fn report_period_ms(model: VirtualIdentity) -> u64 {
	// SDL's Deck parser uses the native 4 ms interval for sensor timestamps.
	// Classic readers obtain the interval from the virtual attributes reply.
	if model == VirtualIdentity::SteamDeck { 4 } else { 10 }
}

fn create_event(model: VirtualIdentity, index: u8) -> Vec<u8> {
	let mut payload = vec![0; 280 + DESCRIPTOR.len()];
	let name = if model == VirtualIdentity::SteamDeck {
		"Steam Deck"
	} else {
		"Steam Controller"
	};
	payload[..name.len()].copy_from_slice(name.as_bytes());
	let serial = format!("PYROVALVE{index:02X}");
	payload[128..128 + serial.len()].copy_from_slice(serial.as_bytes());
	payload[192..192 + serial.len()].copy_from_slice(serial.as_bytes());
	payload[256..258].copy_from_slice(&(DESCRIPTOR.len() as u16).to_ne_bytes());
	payload[258..260].copy_from_slice(&3u16.to_ne_bytes()); // BUS_USB
	payload[260..264].copy_from_slice(&0x28deu32.to_ne_bytes());
	payload[264..268].copy_from_slice(&u32::from(product(model)).to_ne_bytes());
	payload[268..272].copy_from_slice(&0x0111u32.to_ne_bytes());
	payload[280..].copy_from_slice(DESCRIPTOR);
	event(UHID_CREATE2, &payload)
}

#[derive(Clone)]
struct Report {
	model: VirtualIdentity,
	bytes: [u8; 64],
	sequence: u32,
	flags: u32,
	// Classic firmware multiplexes the left stick and left pad. Only normalized
	// D-pad directions survive SDL; no physical left-pad contacts are invented.
	stick: (i16, i16),
}

impl Report {
	fn new(model: VirtualIdentity) -> Self {
		let mut bytes = [0; 64];
		bytes[..4].copy_from_slice(&[
			1,
			0,
			if model == VirtualIdentity::SteamDeck { 9 } else { 1 },
			if model == VirtualIdentity::SteamDeck { 64 } else { 60 },
		]);
		Self {
			model,
			bytes,
			sequence: 0,
			flags: 0,
			stick: (0, 0),
		}
	}

	fn deck(&self) -> bool {
		self.model == VirtualIdentity::SteamDeck
	}

	fn put(&mut self, offset: usize, value: i16) {
		self.bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
	}

	fn buttons(&mut self, flags: u32) {
		self.flags = flags;
		// Preserve touch state; button updates cannot release a contact.
		let touch = self.bytes[10] & 0x18;
		let triggers = self.bytes[8] & 3;
		if self.deck() {
			self.bytes[8..16].fill(0);
		} else {
			self.bytes[8..11].fill(0);
		}
		self.bytes[8] = triggers;
		self.bytes[10] = touch;
		for (flag, byte, bit) in [
			(0x1000, 8, 7),
			(0x2000, 8, 5),
			(0x4000, 8, 6),
			(0x8000, 8, 4),
			(0x100, 8, 3),
			(0x200, 8, 2),
			(1, 9, 0),
			(2, 9, 3),
			(4, 9, 2),
			(8, 9, 1),
			(0x20, 9, 4),
			(0x400, 9, 5),
			(0x10, 9, 6),
			(0x40, 10, 6),
		] {
			if flags & flag != 0 {
				self.bytes[byte] |= 1 << bit;
			}
		}
		if self.deck() {
			for (flag, byte, bit) in [
				(0x80, 11, 2),
				(0x10000, 13, 2),
				(0x20000, 13, 1),
				(0x40000, 10, 0),
				(0x80000, 9, 7),
				(0x200000, 14, 2),
				(0x100000, 10, 2),
			] {
				if flags & flag != 0 {
					self.bytes[byte] |= 1 << bit;
				}
			}
		} else {
			if flags & 0x10000 != 0 {
				self.bytes[10] |= 1;
			}
			if flags & 0x20000 != 0 {
				self.bytes[9] |= 0x80;
			}
			if flags & 0x80 != 0 {
				self.bytes[10] |= 4;
			}
		}
	}

	fn axes(&mut self, update: &GamepadUpdate) {
		self.stick = update.left_stick;
		if self.deck() {
			self.put(44, (u32::from(update.left_trigger) * 32767 / 255) as i16);
			self.put(46, (u32::from(update.right_trigger) * 32767 / 255) as i16);
			self.put(48, update.left_stick.0);
			self.put(50, update.left_stick.1);
			self.put(52, update.right_stick.0);
			self.put(54, update.right_stick.1);
		} else {
			// SDL normalizes the classic raw trigger's 26000/129 endpoint.
			let raw = |trigger: u8| ((u32::from(trigger) * 26000 + (255 * 129 / 2)) / (255 * 129)) as u8;
			self.bytes[11] = raw(update.left_trigger);
			self.bytes[12] = raw(update.right_trigger);
			self.put(20, update.right_stick.0);
			self.put(22, update.right_stick.1);
		}
		// Digital full-press bits are part of the native Valve report too.
		self.bytes[8] =
			(self.bytes[8] & !3) | u8::from(update.right_trigger == 255) | (u8::from(update.left_trigger == 255) << 1);
	}

	fn motion(&mut self, motion: &GamepadMotion) {
		let gyro = matches!(motion.motion_type, JoypadMotionType::GYROSCOPE);
		let scale = if gyro {
			32768.0 / 2000.0
		} else {
			32768.0 / (2.0 * 9.80665)
		};
		let offset = if self.deck() {
			if gyro { 30 } else { 24 }
		} else if gyro {
			34
		} else {
			28
		};
		// Inverse of SDL sensor coordinates: x, z, -y (classic gyro: +y).
		let y = if gyro && !self.deck() { motion.z } else { -motion.z };
		for (i, value) in [motion.x, y, motion.y].into_iter().enumerate() {
			self.put(offset + i * 2, (value * scale).round().clamp(-32768.0, 32767.0) as i16);
		}
	}

	fn touch(&mut self, touch: &GamepadTouch) {
		if touch.event_type == 7 && self.deck() {
			self.bytes[10] &= !0x18;
			self.bytes[56..60].fill(0);
			return;
		}
		if !self.deck() || touch.touchpad > 1 || (touch.pointer_id != 0 && touch.event_type != 7) {
			return;
		}
		let active = match touch.event_type {
			1 | 3 => true,      // DOWN / MOVE
			2 | 4 | 7 => false, // UP / CANCEL / CANCEL_ALL
			_ => return,
		};
		let pad = usize::from(touch.touchpad);
		let mask = 1 << (3 + pad);
		self.bytes[10] = (self.bytes[10] & !mask) | if active { mask } else { 0 };
		self.put(
			16 + pad * 4,
			((touch.x - 0.5) * 65536.0).clamp(-32768.0, 32767.0) as i16,
		);
		self.put(
			18 + pad * 4,
			((0.5 - touch.y) * 65536.0).clamp(-32768.0, 32767.0) as i16,
		);
		self.put(56 + pad * 2, if active { (touch.pressure * 32767.0) as i16 } else { 0 });
	}

	fn next(&mut self, pad_frame: bool) -> [u8; 64] {
		self.sequence = self.sequence.wrapping_add(1);
		self.bytes[4..8].copy_from_slice(&self.sequence.to_le_bytes());
		if !self.deck() {
			let dpad = self.flags & 15;
			self.bytes[10] &= !0x88;
			if pad_frame && dpad != 0 {
				self.bytes[10] |= 0x88; // left pad data with simultaneous stick
				self.put(
					16,
					if dpad & 8 != 0 {
						24000
					} else if dpad & 4 != 0 {
						-24000
					} else {
						0
					},
				);
				self.put(
					18,
					if dpad & 1 != 0 {
						24000
					} else if dpad & 2 != 0 {
						-24000
					} else {
						0
					},
				);
			} else {
				if dpad != 0 {
					self.bytes[10] |= 0x80;
				}
				self.put(16, self.stick.0);
				self.put(18, self.stick.1);
			}
		}
		self.bytes
	}

	fn neutralize(&mut self) {
		let accel = if self.deck() { 24 } else { 28 };
		let orientation: [u8; 6] = self.bytes[accel..accel + 6].try_into().unwrap();
		let sequence = self.sequence;
		*self = Self::new(self.model);
		self.sequence = sequence;
		self.bytes[accel..accel + 6].copy_from_slice(&orientation);
	}
}

struct Features {
	reply: [u8; 65],
	settings: [u16; 256],
	model: VirtualIdentity,
	index: u8,
}

impl Features {
	fn new(model: VirtualIdentity, index: u8) -> Self {
		Self {
			reply: [0; 65],
			settings: [0; 256],
			model,
			index,
		}
	}

	fn set(&mut self, data: &[u8]) -> Result<Option<(u16, u16)>, u16> {
		// hidraw feature I/O always includes report ID zero; hid-steam does too.
		let data = if data.first() == Some(&0) { &data[1..] } else { data };
		if data.is_empty() {
			return Err(libc::EINVAL as u16);
		}
		let size = usize::from(data.get(1).copied().unwrap_or(0));
		if size > data.len().saturating_sub(2) {
			return Err(libc::EINVAL as u16);
		}
		let args = data.get(2..2 + size).unwrap_or(&[]);
		self.reply.fill(0);
		self.reply[1] = data[0];
		match data[0] {
			0x83 => {
				// Attributes reflect this virtual model, not fabricated hardware calibration.
				self.reply[2] = 10;
				self.reply[3] = 1; // ATTRIB_PRODUCT_ID
				self.reply[4..8].copy_from_slice(&u32::from(product(self.model)).to_le_bytes());
				self.reply[8] = 11; // ATTRIB_CONNECTION_INTERVAL_IN_US
				self.reply[9..13].copy_from_slice(&((report_period_ms(self.model) * 1000) as u32).to_le_bytes());
			},
			0xae if args.first() == Some(&1) => {
				let serial = format!("PYROVALVE{:02X}", self.index);
				self.reply[2] = 1 + serial.len() as u8;
				self.reply[3] = 1;
				self.reply[4..4 + serial.len()].copy_from_slice(serial.as_bytes());
			},
			0x87 if args.len().is_multiple_of(3) => {
				for setting in args.as_chunks::<3>().0 {
					self.settings[usize::from(setting[0])] = u16::from_le_bytes([setting[1], setting[2]]);
				}
			},
			0x89 if args.len().is_multiple_of(3) && args.len() <= 60 => {
				self.reply[2] = size as u8;
				for (i, setting) in args.as_chunks::<3>().0.iter().enumerate() {
					self.reply[3 + i * 3] = setting[0];
					self.reply[4 + i * 3..6 + i * 3]
						.copy_from_slice(&self.settings[usize::from(setting[0])].to_le_bytes());
				}
			},
			0x81 | 0x85 => {}, // No keyboard/mouse interfaces or lizard mode to toggle.
			0x88 | 0x8e => self.settings.fill(0),
			0xeb if args.len() >= 7 => {
				return Ok(Some((
					u16::from_le_bytes([args[3], args[4]]),
					u16::from_le_bytes([args[5], args[6]]),
				)));
			},
			// Pulse waveforms cannot be represented by Moonlight's motor amplitudes.
			// Do not acknowledge unsupported calibration, firmware or haptic commands.
			_ => return Err(libc::EOPNOTSUPP as u16),
		}
		Ok(None)
	}
}

pub(super) struct ValveGamepad {
	fd: Rc<AsyncFd<std::fs::File>>,
	report: Rc<RefCell<Report>>,
	reader: JoinHandle<()>,
}

impl ValveGamepad {
	pub(super) fn new(model: VirtualIdentity, index: u8, route: FeedbackRoute) -> io::Result<Self> {
		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
			.open("/dev/uhid")?;
		Self::from_file(model, index, route, file)
	}

	fn from_file(model: VirtualIdentity, index: u8, route: FeedbackRoute, file: std::fs::File) -> io::Result<Self> {
		let fd = Rc::new(AsyncFd::new(file)?);
		write_event(fd.get_ref(), &create_event(model, index))?;
		let report = Rc::new(RefCell::new(Report::new(model)));
		let reader = tokio::task::spawn_local(read_features(
			fd.clone(),
			Features::new(model, index),
			route,
			report.clone(),
		));
		Ok(Self { fd, report, reader })
	}

	fn send(&self) {
		if let Err(e) = send_report(self.fd.get_ref(), &mut self.report.borrow_mut()) {
			tracing::debug!(%e, "Valve input report was not delivered");
		}
	}

	pub(super) fn set_pressed(&self, flags: u32) {
		self.report.borrow_mut().buttons(flags);
		self.send();
	}
	pub(super) fn apply_update(&self, update: &GamepadUpdate) {
		self.report.borrow_mut().axes(update);
		self.send();
	}
	pub(super) fn set_motion(&self, motion: &GamepadMotion) {
		self.report.borrow_mut().motion(motion);
		self.send();
	}
	pub(super) fn touch(&self, touch: &GamepadTouch) {
		self.report.borrow_mut().touch(touch);
		self.send();
	}
	pub(super) fn neutralize(&self) {
		self.report.borrow_mut().neutralize();
		self.send();
	}

	pub(super) fn set_battery(&self, battery: &GamepadBattery) {
		// Deck has no battery status in its controller report. Classic supports
		// the native status message; omit unknown/absent battery values.
		if self.report.borrow().deck()
			|| battery.battery_percentage > 100
			|| !matches!(
				battery.battery_state,
				BatteryState::Discharging | BatteryState::Charging | BatteryState::Full
			) {
			return;
		}
		let mut report = [0u8; 70];
		report[..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
		report[4..6].copy_from_slice(&64u16.to_ne_bytes());
		report[6..10].copy_from_slice(&[1, 0, 4, 11]);
		report[20] = battery.battery_percentage;
		let _ = write_event(self.fd.get_ref(), &report);
	}
}

impl Drop for ValveGamepad {
	fn drop(&mut self) {
		self.reader.abort();
		// Destroy synchronously before another slot can reuse the identity.
		// The aborted reader owns an Rc until cancellation; no callback uses
		// the next device and the fd is closed once that task is dropped.
		let _ = write_event(self.fd.get_ref(), &1u32.to_ne_bytes());
	}
}

fn send_report(file: &std::fs::File, report: &mut Report) -> io::Result<()> {
	for pad_frame in [false, true] {
		if pad_frame && (report.deck() || report.flags & 15 == 0) {
			break;
		}
		let bytes = report.next(pad_frame);
		// Hot-path writes are stack-only, one bounded nonblocking syscall.
		let mut ev = [0; 70];
		ev[..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
		ev[4..6].copy_from_slice(&64u16.to_ne_bytes());
		ev[6..].copy_from_slice(&bytes);
		write_event(file, &ev)?;
	}
	Ok(())
}

async fn read_features(
	fd: Rc<AsyncFd<std::fs::File>>,
	mut features: Features,
	route: FeedbackRoute,
	report: Rc<RefCell<Report>>,
) {
	// Native Valve readers expect a live stream, including unchanged input.
	// Use the existing gamepad runtime, with no thread or input-side waits.
	let mut ticks = tokio::time::interval(std::time::Duration::from_millis(report_period_ms(features.model)));
	ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	loop {
		let mut ready = tokio::select! {
			ready = fd.readable() => match ready { Ok(ready) => ready, Err(_) => break },
			_ = ticks.tick() => {
				// An early tick can precede UHID_START. Subsequent ticks deliver
				// the neutral state as soon as the kernel registers the device.
				let _ = send_report(fd.get_ref(), &mut report.borrow_mut());
				continue;
			},
		};
		let mut bytes = [0u8; EVENT_SIZE];
		let count = match ready.try_io(|fd| fd.get_ref().read(&mut bytes)) {
			Ok(Ok(0)) | Ok(Err(_)) => break,
			Ok(Ok(count)) => count,
			Err(_) => continue,
		};
		if count < 4 {
			continue;
		}
		let kind = u32::from_ne_bytes(bytes[..4].try_into().unwrap());
		let id = &bytes[4..8];
		match kind {
			UHID_SET_REPORT if count >= 12 => {
				let size = usize::from(u16::from_ne_bytes(bytes[10..12].try_into().unwrap()));
				let result = if bytes[8] != 0 || bytes[9] != 0 || size > count - 12 {
					Err(libc::EINVAL as u16)
				} else {
					features.set(&bytes[12..12 + size])
				};
				if let Ok(Some((low_frequency, high_frequency))) = result {
					route.try_deliver(FeedbackCommand::Rumble(RumbleCommand {
						id: u16::from(features.index),
						low_frequency,
						high_frequency,
					}));
				}
				let mut reply = [0u8; 10];
				reply[..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
				reply[4..8].copy_from_slice(id);
				reply[8..].copy_from_slice(&result.err().unwrap_or(0).to_ne_bytes());
				if write_event(fd.get_ref(), &reply).is_err() {
					break;
				}
			},
			UHID_GET_REPORT if count >= 10 => {
				let error = if bytes[8] == 0 && bytes[9] == 0 {
					0u16
				} else {
					libc::EOPNOTSUPP as u16
				};
				let mut reply = [0u8; 77];
				reply[..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
				reply[4..8].copy_from_slice(id);
				reply[8..10].copy_from_slice(&error.to_ne_bytes());
				reply[10..12].copy_from_slice(&65u16.to_ne_bytes());
				reply[12..].copy_from_slice(&features.reply);
				if write_event(fd.get_ref(), &reply).is_err() {
					break;
				}
			},
			_ => {},
		}
	}
	tracing::debug!(fd = fd.as_raw_fd(), "Valve feature handler stopped");
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::fd::OwnedFd;

	fn update() -> GamepadUpdate {
		GamepadUpdate {
			index: 0,
			active_gamepad_mask: 1,
			button_flags: 0,
			left_trigger: 64,
			right_trigger: 128,
			left_stick: (-12345, 23456),
			right_stick: (4567, -7654),
		}
	}
	fn touch(pad: u8, event_type: u8) -> GamepadTouch {
		GamepadTouch {
			index: 0,
			event_type,
			touchpad: pad,
			pointer_id: 0,
			x: 0.25,
			y: 0.75,
			pressure: 0.1,
		}
	}
	fn i16_at(bytes: &[u8], offset: usize) -> i16 {
		i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
	}

	#[test]
	fn native_models_have_valve_descriptor_identity_and_report_layout() {
		for model in [VirtualIdentity::SteamController, VirtualIdentity::SteamDeck] {
			let create = create_event(model, 15);
			assert_eq!(u32::from_ne_bytes(create[..4].try_into().unwrap()), UHID_CREATE2);
			assert_eq!(u32::from_ne_bytes(create[264..268].try_into().unwrap()), 0x28de);
			assert_eq!(
				u32::from_ne_bytes(create[268..272].try_into().unwrap()),
				u32::from(product(model))
			);
			assert_eq!(u16::from_ne_bytes(create[262..264].try_into().unwrap()), 3);
			assert_eq!(&create[284..], DESCRIPTOR);
			let report = Report::new(model);
			assert_eq!(
				&report.bytes[..4],
				if report.deck() { &[1, 0, 9, 64] } else { &[1, 0, 1, 60] }
			);
			assert_eq!(report.bytes.len(), 64);
		}
	}

	#[test]
	fn valve_paddles_isolate_standard_controls_and_every_release() {
		for model in [VirtualIdentity::SteamController, VirtualIdentity::SteamDeck] {
			let deck = model == VirtualIdentity::SteamDeck;
			for pressed in 0u32..16 {
				let mut report = Report::new(model);
				report.buttons((pressed << 16) | 0x1000);
				assert_eq!(report.bytes[8], 0x80); // A, independent of paddles
				assert_eq!(
					report.bytes[9] & 0x80,
					if deck {
						((pressed & 8) != 0) as u8 * 0x80
					} else {
						((pressed & 2) != 0) as u8 * 0x80
					}
				);
				assert_eq!(
					report.bytes[10] & 1,
					if deck {
						((pressed & 4) != 0) as u8
					} else {
						(pressed & 1) as u8
					}
				);
				assert_eq!(
					report.bytes[13],
					if deck {
						(((pressed & 1) != 0) as u8 * 4) | (((pressed & 2) != 0) as u8 * 2)
					} else {
						0
					}
				);
				report.buttons(0);
				assert_eq!(&report.bytes[8..16], &[0; 8]);
			}
		}
	}

	#[test]
	fn analog_and_dpad_state_survive_buttons_and_neutralization() {
		for model in [VirtualIdentity::SteamController, VirtualIdentity::SteamDeck] {
			let mut report = Report::new(model);
			report.axes(&update());
			report.buttons(0x1000 | 1 | 0xf0000);
			if report.deck() {
				assert_eq!(i16_at(&report.bytes, 48), -12345);
				assert_eq!(i16_at(&report.bytes, 54), -7654);
				assert_eq!(i16_at(&report.bytes, 44), (64 * 32767 / 255) as i16);
			} else {
				assert_eq!(report.bytes[11], 51);
				assert_eq!(report.bytes[12], 101);
				let stick = report.next(false);
				let pad = report.next(true);
				assert_eq!(i16_at(&stick, 16), -12345);
				assert_eq!(i16_at(&pad, 18), 24000);
				assert_eq!(stick[10] & 0x88, 0x80);
				assert_eq!(pad[10] & 0x88, 0x88);
				assert_ne!(stick[4..8], pad[4..8]);
			}
			report.neutralize();
			assert_eq!(&report.bytes[8..], &[0; 56]);
			report.buttons(0);
			assert_eq!(&report.next(false)[8..], &[0; 56]);
		}
	}

	#[test]
	fn deck_contacts_release_by_event_and_keep_the_two_pads_separate() {
		let mut report = Report::new(VirtualIdentity::SteamDeck);
		report.touch(&touch(0, 1));
		report.touch(&touch(1, 1));
		assert_eq!(report.bytes[10] & 0x18, 0x18);
		assert_eq!(i16_at(&report.bytes, 16), -16384);
		assert_eq!(i16_at(&report.bytes, 18), -16384);
		report.buttons(0xf0000 | 0x1000);
		assert_eq!(report.bytes[10] & 0x18, 0x18);
		report.touch(&touch(0, 2));
		assert_eq!(report.bytes[10] & 0x18, 0x10);
		assert_eq!(i16_at(&report.bytes, 56), 0);
		report.touch(&touch(1, 3)); // MOVE, not release, even at low pressure
		assert_eq!(report.bytes[10] & 0x18, 0x10);
		report.touch(&touch(0, 7));
		assert_eq!(report.bytes[10] & 0x18, 0);
		assert_eq!(&report.bytes[56..60], &[0; 4]);
		report.touch(&touch(2, 1));
		assert_eq!(report.bytes[10] & 0x18, 0);
		report.touch(&touch(0, 1));
		report.neutralize();
		assert_eq!(report.bytes[10], 0);
		let mut classic = Report::new(VirtualIdentity::SteamController);
		classic.touch(&touch(0, 1));
		assert_eq!(&classic.bytes[8..], &[0; 56], "no fabricated classic touch support");
	}

	#[test]
	fn motion_inverts_sdl_coordinates_units_and_clamps_extremes() {
		for model in [VirtualIdentity::SteamController, VirtualIdentity::SteamDeck] {
			let mut report = Report::new(model);
			let motion = GamepadMotion {
				index: 0,
				motion_type: JoypadMotionType::GYROSCOPE,
				x: 1000.0,
				y: 500.0,
				z: -250.0,
			};
			report.motion(&motion);
			let gyro = if report.deck() { 30 } else { 34 };
			assert_eq!(i16_at(&report.bytes, gyro), 16384);
			assert_eq!(
				i16_at(&report.bytes, gyro + 2),
				if report.deck() { 4096 } else { -4096 }
			);
			assert_eq!(i16_at(&report.bytes, gyro + 4), 8192);
			report.motion(&GamepadMotion {
				motion_type: JoypadMotionType::ACCELERATION,
				x: 0.0,
				y: 9.80665,
				z: 0.0,
				..motion
			});
			let accel = if report.deck() { 24 } else { 28 };
			assert_eq!(i16_at(&report.bytes, accel + 4), 16384);
			let orientation = report.bytes[accel..accel + 6].to_vec();
			report.neutralize();
			assert_eq!(&report.bytes[gyro..gyro + 6], &[0; 6]);
			assert_eq!(&report.bytes[accel..accel + 6], orientation);
			report.motion(&GamepadMotion {
				x: f32::MAX,
				y: f32::MIN,
				..motion
			});
			assert_eq!(i16_at(&report.bytes, gyro), 32767);
			assert_eq!(i16_at(&report.bytes, gyro + 4), -32768);
		}
	}

	#[test]
	fn feature_commands_have_native_serial_settings_attributes_and_bounded_feedback() {
		let mut features = Features::new(VirtualIdentity::SteamDeck, 15);
		assert_eq!(features.set(&[0, 0x83]), Ok(None));
		assert_eq!(&features.reply[..4], &[0, 0x83, 10, 1]);
		assert_eq!(&features.reply[4..8], &0x1205u32.to_le_bytes());
		assert_eq!(&features.reply[9..13], &4000u32.to_le_bytes());
		assert_eq!(report_period_ms(VirtualIdentity::SteamController), 10);
		assert_eq!(features.set(&[0, 0xae, 1, 1]), Ok(None));
		assert_eq!(&features.reply[4..15], b"PYROVALVE0F");
		assert_eq!(features.set(&[0, 0x87, 3, 48, 0x34, 0x12]), Ok(None));
		assert_eq!(features.set(&[0, 0x89, 3, 48, 0, 0]), Ok(None));
		assert_eq!(&features.reply[3..6], &[48, 0x34, 0x12]);
		assert_eq!(
			features.set(&[0, 0xeb, 9, 0, 0, 0, 0x34, 0x12, 0x78, 0x56, 2, 0]),
			Ok(Some((0x1234, 0x5678)))
		);
		assert!(features.set(&[0, 0xeb, 255]).is_err());
		assert!(
			features.set(&[0, 0xaa, 0]).is_err(),
			"unsupported calibration must fail"
		);
		assert!(
			features.set(&[0, 0x8f, 0]).is_err(),
			"no invented pulse waveform translation"
		);
		assert!(features.set(&[]).is_err());
	}

	async fn receive_kind(socket: &tokio::net::UnixDatagram, wanted: u32) -> Vec<u8> {
		tokio::time::timeout(std::time::Duration::from_secs(1), async {
			loop {
				let mut bytes = vec![0; EVENT_SIZE];
				let n = socket.recv(&mut bytes).await.unwrap();
				bytes.truncate(n);
				if u32::from_ne_bytes(bytes[..4].try_into().unwrap()) == wanted {
					return bytes;
				}
			}
		})
		.await
		.unwrap()
	}

	#[tokio::test]
	async fn uhid_lifecycle_feature_replies_feedback_revocation_and_destroy_without_hardware() {
		tokio::task::LocalSet::new()
			.run_until(async {
				let (device, host) = std::os::unix::net::UnixDatagram::pair().unwrap();
				device.set_nonblocking(true).unwrap();
				host.set_nonblocking(true).unwrap();
				let host = tokio::net::UnixDatagram::from_std(host).unwrap();
				let file = std::fs::File::from(OwnedFd::from(device));
				let route = FeedbackRoute::new(0, true);
				let (owner, mut feedback) = tokio::sync::mpsc::channel(10);
				route.claim(&owner);
				let pad = ValveGamepad::from_file(VirtualIdentity::SteamDeck, 0, route.clone(), file).unwrap();
				let create = receive_kind(&host, UHID_CREATE2).await;
				assert_eq!(&create[284..], DESCRIPTOR);
				let command = [0, 0xeb, 9, 0, 0, 0, 0x34, 0x12, 0x78, 0x56, 2, 0];
				let mut payload = vec![0; 8 + command.len()];
				payload[..4].copy_from_slice(&42u32.to_ne_bytes());
				payload[6..8].copy_from_slice(&(command.len() as u16).to_ne_bytes());
				payload[8..].copy_from_slice(&command);
				host.send(&event(UHID_SET_REPORT, &payload)).await.unwrap();
				let ack = receive_kind(&host, UHID_SET_REPORT_REPLY).await;
				assert_eq!(&ack[4..], &[42, 0, 0, 0, 0, 0]);
				assert_eq!(
					feedback.recv().await,
					Some(FeedbackCommand::Rumble(RumbleCommand {
						id: 0,
						low_frequency: 0x1234,
						high_frequency: 0x5678
					}))
				);
				route.revoke();
				host.send(&event(UHID_SET_REPORT, &payload)).await.unwrap();
				receive_kind(&host, UHID_SET_REPORT_REPLY).await;
				assert!(feedback.try_recv().is_err());
				host.send(&event(UHID_GET_REPORT, &[43, 0, 0, 0, 0, 0])).await.unwrap();
				let reply = receive_kind(&host, UHID_GET_REPORT_REPLY).await;
				assert_eq!(&reply[4..12], &[43, 0, 0, 0, 0, 0, 65, 0]);
				pad.set_pressed(0xf0000);
				receive_kind(&host, UHID_INPUT2).await;
				pad.neutralize();
				let neutral = receive_kind(&host, UHID_INPUT2).await;
				assert_eq!(&neutral[14..], &[0; 56]);
				drop(pad);
				receive_kind(&host, 1).await;
				tokio::task::yield_now().await;
			})
			.await;
	}
}
