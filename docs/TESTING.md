# Testing and regression baselines

Pyroshine's checks fall into three tiers. Hardware-independent tests run in
every CI build. Hardware tests and performance baselines need a real GPU (and
sometimes a user systemd session, input devices or a Pulse client) and are run
on test hosts with the commands below. A tier that did not run is reported as
not run, never as passed.

| Tier | Runs where | Pass/fail? | Entry point |
| --- | --- | --- | --- |
| Unit and integration tests | CI and any host | yes | `cargo test --workspace --all-features` |
| Hardware tests (`#[ignore]`d) | GPU test hosts | yes | `scripts/hardware_tests.py run` |
| Lifecycle cycles on the real backend | GPU test hosts | yes | `moonshine-bench --cycles` / `--reconnect-cycles` |
| Performance baselines | GPU test hosts | no (measurements) | `scripts/record_baseline.py`, `scripts/bench_results.py` |

Performance results are measurements, not pass/fail tests: a comparison only
fails on limits you pass explicitly (see [benchmarking](BENCHMARKING.md#comparing-runs)).

## Hardware-independent tests and CI gates

CI (`.github/workflows/ci.yaml`) runs, on every pull request to `main`:

| Gate | Command |
| --- | --- |
| Formatting | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-features --all-targets -- -D warnings` |
| Unit/integration tests | `cargo test --workspace --all-features` |
| Session lifecycle and protocol contracts (named step) | `cargo test -p moonshine-core --lib --all-features -- session::manager:: session::lifecycle:: session::authorization:: session::status:: rtsp:: session::stream::control:: session::stream::video::packetizer:: session::stream::video::pyrowave_protocol:: session::stream::video::tests::` |
| Hardware-test registry and availability report | `python3 scripts/hardware_tests.py check-registry` and `report` |
| Benchmark and hardware tooling | `python3 -m unittest discover -s scripts -p 'test_bench_results.py'` |
| Documentation | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features` |
| Unused dependencies | `cargo machete` |
| Pinned PyroWave ABI | the `ffi_loads_pinned_api` invocation in the CI file |
| Known defects, native sanitizers | `python3 scripts/known_defects.py` |
| Changelog | `python3 scripts/changelog.py check` |
| Vendored sources | `scripts/vendor-inputtino.sh --check`, `scripts/vendor-pixelforge.sh --check`/`--test` |

The session state machine is covered by deterministic tests against a fake
backend in `moonshine-core/src/session/manager/lifecycle_tests.rs`: every
transition held, cancelled or failed; 100 full and 100 reconnect cycles across
every negotiated axis; stale generations and completed sessions unable to
affect newer ones; every application unit stopped exactly once; bounded
teardown. Control-stream peer replacement, silent and clean client loss and
retained controllers are in `session/stream/control/`; media epochs, endpoint
discovery and pause/stop interruption in `session/stream/video/mod.rs` and
`session/stream/audio/mod.rs`.

## Hardware tests

Every ignored test is listed in `scripts/hardware_tests.toml` with what it
needs; CI fails when a new ignored test is not registered. Each test asserts
its own enabling variable, so a test run without it fails instead of passing.

```sh
# What this host can run, without running anything:
python3 scripts/hardware_tests.py report
# Run everything available, logs and a JSON report in one directory:
python3 scripts/hardware_tests.py run --json ~/hw/report.json \
  --pyrowave-library /tmp/pyrowave-install/lib/libpyrowave-shared.so.1
# A host that must exercise the GPU fails when it cannot:
python3 scripts/hardware_tests.py run --json ~/hw/report.json --require render-node vulkan-video-encode
```

A test passes only when its harness reports exactly one passed test. Each
test is killed and failed after `--timeout` seconds (default 900); tests that
normally finish within a second carry a 120 s `timeout` in the registry, so a
hang fails quickly. `--include-informational` also runs the measurement-only
tests. Build the
pinned PyroWave library with `scripts/build-pyrowave.sh WORKDIR PREFIX`.
Point `MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE` at a GPU without Vulkan Video
encode (for example ANV without `ANV_DEBUG=video-encode`) to run the
no-fallback device-selection test. Select a Vulkan driver with
`VK_DRIVER_FILES` on multi-GPU hosts.

Run the compositor and XWayland tests from a release build on NVIDIA. They are
the regression tests for the compositor-thread teardown order (see
[architecture](ARCHITECTURE.md)): with the capture device destroyed while the
exiting compositor thread released its EGL state, eight of them hung on an
RTX 3080 in release builds, while a debug build's slower teardown passed.

The systemd tests create `pyroshine-test-<pid>-*.service` units and never
touch `moonshine-session.service`; the XWayland tests start private
compositors. The device test creates virtual controllers through `/dev/uinput`
and `/dev/uhid`.

## Lifecycle cycles on real hardware

`moonshine-bench --cycles N` and `--reconnect-cycles N` drive the production
backend through repeated sessions and reconnects. Each cycle checks streaming
continuity and, after every full stop, that the video port is free, the
application unit is stopped, no child process (XWayland) remains and no more
DMA-BUF descriptors are open than before the first session. See
[lifecycle cycles](BENCHMARKING.md#lifecycle-cycles). Restrict
`--cycle-codecs` to codecs the GPU encodes; `record_baseline.py` does so from
the probed capabilities.

## Testing beside a live service or without a toolchain

A test host may run a live Pyroshine service. Benchmarks use their own
application unit (`MOONSHINE_APPLICATION_UNIT`) and network namespace, and
`record_baseline.py` refuses to measure while a live session is active; see
[benchmarking](BENCHMARKING.md#recording-an-identified-baseline).

On a host without a Rust toolchain, build on a compatible host and copy:

```sh
cargo build --release --locked -p moonshine-tools --bin moonshine-bench
cargo test --release -p moonshine-core --lib --all-features --no-run   # prints the test binary path
python3 scripts/record_baseline.py --metadata-only --allow-dirty --out /tmp/provenance
# on the test host, with scripts/ copied beside the binaries:
python3 scripts/hardware_tests.py run --test-binary ./moonshine_core-<hash> --json hw/report.json
python3 scripts/record_baseline.py --bench-binary ./moonshine-bench \
  --checkout-from provenance/manifest.json --netns --set phase0 --out baseline
```

## What counts as validation

A build or unit-test run is not hardware validation. Report the GPU, driver,
kernel and the exact checks that ran (the JSON reports record them), and list
the checks that were skipped and why. Client decode, display latency, real
networks and controllers still need a Moonlight/Pyrolight client; follow the
[reconnect validation matrix](reconnect-validation.md) for those.
