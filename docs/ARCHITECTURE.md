# Architecture overview

Pyroshine runs one application at a time in an isolated, headless Linux session
and streams its video, audio and input to a Moonlight-compatible client. This
document describes the current design: components, how they communicate, the
lifecycle from startup to shutdown, and the invariants changes must preserve.
Subsystem guides linked from each section hold the detailed contracts.

Build, install, CI and release procedures live in [CONTRIBUTING.md](../CONTRIBUTING.md).
User-facing configuration lives in [CONFIGURATION.md](CONFIGURATION.md).

## Workspace

| Crate | Output | Role |
| --- | --- | --- |
| Root `moonshine` (`src/main.rs`) | `moonshine` binary, installed as `pyroshine` | CLI, configuration loading, host checks, service wiring, process shutdown; the desktop tray (`src/tray/`, run as a child process) |
| `moonshine-core` | Library | Every server subsystem: protocol endpoints, pairing, sessions, compositor, encoders, transport, input |
| `moonshine-tools` | `moonshine-bench` | Developer benchmark that drives the production session stack without a client |
| `moonshine-management` | Library | Management contract: JSON documents, D-Bus names, error names and client proxies |
| `web/` (npm, not Cargo) | Static files in `share/pyroshine/web` | The web management interface's React/Material UI frontend |

Native dependencies cross explicit boundaries: Smithay (compositor), Pixelforge
(Vulkan Video encoding, vendored at its upstream release plus the patches in
`vendor/pixelforge-patches/`), the optional PyroWave shared library (loaded at
runtime), the vendored Inputtino native backend (virtual input devices), and
systemd/logind over D-Bus. Internal crate names, paths, environment variables
and protocol identifiers keep the upstream `moonshine` spelling.

Core paths below are relative to `moonshine-core/src/`.

## Components and data flow

```mermaid
flowchart LR
  subgraph Client["Moonlight client"]
    C[Client]
  end

  subgraph Server["pyroshine process"]
    HTTP["webserver/<br/>HTTP + HTTPS API, pairing"]
    RTSP["rtsp.rs<br/>stream negotiation"]
    MGR["session/manager.rs<br/>SessionManager"]
    subgraph Session["Session (one at a time)"]
      COMP["session/compositor/<br/>headless Smithay + XWayland"]
      VID["stream/video/<br/>pipeline, packetizer, FEC, UDP"]
      AUD["stream/audio/<br/>Pulse server, Opus, UDP"]
      CTL["stream/control/<br/>ENet control, input, feedback"]
    end
  end

  APP["Application<br/>(systemd user unit)"]

  C -- "pair / launch / resume / cancel" --> HTTP
  C -- "DESCRIBE / ANNOUNCE / PLAY" --> RTSP
  HTTP --> MGR
  RTSP --> MGR
  MGR --> Session
  APP -- "Wayland / X11 surfaces" --> COMP
  APP -- "Wayland / XWayland surfaces" --> COMP
  APP -- "PulseAudio protocol" --> AUD
  COMP -- "DMA-BUF frames" --> VID -- "RTP video" --> C
  AUD -- "RTP audio" --> C
  C -- "input, StartB, IDR, FEC status" --> CTL
  CTL -- "keyboard, pointer, touch, pen" --> COMP
  CTL -- "rumble, LED, HDR, motion" --> C
```

| Component | Responsibility | Does not own |
| --- | --- | --- |
| `src/main.rs` | Load config, start the application library, wait for the user D-Bus session, run host checks, construct services, handle SIGTERM/SIGINT | Session state |
| `config.rs` | Deserialize `config.toml` | Runtime reconfiguration |
| `app_scanner/` | The application catalog: discover Steam, Lutris, Heroic and desktop-entry applications, follow their changes, keep stable IDs and visibility choices, render box art | Sessions (a launch copies its entry) |
| `healthcheck.rs`, `gpu.rs` | Probe GPU, Vulkan, DMA-BUF and encoder profiles; verify the capture and encode devices are the same GPU | Encoding |
| `discovery.rs` | Advertise `_nvstream._tcp` over mDNS | — |
| `tls.rs`, `clients.rs`, `state.rs`, `durable.rs` | Server identity, paired-client trust, durable `state.toml` | Session authorization |
| `webserver/` | GameStream HTTP/HTTPS API: server info, app list, pairing, launch/resume/cancel, PyroWave bandwidth probe | Stream properties |
| `calibration/` | UDP bandwidth calibration runs on the idle video port through the production sender; yields the port to sessions | Calibration runs |
| `rtsp.rs` | RTSP OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY; parse and validate negotiated stream contexts | Encoders or sockets |
| `ingress.rs` | Bounded, cancellable tasks for accepted HTTP, HTTPS and RTSP connections | Protocol semantics |
| `session/manager.rs` | The single session's lifecycle, transitions, authorization generations, keys and teardown | Resource creation details |
| `session/mod.rs` (`SystemSession`) | Builds the compositor, application and streams for each transition | Lifecycle decisions |
| `session/application.rs` | Run the application, pre/post commands and output routing as the `moonshine-session.service` systemd user unit | — |
| `session/compositor/` | Wayland/XWayland server, scene, focus, cursor, Steam classification, output mode, capture and frame export | Codec negotiation, transport |
| `session/stream/video/` | Import, convert and encode captured frames; packetize, protect, encrypt, pace and send | Scene composition |
| `session/stream/audio/` | PulseAudio-compatible capture server, Opus encoding, FEC, encryption and UDP | — |
| `session/stream/control/` | ENet control channel, peer authorization, input decoding and routing, virtual controllers, client feedback | Wayland focus |
| `management/` | Session-bus management interface: status, pairing, clients, configuration store, telemetry aggregation | Session, pairing or trust decisions (it calls the owners) |
| `gpu_sensors/` | Capture GPU sensors and per-process GPU time for telemetry and support bundles | Anything on a media path (it runs on blocking threads, once per second) |
| `support/` | Redacted support bundles: collection, redaction, archive | Changing server state (it only reads) |
| `session/status.rs` | Public session phase from manager status and control-stream ownership | Lifecycle transitions |

Boundaries to preserve:

- Scene visibility and input focus belong to the compositor. Encoding, packet
  transport and FEC belong to `stream/video/`. Control decoding and virtual
  devices belong to `stream/control/`.
- Surface presentation and native color descriptions belong to the compositor;
  frames carry their declared encoding to the video pipeline.
- Developer tools reuse the production session interfaces through
  `SessionManager`; they do not own a parallel stack.

## Process and threading model

The server is a multi-threaded Tokio process. Latency-sensitive or blocking work
runs on dedicated threads that communicate through bounded channels:

| Thread or task | Work |
| --- | --- |
| `compositor` thread | calloop event loop: Wayland dispatch, XWayland, input injection, refresh timer, scene capture |
| `video-pipeline` thread | DMA-BUF import, conversion and encoding (Pixelforge or PyroWave) |
| Video packet task | Packetization, FEC, encryption, pacing and UDP send |
| `pulse-server` thread | PulseAudio protocol server for the application |
| `audio-encode` thread and audio packet task | Opus encoding, audio FEC/encryption and UDP send |
| Control task | ENet service loop, message decoding, input routing, feedback |
| `gamepad-input` thread | Inputtino virtual controller state and native feedback callbacks |
| Manager-owned tasks | Session transitions and teardown |
| `library` thread | Application discovery, change detection and catalog publication, at nice 10 and idle I/O priority |

Every session worker registers a `lifecycle::WorkerGuard` before it is spawned,
so session completion means every worker has exited and released what it owns.

## Startup and capability advertisement

`src/main.rs` loads or creates the configuration, starts the application
library (its initial scan runs on the `library` thread while the host checks
run), waits for the user D-Bus session, then runs the health check. Listeners
start once the initial catalog is published, or after 30 s with the manual
entries if a scan hangs. `pyroshine healthcheck`
runs the same checks and exits. `--no-health-check` skips the report but still
probes GPU capabilities, and the server refuses to start without DMA-BUF import.

Advertised codecs and HDR come from encoder and profile probes, not from finding
a library or extension name:

- `gpu.rs` selects the Vulkan device by the opened capture node and independently
  verifies it matches the GBM/EGL render node
  used for capture (`VK_EXT_physical_device_drm`, with the complete PCI address
  as a fallback). An unresolvable or mismatched identity advertises no codec,
  HDR or DMA-BUF capability and prevents launch. No cross-device
  capture-to-encode path exists.
- The compositor publishes the verified, shared `VideoContext` before it reports
  ready. Conventional encoding, DMA-BUF import and PyroWave device matching use
  that context for the whole session, including reconnects.
- A conventional codec bit is set only after creating its exact Pixelforge
  profile and after the stream converter's known-answer self-check passes for
  the output format that profile encodes from (NV12, P010, YUV444 or
  YUV444P10), on the configured conversion queue. A GPU that converts with
  wrong colors keeps those profiles unadvertised and reports a **Color
  conversion** warning, or a failure when no profile remains. Stream setup runs
  the same self-check and stops the stream with a color-conversion cause
  instead of sending corrupted video. PyroWave bits are set only after loading
  the pinned C API, matching the Vulkan device and creating each SDR/HDR
  encoder. A missing or incompatible PyroWave library leaves the conventional
  codecs available.

The server then constructs, in order, TLS identity, `SessionManager`,
`ClientManager`, the management service, the RTSP server, the webserver and
mDNS discovery, and waits for a shutdown signal. The management service is
optional: failing to connect to the session bus or to own its name is logged
and the server runs unchanged.

## Client lifecycle

```mermaid
sequenceDiagram
  participant C as Client
  participant H as HTTPS API
  participant M as SessionManager
  participant R as RTSP
  participant K as Control stream
  C->>H: /launch (paired certificate)
  H->>M: initialize + launch (new authorization generation)
  M-->>H: compositor and application running
  C->>R: DESCRIBE, SETUP, ANNOUNCE
  R->>M: validated pending video/audio contexts
  C->>R: PLAY
  R->>M: start streams (or commit reconnect)
  C->>K: ENet connect, authenticated control
  K->>M: StartB opens audio/video start latches
  Note over C,K: streaming
  C--xK: disconnect or ping timeout: pause media, release input
  C->>H: /resume, then ANNOUNCE and PLAY again
  C->>H: /cancel (or application exits): one teardown
```

1. **Discovery and pairing.** Clients find the host through mDNS or manually.
   Pairing is approved by the host operator through the management operations
   (web interface, tray or `pyroshine pair`); trust is persisted in
   `state.toml` before it is published. See [Security administration](SECURITY_ADMINISTRATION.md).
2. **Launch.** An authenticated HTTPS `/launch` validates the request, creates an
   authorization generation and session keys, then initializes and launches the
   session: the compositor starts (with XWayland), and the application starts as
   a systemd user unit with `WAYLAND_DISPLAY`, `DISPLAY` and `PULSE_SERVER`.
3. **Negotiation.** RTSP DESCRIBE advertises the probed formats. ANNOUNCE carries
   the client's codec, chroma, dynamic range, resolution, FPS, bitrate, packet
   size, encryption and audio layout; it is validated and stored as a *pending*
   context. PLAY commits it by starting the stream workers.
4. **Streaming.** The control stream admits the authenticated peer. Its `StartB`
   opens the video and audio start latches, and media flows.
5. **Disconnect and resume.** Losing the control peer (ENet disconnect or the
   `[stream].timeout` ping deadline) pauses media delivery and releases its
   input, but keeps the application running. `/resume` starts a new
   authorization generation; the following ANNOUNCE and PLAY either fast-resume
   unchanged settings or reconfigure.
6. **Stop.** `/cancel`, application exit, a worker failure, a failed transition
   or service shutdown starts exactly one teardown.

## Session lifecycle and negotiation

`session/manager.rs` owns the single optional session; `session/mod.rs` builds
its `Initialized`, `Launched` and `Active` states. Application and compositor
lifetime is separate from a client's *stream epoch*: a reconnect can keep the
application while resetting or replacing encoders and transport state.

| Step | Owner and contract |
| --- | --- |
| HTTP launch | Authenticated GameStream API initializes and launches the application and compositor |
| RTSP ANNOUNCE | Validates negotiated formats and numeric domains; for an active session, pauses the live epoch, then publishes pending video/audio contexts |
| RTSP PLAY | After checking every prerequisite, constructs initial streams or commits a reconnect transition. The first PLAY applies the ANNOUNCE output mode (resolution, refresh rate, HDR) to the running compositor before starting streams, or fails; the session context then reports the negotiated mode, as after a reconnect |
| Control `StartB` | Opens the persistent audio/video start latches; tools open the same latches through the manager |
| HTTP resume | Validates and publishes session keys and retains requested session parameters; RTSP remains authoritative for encoded stream properties |
| Unchanged reconnect | Pauses both streams, keeps the video pipeline and Pulse sockets, resets client-visible sequencing and encoder state (IDR), and activates transport before PLAY completes |
| Changed reconnect | Pauses both epochs, reconfigures compositor output only for resolution, refresh rate or HDR changes (configuring existing windows held at the output size to the new size), and commits new video/audio resources before activating delivery |
| Failed ANNOUNCE pause | One medium may already be paused and a worker no longer answers; the session is handed to teardown |
| Cancel, application exit or failure | One teardown stops the application unit and joins every worker; only then can a new launch start |

Negotiation invariants:

- **Pending is not active.** Keep pending and active contexts separate until
  PLAY. Epoch barriers ensure old frames and packets never enter a new stream.
- **Validate before changing anything.** Launch, resume and ANNOUNCE values are
  checked against shared numeric domains (`session/negotiation.rs`,
  `VideoStreamContext::validate`) before the manager pauses, rekeys or
  reconfigures. A rejected request leaves the working stream unchanged. The
  domains are consumer limits (representable extents, one-datagram shards,
  32-bit rate control, implemented audio durations), not quality caps.
- **No silent fallback.** An unsupported codec, chroma, bit depth or range fails
  negotiation; the server never substitutes another format.
- **Epochs belong to one generation.** PLAY snapshots its authorization
  generation and the keys published for it under the manager lock; the start
  request or reconnect plan carries that snapshot to every worker, and each new
  epoch (`BeginEpoch`) is activated for that generation. The video and audio
  senders deliver only after `StartB`, to an endpoint discovered by the active
  epoch's generation, while that generation is the current authorization. A
  `/resume` therefore ends delivery to the replaced client immediately, without
  waiting for ANNOUNCE, and interrupts a paced send in progress; a PING of the
  new generation discovers an endpoint but delivers nothing until its PLAY
  activates an epoch. A PLAY whose generation a newer `/resume` replaced while
  it reconfigured fails; the manager records the contexts the workers now run so
  the next reconnect is planned against reality.
- **Lifecycle commands do not wait for `StartB`.** The start latch gates
  capture, encoding and sending only. Pause, reset, reconfiguration and epoch
  activation are served before it, so a client that completed PLAY and
  disappeared before `StartB` can be resumed (unchanged or changed mode).
- **Audio follows the same barrier.** Every reconnect, including unchanged
  settings, runs Pause → producer reset → begin epoch for audio as for video.
  Opus and RTP/FEC state are recreated and Pulse discards queued PCM before
  activation. Pulse client connections survive; a layout change rebuilds output
  converters without closing application sockets.

Use the [reconnect validation](reconnect-validation.md) matrix for changes here.

### Authorization and keys

- Each authenticated `/launch` or `/resume` starts an **authorization
  generation** (`session/authorization.rs`): the paired client's normalized
  address plus fresh session identifiers. RTSP accepts only that address, and
  ANNOUNCE/PLAY commit only within the generation that issued them. RTSP
  possession alone never implies pairing authorization.
- Media PINGs must come from that address and, for clients announcing
  `ML_FF_SESSION_ID_V1`, echo the generation's ping payload; source ports may
  differ for NAT. A PING discovers an endpoint but cannot activate a paused epoch.
- The control stream (`stream/control/peers.rs`) admits candidates by address
  and connect data, but dispatches only the peer that authenticates with the
  HTTPS-delivered AES-GCM key. Only that active peer extends the stream timeout
  and receives feedback.
- Session keys (`session/keys.rs`) carry a server-owned generation; consumers
  detect changes by generation, never by the client's `rikeyid`. AES-GCM nonce
  counters for video and control belong to the key bytes through a
  process-lifetime ledger, so recreation, reconnect or a later session reusing
  the key continues the same counters. An encrypted video epoch encrypts every
  shard or emits none; nonce exhaustion retires the key. Audio uses the
  protocol's AES-CBC IV (`rikeyid` plus RTP sequence).

### Input ownership across reconnects

The authenticated controlling peer owns input. When it disconnects or its
generation is replaced, the control stream releases held keys and buttons,
cancels touch, pen and text input, neutralizes every virtual controller,
cancels pending Home/Guide timers and revokes the feedback route before the
cleanup is acknowledged.

Virtual controllers stay plugged in across ownership changes, so a retained game
does not observe an unplug. Native Inputtino callbacks hold an owner-switchable
route (`control/input/ownership.rs`), never a peer's channel; feedback produced
while unowned is dropped. The next peer's first input for a slot claims it and
replays LED and trigger-effect state (never rumble). A different controller
identity, a controller missing from the client's active mask, or teardown
destroys the device. A delayed disconnect from a replaced generation cannot
release the new owner's input.

Feedback delivery never blocks: the control loop drains the owner's bounded
feedback channel while it may itself wait to queue gamepad commands, so a
blocking producer could stall input, reconnect and shutdown. When the channel is
full, the route coalesces per kind (the latest rumble supersedes an undelivered
one, so a stop is never lost; LED, trigger and motion-enable state is re-sent
from current state) and the gamepad timer task retries until the owner accepts
it or is revoked. Native controller threads (report, UHID event, force-feedback
listener) are owned and joined by the device's destructor, which therefore
returns only after no callback can run and the kernel device is gone; see
`vendor/inputtino/LOCAL_CHANGES.md`.

## Session ownership and shutdown

The manager's lifecycle is explicit; absence of state never means idle:

| State | Meaning |
| --- | --- |
| `Idle` | No session and no owned resources. The only state that accepts `/launch`. |
| `Live` | A session record (epoch, stop manager, authoritative context, live stream contexts, application unit, start latches) plus either the owned state or one in-flight transition that checked it out. HTTP and RTSP see the context in both cases. |
| `Stopping` | One teardown task owns everything. The session is not reported; its keys, pending contexts and authorization are retired, and a replacement launch waits (bounded) for completion. |

**Transitions** (initialize, launch, PLAY start/resume, active ANNOUNCE pause)
validate every prerequisite under the manager mutex, then run in a
manager-owned task with the mutex released. An HTTP timeout or dropped RTSP
connection does not drop the work. Each transition holds its session's
completion token and runs under the session's cancellation, so a stop cancels
it at any await and teardown waits until it hands back what it checked out. A
result commits only if the session epoch and transition id are still current
and the session is not stopping. A failed transition never leaves half-applied
state: it starts a full teardown. Duplicate, premature or stale requests are
rejected without touching the retained application or streams.

**Workers** (compositor, video pipeline thread and packet task, audio encoder
and packet task, PulseAudio server, control stream, gamepad thread) register a
`lifecycle::WorkerGuard` before they are spawned and drop it last, after their
sockets, threads, GPU objects and frames. Stream workers produce and send media
only after `StartB`, through a persistent `lifecycle::StartLatch`: it may open
before, during or after workers wait, duplicate opens are no-ops, and a stop
before `StartB` cancels the wait and releases the worker's socket. While waiting
they keep serving pause, reset and reconfiguration commands.

**XWayland** is owned by the compositor (`compositor/xwayland_process.rs`),
which holds a pidfd for the child it spawned. Teardown closes its Wayland
client, waits (then `SIGKILL`s via the pidfd), and releases the display lock and
sockets before the compositor's worker guard drops. A surviving process keeps
the session `Stopping`. A retained-session reconnect does not touch XWayland.

The **compositor thread** is joined before its worker guard drops, and it holds
the capture Vulkan device until then. The GPU driver releases the thread's EGL
state in thread-local destructors after the compositor returns; on NVIDIA,
destroying a Vulkan device while they run deadlocks inside the driver. Since
the video pipeline usually drops the last reference to the capture device, the
device must outlive the thread, not only the compositor loop.

**Teardown** has a single owner per session, started by user cancel, a worker or
application exit (through a per-session watchdog that only hands over), a failed
transition, or service shutdown. In order, it stops the application unit while
the compositor and audio still serve it (if a transition is in flight, that is
cancelled first and the unit is stopped after it hands back), triggers the
session stop, drops the owned state, waits for every worker and transition,
drops state handed back by transitions, then reports `Idle`. The
unit name is recorded before a launch starts, so a launch cancelled after
systemd accepted the unit is still stopped. `Application` drop never blocks;
stopping the unit is an awaited, bounded D-Bus job.

`Idle` requires established application termination: after the stop job,
systemd must report the unit unloaded, or inactive/failed with no process left
in its cgroup (a unit awaiting garbage collection counts as stopped). The stop
job's own result is not trusted on its own. The client waits as long as
systemd's stop policy allows (5 s SIGTERM allowance for the application and
again for `ExecStopPost` hooks, then SIGKILL; `application.rs` derives the
deadline and the manager uses it). If termination cannot be established, the
session follows the terminal teardown policy below instead of becoming idle.
A launch first stops a leftover unit of the same name and refuses to start
until it is unloaded.

**Deadlines.** Application stop is bounded to 15 s (`APPLICATION_STOP_DEADLINE`:
the 12 s stop-job wait plus the 2 s termination check and bus connection) and
worker exit to the rest of a 25 s end-to-end deadline
(`SESSION_TEARDOWN_DEADLINE`). Exceeding the deadline, or failing to establish
that the application terminated, is terminal: the session stays `Stopping`, new
sessions are refused, and the service shuts down for its supervisor to restart
it. Service shutdown (SIGTERM/SIGINT) completes
only after session teardown finishes or fails within the same deadline.

Tests: `session/manager/lifecycle_tests.rs` drives the manager through a fake
backend with barriers and fault injection at every transition await; stream
workers have socket-release tests in their modules.

## Compositor and presentation

`session/compositor/` embeds a headless Smithay compositor on its own calloop
thread. It owns:

- the Wayland display, XWayland, and the virtual output (mode, refresh rate,
  scale, HDR state);
- scene, stacking, focus and Steam window classification (`focus.rs`,
  `x11_focus.rs`), following Gamescope's Steam focus model;
- cursor state (`cursor.rs`) and output scaling (`scaling.rs`);
- native `wp_color_management_v1` surface descriptions and scene color draws
  (`color_management.rs`, `color_render.rs`);
- input injection into the Smithay seat (`input.rs`);
- capture and frame export (`capture.rs`, `admission.rs`, `frame.rs`).

Capture pacing is per client epoch. With fixed pacing (every standard client)
the refresh timer is the only capture clock: each refresh deadline offers at
most one capture, independent of encoder completion. A client that presents on
a VRR display asks for presentation-driven capture (`clientVrrRequested`):
each latched buffer commit then opens a capture opportunity, rate-limited to
the negotiated FPS, and frame callbacks follow those flips. Buffer releases,
frame callbacks and input continue even while capture is blocked. Each frame
carries its content time, from which the video stream derives the RTP
timestamp (see [capture pacing](PIPELINE_OPTIMIZATION.md#capture-pacing-and-source-timing)).

**Presentation.** Applications present through ordinary Wayland or X11 surfaces.
**Capture.** Capture visibility and input focus are separate decisions:

- *Direct export* hands the encoder the application's own DMA-BUF when it
  represents the entire visible scene, optionally with a cursor and Steam
  notification as late-composited layers.
- *Composition* renders the scene through GLES into a compositor-owned DMA-BUF
  pool and waits for completion before export.

Commits are latched only once their DMA-BUF has finished rendering, and color
descriptions and metadata (sRGB, BT.2020/PQ, scRGB) follow the captured source.
Direct PQ stays encoded; direct scRGB is converted by the encoder. Composition
normalizes HDR into PQ during the existing scene draw. XWayland without native
color declarations is SDR. See [native presentation](NATIVE_PRESENTATION.md). See
[Compositor](COMPOSITOR.md) for eligibility, cursor and Steam rules.

## Video path

```text
compositor capture ─ExportedFrame─▶ video-pipeline thread ─encoded frame─▶ packet task ─UDP─▶ client
   (admission credit)             import → convert/encode            packetize → FEC → encrypt → pace
```

**Admission.** `compositor/admission.rs` gives the consumer receiver-driven
demand: one credit and one handoff slot. The compositor claims the credit before
any export or rendering. Two signals are independent:

- **Capture admission** says whether the pipeline can accept another scene.
- **Buffer consumption** (`consumed`) says whether the GPU has finished reading
  the source.

Releasing a source buffer never creates a second admission credit, and paced
sending never retains a source buffer after GPU consumption. Every
`ExportedFrame` also holds a `SourceLease` (a strong DMA-BUF reference) so its
descriptors stay valid while queued, imported or read. Epoch changes reject old
completions without replenishing demand. See
[Capture pipeline](PIPELINE_OPTIMIZATION.md).

**Encoders.** `stream/video/format.rs` treats codec, chroma, bit depth,
transfer, primaries, matrix, range and HDR as independent negotiated properties.

| Backend | Formats | Path |
| --- | --- | --- |
| Pixelforge (`pipeline/mod.rs`, `pipeline/convert.rs`) | H.264, HEVC, AV1 via Vulkan Video | DMA-BUF import → compute conversion (with optional cursor/notification layers) → asynchronous encode with three bounded in-flight frames → rate-control padding removed (`pipeline/padding.rs`) → HDR SEI/OBU metadata on keyframes (`pipeline/hdr_sei.rs`) |
| PyroWave (`pyrowave.rs`, `pyrowave_protocol.rs`) | Intra-only wavelet codec, 4:2:0/4:4:4, SDR/HDR10 | DMA-BUF import into a PyroWave-owned Vulkan device on the same GPU → GPU scale, color transform and encode → one frame at a time through send completion |

PyroWave is a separate codec (`bitStreamFormat` 3); it never impersonates a
conventional codec and has no CPU or alternate-codec fallback. `pyrowave.rs`
owns the C API's unsafe boundary: exact ABI checks, handle ownership, and
destroying child resources before the device and library. See
[PyroWave](PYROWAVE.md).

**Transport.** `packetizer.rs` preserves Moonlight's RTP/NV framing and
sequencing. Each frame's shards live in one contiguous `ShardBatch`
(`shard_batch.rs`), which also carries network credits and transport outcomes.
Reed-Solomon FEC (`fec.rs`) covers plaintext shards, then optional AES-GCM
encryption is applied per shard. `fec_mode = "auto"` adjusts parity from the
client's FEC status feedback. `gso_socket.rs` sends with UDP GSO and a
per-datagram fallback; raw `WouldBlock` retries must go through Tokio readiness. PyroWave frames are paced across a
bitrate-derived window anchored to capture time (`pacing_timer.rs`); a frame
whose window elapsed during encode is sent before the next capture slot, and
the schedule rebases after backpressure rather than bursting.

**Failures.** Both encoder backends report frame failures through
`pipeline/failure.rs` with the failed stage, a recovery (`DropFrame`,
`RequestIdr` or `Terminal`) and whether the source's GPU reads are not
submitted, completed or unknown. Only the first two release the source for
reuse. Device or API loss is terminal; a recoverable failure that repeats for
300 frames and 5 s without success is escalated. A terminal failure ends the
pipeline, which stops the session.

**Diagnostics.** With `[stream.video] log_stats`, the pipeline emits five-second
capture, cadence, pipeline, transport, import and process summaries
(`stream/video/diagnostics.rs`). See
[Streaming diagnostics](LONG_SESSION_PERFORMANCE.md).

## Audio path

```text
application ─PulseAudio protocol─▶ pulse-server thread ─PCM─▶ audio-encode thread ─▶ audio packet task ─UDP─▶ client
                                   (sink, resampling)        Opus → RS FEC → AES-CBC
```

`stream/audio/pulse_server/` is an embedded PulseAudio-compatible server whose
socket is passed to the application as `PULSE_SERVER`. It mixes and converts
application streams into the negotiated layout (stereo, 5.1 or 7.1). The
encoder produces Opus packets of the negotiated duration (5 or 10 ms), adds
GameStream audio FEC and optional AES-CBC encryption, and the packet task sends
them over UDP. Opus bitrate is bounded by Moonlight's 1400-byte audio packet
limit. Reconnects follow the epoch barrier described under negotiation.

The stream layout and the bitrate are negotiated separately, independent of the
video codec. The client chooses the layout: channel count, and for surround the
GameStream high-quality layout (`x-nv-audio.surround.AudioQuality`), whose
separate mono streams it must decode. The bitrate of stereo and the normal
surround layouts comes from an audio quality level (`AudioQuality` in
`stream/audio/mod.rs`). DESCRIBE advertises the highest level as
`x-moonshine-audio.quality:2`; Pyrolight requests a level (`0` standard, `1`
high, `2` maximum) with the same attribute in ANNOUNCE. A higher number selects
the highest level; malformed or duplicated values are rejected with 400. Other
clients get `[stream.audio] quality`. Opus packets carry their own size, so every
client decodes every level. The high-quality surround layouts keep their fixed
bitrates.

The server's single thread serves every client, the capture clock and
reconfiguration, so no client may hold it: a receive pass reads at most
256 KiB before others get a turn (the client is rescheduled explicitly, since
its edge-triggered readiness will not fire again), a zero-byte read ends the
client after handling its complete requests, and unread replies above 64 MiB
drop the client while it is being served.

## Control and input path

`stream/control/` runs the GameStream control protocol over ENet (UDP). After
peer authorization it decrypts AES-GCM control messages and dispatches:

| Message class | Destination |
| --- | --- |
| Keyboard, text, mouse, scroll, touch, pen | Compositor input channel, injected into the Smithay seat with the compositor's focus |
| Controller arrival, state, touchpad, motion, battery | `control/input/gamepad.rs` → Inputtino or native Valve UHID devices on the `gamepad-input` thread |
| `StartB`, IDR requests, reference-frame invalidation, FEC status | Stream start latches and the video stream handle |
| Ping | Active-peer liveness (`[stream].timeout`) |

Feedback flows back over the same channel: rumble, trigger effects, LED and
motion-enable requests from virtual controllers, and HDR mode and metadata from
the video stream.

Virtual controller family follows `[stream.control.gamepad] emulation`:
Xbox/Elite, PlayStation (including DualSense Edge), Nintendo or native Valve models. Steam
Input routing on the host is separate from virtual-device delivery. See
[Compositor](COMPOSITOR.md#steam-classification-and-input) and
[native controllers](NATIVE_CONTROLLERS.md) and [DualSense Edge](DUALSENSE_EDGE.md).

## Configuration and persistent state

| Data | Location | Owner |
| --- | --- | --- |
| Server configuration | `config.toml` (`$XDG_CONFIG_HOME/moonshine/` by default), read once at startup | `config.rs` and the owning subsystem's config type |
| TLS identity | `cert.pem`, `key.pem` (default `~/.config/moonshine/`) | `tls.rs` |
| Server UUID and paired-client trust | `state.toml` in the data directory (default `~/.local/share/moonshine/`) | `state.rs`, `clients.rs`; atomic replacement through `durable.rs` |
| Application IDs and visibility choices | `library.toml` beside `state.toml` | `app_scanner/identity.rs`; atomic replacement through `durable.rs` |
| Last successful scan results, rendered box art | `library-cache.json` and `boxart/` in `$XDG_CACHE_HOME/moonshine/` (regenerable) | `app_scanner/service.rs`, `app_scanner/artwork.rs` |
| Negotiated stream properties | RTSP ANNOUNCE, per stream epoch | `rtsp.rs`, session manager |
| Diagnostic switches | `MOONSHINE_*` environment variables | Individual subsystems |

Configuration is not reloaded at runtime. Each subsystem receives its own typed
config section when the session manager or server is constructed. The desktop
app edits the file through the management configuration store, which only
reports that a restart is required. Client-chosen
properties (resolution, FPS, bitrate, codec, chroma, HDR, audio layout) are
negotiated per session, not configured. The application receives
`MOONSHINE_CLIENT_WIDTH`, `MOONSHINE_CLIENT_HEIGHT` and
`MOONSHINE_CLIENT_FRAMERATE`. See [Configuration](CONFIGURATION.md) and
[Security administration](SECURITY_ADMINISTRATION.md).

Accepted HTTP, HTTPS and RTSP connections run in bounded, cancellable tasks
(`ingress.rs`) that hold a global shutdown delay token and apply TLS-handshake,
request-header and RTSP-framing deadlines. Pairing trust changes are serialized,
written atomically and synced before they take effect; revocation drains
in-flight launch/resume/cancel requests and stops the active session.

## Application catalog

`app_scanner/` owns what clients are offered. The `library` thread runs every
scanner, follows their metadata and publishes immutable `CatalogSnapshot`s
through a `watch` channel; `/applist`, `/appasset` and `/launch` clone the
snapshot current when the request starts, and `/launch` copies the chosen
entry's `ApplicationConfig` into the `SessionContext`. The catalog is therefore
never consulted by a running session: a refresh cannot stop, restart or
reconfigure it, and `/serverinfo` keeps reporting the launched ID. The worker
never takes session-manager locks.

```text
launcher metadata ──inotify / periodic fingerprint──► library thread
  (manifests, pga.db,                                  scan changed sources (units)
   library JSON, .desktop)                             reconcile + identities + visibility
                                                       ──watch──► CatalogSnapshot (Arc)
                                                                   ├─ webserver: applist, appasset, launch
                                                                   └─ management: GetLibrary, LibraryChanged
```

**Units and failures.** A scanner reports results per unit (a Steam library
folder, a Heroic library file, a desktop-entry directory, the Lutris database).
A unit that cannot be read keeps its previous entries, marked stale, for up to
seven days; a source that cannot list its units keeps all of them; a single
unreadable Steam manifest keeps that game. Units are dropped only when the
launcher stops listing them. Results are retained across restarts in the cache
directory and reused only under an identical scanner configuration.

**Change detection.** Each scanner names directories and the file names that
matter in them. They are watched with inotify, non-recursively; unrelated
activity (downloads, shader caches, API caches) never wakes the worker. A burst
of changes is scanned once it is quiet for 2 s (at most 10 s after it starts),
no sooner than 15 s after the source's previous scan, and only that source is
scanned. Watches are re-armed after every scan, which follows replaced,
unmounted and newly created directories; a queue overflow marks every source
changed. Every `[library].poll_interval_secs`, a stat-only fingerprint of each
source's metadata is compared, covering what inotify cannot report. Units that
failed to parse are retried after 2, 5, 15, 30 and 60 s. `RescanLibrary` scans
everything at once.

**Identity.** Manual entries keep the ID earlier releases derived from the title
(SipHash-1-3 of the title, implemented in `identity.rs` so a toolchain update
cannot renumber them). Discovered entries are keyed by launcher identity
(`steam:620`, `heroic:legendary/Fortnite`, `lutris:<slug>`, desktop file ID); a
new key takes the title-derived ID when it is free, so the first run after an
upgrade reports the IDs clients already know, and a collision gets a
SHA-256-derived ID instead. The assignment is recorded in `library.toml`, so
renames, reinstalls, reordering and restarts keep it. An ID of a removed game is
not reused for another title; at most 10,000 records are kept, removed visible
entries first. A file written by a newer version is used but never rewritten; a
corrupt one is moved aside.

**Reconciliation** (`catalog.rs`): manual entries first; a discovered entry
with a manual entry's title is shadowed by it; a launcher shortcut to an entry
another scanner provides, or the same identity found twice, is a duplicate;
hidden entries keep their ID but are not offered; equal titles from different
launchers get the launcher's name appended.

**Box art.** Entries carry ordered candidates (launcher art, then icons, then a
theme icon named like the title). `/appasset` renders the first one that
decodes on a blocking thread, at most two at a time, letterboxed to Moonlight's
tile, and caches the PNG keyed by the source's path, size, mtime and inode
(128 MiB / 2,048 files, least recently used first). Undecodable sources are
skipped until they change. Nothing is fetched from the network.

## Management

`management/` is the one control surface over the running server. Its
operations (`management/ops.rs`) call the existing owners; two transports
expose them, and the contract (`moonshine-management`) is shared by both:

```text
browser (any device)                     pyroshine (service)
  web/ (React)  ──────── HTTP/SSE ──▶ web/   ─┐
                                               ├─ ops ── SessionManager, ClientManager,
pyroshine tray/pair/web/diag/open              │         config store, Library, history,
  ── session bus: methods, signals ──▶ dbus.rs┘         updates, telemetry
                                         hub ◀── follow_* tasks (state changes)
```

| Operation | Owner and path |
| --- | --- |
| End session | `SessionManager::stop_session`, the same teardown as `/cancel` (behind the authorization gate) |
| Approve / reject pairing | `ClientManager` pending requests; the approval token (`request`) must match the shown request, and the PIN is decimal, at most 16 digits |
| Revoke client | `ClientManager::revoke_and_stop_session`, shared with loopback `/unpair` |
| Name a client | Optional, display-only metadata in `state.toml`, written through the state transaction |
| Configuration | `config_store` (below) |
| Run health check | `healthcheck::recheck_in_server` (below), serialized |
| Library, rescan, show/hide, box art | `app_scanner::Library` (see [Application catalog](#application-catalog)); box art is the cached `/appasset` rendering |
| Restart | Triggers the global shutdown with `ShutdownReason::Restart` after answering; once shutdown completes, `src/restart.rs` marks every descriptor close-on-exec and executes the executable path recorded at startup (an upgrade takes effect) with the original arguments, keeping the PID, so it works without a supervisor |
| Show tray | Wakes the server's tray launcher (`src/tray/launcher.rs`), which starts a tray hidden from its menu again; `pyroshine open` asks when no tray answers |
| History, updates | `management/history.rs`, `management/updates.rs` (below; web only) |
| Web setup code | `management/auth.rs` (D-Bus only: proves control of the streaming account) |

Transports authenticate the caller and map results; they never re-implement a
rule. Documents are sanitized views: no session keys, PINs, pairing secrets or
private keys cross either transport.

**Event hub.** The `follow_*` tasks (session, pairing, statistics, problems,
library) and the history writer publish each change once to `management/hub.rs`,
a bounded `broadcast`. The D-Bus service turns events into signals; each web
event stream turns them into Server-Sent Events. A transport that lags loses
events and resynchronizes from complete snapshots; no publisher waits for a
consumer. The hub counts open web event streams: statistics and library
progress are serialized only while a page watches them (the tray shows neither).

**Web transport.** `management/web/` serves the frontend's files and
`/api/v1/` on a listener of its own (`[management]`, default
`127.0.0.1:47990`), never on the GameStream ports, with bounded connections
(`Ingress`), header and body deadlines and body limits. Files under
`share/pyroshine/web` are read once at startup into a map; request paths are
only keys into it. Every request passes, in order: the `Host` allow-list (IP
literals, `localhost`, the host's names on non-loopback listeners and
`allowed_hosts`, defeating DNS rebinding); same-origin checks (`Origin` equal to
`scheme://Host`, `Sec-Fetch-Site`); for changes a JSON content type; then the
session (`auth.rs`) and, for changes, the session's CSRF token. Responses carry
a strict CSP and are never CORS-enabled. A non-loopback address requires HTTPS
(browser-facing rustls config without client-certificate requests).
`GET /api/v1/stream` starts with a complete overview after subscribing to the
hub, then forwards events with increasing IDs; it re-checks its session on
revocation and every 20 s heartbeat and closes with an `auth` event when the
session ends.

**Administrator authentication.** `auth.rs` stores one PBKDF2-HMAC-SHA256
password verifier and SHA-256 hashes of session tokens in `management.json`
beside `state.toml` (0600, durable replace). The first password needs a
one-time setup code, issued only over the same-user D-Bus interface
(`CreateWebSetupCode`, `pyroshine web setup`), so neither a loopback
browser nor a paired client can claim the server. Sign-in always issues a new
token (no fixation); sessions expire idle (7 days) and absolutely (30 days);
the CSRF token is derived from the session token. Failed attempts lock the peer
address out progressively within a global budget, and PBKDF2 runs on blocking
threads, at most two at a time. A damaged credentials file fails closed until
a reset code replaces it. Sensitive actions are logged under `pyroshine::audit`.

**D-Bus transport.** `dbus.rs` exports `io.github.karsyboy.Pyroshine` (interface
`…Pyroshine.Management1`) on the service user's session bus (or
`MOONSHINE_MANAGEMENT_BUS` for a test instance). The bus admits only that user
and every method also checks the caller's Unix user. zbus serves methods on its
own executor; work that needs Tokio is handed to the server runtime. The
interface is optional: without a bus name the server and its web interface run
unchanged. `pyroshine pair` pins the current unique bus owner and matches
completion by client ID and request token; PIN submission alone never reports
success. The tray's bus name (`io.github.karsyboy.PyroshineUi`) marks it
present, which hands pairing notifications to it.

**Session status.** `SessionManager` publishes a `ManagerStatus` snapshot when
its mutex is released after a change (the guard publishes, so no transition can
forget), and the control stream publishes `ClientPresence` when the peer that
authenticated the current generation attaches or leaves. `session/status.rs`
derives the public phase:

| Phase | Derived from |
| --- | --- |
| `idle` / `stopping` / `error` | `Idle`; `Stopping` or a triggered stop; teardown past its deadline |
| `starting` | Initialize/launch/start transition, or the first generation's client not yet attached |
| `streaming` | Streams committed and the current generation's authenticated control peer attached |
| `client_disconnected` | That peer left (disconnect or ping timeout) and nobody resumed, or a waiting client exceeded `[stream].timeout` |
| `reconnecting` | `/resume` rotated the generation, or a reconnect ANNOUNCE/PLAY is pending or in progress |

A retained session without its client is therefore never reported as
streaming, and a peer of a replaced generation cannot make it so.

**Client address.** Session details report the client IP from the manager's
current `StreamAuthorization`, falling back to the launch context only during
initialization before the first grant exists. An accepted HTTPS resume rotates
that authorization and publishes its new generation through `ManagerStatus`,
so management publishes a session event with the new address to both
transports. The launch address stays in `SessionContext`; the session
epoch, start time and application lifetime survive client changes. A rejected
resume does not rotate authorization. An accepted resume owns the reported
address even if the client has not completed ANNOUNCE/PLAY.

**Foreground application.** Each session record owns a watch receiver for its
compositor's primary application metadata. The compositor publishes only
changed values on primary-focus selection and title/class/app-ID events.
Management subscribes before reading session details and also waits on that
receiver, so focus-only changes produce `SessionChanged` without a lifecycle
transition; the event reaches the web interface and the tray.
`SessionDetails.application` remains the configured Moonlight entry and ID;
the additive, optional `foreground_application` contains only a display title.
The channel belongs to the session lifetime, surviving disconnect/resume and
being replaced on a new session. Neither reporting nor signal delivery runs in
the frame path or waits for a UI consumer.

**Configuration store.** Pyroshine never rewrites `config.toml` on its own, so
the file is user-authored. A save deserializes the submitted values into
`Config`, runs `Config::validate` plus schema range checks on changed settings,
and edits the user's TOML document with `toml_edit`: only settings whose typed
value changed are touched, unchanged applications and scanners keep their
original tables and comments, an emptied list is written as `[]` (an omitted
one means the defaults), and a created `[webserver]` table is written complete.
The edited document must parse back into exactly the submitted configuration
or nothing is written. Saves are serialized, rejected when the file's content
revision changed since it was read, and replaced atomically with the previous
permission bits through `durable.rs`. Configuration is still read only at
startup; a save reports `restart_required` when the file differs from the
running configuration. The editor's schema (`management/schema.rs`) is tested
to cover exactly the settings `Config` serializes, and its choice values are
serialized from the Rust enums.

**Telemetry.** Both encoders already publish one `FrameStats` per frame on the
benchmark's `broadcast` channel. The management service subscribes while a
client is streaming, drains the channel on a 250 ms timer (no per-frame
wakeups) and summarizes one-second windows. It keeps the last 120 windows for
support bundles, feeds each to the session history and publishes it while a
web page observes (see the event hub). `broadcast::send` never blocks: a receiver that falls behind
loses the oldest samples, which are counted, and with no receiver (no
streaming client) it remains a no-op. Each sample also carries the frame's
capture pacing, content age and content-time interval since the previous new
frame (the frame time the client sees in RTP), summarized as the window's
frame pacing, and its `CaptureOrigin`: direct export, direct export with late
layers, or composition and the first `DirectReject` that required it (a fixed
enum, never a per-frame string), summarized as the window's capture path.

**GPU readings.** Once per window, `gpu_sensors::GpuSampler` reads the capture
GPU on a blocking thread (`StreamStats.gpu`): `hwmon` temperature, power and
clocks for every driver, amdgpu's `gpu_busy_percent`, `vcn_busy_percent` and
VRAM counters, Intel's actual frequency, and NVML (loaded at runtime from the
NVIDIA driver) for NVIDIA. GPU time is attributed from DRM fdinfo
(`drm-engine-*`, or xe's `drm-cycles-*`, per client ID, deduplicated across
descriptors) to two process groups: the application unit's cgroup (looked up
through systemd's `ControlGroup`) and Pyroshine's own service cgroup, which
includes XWayland. The proprietary NVIDIA driver publishes no fdinfo counters:
there, a group is present when NVML's running-process lists name one of its
processes (with its memory), and its busy shares average NVML's per-process
utilization samples in the window, which exist only for processes busy in a
recent sampling period. NVML is loaded and initialized once per process and
never shut down. Missing sensors are omitted, never reported as zero. Opening
the GPU, the cgroup lookup and each reading have a 3 s deadline; a step that
misses it is abandoned for the stream with a warning, and statistics continue
without it. Each stream logs `Collecting stream statistics` with the backend.

**Findings.** Each published window is read by `management/findings.rs`
(`StreamStats.findings`): an encoder slower than the frame budget while frames
are missed, a game rendering below the stream rate while every frame is
delivered, skipped captures, socket failures or discarded packets, high
pipeline latency, mostly composited frames, or uneven frame times under fixed
pacing. GPU readings refine them: an encoder that misses frames names a
saturated encode engine or a GPU kept busy by the game, and a fully busy GPU
that costs frames (`gpu_saturated`) states the application's and Pyroshine's
graphics and compute shares. Each finding has a stable ID, the window's numbers and a hint. Rules
name a cause only when the numbers show it, and they see only the host: client
decoding and display pacing are outside them. Analysis runs on the management
task once per window, never on a media path.

**Failure causes and events.** The launch path returns a typed
`session::failure::SessionFailure` (stable code, message, optional hint)
instead of a bare error: application spawning (empty or missing command, hook
resolution, systemd refusal, an exit during launch with systemd's exit status),
stream sockets, compositor start and manager state. `/launch` answers with it as
`status_message`, which Moonlight shows in its error dialog. A failure that
ends a created session is also recorded as that session's `StopCause` in its
`StopCauseSlot`; so are the application's exit (`ExitStatus` from the unit's
`Service` properties, subscribed before the unit starts because
`CollectMode=inactive-or-failed` unloads it at exit) and a terminal encoder
error. The first cause recorded before the stop wins; teardown copies it into
`StopRecord`, and `LastStop` reports it as `detail` and `hint`. RTSP
negotiation rejections, which stock clients show only as a status code, and
launch failures are reported through `SessionManager::report_problem`.
Management keeps the last 50 problems and unexpected stops as `ServerEvent`s
(`GetEvents`), emits each as `ServerEvent`, and includes them in
`GetDiagnosticReport` with the server, health, session, latest statistics and
host facts (kernel, OS, CPU, memory, GPUs, service unit).

**Support bundles.** `GetSupportBundle` returns a gzip-compressed tar archive
built by `crate::support` on a blocking thread: the diagnostic report, the
statistics history, host facts, one GPU sensor reading, the configuration,
`systemctl status` and the last 3000 journal lines of the service unit (found
from the daemon's own cgroup), and `vulkaninfo --summary`; external commands
have a deadline. `pyroshine diag` saves it, and collects a bundle itself when
no daemon is reachable, running the health check only when no Pyroshine server
runs. Everything passes through one `Redactor`: the home directory, user and
host names, the configured server name, the server ID, client IDs and names
from `state.toml`, fingerprints, UUIDs and IP addresses become placeholders,
and the configuration is redacted by setting (unknown keys are blanked; the
schema test guarantees every real setting is known). `GetDiagnosticReport` is
redacted the same way.

**Health checks.** `GetServer` carries the startup report; each check has a
stable ID, the first line of its message, and the remaining lines (the remedy
the command line prints indented) as its hint. `RunHealthCheck` runs
`recheck_in_server`, which differs from the standalone check in two ways: it
never binds the server's own TCP listeners (and the UDP stream ports only while
idle), and while a session exists it repeats the previous GPU probe results
instead of creating encoders next to the live stream. Capabilities advertised
to clients stay those probed at startup; a recheck whose verified profiles
differ adds a warning that a restart applies them.

**Support bundles over the web.** `POST /api/v1/diagnostics/bundle` returns the
same archive as an attachment; it needs the CSRF token like a change because it
is expensive and holds logs.

**Session history.** `history.rs` keeps a `Recorder` that follows the session
snapshots and statistics windows in memory and produces a row write when a
session opens, its client disconnects or resumes, and when it ends (with the
stop record and a summary of the windows: frame rate, bitrates, drops,
per-stage averages and worst p95, GPU load and temperature, finding counts).
Writes go to one writer thread owning SQLite (`history.sqlite3`, bundled
`rusqlite`) over a bounded channel with `try_send`: a slow disk drops a write
and counts it; nothing on a management task, let alone a media path, waits for
storage. The writer applies age and count retention after each write, migrates
by `user_version` (records are versioned JSON, so fields can be added), moves a
database that fails `quick_check` aside and starts a new one, closes rows left
open by a previous run as `interrupted`, and reports storage failures to the
page. Only host measurements are stored; client addresses and labels only when
`record_client_address` is on.

**Release notifications.** `updates.rs` fetches GitHub's release list over
HTTPS (hyper client, rustls with the system trust store, 20 s and 2 MiB bounds)
only when the operator asks or enabled daily checks (preferences in
`updates.json`, not configuration). Versions are compared by semver precedence
on the stable or prerelease channel; the release link is built from the
validated version, never taken from the response. Update steps follow the
detected installation (package databases, executable location). Nothing is
downloaded or installed.

**Notifications.** While the tray owns `io.github.karsyboy.PyroshineUi` it
follows pairing snapshots and notifies the operator; the daemon's own
notify-rust notification (coalesced to one per 30 s) is the fallback when no
tray runs. The bus releases the tray's name when it exits or crashes. Neither
notification carries a link that approves anything.

**Web frontend.** `web/` is React/Material UI built by Vite into static files
at release time (Node.js only then). A typed `Backend` contract
(`src/api/backend.ts`) is implemented over HTTP (`http.ts`: JSON requests with
the session cookie and CSRF token, the event stream). State
follows the event stream: every (re)connection starts with an overview that
replaces whatever was missed, so a lost connection, a server restart or a
reload never leaves stale state. Pages are hash routes (`#/clients?request=…`)
the tray and notifications can open. The build writes `pyroshine-web.json`
with its version; the server logs a mismatch and the page asks to reload.

**Desktop tray.** The server starts the tray (`src/tray/launcher.rs`) while
the user's desktop session runs, as a child process of its own executable
(`/proc/self/exe tray --attached`, so an upgrade never mixes versions). A
service starts before any login, so the session is read from the user's
systemd manager environment (the desktop exports `WAYLAND_DISPLAY`/`DISPLAY`
there), with the process environment as fallback; it counts while its display
socket answers and the session bus has a tray host or a notification server.
The launcher checks every 10 s: a logout or new login stops or replaces the
tray, a crash restarts it after 5 s doubling to 5 min, and a clean exit
(**Hide Tray Icon**) keeps it hidden until the next session or server start.
The child exits when its standard-input pipe from the server closes; the
service cgroup also ends it. Headless hosts never start it, and
`[management] tray = false` disables it.

The tray shows the server's state as a StatusNotifierItem (`ksni` on
async-io, matching the server's zbus executor) and owns no streaming
resources. Process isolation keeps a tray fault away from streaming. It follows the server's bus
name with the supervision of `attach.rs`: while the same owner stays, a failed
subscription or snapshot is retried with backoff from 0.5 s doubling to 30 s;
an incompatible or denied daemon is reported and waits for a new owner; a new
owner abandons the pending attempt, and each attempt publishes only while its
generation is current. Clicks open web pages through the portal's `OpenURI`
with the click's XDG activation token (`xdg-open` as an argv fallback); the
address comes from `GetWebUrl` and is limited to known routes, so no credential
is ever in it. End Session calls the server directly. One tray runs per session
(its bus name); it exits without an answering Wayland/X11 socket and when the
session bus closes. `pyroshine open [page]` (the application-menu entry) asks
the running tray to open a page, or opens it directly.

## Architectural invariants

Changes must preserve these properties unless a task explicitly redesigns them:

- One session at a time, with one teardown owner and a bounded teardown
  deadline. Absence of state never means idle.
- Pending stream contexts are never the active configuration before PLAY; epoch
  barriers keep old frames, packets, keys and input out of a new stream.
- Negotiation either produces the requested format or fails; no silent codec,
  chroma, depth, range or software fallback.
- Capture admission, GPU buffer consumption and descriptor ownership are
  independent signals; never merge them.
- Capture and encode use the same verified GPU; no cross-device path.
- FEC covers plaintext shards; encryption follows per shard.
- Hot paths (capture, conversion, encoding, packetization, pacing, input) avoid
  extra copies, blocking I/O, unbounded queues and per-frame logging. Existing
  GPU completion waits are ownership requirements.
- Native boundaries (Vulkan, DMA-BUF descriptors, PyroWave, Inputtino) keep
  narrow `unsafe` scopes, explicit ownership and child-before-parent cleanup.
- The management interface observes and calls the owning components; it never
  holds session, pairing or trust state of its own, and nothing on a media path
  waits for it (history writes included). The server streams unchanged without
  a browser, the tray, the D-Bus name or the web listener.
- Management access is authenticated per transport: the session bus by Unix
  user, the web interface by administrator session plus same-origin and CSRF
  checks. A loopback address or a paired client certificate never grants it.
- Catalog refreshes never reach a session: launches copy their entry, request
  handlers read one immutable snapshot, and the library worker takes no
  session locks. Application IDs, once offered, do not change.

## Where to go next

| Topic | Guide |
| --- | --- |
| Scene eligibility, cursor, late composition, Steam focus and input | [Compositor](COMPOSITOR.md) |
| Capture admission, encoder queues and transport accounting | [Capture pipeline](PIPELINE_OPTIMIZATION.md) |
| PyroWave dependency, negotiation, color, FEC, pacing and GPU matrix | [PyroWave](PYROWAVE.md) |
| PyroWave dialects and authenticated bandwidth calibration | [PyroWave compatibility](PYROWAVE_COMPATIBILITY.md) |
| UDP bandwidth calibration | [Bandwidth calibration](BANDWIDTH_CALIBRATION.md) |
| Native color descriptions and Wine/Proton setup | [Native presentation](NATIVE_PRESENTATION.md) |
| Native controller identity and report mapping | [DualSense Edge](DUALSENSE_EDGE.md) |
| Measurements, diagnostics and reconnect acceptance | [Benchmarking](BENCHMARKING.md), [Streaming diagnostics](LONG_SESSION_PERFORMANCE.md), [Reconnect validation](reconnect-validation.md) |

A successful build, loader probe or loopback benchmark does not prove
presentation, client decode, Steam behavior or physical-link performance.
Record tested hardware, clients and unperformed checks with new validation.
