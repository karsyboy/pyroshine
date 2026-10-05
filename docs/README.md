# Pyroshine documentation

Start with the [project README](../README.md) for requirements and a quick start.

## Using Pyroshine

User guides are task-oriented and avoid implementation detail.

- [Installation and upgrades](INSTALLATION.md): native packages, portable builds, headless setup, and service diagnostics.
- [Desktop app](DESKTOP.md): optional system tray, pairing notifications, client management, settings editor, and stream dashboard.
- [Configuration reference](CONFIGURATION.md): every supported `config.toml` setting, defaults, and examples.
- [Tips and troubleshooting](TIPS.md): Steam, Flatpak, Gamescope, desktop sessions, and application logs.
- [Security administration](SECURITY_ADMINISTRATION.md): pairing approval, durable state recovery, TLS identity permissions and revocation.
- [NixOS](NIXOS.md): flake package, service module, and development shell.
- [Changelog](CHANGELOG.md): Pyroshine fork releases and upcoming changes.
- [Upstream changelog](UPSTREAM_CHANGELOG.md): archived Moonshine release history and attribution.

## Development and architecture

Start with the [architecture overview](ARCHITECTURE.md) for components, data
flow, session lifecycle and the invariants changes must preserve. Then choose
the guide for the area being changed:

| Area | Guide |
| --- | --- |
| Build, install, CI and releases | [Contributor guide](../CONTRIBUTING.md) |
| Management interface and desktop app | [Architecture](ARCHITECTURE.md#desktop-management-interface), [pyroshine-ui](../pyroshine-ui/README.md) |
| Scene capture, cursor, focus and Steam input | [Compositor](COMPOSITOR.md) |
| Capture demand, GPU completion and bounded encoding | [Capture pipeline](PIPELINE_OPTIMIZATION.md) |
| PyroWave dependency, negotiation, color, FEC and transport | [PyroWave](PYROWAVE.md) |
| Cross-fork framing and authenticated calibration | [PyroWave compatibility](PYROWAVE_COMPATIBILITY.md) |
| Native Wayland/XWayland presentation, HDR and Wine/Proton | [Native presentation](NATIVE_PRESENTATION.md) |
| Native controller identity/report mapping | [DualSense Edge](DUALSENSE_EDGE.md) |
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
