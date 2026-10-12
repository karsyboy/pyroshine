<p align="center">
  <img src="./assets/logo-with-text.png" width="25%" alt="Pyroshine"/>
</p>

<p align="center">
  <a href="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml"><img src="https://github.com/karsyboy/pyroshine/actions/workflows/ci.yaml/badge.svg" alt="CI status"/></a>
  <a href="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml"><img src="https://github.com/karsyboy/pyroshine/actions/workflows/release.yaml/badge.svg" alt="Release status"/></a>
</p>

# Pyroshine

Stream Linux games to [Moonlight](https://moonlight-stream.org/) or
[Pyrolight](https://github.com/karsyboy/pyrolight) in a separate session.
Your desktop stays usable; a monitor or desktop login is optional.

## Requirements

- Linux with systemd; prebuilt packages need **x86_64 and glibc 2.39+**.
- Compatible GPU/driver: EGL/GBM, DMA-BUF and Vulkan Video encoding for
  H.264/HEVC/AV1. The healthcheck verifies support; see [compatibility](docs/INSTALLATION.md#platform-compatibility).
- Native Steam at `/usr/bin/steam` for the default applications.
- Moonlight 6.0+ or Pyrolight on a trusted LAN/VPN; allow the
  [firewall ports](docs/INSTALLATION.md#network-and-firewall).

PyroWave needs Pyrolight. HDR/VRR are optional. Keep ports off the public internet.

## Quick start

Run on the host as your Steam user. Install Steam separately.

### 1. Install

**Arch / CachyOS:** add the signed pacman repository once, then install:

```sh
curl -fsSL https://raw.githubusercontent.com/karsyboy/pyrowave-packages/main/keys/pyrowave.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key A8054CC188EAE0293E3EA5B509DC88C65239130E
printf '\n[pyrowave]\nServer = https://github.com/karsyboy/pyrowave-packages/releases/download/x86_64\n' | sudo tee -a /etc/pacman.conf
sudo pacman -Syu pyroshine-bin pyroshine-ui-bin
```

Repository already configured? Run only the last command.
`pyroshine-bin` is the server; omit the optional `pyroshine-ui-bin` for headless use.

Other installs: [Arch/Debian/Ubuntu/Fedora/RHEL downloads](docs/INSTALLATION.md#native-packages),
[SteamOS/portable](docs/INSTALLATION.md#portable-installer-and-steamos), [NixOS](docs/NIXOS.md).

### 2. Start

**Headless only:** keep the user session alive while logged out; add `input`
for headless gamepads:

```sh
sudo loginctl enable-linger "$USER"
sudo usermod -aG input "$USER"
```

Log out/in after changing groups; reboot if the user manager retains old groups.

Check **before starting**; a running server causes port-conflict errors:

```sh
pyroshine "$HOME/.config/moonshine/config.toml" healthcheck
```

Fix `FAIL` results using [troubleshooting](docs/TIPS.md#healthcheck-fails). Then start:

```sh
sudo systemctl enable --now "pyroshine@$USER"
systemctl status "pyroshine@$USER" --no-pager
journalctl -u "pyroshine@$USER" -b -n 60 --no-pager
```

Expect **active (running)** and **Pyroshine is ready and waiting for connections.**
Steam and library scanning are configured automatically.

### 3. Pair

**Desktop:** open **Pyroshine** (`pyroshine-ui`). Start pairing in
Moonlight/Pyrolight, then open the host's notification or **Clients**.
Verify the requester, enter the client's PIN and select **Pair**.
Wait for **Paired clients** and the client's application list.
The app cannot start/restart the server.

**Headless / no GUI:** run on the host or in an SSH terminal, without sudo:

```sh
pyroshine pair
```

Start pairing in the client; verify the requester, confirm and enter its PIN.
Wait for **Device paired**. Older releases use [browser pairing](docs/INSTALLATION.md#browser-pairing).

Host missing? Add its LAN/VPN IP (`hostname -I` on the host).
No notification? Open **Clients**, then retry.

### 4. Play

Quit desktop Steam first. In Moonlight/Pyrolight, select **Steam** or an installed
game; sign in if needed. No manual configuration required.

After installing games or saving GUI settings, restart to apply changes
(this ends active streams):

```sh
sudo systemctl restart "pyroshine@$USER"
```

Add other applications in GUI **Settings** or [configuration](docs/CONFIGURATION.md#application).

## More help

- [Installation/headless/upgrades](docs/INSTALLATION.md)
- [Desktop app](docs/DESKTOP.md)
- [Troubleshooting](docs/TIPS.md) 
- [Configuration](docs/CONFIGURATION.md)
- [Security/recovery](docs/SECURITY_ADMINISTRATION.md)
- [All docs](docs/README.md)

## Features

- Hardware H.264/HEVC/AV1
- PyroWave 4:2:0/4:4:4
- [native HDR](docs/NATIVE_PRESENTATION.md) and VRR with compatible clients ([Pyrolight](https://github.com/karsyboy/pyrolight)) / displays. 
- Keyboard, mouse, touch, pen, controllers, haptics and surround audio. 
- Optional desktop pairing, settings and status.

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
