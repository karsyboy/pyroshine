# Installation and upgrades

Use the [README](../README.md#quick-start) for the recommended Arch/CachyOS
pacman setup, startup, pairing and first stream.

## Platform compatibility

Prebuilt native and portable releases target **x86_64 Linux**, built on Ubuntu
24.04 with a **glibc 2.39 baseline**. Check `getconf GNU_LIBC_VERSION` on older
systems. The availability of `.deb`/`.rpm` files does not guarantee every
Debian, Ubuntu, Fedora or RHEL release works. Upgrade an older distribution or
[build from source](../CONTRIBUTING.md#manually-building-the-app); do not replace
its system glibc manually.

All setups need systemd, the streaming user's systemd manager/session bus,
GPU render access, EGL/GBM capture and Vulkan DMA-BUF import on the **same GPU**.
H.264/HEVC/AV1 require usable Vulkan Video encode profiles. The healthcheck
verifies the actual driver and profiles; hardware family alone is insufficient.
The kernel must allow `kcmp(2)`; restrictive sandboxes may block it.
A monitor or active desktop compositor is unnecessary.

PyroWave additionally needs Pyrolight, the matching encoder library (bundled),
and hardware Vulkan compute, timeline semaphores and external memory support.
Software Vulkan is rejected. HDR needs a compatible game, 10-bit profile and
client/display; VRR needs Pyrolight's VRR presentation and a compatible display.
These are optional; see [PyroWave](PYROWAVE.md) and [HDR](NATIVE_PRESENTATION.md).

## Native packages

For maintained Arch/CachyOS updates, use the [pacman repository](../README.md#1-install).
Otherwise download the server and optional `pyroshine-ui` from the same
[release](https://github.com/karsyboy/pyroshine/releases). In the download
directory, keep one version of each and run your distribution's command:

| Distribution | Server | Optional desktop app (add to the same install command) |
| --- | --- | --- |
| Arch / CachyOS | `sudo pacman -U ./pyroshine-[0-9]*.pkg.tar.zst` | `./pyroshine-ui-[0-9]*.pkg.tar.zst` |
| Debian / Ubuntu | `sudo apt install ./pyroshine_*.deb` | `./pyroshine-ui_*.deb` |
| Fedora / RHEL | `sudo dnf install ./pyroshine-[0-9]*.rpm` | `./pyroshine-ui-[0-9]*.rpm` |

The package manager resolves runtime dependencies. Packages install the server,
PyroWave library, systemd unit, udev rules, `uinput`/`uhid` module configuration
and sleep-inhibition policy. The GUI adds GTK 3/WebKitGTK 4.1 and desktop
autostart. Steam and a suitable GPU driver must already be installed separately.
Packages do not start the server or open the firewall.

Continue with [Start → Pair → Play](../README.md#2-start).

## Portable installer and SteamOS

Run as the normal streaming user with sudo access, curl, tar/zstd and systemd.
On SteamOS, use Desktop Mode; set a password with `passwd` if sudo needs one.

```sh
curl -fsSL https://github.com/karsyboy/pyroshine/releases/latest/download/pyroshine-install.sh -o pyroshine-install.sh
bash pyroshine-install.sh --no-start --no-healthcheck
```

Choose your user; enable lingering for logged-out use. The installer writes the
server/library to `/opt/pyroshine` and integration to `/etc`. It registers files
for SteamOS atomic updates when `/etc/atomic-update.conf.d` exists. Compatibility
with every future SteamOS update is not guaranteed.

Check before starting; fix any `FAIL`, then run:

```sh
/opt/pyroshine/start-pyroshine.sh "$HOME/.config/moonshine/config.toml" healthcheck
sudo systemctl enable --now "pyroshine@$USER"
systemctl status "pyroshine@$USER" --no-pager
journalctl -u "pyroshine@$USER" -b -n 60 --no-pager
```

The host supplies Opus, libevdev, libxkbcommon, GBM/EGL, Wayland, Vulkan/driver,
C++ runtime and Xwayland; the archive does not bundle them. Use your host's
supported package mechanism for missing libraries. See
[dependency troubleshooting](TIPS.md#missing-libraries-or-an-older-system).
The installer does not include the GUI.

For unattended headless installation:

```sh
bash pyroshine-install.sh --user "$USER" --enable --linger --no-start --no-healthcheck
/opt/pyroshine/start-pyroshine.sh "$HOME/.config/moonshine/config.toml" healthcheck && sudo systemctl start "pyroshine@$USER"
```

The interactive installer's own check runs before startup but ignores failures;
the separate check above lets you fix them first. A new login shell adds the
binary to PATH; the wrapper works immediately.

[Pair below](#pair-without-the-desktop-app), then [play](../README.md#4-play).
For a first SteamOS stream, stay in Desktop Mode and quit Steam before launching
streamed Steam. Game Mode's same-user Steam can take over the launch; switch to
Desktop Mode and quit it, or configure a different application. For logged-out
operation, apply headless setup below.

Local build artifacts use the [manual installation guide](../CONTRIBUTING.md#manual-installation-and-upgrade).

## Headless setup

Run as the streaming user; replace `$USER` when administering another account:

```sh
sudo loginctl enable-linger "$USER"
# Needed for gamepads without an active desktop seat:
sudo usermod -aG input "$USER"
```

Log out/in after changing groups; reboot if the running user manager retains old
groups. Lingering keeps the user's systemd manager and bus alive while logged
out. Active desktop seats normally provide input ACLs instead of needing
`input`. Check [input access](TIPS.md#keyboard-mouse-or-gamepads-do-not-work)
and [GPU access](TIPS.md#healthcheck-fails) if diagnostics report denied devices.

Use the [README check/start sequence](../README.md#2-start), then pair below.
The native/portable service grants `pyroshine` for sleep inhibition; direct
binary launches need their own membership. [NixOS](NIXOS.md) manages lingering
and uses different service/group names.

## Pair without the desktop app

1. On the host, run `pyroshine pair` as the streaming user, without sudo.
   Over SSH, log in as that user; use `ssh -t user@host 'pyroshine pair'` for
   a single-command session. Portable installs can run
   `/opt/pyroshine/start-pyroshine.sh pair`; NixOS uses `moonshine pair`.
2. Select the host in Moonlight/Pyrolight and start pairing. If it is missing,
   add its LAN/VPN IP. The command waits up to five minutes for a request.
3. Check the requester address and fingerprint. If several requests exist,
   select yours. Confirm, then enter the PIN shown by your device. Wait for
   **Device paired** and the client's application list; select Steam to play.

Optional: `pyroshine pair --name "Living room"` names the paired device.
The command needs the server's user/session bus. A wrong PIN, expired/replaced
request or server restart requires a new attempt.

The subcommand requires a build containing this change. For older releases or
an unavailable management interface, use the browser workflow below.

## Browser pairing

1. Start pairing in the client while watching the host's log:

   ```sh
   journalctl -u "pyroshine@$USER" -f -o cat
   ```

   Use `sudo journalctl` if access is denied; NixOS uses `-u moonshine`.
2. Open the complete latest URL from **Waiting for the host operator to enter
   the PIN**, shaped like `http://localhost:47989/pin?uniqueid=...`.
   Keep the encoded query; bare `/pin` cannot select a request.
3. Check **Requested by**, enter the client PIN and select **Submit**. **PIN
   accepted** means submission, not completion; wait for applications in the
   client. Expired/replaced requests need a fresh pairing/link.

For a browser on another computer, keep this SSH tunnel running there first:

```sh
ssh -N -L 127.0.0.1:47989:127.0.0.1:47989 user@host
```

Open the copied localhost URL on that computer. Both ports must equal the
configured HTTP port; a different local port fails Host/origin validation.
If occupied, use the CLI on the host instead. This tunnel carries approval
only; the streaming client still needs LAN/VPN access. The page supplies the
request-specific token automatically; see [security](SECURITY_ADMINISTRATION.md).

## Network and firewall

Allow these host ports **only from your trusted LAN/VPN**. Packages and the
portable installer do not configure the firewall.

| Protocol | Default ports | Purpose |
| --- | --- | --- |
| TCP | 47989, 47984, 48010 | HTTP, HTTPS, RTSP |
| UDP | 47998, 47999, 48000 | Video, control/input, audio |
| UDP | 5353 | Local mDNS discovery |

With UFW, replace the subnet with your trusted LAN:

```sh
sudo ufw allow from 192.168.1.0/24 to any port 47984 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 47989 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 48010 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 47998:48000 proto udp
```

Manual IP addition avoids mDNS when multicast is blocked or subnets differ.
Guest Wi-Fi isolation can block all host access. Custom ports need matching
firewall/client settings. NixOS `openFirewall` opens streaming ports globally;
restrict public interfaces separately. The protocol is not fully encrypted;
do not forward these ports to the internet. Use a VPN.

## Upgrades and configuration

- Pacman repository: `sudo pacman -Syu`.
- Downloaded packages: install a new release using [the package commands](#native-packages).
- Portable: download and rerun the latest installer. The copy under
  `/opt/pyroshine/bin/` reinstalls its original version.
- NixOS: update the flake input and rebuild; see [NixOS](NIXOS.md).

After a native package upgrade, restart `sudo systemctl restart "pyroshine@$USER"`.
Update server and GUI together. Restarts end active streams; they also apply
settings/GUI saves and refresh scanners.

Back up `~/.config/moonshine` (config/TLS identity) and
`~/.local/share/moonshine` (pairing state) privately before migration.
Do not rename these inherited paths. Native/portable units explicitly use
`/home/<user>/.config/moonshine/config.toml`; other homes/paths need a
[service override](CONFIGURATION.md#file-location-and-loading).
Older builds may need [presentation migration](NATIVE_PRESENTATION.md#upgrade-migration).

## Service diagnostics

```sh
systemctl status "pyroshine@$USER" --no-pager
journalctl -u "pyroshine@$USER" -b -n 100 --no-pager
```

Look for the latest **Pyroshine is ready and waiting for connections.** An
active process may still be waiting for D-Bus or restarting after failure.
Desktop login or lingering provides the user's runtime directory/bus.
Use `sudo journalctl` if needed. See [host troubleshooting](TIPS.md) or
[application logs](TIPS.md#debug-a-failing-application).
