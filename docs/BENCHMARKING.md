# Benchmarking Pyroshine

The `moonshine-tools` crate provides utilities for testing and benchmarking
Pyroshine. The crate and binary names retain their upstream spelling.

## moonshine-bench

Benchmarks Pyroshine's video encoding pipeline by spawning an application inside a headless compositor, running the full encode path, and collecting per-frame timing statistics.

### Building

```bash
cargo build -p moonshine-tools --release
```

The binary will be at `target/release/moonshine-bench`.
See the [contributor guide](../CONTRIBUTING.md#manually-building-the-app) for
build dependencies and the pinned PyroWave library. Streaming benchmarks also
need device rules and a working GPU. Applications present on their ordinary
Wayland/XWayland surfaces; see [native presentation](NATIVE_PRESENTATION.md).

### Usage

```
moonshine-bench [OPTIONS] <COMMAND>
```

`<COMMAND>` is the application to run inside the compositor. A good test target is `/usr/bin/vkcube` (from `vulkan-tools`), which renders an animated rotating cube.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--matrix` | off | Run the built-in 4K, 1440p, and 1080p matrix across 60/120/360 FPS and `hevc`, `h264`, and `av1` |
| `--composited` | off | Force the GLES fallback, independent of fullscreen direct-export eligibility |
| `--cursor <mode>` | off | Emulate client pointer use so the compositor cursor is visible: `static` (one motion) or `moving` (motion every refresh tick). Also applies to `--cycles` and `--reconnect-cycles` |
| `--conversion-queue <mode>` | `auto` | H.264/HEVC/AV1 conversion queue: `auto`/`compute` (dedicated compute family when present) or `graphics` |
| `--pyrowave-queue <mode>` | `auto` | Compare PyroWave `auto` (graphics), `graphics`, or explicit `compute` at normal queue priority |
| `--pyrowave-matrix` | off | Run the PyroWave 1080p/1440p/4K matrix across 60/120/144 FPS |
| `--resolution <WxH>` | `1920x1080` | Stream resolution |
| `--fps <N>` | `60` | Target frame rate |
| `--bitrate <N>` | `20000000` | Target bitrate in bits per second |
| `--encrypt-video` | off | Enable the same video AES-GCM path used in streaming |
| `--fec-mode <mode>` | `fixed` | Select `off`, `fixed`, or `auto` FEC policy |
| `--fec-percentage <N>` | `20` | Requested parity percentage; effective layout still obeys the protocol limits |
| `--minimum-fec-packets <N>` | `2` | Negotiated minimum parity count |
| `--packet-size <N>` | `1400` | Negotiated packet size, validated by the production setup path |
| `--codec <codec>` | `h264` | Video codec: `h264`, `hevc`, `av1`, or `pyrowave` |
| `--chroma <mode>` | `420` | Chroma sampling: `420` or `444`, subject to codec support |
| `--bit-depth <N>` | `8` (SDR), `10` (HDR) | Select `8` or `10` bits; incompatible formats are rejected |
| `--full-range` | off | Use full-range YCbCr; PyroWave always uses full range |
| `--duration <N>` | `0` | Seconds to run before stopping (`0` = run until Ctrl+C) |
| `--warmup <N>` | `4` | Seconds to discard before recording stats |
| `--hdr` | off | Enable HDR mode |
| `--verbose` | off | Print per-frame stats instead of periodic summary |
| `--detach-seconds <N>` | `0` | After the run, measure N s attached, then N s with both media epochs paused by a reconnecting client's ANNOUNCE (no PLAY), then PLAY and time the first frame; reports frames, process CPU and per-engine GPU time of the benchmark process |
| `--runtime-probe` | off | Run a 1 ms timer task on the session runtime and report its lateness percentiles (scheduling delay caused by packetization and transport) |
| `--frame-limit <off\|stream\|N>` | `off` | Apply [`[frame_limit]`](CONFIGURATION.md#frame_limit) to the benchmark application: the stream's frame rate or a fixed rate. Also applies to `--cycles` and `--reconnect-cycles` |
| `--frame-limit-method <method>` | `proton` | Limiter for `--frame-limit`: `proton` or `mangohud` |
| `--vrr` | off | Act as a client that requested VRR presentation, so the compositor uses presentation-driven capture. With `--reconnect-cycles`, the request alternates per cycle |
| `--rtp-trace <path>` | none | Write `frame_index,rtp_timestamp,arrival_us` for every received video frame (unencrypted video only), to compare RTP and arrival intervals |
| `--json <path>` | none | Write a machine-readable result document for the run or matrix; see [machine-readable results](#machine-readable-results) |
| `--capabilities` | off | Probe the capture GPU exactly as the server does at startup, print its codec profiles, HDR and DMA-BUF support as JSON (also to `--json`) and exit; needs no command |
| `--skip-unsupported` | off | Probe capabilities first and record workloads the GPU cannot encode as `unsupported` instead of running them; a single unsupported run exits with status 3 |

### Examples

Run a PyroWave benchmark with the locally built backend:

```sh
MOONSHINE_PYROWAVE_LIBRARY=/tmp/pyrowave-install/lib/libpyrowave-shared.so.1 \
  target/release/moonshine-bench --codec pyrowave --chroma 444 --duration 30 /usr/bin/vkcube
```

Run a quick H.264 benchmark at 1080p60 for 30 seconds:

```bash
moonshine-bench --duration 30 --codec h264 /usr/bin/vkcube
```

Run the conventional resolution/FPS/codec matrix:

```sh
cargo run --release -p moonshine-tools --bin moonshine-bench -- --matrix --duration 30 --warmup 4 /usr/bin/vkcube
```

Use `--pyrowave-matrix` for PyroWave's matrix, `--composited` to force GLES,
and `--pyrowave-queue graphics` or `compute` for queue comparisons. Omitting
`--duration` runs a single mode until Ctrl+C. `--verbose` adds per-frame output;
`MOONSHINE_LOG=debug` enables diagnostic logging.

### Comparing runs

`scripts/bench_results.py` validates, summarizes and compares `--json`
documents and baseline directories:

```sh
python3 scripts/bench_results.py validate ~/pyroshine-baselines/abc1234
python3 scripts/bench_results.py summarize ~/pyroshine-baselines/abc1234
python3 scripts/bench_results.py compare ~/pyroshine-baselines/abc1234 ~/pyroshine-baselines/def5678
python3 scripts/bench_results.py compare BASE NEW --fail-on total_p99_us=10 --fail-on fps=3
```

`compare` takes the median of each metric over a workload's repeats and prints
the change for FPS, delivery ratio, total and frame-age latency percentiles,
conversion and encode wait, arrival-interval p99 and deviation, missed frame
slots, CPU, GPU engine time and peak RSS. It only reports unless `--fail-on`
limits are given; there are no built-in thresholds. Derive limits from the
spread of repeated runs on the same hardware, and compare only baselines whose
manifests show the same GPU, driver, kernel and governor.

Record the checkout/backend revisions, GPU/driver, source application/scene,
resolution/FPS, codec/chroma/depth/HDR, bitrate/FEC, capture path, queue preference,
duration and warmup. Use matching workloads and multiple steady windows.
Avoid concurrent compilation or other GPU work unless contention is the subject.
A low-entropy cube cannot establish game quality or maximum-link throughput.
The benchmark exercises server encoding/loopback sending, not Moonlight decode,
display latency or physical network congestion.

### Recording an identified baseline

`scripts/record_baseline.py` records the provenance a comparison needs and a
fixed workload set in one directory outside the checkout:

```sh
python3 scripts/record_baseline.py --out ~/pyroshine-baselines/$(git rev-parse --short HEAD) --netns
```

`manifest.json` (`pyroshine-baseline/1`) identifies the commit (refusing an
uncommitted tree unless `--allow-dirty`, which also saves `checkout.diff`),
Cargo.lock hash, Git-pinned crates, PyroWave, Inputtino and Pixelforge pins,
toolchain, kernel, CPU, memory, governor, GPU/driver (Vulkan driver name and
version) and the PyroWave library found. The script rebuilds `moonshine-bench`
immediately before measuring and records its hash, so a stale binary is never
measured. It probes the GPU's capabilities (`capabilities.json`), then runs
each workload of the selected set `--repeats` times (default three) and 10
lifecycle and reconnect cycles over the codecs the GPU encodes. Every run keeps
its raw log, its `pyroshine-bench/1` document (validated after the run), exit
code and the benchmark process's CPU time and peak RSS (the application runs in
its own unit). On NVIDIA hosts each run also records `nvidia-smi` GPU and
encoder utilization (CSV beside the log, summarized excluding warmup).
Workloads the GPU cannot encode are recorded as `unsupported` and listed under
`not_run`; they are never forced. The recorded PyroWave library is the one loaded.
`--metadata-only` records provenance without running anything.

Benchmark applications run in `moonshine-bench.service` (`--application-unit`),
never the live service's unit. The script still refuses to start while
`moonshine-session.service` is active, because a live session would compete for
the GPU; `--netns` keeps its ports and mDNS off the host network.

| Set (`--set`) | Workloads |
| --- | --- |
| `default` | H.264/HEVC/AV1 1080p60, HEVC 1440p120 and 2160p60, HEVC composited and encrypted+auto-FEC, PyroWave 1080p60 4:4:4 |
| `phase0` | H.264, HEVC, AV1 and PyroWave (500 Mbps) at 1080p, 1440p and 2160p × 60/120 FPS (SDR 8-bit 4:2:0); 1080p60 4:4:4, 10-bit, HDR10 and 4:4:4 HDR10 variants; composited, moving-cursor (late composition), VRR and encrypted+auto-FEC variants |
| `all` | every named workload once |

Workload names are stable and mean the same workload in every set: add new
ones rather than changing existing entries. Conventional codecs use the
benchmark's default 20 Mbps unless the name says otherwise.

On a host without a Rust toolchain, record provenance where the binary is
built and run the copied binary; see [testing](TESTING.md#testing-beside-a-live-service-or-without-a-toolchain):

```sh
python3 scripts/record_baseline.py --bench-binary ./moonshine-bench \
  --checkout-from provenance/manifest.json --netns --set phase0 --out ~/baselines/nvidia
```

To benchmark beside a live service on the same account, give the benchmark its
own application unit and network namespace, for example:

```sh
MOONSHINE_APPLICATION_UNIT=moonshine-bench.service \
  unshare --user --map-root-user --net sh -c \
  'ip link set lo up && exec unshare --user --map-user=1000 --map-group=1000 "$@"' sh \
  target/release/moonshine-bench --codec hevc --fps 120 --duration 20 /usr/bin/vkcube
```

`MOONSHINE_APPLICATION_UNIT` must be a plain `*.service` name; an invalid value
is ignored with a warning.

A server test instance beside a live service also cannot own the management
name on the shared session bus. `MOONSHINE_MANAGEMENT_BUS` (a D-Bus address,
for example of a private `dbus-daemon`) serves its management interface there
instead; applications still start through the session bus's systemd.

### Output

Every 5 seconds, a summary is printed with:

- **Frame count & FPS** — actual encoded frames per second
- **Bitrate** — average encoded bitrate in Mbps
- **Total latency** — avg/min/max and p50/p95/p99 time for the full pipeline per frame
- **Submit latency** — avg/min/max and p50/p95/p99 CPU time spent submitting a frame to the asynchronous encoder
- **Encode wait latency** — avg/min/max and p50/p95/p99 time waiting for the asynchronous encode/readback future
- **Breakdown** — avg time per stage: channel wait, DMA-BUF import, color conversion, submit, encode wait, packetization, enqueue/residence and actual socket send; consumer queue is reported as a diagnostic included inside encode wait
- **Key frames** — number of keyframes emitted
- **Frame age** — p50/p95/p99/max from the content's capture deadline (or VRR latch) to transport completion, which includes capture deferral that `total` omits; the conventional encoder-slot wait (time a converted frame waited for Pixelforge's input slot, part of `convert`); and how many frames were outstanding when each frame was submitted (`1/2/3/4+`), which shows encode/send overlap

At the end of the run, a final summary covers the entire session (excluding the warmup period). It adds:

- **Capture** — frames per capture path (`direct`, `direct_late` with late-composited cursor/notification, `composited`), VRR captures, the content (source) interval distribution and failed/discarded packets
- **Pacing** — expected frames at the target rate, delivered frames, and the distribution of first-packet arrival intervals at the loopback receiver, with intervals later than 1.5 target intervals and the frame slots they skipped

### Machine-readable results

`--json` writes a `pyroshine-bench/1` document. Fields are only ever added;
a change of meaning gets a new schema version so stored baselines stay
comparable. It contains:

| Field | Content |
| --- | --- |
| `tool` | benchmark version and exact argv |
| `host` | kernel, CPU model and count, memory, CPU governor, every render node with driver and PCI vendor/device ids |
| `environment` | variables that change what is measured (`MOONSHINE_PYROWAVE_LIBRARY`, `MOONSHINE_CONVENTIONAL_FRAMES_IN_FLIGHT`, `MOONSHINE_APPLICATION_UNIT`, `VK_DRIVER_FILES`, `ANV_DEBUG`, `RADV_*`, …) |
| `capabilities` | probed GPU name, codec profiles, HDR and DMA-BUF support (with `--capabilities` or `--skip-unsupported`) |
| `runs[].workload` | codec, resolution, FPS, bitrate, negotiated chroma/bit depth/range, HDR, encoder implementation (`pixelforge-vulkan-video` or `pyrowave`), capture mode, VRR, cursor, encryption, FEC, packet size, queue preferences, duration, warmup, application |
| `runs[].status` | `ok`, `no_frames`, `interrupted`, `failed` (with `error`) or `unsupported` (with `reason`) |
| `runs[].summary` | frames, FPS, encoded and submitted UDP Mbps, latency distributions (avg/min/p50/p95/p99/max µs) for `total`, `frame_age`, `submit`, `encode_wait`, `slot_wait`, `source_interval`; the average stage breakdown; frame sizes; packets; capture paths and composition reasons |
| `runs[].pacing` | target interval, expected/delivered frames, delivery ratio, arrival-interval distribution, standard deviation, late intervals and missed frame slots |
| `runs[].resources` | benchmark-process CPU (percent of one core) and per-engine GPU busy ms/s from DRM fdinfo over the measured window; RSS, peak RSS, threads, descriptors and DMA-BUF descriptors; device-wide busy percentage where the driver exposes it (amdgpu `gpu_busy_percent`) |
| `runs[].stats_lagged` | per-frame statistics the benchmark missed; nonzero means the summary undercounts |

What the numbers cover:

- `total` starts at direct export or composition completion and ends at the
  last UDP submission; `frame_age` starts at the content's capture deadline or
  VRR latch. Neither includes game rendering or client decode/display.
- Arrival intervals are measured by the benchmark's own receiver task on
  loopback; they include its scheduling delay and are not a client's display
  cadence.
- Process GPU time covers the benchmark's own DRM clients (compositor,
  conversion, encode). It attributes encode work to the engine the driver
  reports (amdgpu VCN as `enc`); the application runs in its own unit and is
  excluded. The proprietary NVIDIA driver publishes no fdinfo counters; the
  baseline recorder samples `nvidia-smi` for device and encoder utilization.
- Device-wide busy percentages include every GPU user on the host.
- Engine names are merged across every DRM device the process opened. On a
  multi-GPU host the Vulkan loader also opens the other GPUs, whose engines
  appear with zero time (for example i915 `render`/`video` beside an NVIDIA
  capture GPU).
- A light source such as `vkcube` leaves GPU clocks to the driver's power
  management. NVIDIA in particular may stay near idle clocks for bursty
  PyroWave work, which lengthens encode time at lower frame rates; the
  `nvidia-smi` samples record `clocks.gr`. Compare runs only at matching clock
  behavior, and pin clocks when the comparison needs it.

Sampling happens only inside the benchmark process (procfs/sysfs reads every
250 ms and the loopback receiver); the server's streaming path is unchanged.

With `--matrix`, each combination prints the same per-run summaries and the command finishes with a consolidated latency distribution table plus an average pipeline breakdown table. Matrix mode uses fixed target FPS values of 60, 120, and 360. If `--duration` is left at `0`, matrix mode defaults to 8 seconds per combination so the matrix completes.

### Conventional credit bound

`MOONSHINE_CONVENTIONAL_FRAMES_IN_FLIGHT=1|2|3` overrides the conventional
pipeline's useful-work credits (default 2; see
[capture admission](PIPELINE_OPTIMIZATION.md#encoding-caches-and-queue-selection)).
Compare bounds with workloads that saturate each stage, interleaving runs:
an uncapped incompressible source at a stream rate above what the encoder
sustains (encoder), the same source behind a token-bucket shaper below the
encoded bitrate on loopback inside the benchmark's network namespace (link),
and the [game interference harness](#game-interference-harness) (GPU). Judge
frame-age percentiles together with delivered FPS and the game's FPS; a bound
that only lowers `total` may be deferring captures instead.

### How It Works

The tool launches the application through `SessionManager`, negotiates stream
contexts, triggers stream gates and collects broadcast `FrameStats` after warmup.
It reuses the production compositor/encode/packet path; see
[architecture](ARCHITECTURE.md) and [capture admission](PIPELINE_OPTIMIZATION.md).

### GPU cost and admission diagnostics

With `[stream.video] log_stats = true`, five-second summaries distinguish
`queue_to_readback_us` (wall-clock latency) from the PyroWave GPU timestamp
stages. `pyrowave_stage_gpu_ms_per_delivered_frame` accounts for packet-only
replays; the exclusive stage sum excludes transfers and CPU work. GPU intervals
can include contention/preemption during a stage, and are not a measurement of
active shader cycles. The reports lag by Granite's bounded timestamp contexts.

Capture summaries include direct/composited captures, pre-render rejection
attempts, stale-after-render drops, demand/busy state and optional GLES query
measurements. `compositor_gpu_ms_per_captured_frame` divides measured composition
GPU time by all accepted captures, including the zero-render direct path.
Composited timer samples may cross a five-second reporting boundary; compare
multiple windows. Unsupported or disjoint timer measurements are omitted.

Import hit/miss/recreate/evict counters preserve the existing FD identity and
complete import-layout checks. Direct rejection counters identify the first
blocking condition per attempted capture, rather than every possible condition.

Pipeline summaries show encoded Mbps. Transport summaries distinguish logical
attempts, successful kernel submissions, failures and discards, and report
submitted UDP payload Mbps including FEC and protocol/encryption bytes, plus
estimated submitted Ethernet Mbps including IPv6/UDP,
Ethernet header/FCS, preamble and inter-frame gap. VLAN/tunnel overhead and other
traffic are additional. Do not treat the UDP payload as physical wire bitrate.

The benchmark's latency starts at direct export or composition completion;
it does not include game rendering, client decode/display, or the compositor
CPU fence wait. Measure those separately when evaluating end-to-end latency.
Changes deliberately deferred pending evidence are listed in the
[capture pipeline guide](PIPELINE_OPTIMIZATION.md#measurement-and-changes-needing-more-proof).

### Lifecycle cycles

Repeated-session acceptance on real hardware. Stop any running Pyroshine
service first: cycles bind the default video port and create the
`moonshine-session.service` application unit.

```sh
# Launch → stream → stop → relaunch through one session manager.
moonshine-bench --cycles 100 --cycle-log full.jsonl /usr/bin/vkcube
# Authenticated resume → ANNOUNCE → PLAY epochs on one retained application.
moonshine-bench --reconnect-cycles 100 --cycle-log reconnect.jsonl /usr/bin/vkcube
```

`--reconnect-before-start` first reconnects twice (unchanged, then a new
resolution) before the initial `StartB`, as for a client that disappeared
after PLAY; each must complete within 10 s.

Each cycle changes one property, in order: none (unchanged resume), resolution
(1920x1080/1280x720), FPS (60/120), bitrate, codec (`--cycle-codecs`, default
`h264,hevc,av1,pyrowave`), encryption, SDR/HDR10, 4:2:0/4:4:4, audio
stereo/5.1/7.1, audio quality and 5/10 ms packets. Combinations the Vulkan Video
backend does not encode (conventional 4:4:4, 10-bit H.264) are normalized to the
nearest supported format; the `settings` field records what was negotiated.
The capture GPU is probed as at server startup and `--encoder` applies as in
any other run: `settings` names the encoder each cycle used (`encoder=vaapi`),
and with an explicit encoder a profile it cannot encode fails that cycle's
start rather than falling back to another encoder.
A cycle passes when the epoch delivers at least a quarter of its target frames
in `--cycle-seconds` (default 2) and the loopback client receives video bytes.
Full cycles additionally require a successful stop, a bindable video port, no
active application unit (the one the backend uses, including a
`MOONSHINE_APPLICATION_UNIT` override), no remaining child process (XWayland)
and no more open DMA-BUF descriptors than before the first session afterwards.
Every cycle records process FD, DMA-BUF descriptor and thread counts and RSS;
look for a plateau.
Add `--composited` to run the cycles through forced scene composition instead
of direct export.

The benchmark has no Moonlight client: there is no ENet control, audio endpoint
or client decode. Cycles establish backend ownership and streaming continuity;
client compatibility still needs the [reconnect matrix](reconnect-validation.md).

### Frame limiting

Compare `--frame-limit off` with `--frame-limit stream` on a game that does not
wait for the display. Pyroshine's compositor offers FIFO and MAILBOX, so use
MAILBOX for an unthrottled source (`vkcube --wsi wayland --present_mode 1`; the
compositor does not offer IMMEDIATE). A native Vulkan source needs
`--frame-limit-method mangohud`; `proton` covers only DXVK/vkd3d-proton titles.
Judge the effect by the application, not the stream FPS, which the capture
pacer holds at the target either way: the `Video capture cadence` log's
`game_surface_commits` per capture (frames the game rendered per frame sent),
the application process's GPU engine time and CPU time (`/proc/<pid>/fdinfo`,
`/proc/<pid>/stat`), whole-device `resources.device_busy`, and the host latency
and arrival-interval distributions in the JSON result. Run the pair at least
three times, interleaved.

## Game interference harness

`scripts/gpu_interference/` measures what streaming costs a GPU-bound game:
a headless fixed-cost 4K render loop reports FPS, 1% lows and frame-time
percentiles with and without a concurrent `moonshine-bench` stream, while a
sampler records per-process DRM engine time and amdgpu power, clocks and memory
activity. See its README for calibration and interpretation limits; repeat
runs interleaved because a GPU's sustained clocks drift by about half a percent
over a long session.

## Transport measurement scripts

`scripts/transport_measurements.py` prepares a baseline-revision measurement
harness for the packetizer and compares whole-batch fingerprints as well as
per-trial allocations and timing. Run measurements without concurrent
compilation, keep the governor and diagnostics unchanged, and retain outliers.
The pipeline benchmark keeps a live UDP drain but does not decode video.

The GPU A/B helper `scripts/transport_gpu_measurements.py` records the exact argv,
process CPU/RSS and system-wide GPU busy samples alongside each log. It refuses
to replace an active `moonshine-session.service` or use an occupied video port.
Finish active streaming before benchmarking and do not run compilation during
measurement. The default matrix holds the scene and 750 Mbps configuration fixed;
additional matched runs exercise encryption, FEC, no-GSO and 650/900 Mbps settings.
Configured bitrate is not proof that a physical 1 Gbps link can carry the resulting
FEC, encryption and link overhead.
