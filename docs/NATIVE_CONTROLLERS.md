# Native controller streaming

Set `[stream.control.gamepad] emulation = "auto"` to preserve supported native
controller models reported by Pyrolight. Install matching Pyrolight and
moonlight-common-c changes with the host. The host needs its existing uinput,
UHID and hidraw permissions; see [installation](INSTALLATION.md).

Native Steam recognition and physical USB/Bluetooth behavior require acceptance
on a real host. Automated report tests establish mapping and lifetime contracts,
not Steam's recognition. Valve exposes a subset of firmware features, as below.

## Models and capability boundaries

| Physical model / client detection | Wire family / subtype | Host virtual identity | Extra controls | Motion | Touch | Feedback / limits |
| --- | --- | --- | --- | --- | --- | --- |
| Generic Xbox / SDL Xbox family | Xbox / none | uinput `045e:02dd` | None | No | No | Rumble; trigger rumble has no host backend |
| Elite Series 1 / Microsoft `02e3` | Xbox / Elite | uinput `045e:02e3` | Four grip keys | No | No | Rumble; depends on SDL firmware/profile exposing paddles |
| Elite Series 2 / Microsoft `0b00`, `0b05`, `0b22` | Xbox / Elite + Series 2 | uinput `045e:0b00` | Four grip keys | No | No | Rumble; Steam Elite recognition unverified |
| Classic Steam Controller / Valve `1101`, `1102`, `1105`, `1106`, `1142` | Steam / classic | USB UHID `28de:1102` | Two rear grips | Gyro/accel when SDL exposes them | No physical contact API in audited SDL builds | Classic SDL rumble is unsupported; pulse haptics unsupported |
| Steam Deck built-in / Valve `1205` | Steam / Deck | USB UHID `28de:1205` | Four rear buttons; QAM | Gyro/accel when SDL exposes them | Two pads with audited SDL3 + sdl2-compat; none with native SDL2 | Native simple rumble forwarded; pulse waveforms unsupported; no controller battery report |
| DualSense / SDL PlayStation family | PS / none | Bluetooth UHID `054c:0ce6` | Existing ordinary controls | Existing backend | Existing backend | Existing rumble, LED, adaptive triggers, battery |
| DualSense Edge / Sony `054c:0df2` | PS / Edge | Bluetooth UHID `054c:0df2` | Four native Edge controls | Existing backend | Existing backend | Existing rumble, LED, adaptive triggers, battery |
| Nintendo / SDL Nintendo family | Nintendo / none | uinput `057e:2009` | Existing ordinary controls | No host motion backend | No | Existing rumble |

All rows still require physical regression checks. SDL sensors, battery,
rumble and touch capabilities are queried per opened device, never assumed from
its model. A mapping lacking paddles cannot be repaired by host identity metadata.
Elite onboard profiles can suppress independent paddle events in SDL; use an
unmapped hardware profile when testing. Series 1 has no Bluetooth transport.
The GIP keyboard PID `0b02` is deliberately excluded.

Explicit `xbox` always selects generic Xbox, including for Elite/Valve/Edge.
Explicit `nintendo` selects Switch. Explicit `playstation` retains existing
DualSense behavior, including Edge when the incoming PS model explicitly says
Edge. Changing the emulation setting requires restarting the host.

## Client and protocol

Pyrolight's `GamepadIdentity::subtype()` identifies exact VID/PID pairs without
controller-name or paddle-count heuristics. Known subtypes override an absent
SDL family classification. Unknown Valve devices and newer Valve controller
models are not mislabeled as classic or Deck.

The common-c `Limelight.h` is the protocol source of truth. New values occupy
previously unused bits in the existing 16-bit arrival capability field:

| Capability | Value | Required family / meaning |
| --- | --- | --- |
| DualSense Edge | `0x0200` | PS, existing extension |
| Xbox Elite | `0x0400` | Xbox; without Series 2 modifier selects Series 1 |
| Classic Steam Controller | `0x0800` | Steam |
| Steam Deck | `0x1000` | Steam |
| Elite Series 2 modifier | `0x2000` | Xbox with Elite bit; alone selects no subtype |

The 16-byte arrival packet, 34-byte input packet, supported-button mask and
existing motion/touch/battery packets do not change. Four paddle bits remain
`0x00010000`, `0x00020000`, `0x00040000`, `0x00080000`.
Controller touchpad index is at payload byte 3, after reserved byte 2.

New client → old host: unknown subtype bits are ignored; ordinary controls use
that host's existing family policy (usually Xbox for Valve). Old client → new
host: no subtype means generic family behavior, including Xbox for Valve. The
host never guesses Elite, Edge or Deck from supported buttons. Contradictory
Valve subtype bits select the compatibility device. Unknown future bits do not
change recognized models. This is an optional fork extension; upstream clients
remain supported without gaining model preservation they do not advertise.

Pyrolight consumes SDL's normalized button map. SDL HIDAPI defaults for classic
Steam are enabled during streaming, respecting higher-priority environment
hints. MappingManager loads GameControllerDB and saved/user mappings; these can
omit independent controls. The AppImage pins SDL `release-3.4.18` and
sdl2-compat `release-2.32.74`. Other builds use their packaged SDL runtime.
Native SDL2's Steam/Deck drivers expose sensors and rear controls, but no
controller touchpad contacts. The pinned SDL3 Deck driver exposes two contacts
through sdl2-compat; classic Steam still exposes pad-derived D-pad/right axes,
not contact coordinates. Always inspect actual arrival capability flags.
Classic SDL battery state is generally unknown; native status messages can carry
a valid supplied percentage, but the virtual wired controller has no Linux
wireless-battery device. No voltage or charging data is synthesized.

## Host identities, mapping and lifetime

`VirtualIdentity` models Xbox, both Elite generations, DualSense, Edge, Switch,
classic Steam and Deck. It drives both creation and slot comparison. Different
models destroy the previous device before reusing the slot; identical models
reuse it. Supported-button/capability changes unrelated to hardware identity
update arrival metadata without unplugging it.

| SDL / Moonlight | Xbox Elite evdev | Classic Valve report | Deck Valve report |
| --- | --- | --- | --- |
| PADDLE1 | BTN_GRIPR, upper right | Right grip, byte 10 bit 0 | R4, byte 13 bit 2 |
| PADDLE2 | BTN_GRIPL, upper left | Left grip, byte 9 bit 7 | L4, byte 13 bit 1 |
| PADDLE3 | BTN_GRIPR2, lower right | Unused | R5, byte 10 bit 0 |
| PADDLE4 | BTN_GRIPL2, lower left | Unused | L5, byte 9 bit 7 |

Elite's Inputtino uinput extension reproduces the Linux xpad evdev identity and
capabilities. It retains standard axes, analog triggers, Guide and force
feedback. It creates no Xbox HID/GIP device. Steam's proprietary recognition
must be evaluated separately; a passing evdev test is not proof of Elite
recognition. See [local patch maintenance](../vendor/inputtino/LOCAL_CHANGES.md).

The shared Valve backend lives in `gamepad/valve.rs` in the host adapter,
outside Inputtino's unchanged public Rust API. It creates a vendor HID application
collection with native Valve USB input and feature reports. Classic uses state
message 1, Deck state message 9. This is actual report generation, not an Xbox
backend with Valve VID/PID substituted. It supplies a stable per-slot serial,
model/update-period attributes, settings read/write and native simple-rumble
command handling. Unsupported calibration, firmware, pulse or waveform requests
return an error; no physical calibration or quaternion is invented.

Input writes are nonblocking and stack-only. Feature requests and regular
reports (4 ms for Deck, 10 ms for classic, matching advertised reader intervals)
run in one local Tokio task on the existing gamepad runtime, without
native threads. The task aborts on teardown and UHID_DESTROY is sent synchronously
before slot reuse. Feedback uses the owner-switchable route; after revocation,
feedback cannot be sent to another peer until that peer claims the device.

Classic left axes multiplex stick and normalized D-pad state in native reports.
D-pad directions reconstruct directional pad positions, not physical trackpad
coordinates. The right normalized axis pair is represented in the native right
pad fields; original touch state, pressure and full physical pad coordinates
cannot be recovered from this client path. Do not interpret this as trackpad
parity. No client HID polling or SDL replacement is introduced.

Deck touch packets preserve pad index, coordinates and pressure. Contact lifetime
uses DOWN/MOVE/UP/CANCEL events; low pressure is not a release. CANCEL_ALL and
ownership neutralization clear both contacts. Right pad click can use the
existing TOUCHPAD flag when mapped. The SDL2 API cannot transport SDL3's separate
left pad click or stick capacitive-touch misc slots; these are unavailable.
Motion converts Moonlight degrees/s and m/s² back to the native signed IMU units
and coordinate frame. No orientation quaternion is derived from stale samples.

Neutralization releases all extended/ordinary buttons, triggers, sticks, contacts
and gyro rate. Acceleration retains the last gravity/orientation sample. Active
mask removal destroys the slot; stream ownership loss neutralizes it and clears
feedback routing while retaining the native device for reconnect. Session end
destroys all devices. Home shortcuts preserve extended bits through timers.

## Sources and dependency maintenance

The report and event contracts are grounded in authoritative implementations:

- [Linux xpad](https://github.com/torvalds/linux/blob/master/drivers/input/joystick/xpad.c): Elite identities, native grip capabilities and paddle order.
- [Linux hid-steam](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-steam.c): controller interface selection, state layouts, feature/serial commands and rumble.
- [SDL2 USB IDs](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/usb_ids.h) and [controller list](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/controller_list.h): transport identities.
- [SDL2 normalized mappings](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/SDL_gamecontroller.c), [Xbox HIDAPI](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/hidapi/SDL_hidapi_xboxone.c), [classic Steam HIDAPI](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/hidapi/SDL_hidapi_steam.c), [Deck HIDAPI](https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/hidapi/SDL_hidapi_steamdeck.c): input availability and profile limits.
- [Pinned SDL3 Deck driver](https://github.com/libsdl-org/SDL/blob/release-3.4.18/src/joystick/hidapi/SDL_hidapi_steamdeck.c) and [classic driver](https://github.com/libsdl-org/SDL/blob/release-3.4.18/src/joystick/hidapi/SDL_hidapi_steam.c): runtime-specific touch/feedback support.
- [Inputtino pinned source](https://github.com/games-on-whales/inputtino/tree/d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8): uinput/PS5 architecture; upstream HEAD still matches this pin when inspected.

Compare local Inputtino changes with upstream before updating; preserve the
workspace's patch of only inputtino-sys and the existing Nix source inclusion.
The Valve adapter has no new dependency or packaging permission requirement.

## Automated validation

Run the workspace checks in [CONTRIBUTING](../CONTRIBUTING.md#validation), including
locked build/test/clippy and fmt. Input tests include model/fallback policies,
wire decoding, same-slot replacement, owner transfer, active-mask removal,
reconnect, neutralization and Edge regressions. Native Valve tests check report
mapping, both touchpads, motion units, feature replies and teardown through a
Unix socket fixture using the same production event handler.

```sh
cargo test --locked -p moonshine-core session::stream::control::input
cmake -S vendor/inputtino -B /tmp/native-inputtino-tests \
  -DBUILD_TESTING=OFF -DINPUTTINO_EDGE_TESTS=ON -DINPUTTINO_ELITE_TESTS=ON
cmake --build /tmp/native-inputtino-tests
ctest --test-dir /tmp/native-inputtino-tests --output-on-failure
python3 scripts/check-controller-protocol.py /path/to/moonlight-common-c/src/Limelight.h
```

Client tests use SDL virtual devices for two/four paddle combinations, release
and ordinary-button isolation. Common-c tests verify capability values, arrival,
touch and input sizes/offsets, byte order, old masks and all four high button bits.
Run their CMake suites from Pyrolight as documented in its contributor guide.

## Physical and Steam acceptance

Record host/client revisions, kernel, SDL runtime, Steam version, model,
connection transport and active controller profile. Automated tests do not complete
this procedure. Require an Elite Series 2, classic Steam Controller and actual
Steam Deck client where available.

1. Install matching client/protocol/host builds and existing device rules. Use
   `emulation = "auto"`, enable host DEBUG creation logs and client SDL DEBUG
   arrival logs. Confirm exact VID/PID, family/subtype, supported paddle mask,
   sensor and touch capabilities. No per-input INFO logging is needed.
2. Inspect `/proc/bus/input/devices`, `udevadm info --attribute-walk
   --name=/dev/input/eventN`, and `evtest /dev/input/eventN`. Confirm Elite
   `045e:02e3` or `045e:0b00` and all four BTN_GRIP codes independently.
   Inspect Valve UHID under `/sys/bus/hid/devices/*:28DE:1102.*` or
   `*:28DE:1205.*`, its driver binding and hidraw permissions. UHID is not
   listed by `lsusb`. Confirm no stale or duplicate devices.
3. Open Steam's controller settings. Require the intended native Elite/classic
   Steam/Deck identity and independent controls in its layout editor. Bind every
   rear control to a different action; press individually, together and release.
   Check ordinary buttons, D-pad, Guide, sticks and both analog triggers with
   Steam Input enabled and disabled where meaningful. Evdev success alone is
   insufficient; record failures separately from backend mapping tests.
4. For Elite, verify all four rear buttons with an unmapped physical profile.
   Test main-motor rumble, USB and Series 2 Bluetooth/BLE where available.
   Check low/full analog trigger values and ordinary-button isolation.
5. For classic Steam, check both rear grips, pad-derived D-pad/right axes,
   stick and sensors. Full physical touch contacts and SDL rumble are unavailable
   in the audited path. Record native Steam feature/calibration failures rather
   than claiming full firmware emulation.
6. For Deck, check R4/L4/R5/L5, QAM, sticks, triggers and sensors. With pinned
   SDL3/sdl2-compat verify left/right contacts, independent movement, low-pressure
   contacts, release and cancellation in the host layout. With native SDL2 mark
   contacts unavailable. Test simple rumble separately from unsupported pulse
   haptic effects and unavailable left pad click/stick touch controls.
7. Hold all rear controls, disconnect the physical controller, reconnect, replace
   with a different model in the same slot, and restore it. Changed models must
   unplug/recreate; same identity arrivals must reuse. No rear button may stick.
8. Repeat while disconnecting/reconnecting the stream, transferring ownership,
   retaining a session, stopping it and switching between single/multiple
   controllers at indices 0 and 15. Feedback must go only to the current owner.
9. Repeat in explicit Xbox/PlayStation/Nintendo modes. Xbox must show generic
   `045e:02dd`; no native subtype may override this selection. Repeat baseline
   standard Xbox, DualSense, Edge and Nintendo checks, including Edge motion,
   touch, all four controls, adaptive triggers, LED, rumble and battery as in
   [Edge acceptance](DUALSENSE_EDGE.md#physical-and-steam-acceptance-procedure).
10. Exercise old-client/new-host and new-client/old-host combinations. Require
    ordinary controls and existing family fallback; record missing native features
    as expected version limitations. Build Nix and Windows packages in their
    supported environments before release.
