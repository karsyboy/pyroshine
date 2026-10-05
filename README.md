<p align="center">
  <img src="./assets/logo-with-text.png" width="25%"/>
</p>

<p align="center">
    <a href="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml">
        <img src="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml/badge.svg"></a>
    <a href="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml">
        <img src="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml/badge.svg"></a>
<p>

Pyroshine is a Linux game-streaming server based on
[Moonshine](https://github.com/hgaiser/moonshine). It runs applications in isolated,
headless sessions and streams them to Moonlight-compatible clients.

- Native PyroWave 4:2:0 and 4:4:4 streaming in SDR and HDR10.
- H.264, HEVC, and AV1 through Vulkan Video.
- [Native Wayland HDR](docs/NATIVE_PRESENTATION.md) through standard color management; ordinary XWayland applications remain supported as SDR.
- Hardware encoding with DMA-BUF import, low-latency transport, and forward error correction.
- Keyboard, mouse, touch, pen, controller, haptics, and surround audio support.
- Optional desktop app with a system tray, pairing notifications, client management,
  a settings editor, and a live stream dashboard. The server runs headless without it.

Use [Pyrolight](https://github.com/karsyboy/pyrolight)
for PyroWave streaming. Standard Moonlight clients can use the conventional codecs.

## Requirements

A Linux host with systemd, a working Wayland/Vulkan stack, and a supported GPU
and driver. PyroWave requires hardware Vulkan with compute, timeline semaphore,
external-memory, and DMA-BUF support; software Vulkan devices are rejected.
Run `pyroshine healthcheck` after installation to check your host.

## Quick start

Download a package from the [releases page](https://github.com/karsyboy/pyroshine/releases)
and install it with your distribution's package manager:

| Distribution | Install or upgrade |
| --- | --- |
| Arch Linux / CachyOS | `sudo pacman -U ./pyroshine-*.pkg.tar.zst` |
| Debian / Ubuntu | `sudo apt install ./pyroshine_*.deb` |
| Fedora / RHEL | `sudo dnf install ./pyroshine-*.rpm` |

Then start Pyroshine for your user:

```sh
sudo systemctl enable --now "pyroshine@$USER"
```

For SteamOS or a portable installation, see the [installation guide](docs/INSTALLATION.md).
NixOS users should use the [NixOS module](docs/NIXOS.md).

Pyroshine creates `~/.config/moonshine/config.toml` on first start, with Steam
and a Steam library scanner enabled by default. To configure another application,
add an entry to that file:

```toml
[[application]]
title = "My game"
command = ["/absolute/path/to/game"]
```

Restart the service after editing:

```sh
sudo systemctl restart "pyroshine@$USER"
```

Add the host in Moonlight and enter the PIN it shows on the host, either in the
[desktop app](#desktop-app) or on the host-local page linked in the service log
(`http://localhost:47989/pin?uniqueid=…`). The page only accepts requests from the
host itself (see [headless pairing](docs/CONFIGURATION.md#webserver)).

Use a trusted LAN or VPN and restrict the GameStream ports with a firewall.
Do not expose Pyroshine directly to the public internet.

## Desktop app

On a desktop, install the optional `pyroshine-ui` package from the same release
to manage Pyroshine without editing files or reading logs:

| Distribution | Install or upgrade |
| --- | --- |
| Arch Linux / CachyOS | `sudo pacman -U ./pyroshine-ui-*.pkg.tar.zst` |
| Debian / Ubuntu | `sudo apt install ./pyroshine-ui_*.deb` |
| Fedora / RHEL | `sudo dnf install ./pyroshine-ui-*.rpm` |

It starts in the system tray at login, or open **Pyroshine** from the application
menu. It provides:

- **Tray icon** showing whether a client is streaming (green play), the game is
  still running after the client disconnected (amber pause), or a client is
  waiting to pair, with an **End Session** action.
- **Pairing** from a notification: check the requesting device and enter the
  PIN Moonlight shows. Paired clients can be named and revoked.
- **Settings** for every `config.toml` option, including applications and
  scanners. Changes are validated before saving and keep your comments; restart
  the service to apply them.
- **Dashboard** with the application, negotiated video and audio format, client,
  and per-second frame rate, bitrate, and pipeline timing while streaming.

The app manages a running `pyroshine@<user>` service; it never starts or stops it,
and quitting the app does not affect a stream. It needs WebKitGTK and GTK 3, and a
StatusNotifierItem tray (KDE Plasma natively, GNOME with the AppIndicator
extension). See the [desktop app guide](docs/DESKTOP.md) for details and
troubleshooting.

## Documentation

- [Installation, upgrades, and headless setup](docs/INSTALLATION.md)
- [Desktop app: tray, pairing, and settings](docs/DESKTOP.md)
- [Complete config.toml reference](docs/CONFIGURATION.md)
- [Tips and troubleshooting](docs/TIPS.md)
- [Pairing, revocation, and state recovery](docs/SECURITY_ADMINISTRATION.md)
- [Changelog](docs/CHANGELOG.md)
- [All documentation](docs/README.md)

For development, start with the [architecture overview](docs/ARCHITECTURE.md)
and the [contributor guide](CONTRIBUTING.md) (manual builds, validation, and
releases).

## AI-assisted development

Pyroshine is developed with substantial assistance from AI coding tools,
including Codex, for implementation, refactoring, testing, documentation,
debugging, and code review. The maintainer intentionally guides architectural
direction and project decisions.

AI-generated or AI-modified code is not assumed correct because it came from AI.
All changes are expected to meet the same review, testing, validation,
maintainability, security, and compatibility standards, whether written by a
human or with AI assistance.

## License and credits

Pyroshine is an independent community fork of
[Moonshine](https://github.com/hgaiser/moonshine), which builds on the Moonlight
ecosystem and work pioneered by [Sunshine](https://github.com/LizardByte/Sunshine).
It retains Moonshine's internal crate names, configuration paths, and protocol
names to ease upstream synchronization. The client fork is
[Pyrolight](https://github.com/karsyboy/pyrolight), and
the codec is the pinned [`karsyboy/pyrowave`](https://github.com/karsyboy/pyrowave)
fork of [PyroWave](https://github.com/Themaister/pyrowave).

Licensed under the [BSD 2-Clause License](LICENSE). Original copyright notices
are preserved; dependencies retain their own licenses.
