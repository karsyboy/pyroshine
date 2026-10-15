# Desktop app

The optional app handles pairing, clients, the game library, settings and stream status.
[Install and start the server](../README.md#quick-start) first. The app cannot
start, stop or restart it; closing the app leaves streaming running.

## Install and open

Install `pyroshine-ui-bin` from the Arch/CachyOS repository, or the matching
`pyroshine-ui` [release package](INSTALLATION.md#native-packages).
The package supplies GTK 3/WebKitGTK 4.1 and desktop autostart.
Portable/NixOS installs need a [separate GUI build](../pyroshine-ui/README.md#build)
or [terminal pairing](INSTALLATION.md#pair-without-the-desktop-app).

Open **Pyroshine** from the application menu, or run:

```sh
pyroshine-ui
```

It starts in the tray at desktop login; disable this in your desktop's autostart
settings if desired. The app and server must use the same user's session bus.
Opening the app again shows its existing window.

## Pair a client

1. Select the host in Moonlight/Pyrolight and start pairing. The client shows a
   PIN. If discovery fails, add the host's LAN IP.
2. Open Pyroshine's **Pairing request** notification or **Clients → Pairing requests**.
3. Verify the requester belongs to your device. Enter its PIN, optionally name
   it, and select **Pair**. **Reject** requests you did not initiate.
4. Wait for **Client paired**, the device under **Paired clients**, and the
   client's application list. Select Steam or an installed game.

No notification? Open **Clients** directly. No request? Check the service and
retry in the client. Failed/expired pairings need a fresh request and PIN.
See [troubleshooting](#troubleshooting) or [pairing security](SECURITY_ADMINISTRATION.md).

## Manage clients

In **Clients**, the pencil renames a device; names are assigned by you.
**Revoke** removes its trust immediately, requiring it to pair again.
Revoking **any client ends the active stream**, including another client's.
Other clients remain trusted. Pairing times are available for newer pairings;
last-connection times cover the current server run.

For certificate details and terminal revocation, see
[Security administration](SECURITY_ADMINISTRATION.md#revoke-a-client).

## Library

**Library** lists every application clients can be offered: the manual entries
from **Settings** and the games Pyroshine discovered through its scanners
(Steam, Lutris, Heroic and desktop entries). Installed, removed and renamed
games appear here, and in clients' app lists, within seconds without a restart;
an active stream is never interrupted, even when its game is removed or hidden.

- **Search** matches titles, launchers, launcher IDs (a Steam app ID) and the
  ID clients use. Filter by source, or show only offered, hidden or not offered
  entries. Large libraries are shown a page at a time.
- The **switch** next to a discovered game shows or hides it from clients. The
  choice is kept across restarts, rescans and upgrades, and a hidden game keeps
  its ID, so clients treat it as the same game when it is shown again.
- **Manual** entries are edited in **Settings**. A discovered game with the same
  title as a manual entry is listed as *replaced*; the manual entry is offered.
- **Duplicate** marks a desktop shortcut to a game another scanner already
  offers, or the same game found twice.
- **Rescan** reads every launcher again, for example after changing files the
  watchers cannot see.

Each launcher has a status card: how many games it provides, when it was last
scanned and how long that took, and whether changes are noticed immediately
(**Watching**) or by the periodic check. If a library cannot be read, for
example a Steam library on a drive that is not mounted, the card says why and
what to do, and its games stay offered from the last successful scan (marked
*Offline library*) until the drive is back. Automatic updates can be turned
off in **Settings → Application discovery** (see [`[library]`](CONFIGURATION.md#library)).

## Settings

**Settings** edits applications, scanners and server options; **Show advanced**
reveals less common fields. Fresh native/portable configurations already include
Steam and library scanning. Installing or removing games needs no settings
change or restart (see [Library](#library)). See the
[configuration reference](CONFIGURATION.md).

Saving validates the whole file and preserves unedited settings/comments.
Invalid values or external file changes prevent saving. Apply saved changes by
restarting the service; this ends active streams:

```sh
sudo systemctl restart "pyroshine@$USER"
```

NixOS's generated configuration is read-only: edit
`services.moonshine.settings` and rebuild instead.

## Stream status and tray

The dashboard shows the active application/client, duration, video/audio formats,
the encoder backend and GPU, and streaming performance. The application name
follows the foreground game; **Moonlight application** retains the original
launcher. Disconnected sessions retain status until stopped. Frame pacing
distinguishes VRR from fixed refresh; audio details reflect the negotiated
layout and quality.

While streaming, **Diagnosis** states what limits the stream on the host, for
example an encoder that cannot keep up, a game rendering below the stream's
frame rate, packets the network could not carry, or frames that need
composition, each with what to change. **Capture path** shows whether frames go
straight from the game to the encoder (**Direct export**) or are composited
first, and why. The host cannot see problems on the client or the network
beyond its own socket. To list only warnings, turn off **Settings →
Diagnostics → Informational findings**; this app preference applies at once and
**Copy diagnostics** still includes every finding.

**GPU** shows the capture GPU while streaming: how busy it and its video
encoder are, clocks, temperature, power and video memory, and how much GPU time
the game and Pyroshine each used. Values a driver does not report are left out;
NVIDIA readings come from the driver's NVML library, which samples a process
only while it is busy, so light work can show **No recent sample**.

When a session ends unexpectedly, the dashboard names the cause, such as the
game's exit status or signal and how long after launch it ended.

**Diagnostics** lists **Recent problems** (rejected launches, refused codec or
HDR requests, unexpected stops) with what to do, the **Host checks** with a fix
for each failed check, and the version, GPU, verified codec profiles and ports.
**Run checks** repeats the host checks without restarting Pyroshine; while a
session streams it keeps the startup GPU results instead of probing the GPU
again. **Copy diagnostics** copies one JSON report for a bug report. **Save
support bundle** saves a `.tar.gz` with that report, the last two minutes of
stream statistics, host and GPU details, the configuration and the service log
to your download folder. Both are redacted: home directory, user and host
names, the server name, client names and IDs, certificate fingerprints and IP
addresses are replaced with placeholders.

| Tray indicator | Meaning |
| --- | --- |
| Logo | Ready |
| Green play | Streaming |
| Amber pause | Disconnected; application remains resumable |
| Blue dots | Starting/reconnecting |
| Grey square | Ending session |
| Red exclamation | Cleanup failed; service restarting |
| Indigo keypad | Pairing request waiting |
| Grey logo | Service unavailable |

Left-click opens the window. **End Session** closes the streamed application
and disconnects the client; save progress first. **Quit Pyroshine UI** closes
only the app.

## Desktop environment support

KDE Plasma supports the tray. GNOME needs an AppIndicator/KStatusNotifierItem
extension for a tray; other panels need a StatusNotifierItem host.
The window and notifications work without one: reopen from the application menu.
Wayland may highlight the taskbar instead of focusing the window; select it manually.

On NVIDIA, the app disables WebKitGTK's DMA-BUF renderer to avoid blank windows.
Override only if needed with `WEBKIT_DISABLE_DMABUF_RENDERER=0`.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| No notification | Open **Clients**, retry pairing, check desktop notification settings. |
| A launch or stream fails | Open **Diagnostics → Recent problems** for the cause and fix; see [failure causes](TIPS.md#read-why-a-launch-or-stream-failed). |
| No request | Check server/network; enable **Settings → Pairing & security → Allow new pairings**, save and restart. [More pairing help](TIPS.md#pairing-does-not-complete). |
| "Pyroshine isn't running" | Follow [Start](../README.md#2-start); the app reconnects automatically. |
| Cannot connect / persistent retrying | Check `journalctl -u "pyroshine@$USER" -e` for **Desktop management interface unavailable**. Use the server's user/session bus; update server and GUI together, then restart both. |
| No tray | Use the window; check desktop support above. |
| Blank/unavailable app | Inspect `PYROSHINE_UI_LOG=info pyroshine-ui`; use [terminal pairing](INSTALLATION.md#pair-without-the-desktop-app) meanwhile. |

Use `sudo journalctl` if journal access is denied.
