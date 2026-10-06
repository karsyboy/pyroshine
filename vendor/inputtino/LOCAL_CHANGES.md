# Local inputtino backend patch

Base: https://github.com/games-on-whales/inputtino at
`d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8` (also upstream HEAD when checked
2026-09-30). The native sources, build files, Rust bindings and upstream tests
are copied with their MIT license. This directory is a maintained local patch,
not a replacement upstream or an unpinned fork.

The public Rust `inputtino` dependency remains the upstream Git crate at that
revision. The workspace patches only `inputtino-sys` to this source tree.
Cargo.lock changes only the source of `inputtino-sys`; no dependency revision
was upgraded. Nix includes this directory in its source and no longer needs to
graft another copy of the upstream native library into the Cargo vendor tree.

Changes from upstream:

- `src/uhid/include/uhid/ps5.hpp`: name Edge bits 4–7 of `buttons[2]`.
- `src/uhid/include/uhid/dualsense_edge.hpp`: identity check and explicit,
  allocation-free mapping of the four existing paddle flags.
- `src/uhid/include/uhid/protected_types.hpp`: retain the selected Edge subtype.
- `src/uhid/joypad_ps5.cpp`: enable Edge bits only for Sony 054c:0df2;
  ordinary DualSense still ignores paddle flags. The existing complete-state
  reset clears released Edge buttons. No new input or feedback pipeline.
- `bindings/rust/inputtino-sys/build.rs`: watch native sources for local rebuilds.
- `CMakeLists.txt`, `tests/dualsense_edge.cpp`: optional hardware-independent
  tests of every combination, release, ordinary-button isolation and report layout.

The existing Bluetooth UHID descriptor, 63-byte common input report, headers,
CRC, feature replies and output handlers are retained. Edge uses the same common
input structure as standard DualSense in Linux and SDL, with the previously
unused bits in `buttons[2]` now populated. USB byte 10 / Bluetooth byte 11 contains
that byte. This backend continues exposing a virtual Bluetooth device regardless
of the physical client's connection transport.

Sources:

- [SDL2 canonical mapping](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/SDL_gamecontroller.c)
- [SDL2 PS5 report parser](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/hidapi/SDL_hidapi_ps5.c)
- [Packaged SDL3 mapping](https://github.com/libsdl-org/SDL/blob/ddba673cddfa79ce798eca5ba402e0f1a2a2a243/src/joystick/SDL_gamepad.c)
- [Linux hid-playstation](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-playstation.c)

When updating inputtino, compare these specific changes against upstream first.
If upstream implements the same mapping, remove this patch and update the pinned
Git dependency and Nix packaging together. Preserve `LICENSE` when redistributing.

## Xbox Elite uinput extension

Upstream HEAD remains `d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8` when
checked for this implementation. Upstream has neither Elite paddle emission nor
a Valve backend; no dependency revision was upgraded.

- `src/uinput/include/inputtino/xbox_elite.hpp`: native USB identity guard,
  canonical SDL paddle to Linux grip mapping, shared press/release iterator.
- `src/uinput/joypad_xbox.cpp`: advertise grip capabilities only for Microsoft
  `045e:02e3` / `045e:0b00`; emit changed grip states using the same complete
  button mask and SYN_REPORT as standard controls. Generic Xbox ignores paddles.
- `src/uinput/include/inputtino/protected_types.hpp`: retain the native Elite
  identity in device state. The public Rust/C interfaces are unchanged.
- `tests/xbox_elite.cpp`, `CMakeLists.txt`: opt-in `INPUTTINO_ELITE_TESTS`
  checks actual libevdev capabilities without creating uinput, all 256 paddle
  transitions, ordinary-button isolation and generic-device suppression.

[Linux xpad](https://github.com/torvalds/linux/blob/master/drivers/input/joystick/xpad.c)
uses BTN_GRIPR/BTN_GRIPL/BTN_GRIPR2/BTN_GRIPL2 for Elite paddles.
Older build headers receive the stable Linux UAPI code values, not substitutes.
This reproduces the driver's evdev device, not GIP packets or a hidraw node.
Steam recognition must be checked with the physical acceptance procedure in
[native controllers](../../docs/NATIVE_CONTROLLERS.md); no proprietary Steam
implementation was available to establish that result from source inspection.

Valve support lives in Pyroshine's `gamepad/valve.rs` beside the host input
adapter. It uses Linux UHID and the existing gamepad Tokio runtime. This avoids
changing the pinned public Inputtino crate or adding C/Rust ABI extensions.
Keep both local patches in comparisons when updating Inputtino.

## DualSense UHID report bounds

Review 2026-10-05 BUG-001: an unsupported Bluetooth feature query used to take
the success path's CRC step with an empty payload (`size - 4` underflow), so
the CRC read past the reply buffer.

- `src/uhid/joypad_ps5.cpp`: `feature_report` builds only valid replies:
  feature report type, a supported number (calibration 0x05, pairing 0x09,
  firmware 0x20), payload within `UHID_DATA_MAX`, and a Bluetooth CRC only
  for a payload longer than its 4-byte trailer. Every other `UHID_GET_REPORT`
  gets `err = EIO` with no payload (previously `-EINVAL` truncated to the u16
  field; the kernel reports any nonzero `err` as EIO). Non-feature report types
  for a supported number are now errors as well.
- `output_report` parses a `UHID_OUTPUT` event only when it contains the whole
  47-byte common block after the USB, SDL Bluetooth or kernel Bluetooth (tag
  byte) header: at least 48, 49 or 50 bytes. Shorter reports and unknown report
  ids are ignored instead of reading stale event bytes. Supported replies and
  full-size output reports are unchanged.

Checked with the virtual device through `/dev/uhid`: the kernel's
`hid-playstation` driver registers both DualSense (054c:0ce6) and Edge
(054c:0df2), and `HIDIOCGFEATURE` returns 41/20/64 bytes for 0x05/0x09/0x20 and
EIO for other numbers. SDL and Steam on physical clients remain a release check.

## Native thread ownership

Review 2026-10-05 STAB-005: upstream detaches its worker threads and shares
plain fields with them. The DualSense report thread and UHID event thread, and
the uinput force-feedback listeners (Xbox/Elite, Switch, non-UHID PlayStation),
could run after their joypad's destructor, race with setters and callback
installation, and read a closed (possibly reused) UHID descriptor.

- `src/uhid/include/uhid/uhid.hpp`: the event thread is owned, polls an eventfd
  beside the UHID fd and exits at once on stop; `~Device` joins it before
  writing `UHID_DESTROY` and closing the fd. Stop flags are atomic.
- `src/uhid/joypad_ps5.cpp`, `src/uhid/include/uhid/protected_types.hpp`,
  `include/inputtino/input.hpp`: a mutex guards the input report, touch id,
  trigger caches and callbacks. `send_report` copies the report under it and
  writes without it; callbacks are copied under it and invoked without it. The
  report thread is owned and joined (the move constructor now moves it);
  `is_bluetooth` is set before the event thread starts.
- `src/uinput/joypad_utils.hpp`, `src/uinput/include/inputtino/protected_types.hpp`,
  `src/uinput/joypad_{xbox,nintendo,ps}.cpp`: `start_event_listener` /
  `stop_event_listener` own the listener with an eventfd wake (including the
  initial 100 ms settle wait); destructors join it before the uinput device can
  be released. `on_rumble` is guarded the same way.

Destruction therefore returns only after every worker and callback of the
joypad has finished; no callback runs afterwards and the kernel device is gone.
Pyroshine's callbacks never block (see `control/input/ownership.rs`), so these
joins cannot wait on its feedback queue. The public C/C++ and Rust APIs are
unchanged.

## Native regression tests

- `tests/ps5_feature_reports.cpp`, `CMakeLists.txt`: opt-in
  `INPUTTINO_PS5_FEATURE_TESTS` compiles the DualSense UHID handler's
  translation unit and reads its replies from a pipe, without `/dev/uhid`.
  `supported` checks byte-exact USB and Bluetooth calibration, pairing and
  firmware replies, including golden CRC trailers; `unsupported` every other
  report number and type; `output` complete and truncated USB/Bluetooth output
  reports; `fuzz` seeded random requests and output reports.
- `tests/joypad_lifetimes.cpp`, `CMakeLists.txt`: opt-in `INPUTTINO_DEVICE_TESTS`
  creates DualSense (UHID), Xbox, Elite and Switch (uinput) devices, runs
  setters and callback replacement on two threads while kernel feedback arrives,
  destroys them and requires prompt destruction, no callback afterwards, no
  surviving kernel node and the thread/fd counts back at baseline. It needs
  `/dev/uhid` and `/dev/uinput` access (exit 77 otherwise). The pre-fix source
  produced TSan data races in `send_report`, setters, `on_uhid_event` and the
  destructor, and an Xbox device that outlived its destructor.
- `scripts/known_defects.py` in the Pyroshine root builds the hardware-independent
  tests with AddressSanitizer/UBSan and runs them with CTest in CI;
  `--native-devices` adds the device test under ThreadSanitizer on a host with
  device access.
