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
`compositor/{focus,x11_focus}.rs`, input injection in `compositor/input.rs`,
native color protocols and draws in
`compositor/{color_management,color_render}.rs`,
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

### Game-driven cursor warps and transient hides

Games with native controller support move the cursor themselves with
`SetCursorPos`, which Wine turns into `XWarpPointer`. Rootless XWayland reports a
warp to the compositor (as a pointer-lock position hint) only while the X cursor
is hidden, so Proton brackets each warp with `XFixesHideCursor` and
`XFixesShowCursor`. XWayland forwards the hide immediately but delays the re-show
until the next motion or 5 ms, so the compositor receives a hide/show pair for
every warp; an unmodified cursor would be missing from the frames captured in
between. Gamescope never shows these hides, because it reads the cursor image
through XFixes.

`CursorState` therefore keeps presenting the previous image for one output
refresh interval after a hide request. A hide that is reverted within that
interval is never presented; a hide that lasts longer is presented at the next
refresh, so an application hide still takes effect within one frame. The
request itself is recorded immediately: pointer use still cannot undo it.

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

Late layers currently require an SDR base and SDR layer declarations: the
encoder overlay APIs do not carry per-layer color descriptions. HDR scenes
with overlays use color-managed GLES composition; a clean HDR scene remains
directly exportable.

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
An interactive overlay routes pointer input before game-surface hit testing. Mode 2 keeps the
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
compositor acknowledges the iconic state without unmapping the window
(gamescope's `handle_wm_change_state`), and a window becoming the focus is set
back to normal (`DetermineAndApplyFocus`). Steam's overlay moves only keyboard
focus and leaves the focus window unchanged, so an acknowledged iconic window is
also set back to normal when it regains keyboard focus; Wine then restores the
game and it recreates its swapchain.

X keyboard focus is owned by the compositor. Its chosen keyboard window is
watched for `FocusOut` on the compositor's own X11 connection (event-driven, no
polling); when a client moves focus to another toplevel or drops it to `None`,
focus and `_NET_ACTIVE_WINDOW` are restored, as gamescope does. Focus moving to a
window inside the chosen one (a Wine or Steam CEF child) is kept. Steam's CEF
windows otherwise take focus right after the overlay closes, and because
Smithay's XWM publishes every focused window as `_NET_ACTIVE_WINDOW`, Wine would
deactivate the game again.

That Xlib connection installs a process-wide, non-fatal error handler when it
opens. Its requests target client windows that can disappear at any moment, and
X reports the resulting errors asynchronously; Xlib's default handler would exit
the server.

Steam hides overlays and notifications by clearing `STEAM_OVERLAY` and
`STEAM_INPUT_FOCUS` or by setting `_NET_WM_WINDOW_OPACITY` to zero. Smithay reports
the opacity change as its own property kind rather than as a raw atom; both
refresh the cached classification.

Controller Guide shortcuts are described in
[configuration](CONFIGURATION.md#streamcontrolgamepadhome_button). Prefer physical
Guide or the explicit Back+Start policy so games receive real Select holds.
Activation rumble deadlines preserve every held button; disconnect drops all
per-controller shortcut state.

DEBUG logs report classification, cursor, virtual-device creation, native color
transactions, X11 window retirement, iconic acknowledgements
and restores, and capture path transitions (`direct`,
`composited`); rendering itself does not emit per-frame INFO messages. Window,
focus and overlay transitions use the `focus` target and need it enabled
(for example `MOONSHINE_LOG=moonshine_core=debug,focus=debug`).

## Color and output changes

Direct export preserves the source format and color metadata. Composition
converts each surface in its existing texture draw to the primary scene
encoding; HDR composition exports BT.2020/PQ.
`sRGB`, BT.2020/PQ and scRGB linear frames are distinct: scRGB requires gamut/PQ
conversion in the encoder on direct export, while PQ input is already encoded. Do not
replace them with a generic HDR boolean or insert an SDR intermediate.
Output-mode changes retire pools until their buffers are consumed; coordinate
resolution/refresh/HDR changes with the session epoch rather than resizing only
the scene. See [native HDR](NATIVE_PRESENTATION.md), [PyroWave ownership](PYROWAVE.md#gpu-path-and-ownership) and
[reconnect validation](reconnect-validation.md).

## Foreground application reporting

`focused_window` is the primary application selected by
`pick_primary_focus_and_override` and installed by `apply_focus`. Reporting uses
that window, independently of `input_focus_window`, `pointer_focus_window`,
overlay, notification and dropdown/override roles. The existing classification,
Steam focus contract, candidate filtering and ranking remain authoritative.
XDG popups are tracked separately from primary toplevel windows.

`foreground.rs` resolves nonempty, trimmed display metadata. XWayland uses
Smithay's cached title (`_NET_WM_NAME`, then `WM_NAME`), then window class and
instance. Native Wayland uses XDG role attributes: title, then app ID. No Steam
manifest, process scan or numeric focus ID is used for reporting. Applications
may expose captions or technical classes/app IDs rather than a friendly game
name; absent metadata produces `None` and the UI falls back to the launch entry.

Focus selection and no-candidate paths publish through a session-scoped Tokio
watch sender, suppressing identical values. XWM `Title`/`Class` notifications
and XDG `title_changed` refresh only the selected window, without dirtying
rendering or recalculating focus. Existing XDG `app_id_changed` handling also
refreshes the fallback name. Window retirement/destruction reuses normal focus
selection. A client disconnect does not clear the channel; only compositor
focus/lifetime events change its contents.

## Validation and runtime checks

### Automated tests

Foreground tests cover metadata fallbacks, deduplicated publication, missing
metadata, session lifetime/reconnect isolation, DTO compatibility, private
D-Bus `GetSession`/`SessionChanged` updates without lifecycle changes, dashboard
rendering and tray fallback. The ignored
`foreground_follows_primary_focus_titles_and_window_lifetime` XWayland test
checks game/launcher transitions, title updates, notification/overlay input
separation and destruction. Run it with the other `xwayland_tests` below on a
GPU host; it is not exercised by ordinary unit tests.

Unit tests cover cursor activation/idle/hide, real Wayland resource replacement
and destruction, scene extras and automatic restoration, capture configuration,
cropping/scaling/rotation, Steam classification transitions, notification focus
and dropdown exclusion, unchanged focus-contract suppression, and controller
kind/policy parsing. Native color tests cover transactional surface declarations,
PQ/scRGB distinction, metadata isolation, SDR transitions and cleanup.
`focus` tests cover the overlay open/close
cycle (modes 2, 1 and 0, opacity 0), input routing back to the game, and role
cleanup for an overlay destroyed without being hidden or unmapped; `cursor` tests
cover the hide hold (reverted hides never presented, sustained hides presented
after one frame, no exposure of an inactive or destroyed cursor). They do not
prove Steam or game behavior on hardware.

The ignored `xwayland_tests` start the compositor and XWayland on a GPU host (no
session or systemd units). The `game_restored_when_steam_*` tests replay the
Steam overlay sequence observed with Grim Dawn (overlay takes keyboard focus,
game requests iconic, overlay hidden by property or opacity) and require the game
to return to `NormalState`; `keyboard_focus_taken_by_steam_is_reclaimed` moves X
focus to a Steam window and to `None` and requires the game to get it back, and
`x11_errors_on_destroyed_focus_windows_are_not_fatal` requires asynchronous X
errors from vanished windows to leave the process running:

```sh
cargo test -p moonshine-core --all-features --lib -- --ignored xwayland_tests
```

### Hardware acceptance

For foreground reporting, run these checks with the desktop app open. They
require a GPU host, Moonlight, Steam and a game; automated metadata tests do not
establish real game behavior:

1. Launch the **Steam** Moonlight entry. Confirm the dashboard/tray report Steam
   and Session details retain its Moonlight name and application ID.
2. Launch a Proton/XWayland game such as Grim Dawn. Confirm the prominent name
   changes to the game while the Moonlight name/ID remain unchanged. Exit the
   game and confirm the name returns to Steam.
3. While the game runs, trigger a Steam notification, open/close the Steam
   overlay, and exercise dropdowns/tooltips. Confirm the reported name follows
   primary focus and stays on the game while only overlay/input roles change.
4. Launch a native Wayland application inside the session. Confirm its XDG
   title (or app ID fallback) is shown, including a live title change. Close it
   and confirm the next primary application is reported. Also test an XWayland
   caption change and a window without a title when practical.
5. Disconnect Moonlight without ending the session. Confirm the foreground name
   remains available; reconnect and confirm title/focus updates continue. End
   the session and confirm its foreground details disappear. Start another
   session and confirm no previous game's name carries over.

These checks need a GPU host with `/dev/dri`, a client and Steam. Run the server
with DEBUG logging (`MOONSHINE_LOG=moonshine_core=debug,focus=debug`).

1. **Direct export.** Run a fullscreen workload with an explicitly hidden cursor
   and no extra content. Confirm the capture path is `direct` or
   `direct+late`, and check GPU utilization and frame latency.
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
   frame, and `direct` must return when the scene is eligible. Each game
   must retain its actual compositor-visible surface.
6. **Controller cursor.** In a game whose own controller support moves a
   visible cursor (for example Grim Dawn under Proton), move it continuously
   with the controller, then with the mouse, alternating repeatedly. The cursor
   must not flash or jump and clicks must land at it, although DEBUG logs still
   show rapid `Cursor state changed` hide/show requests. An application-hidden
   cursor must disappear within a frame and plain controller gameplay must not
   show one.
7. **Controllers.** Test arrival, duplicate arrival, update before arrival,
   active-mask removal and reconnect at indices 0 and 15, plus native
   PlayStation motion/touch/rumble and forced emulation policies.
8. **Formats.** Repeat with H.264, HEVC, AV1, PyroWave, HDR, YUV 4:4:4, output
   scaling and a static screen. HDR render format and color metadata must stay
   intact, with no CPU capture, extra frame queue or GPU-to-CPU transfer.
