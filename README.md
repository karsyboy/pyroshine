# Pyroshine

<p align="center"><img src="assets/logo-no-text.png" alt="Pyroshine" width="160"></p>

<p align="center">
  <a href="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml"><img src="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml/badge.svg" alt="CI status"></a>
  <a href="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml"><img src="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml/badge.svg" alt="Release status"></a>
</p>

Pyroshine is a Linux game-streaming server based on
[Moonshine](https://github.com/hgaiser/moonshine). It streams games to
[Moonlight](https://moonlight-stream.org/) and
[Pyrolight](https://github.com/karsyboy/pyrolight) in a separate session,
so your local desktop stays usable. A monitor or desktop login is optional.

- Hardware H.264, HEVC, and AV1 encoding for Moonlight-compatible clients.
- GPU PyroWave encoding in 4:2:0 and 4:4:4, SDR and HDR10, for Pyrolight.
- [Native HDR](docs/NATIVE_PRESENTATION.md) with compatible games and clients.
- VRR capture that follows the game's frame timing when Pyrolight requests it.
- Keyboard, mouse, touch, pen, controllers, haptics, and surround audio.
- Native model support for Xbox Elite, classic Steam Controller, Steam Deck,
  and DualSense Edge with matching Pyrolight clients; see
  [controller support and limits](docs/NATIVE_CONTROLLERS.md).
- A [web management interface](docs/MANAGEMENT.md) for pairing, settings,
  clients, stream status, session history and diagnostics, the same on a
  desktop or a headless server, plus an optional desktop tray.

PyroWave is an intra-only wavelet codec designed for high-bandwidth local
networks. Select it explicitly in Pyrolight; Moonlight clients use the
standard codecs.

## Requirements

- Linux with systemd. Prebuilt packages and portable releases require
  **x86_64 and glibc 2.39 or later**; see
  [platform compatibility](docs/INSTALLATION.md#platform-compatibility).
- A GPU and driver with EGL/GBM capture and Vulkan DMA-BUF import on the same
  GPU. H.264, HEVC, and AV1 also need usable Vulkan Video encode profiles.
  Run the healthcheck below to verify your setup.
- Native Steam at `/usr/bin/steam` for the default applications. Install Steam
  and your games separately; other launchers and Flatpak Steam need
  [configuration](docs/TIPS.md#steam-or-games-are-missing).
- Moonlight 6.0 or later, or Pyrolight, on a trusted LAN or VPN. Allow the
  [streaming ports](docs/INSTALLATION.md#network-and-firewall) through the host
  firewall; keep them off the public internet.

PyroWave additionally requires Pyrolight and a hardware Vulkan GPU that passes
Pyroshine's PyroWave probe. Its matching encoder library is included in releases.
HDR needs a compatible game, GPU profile, client, and display; VRR presentation
needs Pyrolight and a VRR display. Both are optional.

## Installation

Download the file for your system from the
[releases page](https://github.com/karsyboy/pyroshine/releases), or use the
pacman repository on Arch Linux / CachyOS:

| System | Download | Install |
| --- | --- | --- |
| Arch Linux / CachyOS (pacman repository) | `pyroshine-bin` | Add the [`[pyrowave]` repository](https://github.com/karsyboy/pyrowave-packages#setup), then `sudo pacman -Syu pyroshine-bin` |
| Arch Linux / CachyOS | `pyroshine-*.pkg.tar.zst` | `sudo pacman -U ./pyroshine-[0-9]*.pkg.tar.zst` |
| Debian / Ubuntu | `pyroshine_*.deb` | `sudo apt install ./pyroshine_*.deb` |
| Fedora / RHEL | `pyroshine-*.rpm` | `sudo dnf install ./pyroshine-[0-9]*.rpm` |
| SteamOS / other Linux | `pyroshine-install.sh` | Follow the [portable installation guide](docs/INSTALLATION.md#portable-installer-and-steamos) |
| NixOS | Flake / service module | Follow the [NixOS guide](docs/NIXOS.md) |

Every installation is one package with the server, its web management
interface and a desktop tray: while you are signed in to a desktop, the
service shows a status icon with pairing notifications; on a headless host it
skips it. Nothing needs GTK, WebKitGTK or Node.js.

- Native packages install the server, PyroWave library, service, and device
  permissions. They do not start the server or configure the firewall.
- Package availability does not guarantee compatibility with older distribution
  releases. Check the glibc baseline above before installing.
- Each release includes `SHA256SUMS` for checking downloads.
- To upgrade, install the newer release the same way, or run `sudo pacman -Syu`
  with the repository, then restart the service; this ends active streams. See [upgrades](docs/INSTALLATION.md#upgrades-and-configuration).

## Quick start

These steps assume a native package installation. Run commands on the host as
the user who owns your Steam library. Portable and NixOS installations use
the commands in their installation guides.

### 1. Install

Install the server using the [installation options above](#installation).
Install Steam and your games as the same user before starting Pyroshine.

### 2. Start

For headless use, keep the user's session alive while logged out and grant
access to gamepad devices:

```sh
sudo loginctl enable-linger "$USER"
sudo usermod -aG input "$USER"
```

Log out and back in after changing groups; reboot if the user manager retains
old groups. A normal desktop session with working device permissions does not
need these headless steps.

Run the healthcheck **before starting the server**; it checks the configured
ports and reports conflicts if the server is already running:

```sh
pyroshine "$HOME/.config/moonshine/config.toml" healthcheck
```

Fix any `FAIL` results using [healthcheck troubleshooting](docs/TIPS.md#healthcheck-fails),
then start the service:

```sh
sudo systemctl enable --now "pyroshine@$USER"
systemctl status "pyroshine@$USER" --no-pager
journalctl -u "pyroshine@$USER" -b -n 60 --no-pager
```

Expect **active (running)** and **Pyroshine is ready and waiting for connections.**
Fresh configurations include Steam and Steam library scanning automatically.

### 3. Set up the web interface

Pyroshine serves its management interface at <http://127.0.0.1:47990/> on the
host. Choose the administrator password once with a setup code:

```sh
pyroshine web setup
```

Open the printed address, enter the code and a password. On a desktop, the
**Pyroshine** menu entry or the tray opens the same page. On a headless host,
forward the port from your computer first and open the address there:

```sh
ssh -L 47990:127.0.0.1:47990 user@host
```

See [the web interface guide](docs/MANAGEMENT.md) for details and remote access.

### 4. Pair

Select the host in Moonlight or Pyrolight to start pairing. In the web
interface, open **Clients** (or the tray's pairing notification), verify the
requester, enter the client's PIN, and select **Pair**. Wait for the device
under **Paired clients** and the application list on the client.

To pair from a terminal instead, run this on the host or in an SSH terminal as
the streaming user, without sudo:

```sh
pyroshine pair
```

Start pairing in the client, verify and confirm the requester in the terminal,
and enter the client's PIN when prompted. Wait for **Device paired**.

If the host is missing, add its LAN or VPN IP manually in the client. Run
`hostname -I` on the host to find its addresses.

### 5. Play

Quit Steam on the host's local desktop first. In Moonlight or Pyrolight,
select **Steam** or an installed game and sign in if needed.

Installed and removed games appear in clients' lists on their own. Add other
applications on the web interface's **Applications** page or follow the
[configuration guide](docs/CONFIGURATION.md#application). Saved settings and
applications apply after a restart, which ends active streams: choose
**Restart now** in the web interface, or run:

```sh
sudo systemctl restart "pyroshine@$USER"
```

## Streaming with PyroWave

1. Pair Pyrolight with the host using the steps above.
2. In Pyrolight's **Settings**, select **PyroWave** as the **Video codec**.
   The option appears when the client's GPU passes its decoder probe.
3. Start with SDR and 4:2:0. Enable **HDR** or **YUV 4:4:4** when the host,
   game, and client support the selected mode.
4. Use **Calibrate network bandwidth** in Pyrolight to choose a bitrate your
   network can sustain. PyroWave needs much more bandwidth than standard codecs.

Unsupported modes fail with an error instead of silently changing codecs.
See the [Pyrolight streaming guide](https://github.com/karsyboy/pyrolight#streaming-with-pyrowave)
for client settings and bitrate guidance.

## Troubleshooting

- **Host missing or connection fails:** check the service's ready message,
  add the host IP manually, and verify the
  [firewall ports](docs/INSTALLATION.md#network-and-firewall).
- **Server fails to start:** read the service log above, then follow
  [healthcheck troubleshooting](docs/TIPS.md#healthcheck-fails). Stop a running
  server before repeating the standalone healthcheck.
- **Pairing does not finish:** retry with a fresh request and PIN. If no
  notification appears, open **Clients** in the web interface. See
  [pairing help](docs/TIPS.md#pairing-does-not-complete).
- **Steam opens locally or the client reports 503:** quit the host's desktop
  Steam before launching streamed Steam. See the
  [Steam workaround](docs/TIPS.md#close-a-desktop-steam-before-streaming-steam).
- **Gamepads do not work when logged out:** check
  [headless setup](docs/INSTALLATION.md#headless-setup) and
  [input permissions](docs/TIPS.md#keyboard-mouse-or-gamepads-do-not-work).
- **PyroWave is unavailable:** check the host's healthcheck or the web
  interface's **Diagnostics**, use matching server/client releases, and compare with
  H.264, HEVC, or AV1 to isolate codec-specific issues.

When [reporting a bug](https://github.com/karsyboy/pyroshine/issues), include the
release, OS, GPU and driver, client version, stream settings, and host and client
logs. Capture host logs with `journalctl -u "pyroshine@$USER" -b --no-pager`;
use `sudo journalctl` if access is denied. See
[application logs](docs/TIPS.md#debug-a-failing-application) for game launch failures.

## Documentation

- [Installation, headless setup, networking, and upgrades](docs/INSTALLATION.md)
- [Web interface and tray](docs/MANAGEMENT.md)
- [Configuration reference](docs/CONFIGURATION.md)
- [Troubleshooting and application recipes](docs/TIPS.md)
- [Security administration and recovery](docs/SECURITY_ADMINISTRATION.md)
- [NixOS installation](docs/NIXOS.md)
- [Native HDR and presentation](docs/NATIVE_PRESENTATION.md)
- [All documentation](docs/README.md)
- [Building, testing, and releasing](CONTRIBUTING.md)

## AI-assisted development

AI coding tools, including Codex, assist implementation, testing and documentation.
The maintainer directs the project. All changes require the same review,
validation and security standards, whether written by a human or with AI assistance.

## License and credits

Pyroshine is an independent community fork of
[Moonshine](https://github.com/hgaiser/moonshine), which builds on the Moonlight
ecosystem and work pioneered by [Sunshine](https://github.com/LizardByte/Sunshine).
It retains Moonshine's internal names and paths for compatibility. The client fork is
[Pyrolight](https://github.com/karsyboy/pyrolight), and
the codec is the pinned [`karsyboy/pyrowave`](https://github.com/karsyboy/pyrowave)
fork of [PyroWave](https://github.com/Themaister/pyrowave).

VRR streaming is based on [Nonary](https://github.com/Nonary)'s idea and
reference implementations in [Vibepollo](https://github.com/Nonary/Vibepollo)
and [moonlight-qt](https://github.com/Nonary/moonlight-qt)
([moonlight-stream/moonlight-qt#1956](https://github.com/moonlight-stream/moonlight-qt/pull/1956)).

Licensed under the [BSD 2-Clause License](LICENSE). Original copyright notices
are preserved; dependencies retain their own licenses.
