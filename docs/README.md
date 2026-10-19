# Pyroshine documentation

## Getting started

- [README quick start](../README.md): requirements and **Install → Start → Set up → Pair → Play**.
- [Installation](INSTALLATION.md): platform compatibility, SteamOS/portable setup, headless use, networking and upgrades.
- [Web interface and tray](MANAGEMENT.md): sign in locally or over SSH, pair devices, manage clients, save settings, read stream status, history and updates; the desktop tray.
- [Manage a headless host](INSTALLATION.md#manage-a-headless-host) and [pair from a terminal](INSTALLATION.md#pair-from-a-terminal): `pyroshine web setup` and `pyroshine pair`, including over SSH.
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
| Web management interface, tray and management API | [Architecture](ARCHITECTURE.md#management), [web interface](../web/README.md) |
| Scene capture, cursor, focus and Steam input | [Compositor](COMPOSITOR.md) |
| Capture demand, GPU completion and bounded encoding | [Capture pipeline](PIPELINE_OPTIMIZATION.md) |
| PyroWave dependency, negotiation, color, FEC and transport | [PyroWave](PYROWAVE.md) |
| Cross-fork framing and authenticated calibration | [PyroWave compatibility](PYROWAVE_COMPATIBILITY.md) |
| UDP bandwidth calibration protocol, safety and bitrate suggestion | [Bandwidth calibration](BANDWIDTH_CALIBRATION.md) |
| Native Wayland/XWayland presentation, HDR and Wine/Proton | [Native presentation](NATIVE_PRESENTATION.md) |
| Native controller identity/report mapping | [Native controllers](NATIVE_CONTROLLERS.md), [DualSense Edge](DUALSENSE_EDGE.md) |
| Repeatable pipeline measurements | [Benchmarking](BENCHMARKING.md) |
| Test tiers, CI gates, hardware tests and baselines | [Testing](TESTING.md) |
| What each codec, format, GPU and lifecycle feature does and how it is tested | [Capability inventory](CAPABILITIES.md) |
| Runtime stalls, resource bounds and long-run checks | [Streaming diagnostics](LONG_SESSION_PERFORMANCE.md) |
| Mode changes, epochs and teardown | [Reconnect validation](reconnect-validation.md) |
| Coding-agent rules and upstream policy | [AGENTS.md](../AGENTS.md) |

Guides describe the current design and its acceptance checks. Dated
investigation reports and measurement logs are not kept in this repository;
record new evidence (revision, hardware, workload and unperformed checks) with
the change that needs it rather than in a guide.

The streaming client is maintained separately in
[Pyrolight](https://github.com/karsyboy/pyrolight).
