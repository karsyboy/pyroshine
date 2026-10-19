# Web interface and tray

Pyroshine has one management interface: a web page the server itself serves.
It is the same on a desktop and on a headless server, needs no desktop
environment, browser or Node.js on the host, and keeps working whether or not
the desktop tray runs. Closing the page never affects the server or a
stream.

[Install and start the server](../README.md#quick-start) first.

## Open it

| Where you are | How |
| --- | --- |
| On the host's desktop | **Pyroshine** in the application menu, or click the tray icon. Both open your default browser. |
| On the host, any browser | <http://127.0.0.1:47990/> |
| Another computer | Forward the port over SSH, then open <http://127.0.0.1:47990/> on that computer: `ssh -N -L 47990:127.0.0.1:47990 user@host` |
| A phone or tablet | Through a VPN to the host and a [non-loopback address](#access-from-another-device). |

`pyroshine web url` prints the address the server uses. The port and address
are configured in [`[management]`](CONFIGURATION.md#management).

## Sign in

The first time, choose an administrator password with a setup code that only
the streaming user can obtain. Run on the host (or over SSH) as that user:

```sh
pyroshine web setup
```

Enter the printed code (valid for 15 minutes) and the new password on the
page; you are signed in. On a desktop, the tray's **Set Up Web Access** shows
a code in a notification instead. Later, sign in with the password. A browser
stays signed in for up to 30 days, or 7 days without use.

- **Forgot the password:** `pyroshine web setup --reset` issues a code that
  sets a new password and signs out every browser.
- **Change the password:** **Maintenance → Administrator password**. Other
  browsers are signed out.
- **Signed-in browsers:** **Maintenance** lists them; **Sign out** ends one.

A loopback address grants nothing by itself: other users and programs on the
host need the password too. A paired Moonlight client has no access to the
management interface. See [Web management security](SECURITY_ADMINISTRATION.md#web-management).

## Access from another device

Prefer an SSH tunnel (above): the interface stays on `127.0.0.1` and the
tunnel encrypts and authenticates the connection. To reach it directly over a
VPN or trusted LAN instead, listen on another address; HTTPS is then required:

```toml
[management]
address = "0.0.0.0"
tls = true
# Optional: names browsers use, besides IP addresses and localhost.
allowed_hosts = ["streambox.lan"]
```

Restart the service. By default HTTPS uses Pyroshine's own self-signed
certificate, which browsers warn about until you accept it; set `certificate`
and `private_key` to use one your devices trust. Allow TCP 47990 only from
your trusted network, and never forward it to the internet.

## Pair a client

1. Select the host in Moonlight/Pyrolight and start pairing. The client shows
   a PIN. If discovery fails, add the host's LAN IP.
2. Open **Clients**: the request appears at once (the tray's **Pairing
   request** notification opens it directly).
3. Verify the requester belongs to your device. Enter its PIN, optionally name
   it, and select **Pair**. **Reject** requests you did not initiate.
4. Wait for **Client paired**, the device under **Paired clients**, and the
   client's application list. Select Steam or an installed game.

A PIN applies only to the request it was entered for: when the client sends a
new request, enter the new PIN. Failed or expired pairings need a fresh
request. Without the web interface, [pair from a terminal](INSTALLATION.md#pair-from-a-terminal).

## Manage clients

In **Clients**, the pencil renames a device; names are assigned by you.
**Revoke** removes its trust immediately, requiring it to pair again.
Revoking **any client ends the active stream**, including another client's.
Other clients remain trusted. Pairing times are available for newer pairings;
last-connection times cover the current server run.

For certificate details and terminal revocation, see
[Security administration](SECURITY_ADMINISTRATION.md#revoke-a-client).

## Applications

**Applications** lists every application clients can be offered, with the box
art clients show: your own (manual) entries and the games Pyroshine discovered through its scanners
(Steam, Lutris, Heroic and desktop entries). Installed, removed and renamed
games appear here, and in clients' app lists, within seconds without a restart;
an active stream is never interrupted, even when its game is removed or hidden.

- **Search** matches titles, launchers, launcher IDs (a Steam app ID) and the
  ID clients use. Filter by source, or show only offered, hidden or not offered
  entries. Large libraries are shown a page at a time.
- The **switch** next to a discovered game shows or hides it from clients. The
  choice is kept across restarts, rescans and upgrades, and a hidden game keeps
  its ID, so clients treat it as the same game when it is shown again.
- **Add application** adds your own entry (a title and the command to run, plus
  optional box art, scale, hooks and timeouts); **Edit** on a **Manual** entry
  changes or removes it. Like other settings, these are saved to the
  configuration file and offered to clients after a restart (**Restart now**).
  A discovered game with the same title as a manual entry is listed as
  *replaced*; the manual entry is offered.
- **Duplicate** marks a desktop shortcut to a game another scanner already
  offers, or the same game found twice.
- **Rescan** reads every launcher again, for example after changing files the
  watchers cannot see.

Under **Game launchers** each scanner has a status card: how many games it
provides, when it was last scanned and how long that took, and whether changes
are noticed immediately (**Watching**) or by the periodic check. If a library
cannot be read, for example a Steam library on a drive that is not mounted, the
card says why and what to do, and its games stay offered from the last
successful scan (marked *Offline library*) until the drive is back.

- **Add launcher** adds a Steam, Lutris, Heroic or desktop-entry scanner with
  its usual paths and launch command filled in; **Edit** on a card changes its
  library location, launch command and options, or removes it.
- **Discovery options** turns automatic updates on or off and sets the
  periodic check (see [`[library]`](CONFIGURATION.md#library)).

Launcher and application changes are saved to the configuration file with the
same checks as **Settings** and apply after a restart (**Restart now**).

## Settings

**Settings** edits the server options (applications and game launchers are on
the [Applications](#applications) page); **Show advanced**
reveals less common fields. Fresh native/portable configurations already include
Steam and library scanning. Installing or removing games needs no settings
change or restart (see [Applications](#applications)). See the
[configuration reference](CONFIGURATION.md).

Saving validates the whole file and preserves unedited settings and comments.
Invalid values are refused with the setting highlighted; if the file changed
since the page loaded it (another browser, an editor), saving reports a
conflict and nothing is overwritten. Saved settings apply when Pyroshine
restarts, which ends active streams: choose **Restart now** in the notice after
saving, or **Restart Pyroshine** on **Diagnostics** or **Maintenance**, and
confirm. Pyroshine shuts down normally and starts again by itself (it does not
need systemd); the page shows that it is restarting and reconnects when it is
back. From a terminal:

```sh
sudo systemctl restart "pyroshine@$USER"
```

NixOS's generated configuration is read-only: edit
`services.moonshine.settings` and rebuild instead.

## Stream status

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
Diagnostics → Informational findings**; this browser preference applies at once
and **Copy diagnostics** still includes every finding.

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
support bundle** downloads a `.tar.gz` with that report, the last two minutes of
stream statistics, host and GPU details, the configuration and the service log
through your browser. Both are redacted: home directory, user and host
names, the server name, client names and IDs, certificate fingerprints and IP
addresses are replaced with placeholders.

## Session history

**History** lists past sessions, newest first: the application (and the game
started from it), the client, when and how long, the negotiated mode, codec and
HDR, disconnects and resumes, and how the session ended. A session shows its
timeline and a summary measured on the host: frame rate, bitrates, dropped
frames and packets, per-stage times, GPU load and temperature, and recurring
findings, compared with the previous session of the same application. The host
cannot measure what the client displayed; nothing in the history comes from the
client. **Only unexpected stops** lists failed sessions.

History is stored on the host (`~/.local/share/moonshine/history.sqlite3`),
kept for 90 days and at most 1000 sessions by default; see
[`[history]`](CONFIGURATION.md#history) to change this, stop recording client
addresses, or turn history off. **Clear** removes it. A history that cannot be
written (full or read-only disk) is reported on the page; streaming is never
affected.

## Updates

**Updates** checks the project's releases on GitHub: once a day when you turn
on automatic checks (off by default), or when you select **Check now**. Choose
stable releases only or include prereleases. When a newer release exists, the
page links to its release notes and shows the update steps for how this host
was installed (pacman repository, deb/rpm package, portable installer or Nix).
Pyroshine never downloads or installs anything itself. Dismiss a release to
stop the reminder for it.

## Maintenance

**Maintenance** shows the running version, configuration file and process,
**Restart Pyroshine**, the commands to restart the service and read its log,
the administrator password and the signed-in browsers.

## Desktop tray

The service shows a status icon with pairing notifications while you are
signed in to a desktop: it starts the tray when your desktop session appears
(also when you log in after the service started) and stops it when you log
out. On a headless host, or with `[management] tray = false`, it never starts.
The tray is a separate process of the same program and holds no streaming
resources: hiding it or a crash leaves the server, the web interface and any
stream running; after a crash the service starts it again. Every management task
opens in the browser, where you sign in as usual; the tray never passes a
password or token to the page.

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

Left-click opens the dashboard, or the waiting pairing request. The menu shows
the session (application, mode, codec, HDR, client) and opens **Pyroshine**,
**Pair a Client** (or the waiting request), **Session History** and
**Updates**. **End Session** closes the streamed application and disconnects
the client directly, without a browser; save progress first. **Set Up Web
Access** shows a setup code. **Hide Tray Icon** closes only the tray, until
your next desktop login or service restart.

Pairing requests share one notification, replaced rather than repeated when a
client retries; clicking it opens that request. Failed launches and unexpected
stops notify at most once a minute. Without the tray, the web interface shows
the same requests and problems, and the server shows a basic notification of
its own.

KDE Plasma shows the icon natively. GNOME needs the AppIndicator and
KStatusNotifierItem Support extension; other panels need a StatusNotifierItem
host. Without one, use the application-menu entry (`pyroshine open` from a
terminal). To turn the tray off, set **Settings → General → Desktop tray icon**
off (`[management] tray = false`) and restart the service.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| The page does not load | Check `systemctl status "pyroshine@$USER"` and that the log shows **Web management interface available**. Over SSH, keep the tunnel open. |
| **Pyroshine can't be reached** on the page | The service stopped or restarts; the page reconnects by itself. |
| **Open Pyroshine through its own address** | Use `127.0.0.1` (or `localhost`), or add the name to `allowed_hosts`. |
| The page says it is from another version | Reload it after an upgrade. |
| No notification | Open **Clients**, retry pairing, check desktop notification settings. |
| A launch or stream fails | Open **Diagnostics → Recent problems** for the cause and fix; see [failure causes](TIPS.md#read-why-a-launch-or-stream-failed). |
| No request | Check server/network; enable **Settings → Pairing & security → Allow new pairings**, save and restart. [More pairing help](TIPS.md#pairing-does-not-complete). |
| No tray icon | Check desktop support above and that the service runs as your user. The service log says when it found your desktop session (`journalctl -u "pyroshine@$USER" -g tray`). |
| The browser does not open | Install `xdg-desktop-portal` or `xdg-utils`, or open the address yourself. |

Use `sudo journalctl` if journal access is denied.
