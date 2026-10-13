# Tips and troubleshooting

Start with the symptom below. Commands assume a native/portable installation
running for your current user. [NixOS](NIXOS.md) uses `moonshine.service`.
If journal access is denied, prefix `journalctl` with `sudo`.

| Problem | First action |
| --- | --- |
| Host missing or connection fails | [Check address and firewall](#host-not-found-or-connection-fails). |
| Server fails to start | Read `journalctl -u "pyroshine@$USER" -b -n 100 --no-pager`, then [check the host](#healthcheck-fails). |
| Pairing does not finish | [Use a fresh request and correct approval URL](#pairing-does-not-complete). |
| Launch or stream fails with an error | Read the client's error message, then **Diagnostics → Recent problems** in the desktop app; [details](#read-why-a-launch-or-stream-failed). |
| Steam opens locally / client reports 503 | Quit desktop Steam, then retry; [Steam workaround](#close-a-desktop-steam-before-streaming-steam). |
| Steam or games missing / cannot launch | [Check default applications](#steam-or-games-are-missing). |
| Keyboard, mouse or gamepad unavailable | [Check input access](#keyboard-mouse-or-gamepads-do-not-work). |
| Desktop app unavailable / no tray | [GUI troubleshooting](DESKTOP.md#troubleshooting); browser pairing still works. |
| New settings have no effect | Restart the service after saving; this ends an active session. |

## Host not found or connection fails

1. Check the [service status and ready message](INSTALLATION.md#service-diagnostics).
2. Run `hostname -I` on the host. In Moonlight/Pyrolight, add its LAN or VPN IP
   manually. Choose the address reachable from that device, not a container
   address. Default listeners use IPv4 (`0.0.0.0`).
3. Check the [firewall ports](INSTALLATION.md#network-and-firewall). Host discovery
   uses multicast and may not cross VLANs/subnets or a VPN. A manually added IP
   can work even when discovery does not. Guest Wi-Fi/client isolation can block
   both discovery and streaming.
4. If pairing works but streaming fails, check UDP 47998–48000 as well as TCP
   48010. Client and host must share a compatible advertised codec. Start with
   SDR and a codec listed by the host's **Diagnostics** or healthcheck.

Do not solve discovery problems by exposing ports on a public router; use a VPN.

## Healthcheck fails

The standalone healthcheck binds the configured ports. Stop the server before
running it, as your streaming user; this ends any active stream:

```sh
sudo systemctl stop "pyroshine@$USER"
pyroshine "$HOME/.config/moonshine/config.toml" healthcheck
```

For portable installations, replace `pyroshine` with
`/opt/pyroshine/start-pyroshine.sh`. For a service override, use its actual config
path. There is no `--config` option. Fix `FAIL` results, then start again:

```sh
sudo systemctl start "pyroshine@$USER"
```

| Report | Corrective action |
| --- | --- |
| Ports in use | If the server is stopped, inspect `sudo ss -lntup` for another listener (for example Sunshine/Moonshine). Stop the conflicting service or configure distinct ports; do not run two servers on the defaults. |
| Render nodes / GPU / EGL / Vulkan / DMA-BUF / Codecs | Install the current hardware driver for your GPU using your distribution's instructions, then repeat the check. A GPU name alone does not guarantee the required capabilities. No usable encode profile or DMA-BUF support means this host cannot stream with the current setup. |
| Render access denied | Inspect `ls -l /dev/dri/renderD*` and the group named in the check. Give the streaming user that device group's access, then log out/in (reboot if the headless user manager retains old groups). Do not use world-writable device permissions. |
| Color conversion warning/failure | The GPU converted the check pattern to the listed format with wrong colors, so those profiles are not offered (streams would show wrong colors). This is a Pyroshine or driver bug, not a setting: report it with the check output or **Copy diagnostics**. Meanwhile use the profiles the check still lists, or PyroWave. |
| Capture/Vulkan device mismatch | The capture and encoder must use the same physical GPU. Follow [GPU selection](CONFIGURATION.md#compositor), using the identities in the error. There is no cross-GPU copy fallback. |
| Runtime dir / waiting for D-Bus | Use a regular desktop login or enable [lingering](INSTALLATION.md#headless-setup); the packaged wrapper sets the runtime/bus paths. Do not run the server as root. |
| kcmp(2) blocked/unavailable | Run on the host outside a restrictive sandbox, or correct the kernel/seccomp restriction described in the error. It is required for buffer validation. |
| PyroWave unavailable | Optional if a conventional codec passes; use H.264/HEVC/AV1. For PyroWave, keep the package's matching library and compatible Pyrolight client; see [PyroWave](PYROWAVE.md). |
| Sleep inhibit warning | The native/portable service grants the `pyroshine` group; a direct terminal check may lack it. See [sleep inhibition](#prevent-the-host-from-suspending-while-streaming). NixOS uses `moonshine`. |
| input group warning | Needed for gamepads when logged out; follow [headless setup](INSTALLATION.md#headless-setup). Not required with working active-seat ACLs. |

Passing the standalone check verifies host prerequisites; it does not test an
actual client, game or stream. Startup runs its own check before listening.
While the server runs, use **Run checks** on the GUI's **Diagnostics** page
instead: it repeats the checks without stopping the server, treats the server's
own listeners as in use by it, and repeats the GPU probes only while no session
streams. Each failed check shows what to change.

## Pairing does not complete

- Start pairing in the client first. An empty **Clients** page or "No pending
  pairing request" can mean the request expired, disconnected or was replaced.
  Retry and use the latest PIN/link.
- Without the GUI, run `pyroshine pair` as the server's user, without sudo.
  If the command cannot connect, check the service and lingering/session bus.
  For [browser pairing](INSTALLATION.md#browser-pairing), use the complete
  logged URL; bare `/pin` cannot select a request.
- A 403 on approval means loopback/Host/origin validation failed. Open the URL
  on the host or use an SSH tunnel with the configured HTTP port at both ends.
  A host LAN address or a different local forwarding port will not work.
- If new requests are disabled, enable **Allow new pairings** in GUI settings
  (or `webserver.enable_pairing`), save, and restart before retrying.
- **PIN accepted** is not proof that pairing completed. Wait for the client's
  application list; a wrong PIN can still fail the later handshake.

## Steam or games are missing

Fresh configurations already include **Steam** and a scanner for
`$HOME/.local/share/Steam`, launching `/usr/bin/steam`. Install and sign in to
Steam as the service's user, and install games before starting Pyroshine.
The scanner reads Steam's library locations and runs at server startup; restart
to pick up newly installed games. Pyroshine does not install games for you.

If Steam is listed but cannot launch, check that `/usr/bin/steam` exists and
quit any Steam already running on the local desktop. For Flatpak Steam, use the
[Flatpak recipe](#run-flatpak-steam-inside-pyroshines-compositor). For NixOS use
its [Steam configuration](NIXOS.md#install-and-configure). For other launchers,
set applications/scanners in the GUI or [configuration reference](CONFIGURATION.md#application).
Keep executable paths absolute when outside the service's PATH.
For crashes or launch timeouts, capture [application logs](#debug-a-failing-application).

## Keyboard, mouse or gamepads do not work

Native packages and the portable installer provide input-device rules and load
`uinput`/`uhid`. If the healthcheck reports missing modules, load them with
`sudo modprobe uinput uhid`. If they are loaded but access is denied, check the
installed rules: `/usr/lib/udev/rules.d/60-pyroshine.rules` for native packages,
`/etc/udev/rules.d/60-pyroshine.rules` for portable installs. The healthcheck's
inherited `60-moonshine.rules` hint applies to NixOS, not native Pyroshine packages.
After correcting integration, reload with `sudo udevadm control --reload-rules`
and `sudo udevadm trigger`, or reboot.

Without an active desktop seat, [input group membership](INSTALLATION.md#headless-setup)
is needed. Ensure the user's systemd manager and streamed games have the new
groups, not only a new shell. Do not make devices world-writable.

## Missing libraries or an older system

If startup reports `GLIBC_2.39 not found`, upgrade the distribution or
[build from source](../CONTRIBUTING.md#manually-building-the-app). If a `.so` is missing, native
package users should check that the package manager resolved its dependencies;
portable users must supply [host runtime libraries](INSTALLATION.md#portable-installer-and-steamos).
`ldd /opt/pyroshine/bin/pyroshine` lists direct missing libraries for a portable
install, but Vulkan/EGL/encoder libraries loaded at runtime also need to work.
The [compatibility guide](INSTALLATION.md#platform-compatibility) distinguishes package
availability from verified compatibility.

## Application recipes

These are optional adjustments. Edit existing application entries rather than
adding a second Steam entry with the same title. Restart after configuration
changes. See the [reference](CONFIGURATION.md) for command fields and scanners.

- [Close local Steam automatically](#close-a-desktop-steam-before-streaming-steam)
- [Prevent suspend](#prevent-the-host-from-suspending-while-streaming)
- [GameMode](#run-games-in-high-performance-mode)
- [Gamescope resolution](#use-gamescope-with-the-clients-resolution)
- [Game fullscreen resolution](#match-a-games-fullscreen-resolution-to-the-stream)
- [Flatpak Steam](#run-flatpak-steam-inside-pyroshines-compositor)
- [A full remote desktop](#run-a-desktop-environment-for-a-full-remote-desktop)
- [Application logs](#debug-a-failing-application)

## Close a desktop Steam before streaming Steam

Steam is single-instance per user. An existing desktop instance takes over the
streamed launch, opening Big Picture locally and causing a 503 error.
Quit it manually, or edit the existing Steam entry to close it automatically:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
pre_command = [
    ["/usr/bin/bash", "-c", "if pgrep -x steam >/dev/null; then /usr/bin/steam -shutdown &>/dev/null; for i in $(seq 1 30); do ! pgrep -x steam >/dev/null && break; sleep 1; done; fi"],
]
```

This closes local Steam and waits up to 30 seconds before streaming it.
See [upstream issue #134](https://github.com/hgaiser/moonshine/issues/134).

## Prevent the host from suspending while streaming

The native/portable service grants the `pyroshine` group and inhibits sleep
automatically during streams. [NixOS](NIXOS.md#headless-input-and-sleep)
requires membership in `moonshine`. For direct binary launches:

```sh
sudo usermod -aG pyroshine "$USER"
```

Log out/in to refresh groups. Manual installs also need the shipped policy and
group; follow [manual installation](../CONTRIBUTING.md#manual-installation-and-upgrade).
A sleep-inhibition warning does not prevent streaming, but the host may suspend.
To disable inhibition, set at the top of the configuration:

```toml
inhibit_sleep = false
```

## Run games in high-performance mode

Install [GameMode](https://github.com/FeralInteractive/gamemode), then set each
game's Steam launch option:

```sh
gamemoderun %command%
```

Use it for games, not Steam's application command: Steam can pass its
`LD_PRELOAD` into child processes and prevent launch. The daemon normally starts
through D-Bus. Check with `gamemoded -t`; if your package requires the `gamemode`
group, join it and log out/in:

```sh
sudo usermod -aG gamemode "$USER"
```

Without a working daemon, games may run without the requested optimizations.

## Use Gamescope with the client's resolution

Install Gamescope and set the game's Steam launch option:

```sh
/usr/bin/gamescope -f -W "$MOONSHINE_CLIENT_WIDTH" -H "$MOONSHINE_CLIENT_HEIGHT" -w "$MOONSHINE_CLIENT_WIDTH" -h "$MOONSHINE_CLIENT_HEIGHT" -r "$MOONSHINE_CLIENT_FRAMERATE" -- %command%
```

Pyroshine supplies these variables to streamed applications and their children.
`-W/-H` set output size, `-w/-h` set game render size, and `-r` sets refresh rate.
Lower render sizes upscale; higher ones supersample. For 1.5× supersampling:

```sh
/usr/bin/gamescope -f -W "$MOONSHINE_CLIENT_WIDTH" -H "$MOONSHINE_CLIENT_HEIGHT" -w $((MOONSHINE_CLIENT_WIDTH * 3 / 2)) -h $((MOONSHINE_CLIENT_HEIGHT * 3 / 2)) -r "$MOONSHINE_CLIENT_FRAMERATE" -- %command%
```

Gamescope can also help with focus/scaling problems. Outside a stream, provide
fallbacks such as `${MOONSHINE_CLIENT_WIDTH:-2560}`; Pyroshine's variables are absent.

## Match a game's fullscreen resolution to the stream

Set the in-game resolution to the client's resolution, or use borderless/windowed
mode at that size. A saved fullscreen mode from another monitor can hide menus
or render at the wrong size (for example in Civilization VI under Proton).
Pyroshine scales smaller windows; use [Gamescope above](#use-gamescope-with-the-clients-resolution)
to render at a different internal resolution.

## Run Flatpak Steam inside Pyroshine's compositor

Replace the native Steam entry with:

```toml
[[application]]
title = "Steam Flatpak"
command = [
    "dbus-run-session", "--", "flatpak", "run",
    "com.valvesoftware.Steam", "steam://open/bigpicture",
]
```

For Flatpak Steam game scanning:

```toml
[[application_scanner]]
type = "steam"
library = "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"
command = [
    "dbus-run-session", "--", "flatpak", "run",
    "com.valvesoftware.Steam", "steam://rungameid/{game_id}",
]
```

The separate D-Bus session prevents host portals from redirecting Flatpak to the
physical desktop. It keeps Pyroshine's display and audio environment effective.
Quit any existing Flatpak Steam before launching the stream.

## Run a desktop environment for a full remote desktop

Add your installed desktop as an application, for example Plasma:

```toml
[[application]]
title = "Plasma Desktop"
command = ["/usr/bin/startplasma-wayland"]
output_scale = 1.5
```

Or COSMIC:

```toml
[[application]]
title = "COSMIC Desktop"
command = ["/usr/bin/start-cosmic"]
```

Other nested compositors use the same mechanism, such as `/usr/bin/sway`.
The packaged service supplies the runtime directory and session bus; Plasma
needs no extra wrapper. Log out of local Plasma first: two full sessions for
one user conflict through D-Bus and user services.

`output_scale` is optional (default `1.0`); it changes logical desktop size while
preserving the client's physical stream resolution. Fully stop/relaunch after
changing it. Use this setting rather than nested `kscreen-doctor` changes or
`QT_SCALE_FACTOR`, which can produce mismatched geometry/clipped panels.

This starts an already-authenticated user's desktop. A display manager/login
screen cannot run as a streamed application; keep host logins managed by the
system display manager.

## Read why a launch or stream failed

Pyroshine reports the cause of a failure where you can see it:

- **Launch failures** (missing command, failed hook, an application that exits
  right away, a port held by another program) are returned to the client, which
  shows them in its error dialog, for example *The application command 'heroic'
  was not found*. An application that exits during launch names its exit status
  or signal.
- **Rejected stream settings** (a codec, 4:4:4 or HDR combination the GPU did not
  pass at startup, invalid audio settings) reach Moonlight only as an RTSP error
  code. The desktop app's **Diagnostics → Recent problems** lists the exact
  request and the profiles that work.
- **Sessions that end unexpectedly** show the cause on the dashboard: the
  application's exit status or signal and how long after launch it ended, or
  the encoder failure that stopped the stream.

The desktop app also notifies about these when its window is closed.
**Copy diagnostics** on the Diagnostics page copies the host checks, session,
latest stream statistics and recent problems as one JSON report for a bug report;
it contains no keys, PINs or certificates.

## Debug a failing application

Applications discard output by default. Capture it by editing the existing entry:

```toml
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"]
stdout = "journal"
stderr = "journal"
```

Restart, retry the launch, then read its logs:

```sh
journalctl --user -u moonshine-session.service -e
```

Add `-f` to follow retries. Scanners also accept `stdout`/`stderr`; file output
such as `"append:/path/to/log.txt"` and other systemd output values are supported.
