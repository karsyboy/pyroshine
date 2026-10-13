# Desktop app

The optional app handles pairing, clients, settings and stream status.
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

## Settings

**Settings** edits applications, scanners and server options; **Show advanced**
reveals less common fields. Fresh native/portable configurations already include
Steam and library scanning. See the [configuration reference](CONFIGURATION.md).

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
beyond its own socket.

When a session ends unexpectedly, the dashboard names the cause, such as the
game's exit status or signal and how long after launch it ended.

**Diagnostics** lists **Recent problems** (rejected launches, refused codec or
HDR requests, unexpected stops) with what to do, the **Host checks** with a fix
for each failed check, and the version, GPU, verified codec profiles and ports.
**Run checks** repeats the host checks without restarting Pyroshine; while a
session streams it keeps the startup GPU results instead of probing the GPU
again. **Copy diagnostics** copies one JSON report for a bug report.

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
