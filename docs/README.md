# Pyroshine documentation

## Getting started

- [README quick start](../README.md): requirements and **Install → Start → Pair → Play** for a standard desktop.
- [Installation](INSTALLATION.md): platform compatibility, SteamOS/portable setup, headless use, networking and upgrades.
- [Desktop app](DESKTOP.md): open the app, pair devices, manage clients, save settings and read stream status.
- [Pair without the GUI](INSTALLATION.md#pair-without-the-desktop-app): `pyroshine pair`, including over SSH; browser fallback.
- [NixOS](NIXOS.md): declarative installation, Steam configuration and the `moonshine` service.

## Advanced configuration and troubleshooting

- [Troubleshooting and recipes](TIPS.md): host discovery, healthcheck failures, pairing, Steam, input access, Flatpak and application logs.
- [Collect a support bundle](TIPS.md#collect-a-support-bundle): `pyroshine diag` writes a redacted `.tar.gz` for bug reports.
- [Configuration reference](CONFIGURATION.md): every supported `config.toml` setting, defaults and examples.
- [Security administration](SECURITY_ADMINISTRATION.md): pairing security, certificate identity, revocation and state recovery.
- [Changelog](CHANGELOG.md): Pyroshine releases and upcoming changes.
- [Upstream changelog](UPSTREAM_CHANGELOG.md): archived Moonshine release history and attribution.

## Development and architecture

Start with the [architecture overview](ARCHITECTURE.md) for components, data
flow, session lifecycle and the invariants changes must preserve. Then choose
the guide for the area being changed:

| Area | Guide |
| --- | --- |
| Build, install, CI and releases | [Contributor guide](../CONTRIBUTING.md) |
| Inputtino and Pixelforge patch maintenance | [Vendored dependencies](../vendor/README.md) |
| Management interface and desktop app | [Architecture](ARCHITECTURE.md#desktop-management-interface), [pyroshine-ui](../pyroshine-ui/README.md) |
| Scene capture, cursor, focus and Steam input | [Compositor](COMPOSITOR.md) |
| Capture demand, GPU completion and bounded encoding | [Capture pipeline](PIPELINE_OPTIMIZATION.md) |
| PyroWave dependency, negotiation, color, FEC and transport | [PyroWave](PYROWAVE.md) |
| Cross-fork framing and authenticated calibration | [PyroWave compatibility](PYROWAVE_COMPATIBILITY.md) |
| Native Wayland/XWayland presentation, HDR and Wine/Proton | [Native presentation](NATIVE_PRESENTATION.md) |
| Native controller identity/report mapping | [Native controllers](NATIVE_CONTROLLERS.md), [DualSense Edge](DUALSENSE_EDGE.md) |
| Repeatable pipeline measurements | [Benchmarking](BENCHMARKING.md) |
| Runtime stalls, resource bounds and long-run checks | [Streaming diagnostics](LONG_SESSION_PERFORMANCE.md) |
| Mode changes, epochs and teardown | [Reconnect validation](reconnect-validation.md) |
| Coding-agent rules and upstream policy | [AGENTS.md](../AGENTS.md) |

Guides describe the current design and its acceptance checks. Dated
investigation reports and measurement logs are not kept in this repository;
record new evidence (revision, hardware, workload and unperformed checks) with
the change that needs it rather than in a guide.

The streaming client is maintained separately in
[Pyrolight](https://github.com/karsyboy/pyrolight).
