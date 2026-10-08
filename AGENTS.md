# Working in Pyroshine

Pyroshine is a Linux game-streaming server derived from Moonshine, serving
Moonlight-compatible clients and adding native PyroWave streaming. This file is
a repository map and engineering contract for coding agents. It points to the
authoritative documents; read the ones relevant to the change, not every guide.

Related repositories:

- [Pyrolight](https://github.com/karsyboy/pyrolight):
  the client fork. Protocol, capability-bit and PyroWave wire changes must stay
  compatible with it (and its `moonlight-common-c` submodule fork).
- [`karsyboy/pyrowave`](https://github.com/karsyboy/pyrowave): the pinned codec fork.

## Working approach

- Inspect the relevant code, tests and guide before editing. Treat the current
  implementation as the source of truth; documents can be stale.
- Prefer root-cause fixes over symptom workarounds, and explain the cause.
- Keep changes focused. Avoid unrelated refactors, renames and formatting churn.
- Preserve existing behavior, protocols and defaults unless the task explicitly
  changes them. If you find an unrelated defect, report it instead of silently
  redesigning behavior.

## Identity and compatibility

- Native packages and portable installations expose the `pyroshine` command and
  service; the Rust server artifact remains `moonshine`.
- Internal crate names, config/state paths, `MOONSHINE_*` environment variables,
  and protocol identifiers intentionally retain `moonshine`. Do not rename them:
  this preserves compatibility and eases upstream synchronization.
- Nix also retains the `moonshine` package/executable and `services.moonshine`
  module interface; follow `docs/NIXOS.md` for that integration.
- `dist/` holds `pyroshine-*` files (used by `nfpm.yaml` and the release
  workflow) alongside upstream-named `moonshine-*` files, some of which Nix uses
  (`nix/package.nix`). Check both when changing service, udev or policy
  integration.
- Preserve license notices and attribution.

## Repository map

Paths are relative to the repository root; core rows use `moonshine-core/src/`
unless explicitly marked as root. `docs/ARCHITECTURE.md` explains how they fit together.

| Area | Responsibility |
| --- | --- |
| Root `src/main.rs` | CLI, config loading, startup probes, server wiring, shutdown |
| `config.rs`, `healthcheck.rs`, `gpu.rs` | Config loading/defaults; host/codec capability checks; capture/encode GPU identity |
| `app_scanner/` | Steam, Lutris, Heroic and desktop-entry discovery |
| `clients.rs`, `state.rs`, `durable.rs`, `tls.rs`, `crypto.rs` | Paired-client trust, durable `state.toml`, atomic private writes, TLS identity, AES helpers |
| `webserver/` (`mod.rs`, `pairing.rs`, `bandwidth.rs`) | GameStream HTTP/HTTPS API, operator pairing, PyroWave bandwidth probe |
| `rtsp.rs`, `ingress.rs`, `discovery.rs` | RTSP negotiation; bounded connection supervision; mDNS |
| `session/manager.rs`, `session/mod.rs`, `session/lifecycle.rs` | Session states, transitions, teardown ownership, worker guards and start latches |
| `session/application.rs`, `session/inhibit.rs` | Application systemd unit and environment; logind sleep inhibition |
| `session/authorization.rs`, `session/keys.rs`, `session/negotiation.rs` | Authorization generations; validated keys and nonce ownership; shared numeric domains |
| `session/compositor/` | Headless Smithay compositor: scene, focus, cursor, Steam classification, input injection, XWayland ownership, native color-management protocols, capture admission and DMA-BUF export |
| `session/stream/audio/` | Embedded PulseAudio-compatible server, Opus encoding, audio FEC/encryption, UDP |
| `session/stream/control/` | ENet control protocol, peer authorization (`peers.rs`), input decoding/routing, Inputtino devices, ownership and feedback |
| `session/stream/video/` | Negotiated formats, stream epochs, packetization, FEC, GSO/pacing, diagnostics |
| `session/stream/video/pipeline/` | DMA-BUF import, compute conversion (`convert.rs`, `shaders/`), Pixelforge encoding, HDR metadata, failure classification |
| `session/stream/video/pyrowave.rs`, `pyrowave_protocol.rs` | Dynamic PyroWave C API, ABI/provenance checks, native resource ownership; dialect/profile negotiation |
| `session/status.rs` | Public session phase from manager status and control-stream peer ownership |
| `management/` | Session-bus management interface for the desktop app: status, pairing, clients, config store (`config_store.rs`), editor schema (`schema.rs`), telemetry aggregation |

Other workspace and integration areas:

- `moonshine-tools/`: developer tools, including the `moonshine-bench` pipeline benchmark.
- `moonshine-management/`: the management API contract (D-Bus names, JSON documents,
  error names, proxies) shared by the daemon and the desktop app.
- `pyroshine-ui/`: optional desktop app (Tauri 2, React/Material UI, `ksni` tray) in a
  separate Cargo workspace and npm project, packaged by `nfpm-ui.yaml`. It is a client of
  the management interface only; server crates must never depend on it.
- `scripts/`: pinned PyroWave build helper, embedded SPIR-V regeneration
  (`build-shaders.sh`), changelog tooling/tests, measurement harnesses.
- `dist/`, `nfpm.yaml`, `.github/workflows/release.yaml`: native/portable packaging,
  installers, systemd, device permissions and system policy. The Arch
  `pyroshine-bin`/`pyroshine-ui-bin` PKGBUILDs live in `karsyboy/pyrowave-packages`.
- `nix/`, `flake.nix`: Nix package, dependency build, development shell and service module.
- `vendor/inputtino/`: maintained native Inputtino patch, including build/binding sources.

## Choose the source of truth

| Change | Consult |
| --- | --- |
| Components, data flow, startup, session lifecycle, invariants | `docs/ARCHITECTURE.md` |
| Build, install locally, contribute, release | `CONTRIBUTING.md`; CI commands in `.github/workflows/ci.yaml` |
| Configuration fields, defaults or semantics | `docs/CONFIGURATION.md` and the owning Rust config/default implementations |
| PyroWave, codec negotiation, GPU ownership, FEC or transport | `docs/PYROWAVE.md`, `docs/PYROWAVE_COMPATIBILITY.md` |
| Capture, cursor lifetime, focus, Steam surfaces/overlays or input | `docs/COMPOSITOR.md` |
| Native presentation, HDR/color management and Wine/Proton setup | `docs/NATIVE_PRESENTATION.md` |
| Capture admission, completion or pipeline backpressure | `docs/PIPELINE_OPTIMIZATION.md` |
| Performance measurements | `docs/BENCHMARKING.md`; lifetime/soak diagnostics in `docs/LONG_SESSION_PERFORMANCE.md` |
| Reconnect or stream reconfiguration | `docs/reconnect-validation.md` |
| Native controller backend/Edge mapping | `docs/DUALSENSE_EDGE.md`, `vendor/inputtino/LOCAL_CHANGES.md` |
| Pairing, trust state, TLS identity | `docs/SECURITY_ADMINISTRATION.md` |
| Management interface, desktop app | `docs/ARCHITECTURE.md` (Desktop management interface), `docs/DESKTOP.md`, `pyroshine-ui/README.md` |
| Packaging, service or host integration | `docs/INSTALLATION.md`, `CONTRIBUTING.md`; `docs/NIXOS.md` for Nix |
| Release notes and inherited history | `docs/CHANGELOG.md`; `docs/UPSTREAM_CHANGELOG.md` is the upstream archive |

## Architectural boundaries

- Keep protocol/session orchestration, compositor scene decisions, encoding,
  packet transport, input/control and packaging in their owning
  layers. Extend existing interfaces rather than coupling unrelated layers or
  introducing parallel systems; inspect nearby patterns first.
- Capture, rendering, encoding, packetization, UDP/FEC/pacing, Wayland presentation
  and controller/input routing are latency-sensitive. Avoid adding unnecessary
  allocations, copies, blocking I/O, locks, synchronous waits, per-frame logging
  or indirection in these loops. Preserve bounded queues/backpressure and buffer
  release semantics; existing GPU completion waits are ownership requirements,
  not invitations to remove synchronization without proof.
- For session/audio/video/HDR/resolution/codec/reconnect changes, cover initial
  connection, disconnect, reconnect, changed parameters and unchanged-mode resume.
  Check stream epochs/state transitions, cleanup and stale keys/frames/resources
  from the prior connection. Use the reconnect validation matrix.
- Capture visibility and input focus are distinct but interacting responsibilities.
  Review cursor, scene, Steam overlays and input routing together before fixing
  capture/focus/cursor/Steam-input symptoms; follow `docs/COMPOSITOR.md`.
- Applications present on their actual Wayland/XWayland surfaces. Do not add
  Vulkan interception or replacement surfaces. Native color descriptions are
  the only HDR declarations; buffer depth/format never implies HDR.
- At unsafe/native boundaries (Vulkan, DMA-BUF handles, Inputtino, PyroWave), keep
  new `unsafe` scopes narrow. Explain non-obvious ownership, lifetime,
  synchronization, ABI and cleanup assumptions, including failure paths.

## Dependencies and configuration

- Root `Cargo.toml` patches only `inputtino-sys` to the vendored native backend;
  the public Rust `inputtino` API stays on the pinned upstream Git dependency.
  Do not replace or reorganize this arrangement casually. Follow `LOCAL_CHANGES.md`
  when updating it, comparing the patch with upstream and coordinating Nix.
- For PyroWave pins, C API assumptions or build changes, follow the pinned
  dependency process in `docs/PYROWAVE.md`. Keep Rust provenance/API requirements,
  `scripts/build-pyrowave.sh`, `nix/pyrowave.nix` and documented pins synchronized.
  Keep dependency patches under `nix/patches/` consistent across build paths;
  never substitute another PyroWave source or ABI. Check that the client's
  pinned decoder still accepts the bitstream family before changing pins.
- Configuration additions/removals/renames/default or behavior changes must update
  owning Rust structures/defaults, examples and `docs/CONFIGURATION.md` together,
  plus the editor schema in `management/schema.rs` (its tests fail otherwise) and
  the UI fixture (`MOONSHINE_UPDATE_UI_FIXTURES=1 cargo test -p moonshine-core
  ui_schema_fixture_is_current`).
  Update healthcheck/capability behavior where affected. Keep user-facing settings
  documented; code-only options must be intentionally internal/diagnostic.

## Validation

Run commands from the repository root. Use the narrowest relevant test while
iterating. For completed implementation work, run appropriate workspace checks
from CI (native prerequisites are in `CONTRIBUTING.md`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features
```

CI also runs `cargo machete` (install with `cargo install cargo-machete`),
`python3 scripts/changelog.py check`,
`python3 -m unittest discover -s scripts -p 'test_changelog.py'`, and
`python3 scripts/known_defects.py` (known-defect characterizations must still
fail for their recorded reason; native controller tests under ASan/UBSan).
When fixing a listed finding, un-ignore its tests and remove their entries in
`scripts/known_defects.toml` in the same change; never weaken them to pass.
Its test leg builds PyroWave with `scripts/build-pyrowave.sh` and runs the
`#[ignore]`d `session::stream::video::pyrowave::tests::ffi_loads_pinned_api` in
`moonshine-core` with `--ignored`, `MOONSHINE_TEST_PYROWAVE=1` and
`MOONSHINE_PYROWAVE_LIBRARY` pointing to the built library; use the full
invocation in `CONTRIBUTING.md` or CI. Hardware-dependent tests are ignored by
default so a run without hardware reports them as not executed, never as passed.

- Desktop app changes run the `pyroshine-ui` checks listed in `CONTRIBUTING.md`
  (npm typecheck/test/build, and fmt/clippy/test with `--manifest-path
  pyroshine-ui/src-tauri/Cargo.toml`). Management-interface tests need `dbus-daemon`.
- Documentation-only work needs Markdown, relative-link and path checks and
  `git diff --check`, not a full native build. Select other checks according to
  the affected behavior.
- Fix reproducible bugs with regression coverage when reasonably automatable.
  Test externally meaningful behavior or stable internal contracts, not incidental
  implementation details. For live-only behavior, cover lower-level contracts
  where practical and record the remaining manual checks.
- GPU capture, PyroWave/Vulkan/HDR, compositor/Steam/controller behavior,
  reconnects and transport/performance need relevant documented GPU/client,
  benchmark or acceptance checks in addition to automated coverage. Report tested
  hardware/clients and checks not performed; a build/unit test is not hardware validation.

## Change discipline and documentation

- Remove obsolete paths when a replacement is intentionally complete; add
  compatibility shims only for an actual compatibility requirement. Update
  relevant tests and docs with the implementation.
- Record meaningful user-visible changes under `Unreleased` in `docs/CHANGELOG.md`
  using the contributor guide's categories. Do not add fork entries to the
  archived `docs/UPSTREAM_CHANGELOG.md`; follow `CONTRIBUTING.md` for release work.
- Comments should explain architectural intent, compatibility/protocol quirks,
  safety invariants and non-obvious performance decisions that could otherwise
  be incorrectly simplified. Avoid narrating code; document public/complicated
  interfaces where it materially helps humans and agents maintain them.

Documentation follows two audiences:

- **User-facing** (`README.md`, `INSTALLATION.md`, `CONFIGURATION.md`, `TIPS.md`,
  `SECURITY_ADMINISTRATION.md`, `NIXOS.md`): task-oriented, concise, with
  prerequisites, copyable commands and links to deeper material. Keep
  implementation detail out unless users act on it.
- **Technical** (`ARCHITECTURE.md` and subsystem guides): current design,
  responsibilities, contracts, invariants and acceptance checks. Describe the
  resulting design, not how it evolved; history belongs in `docs/CHANGELOG.md`
  and commit messages. Keep rationale only when it prevents an incorrect
  simplification. Put design sections first and automated checks and hardware
  acceptance procedures in a final validation section. Do not commit dated
  investigation reports to `docs/`.

Style: one `#` title, descriptive `##` headings, short paragraphs, tables for
reference data, `sh`/`toml`-tagged code blocks, relative links between repository
documents, and no marketing or unsupported claims. Link a new guide from
`docs/README.md`; prefer one authoritative location plus links over duplicated
explanations.

Update this file when architectural boundaries, canonical commands, document
locations or repository-wide invariants change; ordinary features need no edit.

## Upstream synchronization

Upstream is [Moonshine](https://github.com/hgaiser/moonshine), configured as
the `upstream` remote (branch `main`); `origin` is the fork. Verify with
`git remote -v` rather than assuming. Fork history begins after upstream
v0.16.1 (see `docs/UPSTREAM_CHANGELOG.md`). Pinned third-party sources have
their own upstreams: PyroWave (`karsyboy/pyrowave`, derived from
`Themaister/pyrowave`; see `docs/PYROWAVE.md`), Inputtino
(`games-on-whales/inputtino`; see `vendor/inputtino/LOCAL_CHANGES.md`), and the
Git-pinned Smithay, Pixelforge and ash revisions in `Cargo.toml`.

Staying reasonably compatible with upstream is an ongoing goal. Bug fixes,
security fixes, performance, compatibility and maintenance improvements, useful
features and architectural improvements are worth bringing in when they fit the
fork and do not break, remove or undermine fork-specific functionality.

Synchronization is never automatic:

1. Identify the upstream repository and branch from the remote configuration
   and these documents.
2. Determine how the fork differs from upstream in the affected areas
   (`git merge-base`, `git log`, `git diff`).
3. Review the upstream changes being considered.
4. Evaluate conflicts and regression risks.
5. Decide whether the changes provide meaningful value to the fork.
6. Identify fork-specific adaptations required.

**Explicit approval is required before applying anything.** Agents may fetch,
inspect and compare upstream and prepare a recommendation summarizing the
relevant changes, why they are useful, affected areas, expected conflicts,
fork behavior at risk and proposed validation. No agent may merge, cherry-pick,
rebase onto, copy, port or otherwise apply upstream changes into this fork
without explicit approval from the repository owner, even when the change is
small, documentation-only, conflict-free or obviously beneficial.

**Upstream repositories are read-only.** Never push to, commit to, open or
update pull requests against, modify branches or settings of, merge into, or
create releases in any upstream repository. All work stays in this fork unless
the owner explicitly instructs otherwise.

**Fork functionality takes priority.** Do not remove or weaken fork-specific
behavior (PyroWave streaming, the authorization/teardown model, late
composition, DualSense Edge support, packaging identity and others documented
here) merely to reduce divergence. When upstream and fork requirements conflict,
understand why the fork differs, preserve intentional behavior, adapt the
upstream change cleanly where possible, and report unavoidable divergence.
