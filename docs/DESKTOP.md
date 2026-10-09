# Desktop app

The optional `pyroshine-ui` desktop app adds a system tray icon, pairing
notifications, PIN approval, paired-client management, a settings editor and a
stream dashboard. It manages a Pyroshine service that is already running for
your user; it never starts, stops or restarts that service. Pyroshine works the
same without it, including on hosts with no graphical session.

## Install

Install the `pyroshine-ui` package from the
[releases page](https://github.com/karsyboy/pyroshine/releases) next to the
server package:

```sh
# Arch Linux / CachyOS
sudo pacman -U ./pyroshine-ui-*.pkg.tar.zst

# Debian / Ubuntu
sudo apt install ./pyroshine-ui_*.deb

# Fedora / RHEL
sudo dnf install ./pyroshine-ui-*.rpm
```

On Arch Linux and CachyOS, `pyroshine-ui-bin` from the
[`[pyrowave]` pacman repository](https://github.com/karsyboy/pyrowave-packages)
installs the same app (`sudo pacman -Syu pyroshine-ui-bin`).

It needs WebKitGTK 4.1 and GTK 3, which the package pulls in. The server
package does not depend on it. The portable installer and the NixOS module do
not include the app; build it from source as described in
[pyroshine-ui/README.md](../pyroshine-ui/README.md).

The app starts in the tray at each desktop login (`/etc/xdg/autostart`); turn
that off in your desktop's autostart settings. Open it from the application
menu as **Pyroshine** or run `pyroshine-ui`. Starting it again shows the
running instance instead of opening a second one.

The app and the service must belong to the same user and share that user's
D-Bus session bus. This is the case for the packaged `pyroshine@<user>`
service. If the app reports that Pyroshine isn't running while the service is
active, check the service log for "Desktop management interface unavailable".

## Tray

| Icon | Meaning |
| --- | --- |
| Logo | Ready for connections |
| Green play button | A client is streaming |
| Amber pause button | The application is running, but the client disconnected; Moonlight can resume it |
| Blue dots | A session is starting or a client is reconnecting |
| Grey square | The session is ending |
| Red exclamation mark | The session could not be stopped cleanly; the service is restarting |
| Indigo keypad (attention) | A client is waiting to pair |
| Grey logo | The Pyroshine service is not running |

Left-click opens the window. The menu shows the current application, mode and
client, and offers **Pair a Client**, **End Session** and **Quit Pyroshine UI**.

The dashboard and tray show the primary application selected by Pyroshine's
compositor. Launching a game from Steam changes that name to the game's window
title; exiting it returns to the launcher. Session details show both the
foreground application and the original **Moonlight application**, whose
**Application ID** stays unchanged. If a window supplies no name, the main
display falls back to the Moonlight application. Reporting continues while a
disconnected session is retained. Overlays and notifications follow the
compositor's existing primary-focus policy.

**End Session** ends the session exactly like quitting from Moonlight: it
closes the streamed application (unsaved progress may be lost) and disconnects
the client. **Quit Pyroshine UI** only closes the app; the server and any stream
keep running.

## Pair a client

1. In Moonlight, add the host and select it. Moonlight shows a PIN.
2. A **Pairing request** notification appears. Click it, or open **Clients**.
3. Check the requester address and certificate fingerprint, enter the PIN and,
   optionally, a name for the device, then select **Pair**.

The PIN applies only to the request shown. If the client restarts pairing, the
new request replaces the old one and needs its own PIN. Requests expire after
five minutes. A wrong PIN makes pairing fail on both sides; start again in
Moonlight. **Reject** refuses a request you do not recognize.

Moonlight does not send a device name, and many clients send the same client
ID, so the certificate fingerprint identifies a device. Name devices when you
pair them to tell them apart later.

Without the app, for example on a headless host, Pyroshine shows a desktop
notification where possible and logs a host-local approval link
(`http://localhost:47989/pin?uniqueid=…`); see
[Security administration](SECURITY_ADMINISTRATION.md).

## Manage clients

**Clients** lists every paired certificate with its name, when it was paired
(for pairings made by this version or later) and when it last connected since
Pyroshine started. Rename a client with the pencil icon. **Revoke** removes its
trust immediately; it must pair again to connect. Revoking also ends any active
session, because sessions do not record which client started them.

## Settings

**Settings** edits every `config.toml` setting described in the
[configuration reference](CONFIGURATION.md), grouped by area, including
applications and application scanners. Rarely needed settings appear with
**Show advanced**; settings that can break streaming or require re-pairing show
a warning.

Saving validates the whole configuration first; nothing is written if any
value is invalid or if the file changed on disk since it was loaded. The save
changes only the settings you edited and keeps the rest of the file, including
comments and formatting.

Pyroshine reads its configuration only at startup. After saving, restart it to
apply the changes (this ends any active session):

```sh
sudo systemctl restart "pyroshine@$USER"
```

The app cannot edit a configuration it cannot write, such as the NixOS
module's generated file; change `services.moonshine.settings` instead.

## Dashboard and diagnostics

The dashboard shows the session state, application, negotiated video and audio
format, client address and session duration. While a client streams it adds
one-second statistics from the video pipeline: frame rate, encoded and wire
bitrate, transport overhead and the time spent in each pipeline stage. Stages
the active encoder does not use are not shown.

**Frame pacing** shows how the game's frames reach the client: **VRR capture**
when a client with VRR presentation (Pyrolight with VRR enabled) has the host
capture each frame as the game presents it, otherwise **Fixed refresh**. It
reports the game's frame rate and frame times as sent to the client (median,
p95, p99 and maximum), the share of frame times that change by more than 2 ms
from one frame to the next (visible unevenness on a VRR display), and the
delay from new content to its capture.

**Audio** shows the stream the client receives: channel layout and mask, the
audio quality level (Standard, High or Maximum) and whether the client
requested it or the host default (`[stream.audio] quality`) applied, the
encoded Opus bitrate after the audio packet-size limit, the Opus stream layout
(GameStream high-quality surround uses one mono stream per channel), sample
rate, packet duration and encryption. It updates when a reconnect negotiates a
different audio mode.

**Diagnostics** shows the version, GPU, verified codec profiles, HDR and DMA-BUF
support, listening ports and the startup health check.

## Desktop environment support

| Environment | Tray | Notes |
| --- | --- | --- |
| KDE Plasma (Wayland or X11) | Yes | Native StatusNotifierItem support. |
| GNOME (Wayland or X11) | With an extension | GNOME has no tray; install the AppIndicator/KStatusNotifierItem extension (preinstalled on Ubuntu). Notifications and the window work without it. |
| Other X11 or Wayland panels | If the panel hosts StatusNotifierItem icons | Xfce, Cinnamon, MATE, LXQt and waybar support it; legacy XEmbed-only trays do not. |

Without a tray, closing the window keeps the app running for notifications;
open it again from the application menu.

On Wayland a window may only take focus with an activation token from the
compositor. The app uses the token that comes with the click on a
notification, the tray icon or the application menu entry, so the window comes
to the front. Desktops that do not pass a token (some notification servers and
tray extensions) only highlight the window in the taskbar instead.
On NVIDIA systems the app disables WebKitGTK's DMA-BUF renderer, which can show
blank windows there; set `WEBKIT_DISABLE_DMABUF_RENDERER=0` to override.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| "Pyroshine isn't running" | `systemctl status "pyroshine@$USER"`; the app reconnects by itself once the service runs. |
| "Pyroshine is running, but this app can't connect to it" | A transient error (for example the session bus or a service still starting) is retried automatically with growing delays up to 30 s. If it says retrying won't help, the app and the service are different versions or access is denied: update both, then restart the app. |
| Service runs, app cannot reach it | The service log line "Desktop management interface unavailable" explains why, for example a different user or session bus. |
| No tray icon | The table above; the app logs "System tray unavailable" when no host exists. |
| App diagnostics | Run `PYROSHINE_UI_LOG=info pyroshine-ui` from a terminal. |
