# Managed Desktop Mode

Desktop Mode streams the existing signed-in Wayland desktop. It creates one
virtual display at the client's resolution and refresh rate, moves the desktop
onto it by disabling the other outputs, and restores the saved display
configuration when the session ends. Existing applications stay running.
Window positions after restoration depend on the desktop compositor.

This implementation has native KDE Plasma and GNOME adapters. It requires the
compositor capabilities detected by `pyroshine desktop status`; other desktops
are refused with a reason. Wayland has no common protocol for this combination
of virtual-output creation and display management. Shared policy and recovery
are desktop independent; adding another compositor requires an adapter.
No virtual-display kernel module is used.

## Enable and use

Install Pyroshine normally, including its user service
`pyroshine-desktop-guard.service`. Sign in to exactly one Wayland session as the
same user that runs Pyroshine. PipeWire and a PulseAudio-compatible control
server (normally `pipewire-pulse`) must be running for desktop audio.

```toml
[desktop]
enabled = true
title = "Desktop Mode"
input = true
audio = true
disconnect_grace_secs = 30
```

Restart Pyroshine after changing configuration. On the host, run:

```sh
pyroshine desktop status
pyroshine desktop authorize
```

Authorize remote keyboard and pointer access in the host consent dialog. This
input-only authorization does not change displays. The Dashboard also provides
**Authorize remote input**. Permission can be requested again when the host
revokes it. `input = false` permits viewing without an input portal session.

Select **Desktop Mode** in the streaming client. Games selected from the library
continue to use their isolated Game Mode session. A Desktop Mode launch does
not execute application commands or hooks. The first implementation uses SDR,
fixed refresh, a per-client UI scale (initially 100%), an embedded cursor and
direct PipeWire DMA-BUF capture.
It refuses incompatible modes or buffers rather than scaling a physical monitor.
The compositor chooses how existing windows migrate to the remaining output.

On KDE, change **Scale** for **Pyroshine Desktop Mode** in the desktop's
**Display Configuration** and select **Apply** while streaming. Fractional and
integer scaling enlarge icons and text without changing the captured pixel
resolution. Pyroshine preserves that choice while enforcing physical-display
takeover, and input coordinates follow the compositor's scaled EIS region.
Pyroshine remembers the chosen scale separately for each paired client and
applies it on future launches, including after a server or host restart.
Same-client reconnects and resolution/rate changes retain the current scale.
If a different paired client resumes the retained desktop, its saved scale is
applied; a client without a saved preference starts at 100%.
Restoration uses the original physical displays' saved scales.

Preferences live in `$XDG_STATE_HOME/moonshine/desktop-preferences.json`
(normally `~/.local/state/moonshine/desktop-preferences.json`), separately from
the recovery journal. The key is the client's verified pairing certificate,
so changing its name or network address does not reset the scale. A new
certificate gets a new preference. Local diagnostic probes have no client
identity and do not overwrite these preferences. Changes are saved while the
virtual output is active and once more before restoration. A preference write
failure is logged and never blocks monitor recovery. Both native adapters apply
the saved scale; unsupported scale/mode combinations are refused by the desktop.

With `audio = true`, desktop playback moves to an owned streaming sink, including
new playback streams. The client hears desktop sound while local speakers are
silent. On exit the saved default output and surviving streams' destinations
are restored, using an available output if a saved audio device is missing.
With `audio = false`, playback routing stays local.

A lost controlling connection starts `disconnect_grace_secs` (0–86400 seconds).
An authenticated reconnect before expiry cancels that deadline. Changed client
modes prepare and validate a replacement output before switching. **End Session**
or **Restore desktop** restores immediately, without waiting for the grace
period. Unauthenticated protocol activity cannot extend takeover.

## Recovery

The independently supervised guard owns native outputs, audio routing and a
private recovery journal. It survives termination of the server's service.
Loss of the server process or its capture-worker ownership connection triggers
restoration. Guard restart replays a journal only in the same graphical login,
identified by user, boot, logind session and Wayland socket identity.

To restore without a running server:

```sh
pyroshine desktop restore
```

The Dashboard provides the same operation and reports incomplete recovery.
The session cannot become Idle or accept another launch until recovery is
confirmed. If saved monitors are disconnected, available displays recover and
the journal remains; reconnect missing monitors and retry Restore. If no
physical output is available, the guard retains its virtual output while it
can, rather than intentionally removing the last usable image. Exact window
placement is not journaled.

Inspect the guard independently:

```sh
systemctl --user status pyroshine-desktop-guard.service
journalctl --user -u pyroshine-desktop-guard.service -b
```

Restart the guard after changing compositor capabilities or updating its
executable. Stopping it attempts restoration first. During compositor logout or
a compositor crash, its virtual outputs may disappear before restoration can
run; the next graphical login uses the desktop's persisted display settings.
A journal from another login is retained and reported, never blindly applied.

For a source build, install the user unit following
[Contributing](../CONTRIBUTING.md). The main server command can create a separate
transient user unit using its own executable when the installed unit is absent.
The standalone `moonshine-desktop-probe` needs the guard installed or already
started with the server executable. Use `--status` to query the running guard
without starting a service or changing displays. Without `--status`, the probe
performs a real takeover and restores afterward (1920×1080 at 60 Hz by default).

```sh
moonshine-desktop-probe --status
moonshine-desktop-probe --width 2880 --height 1920 --fps 120 --duration 10 --timings --json capture.json
# Repeat the same workload using implicit synchronization:
moonshine-desktop-probe --width 2880 --height 1920 --fps 120 --duration 10 --timings --implicit-sync --json capture-implicit.json
```

Capture reports include the requested mode, the guard's observed native mode
(`null` with older guards), PipeWire's rational rate/ceiling, format/modifier,
plane count, actual pool, sync mode and first completed frame. Delivered FPS is
measured separately and can be lower on idle desktops. `--timings` adds bounded
histograms for callback time, producer-ready delay, acquire-to-publish delay and
consumer hold duration. Percentiles are logarithmic bucket upper bounds in µs;
maxima are exact. Timing collection is disabled during ordinary sessions and
adds measurement overhead when enabled. The existing pipeline benchmark's
`frame_age` distribution covers content timestamp through final socket submission;
the capture-only probe does not measure encoding or transport.

The implicit override affects this diagnostic probe only. Normal capture probes
explicit timeline synchronization on the selected GPU, negotiates it only when
the producer supports it, and reopens an implicit-only candidate after setup
failure. Neither mode permits CPU video buffers or another GPU.

## Ownership and adapters

`moonshine-core/src/desktop/` holds lifecycle-only control. The guard snapshots
before mutation and persists each owned resource before native creation. It
prepares an output while physical displays remain enabled. The capture worker
validates native size/rate, importable frames and authorized input devices before
asking the guard to commit the sole virtual display. Input coordinates are then
verified against the virtual region before readiness is published.

The guard's bounded same-user Unix socket carries administrative messages only.
Frames travel directly from the native compositor through PipeWire to capture
admission and the existing encoder. Retired capture streams keep their buffers
until GPU consumers release them. No per-frame IPC passes through the guard.

KDE uses pinned output-device, output-management and screencast Wayland
protocols, including custom modes and output priority. Discovery prefers the
output registry and otherwise uses classic per-output globals; both require
management/device capabilities sufficient for complete mode and topology recovery. GNOME uses Mutter's
native ScreenCast platform virtual monitors and temporary DisplayConfig changes,
including logical mirror groups and layout mode. Both snapshots include mode,
position, scale, transform, primary/priority and exposed color state. The guard
reconciles stable output identities on recovery and verifies observed settings.
KWin virtual outputs cannot be disabled through output configuration: recovery
first restores physical displays with the owned output still live, then closes
its owner and verifies the final topology. Replacement waits for old capture
buffers to drain and disconnects the old PipeWire consumer before closing its
native producer owner. PipeWire format negotiation may
briefly report the initial 60 Hz mode; readiness waits for the requested mode
within its existing deadline.

Native APIs are capability checked; compositor updates may require adaptation.

Remote input uses a separate RemoteDesktop portal and EIS session. Its restore
token is `desktop-input-restore-token`; old monitor-selection grants are not
reused. Revocation, session lock or input transport failure stops Desktop Mode
and restores. Display changes can temporarily pause or replace EIS devices;
input is suppressed until the authorized devices and the virtual display's
region are ready again, for at most three seconds. Repeated partial updates do
not extend that deadline, and held keys/buttons are not replayed on replacement
devices. Controllers retain the existing Inputtino path.

KDE's EIS region physical scale also identifies the virtual output after a UI
scale change: its logical size, multiplied by that factor, must describe the
captured pixel mode (allowing logical-pixel rounding). An arbitrary sole region
cannot substitute for a missing managed output.

## Validation

Automated tests cover per-client scale persistence/isolation, quick Stop,
same-client and different-client resume, replacement modes and preference-write
failures, as well as durable snapshots before mutation, prepare/commit faults,
failed restoration, helper restart, missing monitors and reconnect, stale
leases/deadlines, capture-owner loss, grace expiry, configuration and management
protection. These tests use an injected platform; passing them does not validate
real compositor protocols or GPU capture.

The opt-in KDE capture test adds one virtual output, checks 2880×1920 at
120 Hz (within native timing rounding), applies 125%, 175%, 200% and 100% UI
scales while checking that capture stays at 2880×1920, then removes it and
verifies the original topology. Physical monitors remain enabled:

```sh
MOONSHINE_TEST_DESKTOP=1 cargo test -p moonshine-core \
  desktop::native::kde::tests::native_120hz_capture_and_owner_cleanup_on_kde \
  -- --ignored --exact --nocapture
```

It does not validate full takeover, input, audio, reconnects or GNOME.

A separate test temporarily disables physical monitors for five seconds and
receives frames with authorized input, without injecting events. Run it only
when a brief desktop takeover is acceptable, after authorizing remote input:

```sh
MOONSHINE_TEST_TAKEOVER=1 cargo test -p moonshine-core \
  session::capture::portal::takeover_tests::managed_takeover_keeps_input_across_output_changes \
  -- --ignored --exact --nocapture
```

This opt-in is also required before the hardware runner selects the test. It
checks host capture/input continuity and restoration; client playback, audio
and reconnect acceptance remain separate.
Before declaring either desktop supported for release, run this matrix on
physical KDE and GNOME hosts with a real streaming client:

| Case | Required result |
| --- | --- |
| One monitor and several open apps | Client-sized virtual output only; existing desktop and apps remain usable remotely |
| Several monitors, mixed scales/rotation, mirrored displays | All physical outputs disabled during takeover; saved topology, primary, modes and exposed color state restored |
| KDE Display Configuration scale changes during streaming | Apply changes icons/text at 125%, 175% and 200%; pointer remains aligned, stream pixel resolution unchanged, original physical scales restored on exit |
| Per-client scale after Stop, restart and reconnect | Each paired client recovers its own scale; changed-size/rate reconnect preserves it; diagnostic probes do not overwrite client preferences |
| Input consent, denied consent, revoked consent and lock | Readiness only with requested input available; denial/revocation/lock rolls back |
| Audio already playing, new playback, removed audio device | Remote-only audio, then restored destinations/default or a working fallback |
| Same-mode and changed-size/rate reconnect | Grace cancelled only by authenticated control; replacement validated before switch; no old frames/resources survive |
| Disconnect, zero grace, grace expiry, explicit Stop | Correct deadline; Stop restores immediately; Idle follows verified cleanup |
| Server SIGTERM, server SIGKILL, capture-worker failure, guard SIGKILL | Independent recovery; restart replays only the matching graphical login |
| Monitor unplug/replug during takeover and restoration | No false Idle; recover available monitors, retain recovery record until complete |
| Unsupported compositor/API, several Wayland logins, GPU mismatch | Clear refusal before destructive takeover; games still use their existing path |
| Repeated takeover/reconnect/restore cycles | No accumulated PipeWire streams, native outputs, sinks, modules, FDs or child units |

Record compositor versions, GPU/driver, client, negotiated modes, restored
settings and cases not executed with the change's validation results.
