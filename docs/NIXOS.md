# Pyroshine on NixOS

Use this module instead of the prebuilt `.deb`/`.rpm`/portable installer.
It builds against your NixOS dependency set, installs device integration, enables
user lingering, and starts the server declaratively. GPU/driver and client
[requirements](../README.md#requirements) still apply. The flake defines
`x86_64-linux` and `aarch64-linux` packages; that does not establish that every
ARM system can stream.

Nix retains the upstream names: executable **`moonshine`**, module
**`services.moonshine`**, system service **`moonshine.service`**. There is no
`pyroshine@<user>` unit here. The package includes the web management
interface and its application-menu entry (add the package to
`environment.systemPackages` to list it). The service shows the desktop tray
itself while its user is signed in to a desktop, as on other distributions.

## Install and configure

1. Add Pyroshine to your system flake's inputs:

   ```nix
   inputs.pyroshine.url = "github:karsyboy/pyroshine";
   ```

   Pass your flake inputs to the NixOS configuration as usual (for example with
   `specialArgs = { inherit inputs; };` in your `nixosSystem` definition).
2. Import the module and set the streaming user. Replace `alice` and the UID
   with your existing account; `id -u alice` reports its UID. This example
   installs native Steam and fixes both default Steam executable paths:

   ```nix
   { inputs, ... }:
   {
     imports = [ inputs.pyroshine.nixosModules.default ];
     programs.steam.enable = true;

     services.moonshine = {
       enable = true;
       user = "alice";
       # Omit only if users.users.alice.uid is already declared.
       uid = 1000;
       # Opens streaming ports globally: use only on a trusted LAN/VPN host.
       # For a host with public interfaces, keep false and restrict the firewall yourself.
       openFirewall = true;
       settings = {
         application = [ {
           title = "Steam";
           command = [ "/run/current-system/sw/bin/steam" "steam://open/bigpicture" ];
         } ];
         application_scanner = [ {
           type = "steam";
           library = "$HOME/.local/share/Steam";
           command = [ "/run/current-system/sw/bin/steam" "-bigpicture" "steam://rungameid/{game_id}" ];
         } ];
       };
     };
   }
   ```

   Set up your GPU driver through your NixOS hardware configuration. The module
   enables `hardware.graphics`, but it does not select/configure a proprietary
   NVIDIA driver for you. The built package includes the PyroWave library,
   libva and host driver search paths. Intel GPUs encode H.264/HEVC/AV1
   through VA-API: add Intel's media driver with
   `hardware.graphics.extraPackages = [ pkgs.intel-media-driver ];`.
3. Apply the configuration:

   ```sh
   sudo nixos-rebuild switch
   systemctl status moonshine --no-pager
   journalctl -u moonshine -b -n 100 --no-pager
   ```

   There is no separate `systemctl enable` step. Startup runs the healthcheck
   before binding its ports. Look for **Pyroshine is ready and waiting for
   connections.** If startup fails, use the reported error and
   [troubleshooting](TIPS.md#healthcheck-fails). Do not run a standalone healthcheck
   while the service is listening; it will report the service's ports as conflicts.
4. Install/sign in to Steam as the configured user, install a game, and quit
   local Steam. Restart `sudo systemctl restart moonshine` to refresh scanning.
   Run `moonshine web setup` as that user and open <http://127.0.0.1:47990/>
   (over SSH: [tunnel the port](INSTALLATION.md#manage-a-headless-host)) to
   pair in the browser, or run `moonshine pair` and follow the terminal
   prompts. Select Steam to play.

The built-in Steam command paths use `/usr/bin/steam`, which is unsuitable on
NixOS; set both the application and scanner as above. Omit the scanner or use
`application_scanner = [];` to disable scanning. Other omitted fields follow the
[configuration reference](CONFIGURATION.md#toml-structure-and-defaults).
Custom apps need an executable available to the service, preferably an absolute
Nix path.

## Headless input and sleep

Lingering is enabled automatically so the user's systemd manager and D-Bus bus
remain available without a graphical login. For headless gamepads, add `input`
to the user's groups. For automatic sleep inhibition, add `moonshine`: the
module declares this group and installs its policy, but does not grant it to
the service or user automatically.

```nix
users.users.alice.extraGroups = [ "input" "moonshine" ];
```

Rebuild, then log out/in; reboot if the already-running user manager retains its
old groups. With an active desktop seat, input-device ACLs normally make `input`
optional. The service grants `video` for GPU access. A monitor and GUI are not
required. See [input troubleshooting](TIPS.md#keyboard-mouse-or-gamepads-do-not-work)
for module/device checks.

## Settings and upgrades

Change `services.moonshine.settings` and run `sudo nixos-rebuild switch`; changes
to the service or generated configuration restart it and end active streams.
The TOML lives in the read-only Nix store, so the desktop settings editor cannot
save it. Mutable pairing state stays in the user's data directory
(`~/.local/share/moonshine` by default), and TLS defaults remain under
`~/.config/moonshine`. The service uses `UMask=0077`.
See [Security administration](SECURITY_ADMINISTRATION.md) for private backups,
identity recovery and revocation.

To upgrade, update the Pyroshine input in your system flake, then rebuild:

```sh
nix flake update pyroshine
sudo nixos-rebuild switch
```

## Package-only builds and development

```sh
nix build github:karsyboy/pyroshine
./result/bin/moonshine --help
```

Building the package alone does not activate the service, device rules,
lingering or firewall; use the module for a complete installation. To develop:

```sh
nix develop
cargo build
```

The dev shell supplies build dependencies, clippy and rustfmt. Rust dependencies
come from `Cargo.lock`; maintainers must also check Git hashes in
[package.nix](../nix/package.nix) and PyroWave pins in
[pyrowave.nix](../nix/pyrowave.nix). See the
[pinned dependency guide](PYROWAVE.md#pinned-dependency).
