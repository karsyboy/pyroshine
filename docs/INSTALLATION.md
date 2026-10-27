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
H.264/HEVC/AV1 use Vulkan Video encode profiles where the driver offers them,
otherwise VA-API or NVENC. The healthcheck verifies the actual driver and
profiles; hardware family alone is insufficient. A GPU without any usable
encoder still starts and offers only PyroWave; see
[GPU compatibility](GPU_COMPATIBILITY.md) for verified GPUs and drivers.

Intel GPUs encode H.264/HEVC/AV1 through VA-API, which needs libva and
Intel's media driver; packages recommend libva but do not install the driver:

| Distribution | Install |
| --- | --- |
| Arch / CachyOS | `sudo pacman -S libva intel-media-driver` |
| Debian / Ubuntu | `sudo apt install libva2 libva-drm2 intel-media-va-driver-non-free` |
| Fedora | `sudo dnf install libva intel-media-driver` (RPM Fusion) |

NVENC uses the NVIDIA driver's own libraries and needs nothing extra.
Linux 5.3 or later is needed. Where a sandbox blocks `kcmp(2)` (Podman's default
seccomp profile, Flatpak), Pyroshine compares DMA-BUF inode numbers instead.
A monitor or active desktop compositor is unnecessary.

PyroWave additionally needs Pyrolight, the matching encoder library (bundled),
and hardware Vulkan compute, timeline semaphores and external memory support.
Software Vulkan is rejected. HDR needs a compatible game, 10-bit profile and
client/display; VRR needs Pyrolight's VRR presentation and a compatible display.
These are optional; see [PyroWave](PYROWAVE.md) and [HDR](NATIVE_PRESENTATION.md).

## Native packages

For maintained Arch/CachyOS updates, use the [pacman repository](../README.md#1-install).
Otherwise download the package for your distribution from the
[release](https://github.com/karsyboy/pyroshine/releases). In the download
directory, keep one version and run your distribution's command:

| Distribution | Install |
| --- | --- |
| Arch / CachyOS | `sudo pacman -U ./pyroshine-[0-9]*.pkg.tar.zst` |
| Debian / Ubuntu | `sudo apt install ./pyroshine_*.deb` |
| Fedora / RHEL | `sudo dnf install ./pyroshine-[0-9]*.rpm` |

The package manager resolves runtime dependencies. The package installs the
server, its web management interface, the desktop tray, the **Pyroshine**
application-menu entry, PyroWave library, systemd unit, udev rules,
`uinput`/`uhid` module configuration and sleep-inhibition policy. It replaces
the older `pyroshine-ui` desktop app. The service shows the tray only while you
are signed in to a desktop; it needs no GUI toolkit.
Steam and a suitable GPU driver must already be installed separately. Packages
do not start the server or open the firewall.

Continue with [Start → Set up → Pair → Play](../README.md#2-start).

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
The installer also deploys the web interface and its application-menu entry;
the service shows the desktop tray itself in desktop sessions only.

For unattended headless installation:

```sh
bash pyroshine-install.sh --user "$USER" --enable --linger --no-start --no-healthcheck
/opt/pyroshine/start-pyroshine.sh "$HOME/.config/moonshine/config.toml" healthcheck && sudo systemctl start "pyroshine@$USER"
```

The interactive installer's own check runs before startup but ignores failures;
the separate check above lets you fix them first. A new login shell adds the
binary to PATH; the wrapper works immediately.

[Set up the web interface](#manage-a-headless-host), pair, then [play](../README.md#5-play).
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

Use the [README check/start sequence](../README.md#2-start), then set up
management below.
The native/portable service grants `pyroshine` for sleep inhibition; direct
binary launches need their own membership. [NixOS](NIXOS.md) manages lingering
and uses different service/group names.

## Manage a headless host

The server serves its [web management interface](MANAGEMENT.md) without any
desktop: no display, browser, GTK or Node.js is needed on the host.

1. On the host (or over SSH) as the streaming user, get the address and a
   one-time setup code. Portable installs run
   `/opt/pyroshine/start-pyroshine.sh web setup`; NixOS uses `moonshine web setup`.

   ```sh
   pyroshine web setup
   ```

2. On your computer, forward the port and keep the tunnel open:

   ```sh
   ssh -N -L 47990:127.0.0.1:47990 user@host
   ```

3. Open <http://127.0.0.1:47990/> on your computer, enter the code and choose
   the administrator password. Afterwards, sign in with that password.

The interface listens on `127.0.0.1` only, so it is reachable through the
tunnel (or a VPN to the host plus a [non-loopback address](MANAGEMENT.md#access-from-another-device))
but not from the LAN by default. Forgot the password? Run
`pyroshine web setup --reset` on the host.

| Problem | Check |
| --- | --- |
| Connection refused | The service runs (`systemctl status "pyroshine@$USER"`) and the log shows **Web management interface available**; the tunnel is open. |
| "Open Pyroshine through its own address" | The browser used another host name. Use `127.0.0.1`, or add the name to [`[management] allowed_hosts`](CONFIGURATION.md#management). |
| Sign-in refused or "Too many failed attempts" | Wait for the delay shown, or reset the password with `pyroshine web setup --reset`. |
| `pyroshine web setup` cannot reach Pyroshine | Run it as the streaming user, without sudo; headless use needs lingering. |

## Pair from a terminal

Use this when the web interface is unreachable, or if you prefer a terminal:

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
request or server restart requires a new attempt. The former host-local
`/pin` page is gone: it let any local user approve a request.

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
Restarts end active streams; they also apply
saved settings. Open web interface pages reconnect on their own; reload them
after an upgrade when they say so. The web interface's **Updates** page can
check for new releases and shows these steps for your installation.

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
