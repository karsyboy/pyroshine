# Scene capture, cursor lifetime, and Steam input

Pyroshine captures the visible compositor scene independently of the selected
keyboard, pointer, and Steam controller input targets. Virtual controller
emulation selects the device family; Steam Input routing and compositor focus
determine how applications receive that input.

## Ownership and frame handoff

The embedded Smithay compositor owns the visible scene and input focus, not
codec negotiation or packet transport. `frame.rs` carries the exported DMA-BUF,
color/HDR metadata and consumption signal to the video pipeline. Capture demand
is separately generation-tagged; see [capture admission](PIPELINE_OPTIMIZATION.md#capture-admission-and-presentation).
Service buffer releases and input even when capture is blocked or the scene is
clean. [Architecture](ARCHITECTURE.md) explains session and stream ownership.

The implementation is under `moonshine-core/src/session/`: event loop, state
and protocol handlers in `compositor/{mod,state,handlers}.rs`, capture and
eligibility in `compositor/{capture,admission,frame}.rs`, cursor in
`compositor/cursor.rs`, focus and Steam classification in
`compositor/{focus,x11_focus}.rs`, input injection in `compositor/input.rs`
(XTest emulation in `compositor/emulated_input.rs`), swapchain and color
protocols in `compositor/{gamescope_swapchain,wsi_bindings,color_management}.rs`,
and controller emulation in `stream/control/input/{gamepad,mod}.rs`.

## Configuration

Both settings are optional and default to automatic behavior (see the [configuration reference](CONFIGURATION.md)):

```toml
[compositor]
capture_mode = "auto" # auto | composited

[stream.control.gamepad]
emulation = "auto" # auto | xbox | playstation | nintendo
```

`auto` capture directly exports a fullscreen DMA-BUF only when it represents the
whole visible output. `composited` uses the existing GLES output DMA-BUF pool for
compatibility diagnosis. There is no forced direct mode.

Automatic gamepad emulation preserves Xbox, PlayStation/PS5, and Nintendo/Switch
families. Steam/Valve (`LI_CTYPE_STEAM = 0x04`), unknown, and future kinds use the
Xbox compatibility target. Forced policies choose the requested virtual family.
The existing motion, touch, battery, feedback, hotplug, and active-mask paths
remain in place; advanced features depend on the virtual family selected.

## Cursor source of truth

There is no inactivity timer. An active visible cursor remains visible until
client cursor state changes. Mouse/pen use can activate the initial fallback;
controller events do not activate it and cannot change application cursor intent.
Pointer activity cannot undo an explicit `Hidden` request.

### Emulated pointer input (XTest)

Steam Input turns a controller into mouse motion, clicks and keys with XTest
requests on the session's X display. A rootless XWayland built with libei sends
XTest to the EIS server named by `LIBEI_SOCKET` and otherwise moves only its own
pointer sprite, behind the compositor's back (the next compositor pointer event
snaps it back). The compositor listens on `$XDG_RUNTIME_DIR/<wayland-display>-ei`
(`compositor/emulated_input.rs`, Smithay's libei backend) and passes the socket
to XWayland only, as gamescope does. Its seat offers a keyboard, a relative
pointer and an absolute pointer spanning all X11 root coordinates: XWayland
emulates nothing until every capability it binds has a device.

Emulated events take the Moonlight input paths in `compositor/input.rs`: relative
motion honors an active pointer lock, coordinates are scene coordinates (XWayland
is unscaled), and pointer events activate the cursor exactly like mouse use.
Emulated keyboard events reach the seat keyboard without touching the cursor.
Controller input that Steam does not convert never reaches this path.

The pinned Smithay revision is
[`0ff00983b6007257a7a161a4fe8b14a778e2ac8f`](https://github.com/Smithay/smithay/tree/0ff00983b6007257a7a161a4fe8b14a778e2ac8f).
Its [`wl_pointer.set_cursor` handler](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/wayland/seat/pointer.rs#L421)
reports a custom `Surface` or `Hidden` for an accepted client request. Its
[pointer replacement](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/input/pointer/mod.rs#L141)
and [leave handling](https://github.com/Smithay/smithay/blob/0ff00983b6007257a7a161a4fe8b14a778e2ac8f/src/input/pointer/mod.rs#L823)
also report `default_named()` as a framework fallback. Pyroshine does not
advertise `wp_cursor_shape_manager_v1`; a default named callback is therefore
not sufficient evidence to activate an untouched startup cursor. Other images
activate cursor state, while fallback resets retain existing activation.

A destroyed custom cursor falls back to the default image only for an already
active cursor. Destroying an older, replaced surface cannot replace the current
image. Image/hotspot requests, cursor commits, movement, and destruction mark the
scene dirty. Existing output damage tracking and static-screen keepalives remain.
If cursor-shape protocol support is added later, explicit named requests must be
distinguished from Smithay's fallback callbacks.

## Scene and input decisions

`can_direct_scanout_scene()` consumes classified special-window state and cursor
visibility, then checks the actual top visible source. Cursor, Steam overlays,
notifications, external overlays, dropdowns, decorations, scaling, popup trees,
and independently visible subsurfaces require composition. An opaque fullscreen
source can cover mapped background windows without making them independently
visible. Buffer dimensions, lifetime, crop, scale, rotation, position, and the
existing DMA-BUF checks still apply. Eligibility performs no X11 queries and
allocates no window list. Current color management passes through pixel data and
metadata; no new color conversion or SDR intermediate is introduced.

### Late composition (cursor and Steam notifications)

Two scene elements can be composited after capture instead of rendering the
whole frame through GLES: a Steam notification and, above it, the cursor. The
active consumer declares which layer kinds it draws
(`CaptureReceiver::set_overlay_caps`: CPU texels and/or client DMA-BUFs; the
conventional codecs' packed converter and PyroWave's 1:1 scaler since C API 0.9
draw both). When the scene *without* those elements is directly exportable, the
compositor exports the game's DMA-BUF with `ExportedFrame::overlays`, bottom to
top, and the encoder blends each layer the way GLES would: texels premultiplied,
scaled by the element alpha (the window's `_NET_WM_WINDOW_OPACITY` for a
notification, one for the cursor), source-over in the frame's own encoding,
before color conversion. The direct-export source lookup skips a notification
it composites as a layer.

* Cursor: drawn at the GLES position (named cursors at the pointer, client
  cursors at pointer minus hotspot plus surface offset); the default xcursor or
  a client `wl_shm` ARGB/XRGB8888 surface. Texels are copied only when the image
  changes (surface + commit counter).
* Notification: drawn at the window's position; a single-plane 8-bit
  ARGB/ABGR/XRGB/XBGR DMA-BUF (XWayland normally delivers these) is read in
  place and held with the game buffer until the encoder's reads finished; a
  `wl_shm` buffer is copied like a cursor. X formats ignore the alpha byte.

Every layer must be 1:1: one surface without subsurface content, buffer scale
1, normal transform, no viewport crop or scale. Anything else (scaled output,
fractional scale, interactive Steam overlays, dropdowns, decorations, cursor
DMA-BUFs, multi-plane formats, extents the packed converter cannot represent)
keeps GLES composition; counters report `direct_reject_cursor` or
`direct_reject_notification`. A frame whose layers a consumer can no longer draw
(a racing epoch switch) is dropped, never encoded without them. PyroWave blends
layers in its scaler's unscaled input fetch; that specialization may round
differently from the plain scaler at the level of relaxed-precision arithmetic
(62-71 dB PSNR in the fork's overlay test), while full-opacity binary-alpha
layers decode identically to pre-composited input under the same variant.
`late_cursor_frames`/`late_notification_frames` in `Video capture resources`
and `late_layer_frames` in `Video conversion summary` count them.

### Buffer readiness

A surface commit whose new DMA-BUF still has pending GPU writes is applied
only when those implicit fences signal (a Smithay pre-commit blocker), so the
compositor latches the newest completed frame and delivers that commit's frame
callbacks afterwards. Latching earlier would hand captures buffers still
queued behind a GPU-bound game, making every consumer (GLES composition or
encoder import) wait on them. `MOONSHINE_DISABLE_READY_LATCH=1` restores
immediate latching for diagnosis.

### WSI and transparency

Both swapchain protocols convey Vulkan composite-alpha intent. The
compositor honors `VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR` for the declared root
image in direct eligibility and GLES blending, including alpha-capable HDR
buffers. Child surfaces retain their own transparency. See the
[Vulkan definition](https://github.khronos.org/Vulkan-Site/spec/latest/chapters/VK_KHR_surface/wsi.html).

### WSI presentation bindings

`override_window_content` binds a swapchain `wl_surface` to the X11 window it
presents (`compositor/wsi_bindings.rs`), like gamescope's per-window override
surface. Each window keeps its own binding, so simultaneously running games never
replace each other's presentation; rendering, direct export, source size and
focus readiness ask which surface presents a given window. The reported window
(often a Wine/DXVK child) resolves to the rendered toplevel when it maps; a
resolved binding issues no further X11 queries.

A surface binds one window, and a newer surface for the same window replaces the
older binding. A binding is removed when its owning swapchain object is
destroyed (an older swapchain cannot remove a newer chain's binding of the same
surface), when its surface dies, or when its X11 window is destroyed (X11 ids are
reused). Unmap keeps it: Wine restores a minimized window by withdrawing and
remapping it. Every live binding receives its window's frame callbacks on
composited and idle refresh ticks, so a background game stays resumable; direct
export services only the exported surface, as for any window. Presentation
feedback reports a binding as displayed only when the frame composed it.

### Unsafe XWayland replacement cleanup

Wine presentation safety is checked before accepting any top-level bypass.
Unsafe windows present via the retained XCB surface without a replacement binding.
When safety changes, the WSI layer destroys its protocol override before requesting
swapchain recreation. The compositor clears only the owning swapchain's binding,
invalidates damage, and restores the XWayland scene even though the Vulkan/Wayland
surface itself may remain alive. Capture eligibility, focus classification, and
GPU export rules remain independent of this presentation decision. See
[presentation topology and fullscreen diagnosis](VULKAN_IMAGE_COUNTS.md#presentation-topology-and-fallback).

### Steam classification and input

Steam classification has one rule: `STEAM_OVERLAY != 0` is interactive when it
spans the virtual output width or requests `STEAM_INPUT_FOCUS`; otherwise it is a
passive notification. Property and geometry events update this classification.
There is no second fixed-pixel-width detector. Classified layers paint above the
game, its decorations, and dropdowns with their cached window opacity.

Passive notifications are excluded from primary focus, dropdown selection, and
pointer hit testing. Mapping, updating, or unmapping one changes the video scene
but does not reapply unchanged input targets. Compositor-owned root properties
are written only when their desired values change, including focusable lists.
An interactive overlay routes pointer input before WSI routing. Mode 2 keeps the
keyboard on the game; closing the overlay restores the game target and unified
focused-app contract. These match the inspected
[gamescope focus logic](https://github.com/ValveSoftware/gamescope/blob/master/src/steamcompmgr.cpp).

Steam closes its overlay by making it transparent; a transparent Steam surface
holds no role. A window that is unmapped or destroyed (with or without a prior
unmap) is retired by one idempotent path: it leaves the space, its metadata and
transient links, and every overlay, notification, external-overlay, dropdown,
decoration, input-focus and pointer-focus reference, then focus is re-evaluated
so input returns to the game.

When the overlay takes keyboard focus, Wine deactivates a fullscreen game, which
may minimize itself with `WM_CHANGE_STATE` and then waits for the window manager
to update `WM_STATE` before any further state change, including its restore. The
compositor acknowledges the iconic state without unmapping the window, and a
window becoming the focus is set back to normal, as gamescope does
(`handle_wm_change_state`, `DetermineAndApplyFocus`).

Controller Guide shortcuts are described in
[configuration](CONFIGURATION.md#streamcontrolgamepadhome_button). Prefer physical
Guide or the explicit Back+Start policy so games receive real Select holds.
Activation rumble deadlines preserve every held button; disconnect drops all
per-controller shortcut state.

DEBUG logs report classification, cursor, virtual-device creation, WSI binding
registration/resolution/release, X11 window retirement, iconic acknowledgements,
EIS connections, and capture path transitions (`direct`, `direct_override`,
`composited`); rendering itself does not emit per-frame INFO messages. Window,
focus and overlay transitions use the `focus` target and need it enabled
(for example `MOONSHINE_LOG=moonshine_core=debug,focus=debug`).

## Color and output changes

Direct and composited paths must preserve the source format and color metadata.
`sRGB`, BT.2020/PQ and scRGB linear frames are distinct: scRGB requires gamut/PQ
conversion in the encoder, while PQ input is already transfer-encoded. Do not
replace them with a generic HDR boolean or insert an SDR intermediate.
Output-mode changes retire pools until their buffers are consumed; coordinate
resolution/refresh/HDR changes with the session epoch rather than resizing only
the scene. See [PyroWave ownership](PYROWAVE.md#gpu-path-and-ownership) and
[reconnect validation](reconnect-validation.md).

## Validation and runtime checks

### Automated tests

Unit tests cover cursor activation/idle/hide, real Wayland resource replacement
and destruction, scene extras and automatic restoration, capture configuration,
cropping/scaling/rotation, Steam classification transitions, notification focus
and dropdown exclusion, unchanged focus-contract suppression, and controller
kind/policy parsing. `wsi_bindings` tests cover two simultaneous games,
swapchain recreation and old-owner release, deterministic fallback, late window
resolution and window destruction; `focus` tests cover the overlay open/close
cycle (modes 2, 1 and 0, opacity 0), input routing back to the game, and role
cleanup for an overlay destroyed without being hidden or unmapped. They do not
prove Steam or game behavior on hardware.

The ignored `xtest_motion_moves_the_compositor_pointer` test starts the
compositor and XWayland on a GPU host (no session or systemd units), sends XTest
motion from an X11 client and checks that the next compositor pointer event
continues from that position:

```sh
cargo test -p moonshine-core --all-features --lib -- --ignored xtest_motion_moves_the_compositor_pointer
```

### Hardware acceptance

These checks need a GPU host with `/dev/dri`, a client and Steam. Run the server
with DEBUG logging (`MOONSHINE_LOG=moonshine_core=debug,focus=debug`).

1. **Direct export.** Run a fullscreen workload with an explicitly hidden cursor
   and no extra content. Confirm the capture path is `direct` or
   `direct_override`, and check GPU utilization and frame latency.
2. **Cursor.** Expose the cursor, leave the mouse idle for more than three
   seconds, and navigate a game (for example Grim Dawn) using only a controller.
   Visible cursor composition must persist; an explicit application hide must
   immediately restore direct eligibility.
3. **Notifications.** Hold controller input while a Steam notification appears,
   updates and disappears. The notification must be captured, game input must
   continue uninterrupted, and no focus-contract rewrites may occur.
4. **Overlay and menus.** Open and close the Steam overlay and small interactive
   menus. Confirm visibility, pointer/controller routing, the mode-2 keyboard
   split and immediate restoration. Repeat with Steam Input enabled and
   disabled to separate Steam routing from virtual-device delivery. Include a
   fullscreen Proton game that minimizes on focus loss (for example Grim Dawn):
   open and close the overlay at least ten times, in `capture_mode = "auto"` and
   `"composited"`. The game must return immediately; the log shows
   `WM_CHANGE_STATE iconic acknowledged` when it minimized.
5. **Simultaneous games.** Launch a Vulkan game, then a second one without
   closing the first, and switch between them through Steam at least ten times,
   then close the second, continue the first and relaunch the second. The
   selected game must always show its own picture, with no persistent black
   frame, and `direct_override` must return when the scene is eligible. Check
   `WSI binding registered`/`released` logs per game. Repeat without
   `MOONSHINE_WSI_DISABLE_BYPASS`.
6. **Controller cursor.** In a game with a visible cursor, move it continuously
   with the mouse, then with Steam Input (controller as mouse), alternating
   repeatedly. The cursor must not flash or jump, clicks must land at it, the
   log shows `EIS client connected for XTest input emulation`, and
   `$TMPDIR/moonshine/xwayland.log` must not contain `[xwayland ei] EI setup
   failed`. An application-hidden cursor must stay hidden and plain controller
   gameplay must not show one.
7. **Controllers.** Test arrival, duplicate arrival, update before arrival,
   active-mask removal and reconnect at indices 0 and 15, plus native
   PlayStation motion/touch/rumble and forced emulation policies.
8. **Formats.** Repeat with H.264, HEVC, AV1, PyroWave, HDR, YUV 4:4:4, output
   scaling and a static screen. HDR render format and color metadata must stay
   intact, with no CPU capture, extra frame queue or GPU-to-CPU transfer.
