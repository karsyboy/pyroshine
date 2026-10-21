# Capture backends

A capture backend produces the frames a session streams. The session layer
selects it, starts it, wires its endpoints into the streams, applies negotiated
output modes and stops it; everything specific to a source stays inside the
backend. Game Mode uses the session's own headless Smithay compositor.
[Managed Desktop Mode](DESKTOP_MODE.md) uses a virtual monitor on the existing
Wayland desktop. Both meet the contract below and share encoders and transport. Code paths are relative to `moonshine-core/src/session/`.

## Responsibilities

| Responsibility | Owner | Where |
| --- | --- | --- |
| Session lifecycle, transitions, teardown order, generations | Session manager, backend-independent | `manager.rs` (`SessionBackend`) |
| Selecting the backend | Session layer, when it initializes a session | `capture/mod.rs` `CaptureBackend`, `mod.rs` `InitializedSession::new` |
| Starting it, readiness, startup failure | Backend thread run through `start_capture_thread` | `capture/mod.rs` |
| Frame production and buffer reuse | Backend, through `CaptureSender` | `capture/admission.rs` |
| Frame contract | Shared | `capture/frame.rs` (`ExportedFrame`) |
| Capability report | Backend, once with readiness | `CaptureCapabilities` |
| Output mode and capture pacing | Backend, through `LiveCapture::reconfigure` | `OutputMode`, `CapturePacing` |
| Shutdown | Session stop; the backend's worker guard holds completion | `lifecycle.rs` |
| Failures after readiness | The backend thread ends, which stops the session | `start_capture_thread` |
| Import, conversion, encoding, transport | Video pipeline, unchanged per backend | `stream/video/` |

The session talks to a backend through two enums: `CaptureSource` before it
runs and `LiveCapture` once it is ready. Each has one variant per backend, so
adding a backend makes the compiler list every operation it must provide.
There is no trait object and no per-frame indirection: frames travel through
the same admission channel whichever backend produced them.

## Isolated Game Mode

`CaptureBackend::Isolated` runs `compositor/`. It owns, and the shared layer
never touches:

- the Wayland display, XWayland (spawned and terminated by the compositor) and
  the application surfaces;
- the virtual output, its mode, scale and HDR state;
- the scene, Steam classification and overlays, cursor and input focus;
- input injection into its seat;
- its buffer pools and the client buffers it holds until they are consumed.

Lifecycle:

1. `CaptureSource::new` creates the frame channel, the input channel and the
   reconfiguration channel. Nothing runs yet.
2. `CaptureSource::launch` starts the `compositor` thread through
   `start_capture_thread`. The thread opens the render node, builds the
   verified capture `VideoContext`, selects formats, starts XWayland and
   reports `CompositorReady` (application display and capabilities) once
   XWayland is ready, within 5 s.
3. The application is launched with `WAYLAND_DISPLAY` and `DISPLAY` from
   `LiveCapture::application_display`, and HDR application paths enabled when
   `CaptureCapabilities::hdr` is set.
4. PLAY and reconnects call `LiveCapture::reconfigure` with the negotiated
   `OutputMode`. An unchanged mode is not sent to the thread. The effective HDR
   state is returned; the session refuses a stream whose HDR state differs.
5. The session stop ends the event loop. The compositor closes its helpers,
   terminates XWayland, drops its state and returns; completion is released
   after the thread has fully exited.

## Start and ownership contract

`start_capture_thread` is the single way a backend runs. It provides, for
every backend:

- **Registration first.** The backend's `WorkerGuard` is registered before
  the thread exists. Session completion therefore means the thread exited and
  dropped everything it owns. A session that already completed refuses to
  start a backend.
- **GPU outlives the thread.** The `CaptureContextHold` keeps the capture
  GPU's Vulkan device alive until the thread has fully exited, thread-local
  destructors included. Drivers release a thread's EGL state there; on NVIDIA,
  destroying the device while they run deadlocks in the driver. The hold is
  dropped before completion is released.
- **Any exit stops the session.** A return, an error or a panic of the
  backend thread triggers the session stop with the backend's reason
  (`CompositorStopped` for the isolated backend).
- **Readiness or a reason.** The launch returns the readiness value, the
  error the thread returned, or why it ended; a backend that is still
  initializing when the session stops returns without reporting readiness. A
  readiness timeout fails the launch; the manager's teardown then stops and
  waits for the thread.

Startup failures reach the client as the launch's `SessionFailure`
(`compositor_start_failed` for the isolated backend) and the operator as the
session's stop cause.

## Frame contract

`capture/frame.rs` documents the frame contract property by property. In
short:

- Frames are DMA-BUFs on the capture GPU, in a format and modifier pair that
  `gpu::ImportableModifiers` lists for that GPU. They are full-range packed RGB
  (8-bit, 10-bit or FP16), uncropped, without damage information.
- Color encoding comes from the source's declaration (`FrameColorSpace` and
  HDR10 metadata), never from the buffer format.
- Producer writes have completed before a frame is sent. Consumers do not wait
  for the producer.
- Three signals stay independent: the `SourceLease` keeps descriptors and
  memory valid, `consumed` permits content reuse once no GPU work can read the
  buffer, and the capture credit bounds capture demand through transport.
- `source_time` is the content time (the RTP timestamp derives from it) and is
  never later than `created_at`.
- Late-composition layers are attached only when the consumer declared it can
  draw them (`OverlayCaps`).

A source that cannot meet a property composes or converts the frame itself, or
fails to start. The pipeline never receives a frame it cannot read correctly.

## Capabilities

`CaptureCapabilities` is reported once with readiness and logged as
`Capture ready`:

| Field | Meaning | Isolated backend |
| --- | --- | --- |
| `backend` | Which backend serves the session | `isolated` |
| `render_node` | Capture GPU; its verified context travels with the frame channel | The configured or detected render node |
| `sdr_format` | Format of frames the backend composes in SDR | First 8-bit RGB format EGL renders and Vulkan imports |
| `hdr_format` | Format of composed HDR frames, `None` without HDR | FP16 or 10-bit when `compositor.hdr` and the GPU allow it |
| `presentation_pacing` | Presentation-driven capture is offered to clients that request it | `compositor.vrr_capture = "auto"` |

DMA-BUF export is not a capability: it is required. Output dimensions and
refresh rate are the live `OutputMode`.

## Existing desktop and virtual monitor capture

Desktop Mode implements the virtual-monitor column through native KDE and GNOME
adapters. Its guard owns display/audio transactions and recovery; capture receives
PipeWire DMA-BUFs directly, with embedded cursor and EIS input. Ordinary physical
monitor sharing is not a Desktop Mode fallback. See [Desktop Mode](DESKTOP_MODE.md)
for capability requirements and acceptance checks.

| Aspect | Isolated Game Mode | Existing desktop | Virtual monitor on that desktop |
| --- | --- | --- | --- |
| Compositor | Owned by the session | The user's desktop compositor | The desktop compositor, with a monitor the backend creates and removes |
| Frame source | Client buffers or the compositor's own pool | The desktop's screen-capture DMA-BUFs | The same, for the virtual monitor |
| Producer completion | Commits latched after rendering; composition waits its fence | Whatever the capture API signals; the backend waits before sending | Same |
| Application display | The session's Wayland and XWayland sockets | The desktop session's environment | Same |
| Keyboard, pointer, touch | Injected into the compositor seat | Virtual input devices or an input-emulation protocol | Same, limited to the monitor's region |
| Output mode | Reconfigured to the negotiated mode | The desktop's mode: refuse or scale in the encoder | Created at the negotiated mode |
| HDR | The compositor's native color management | The desktop output's state | The virtual monitor's state |
| Capture pacing | Refresh timer, or presentation-driven flips | The capture source's frame delivery | Same |
| Cursor and overlays | Late composition or GLES composition | Embedded by the desktop, or cursor metadata | Same |
| Steam focus and overlays | The compositor's Steam model | Not controlled by Pyroshine | Not controlled by Pyroshine |
| Audio | The session's PulseAudio-compatible server | The desktop's audio output | Same |
| Controllers | Inputtino devices | Unchanged | Unchanged |

## Managed PipeWire completion and ownership

The capture loop owns a generation ledger for every announced buffer. A removed
announcement cannot be requeued by an old GPU completion, even when PipeWire
reuses its address. Removed published frames retain their DMA-BUF lease and
consumption hold until GPU reads finish. Pool census counts actual announcements,
separately from the requested minimum of three buffers (producer, pending and GPU
ownership).

`process` drains to the newest buffer and retains one pending frame, including
while media delivery is paused. A damage-driven producer may send no further
picture until the desktop changes, so renewed demand must be able to publish the
retained picture. Only publication requires admission; reconnect resets replace
an obsolete credit without discarding those pixels. Replacing pending pixels
transfers a current credit. Producer completion is registered with a shared epoll source; the capture
loop publishes only after nonblocking readiness succeeds. Error/hangup or an
unsignaled fence beyond 50 ms fails capture; elapsed time never implies readiness.
Startup validates completed pixels before committing takeover and discards those
validation pixels before streaming starts.

Explicit synchronization is offered only after timeline create/import, eventfd
wait and signal succeed on the verified capture node. Negotiation offers a
three-block layout requiring timeline metadata, followed by the one-plane
implicit layout. Two syncobj descriptors are distinct from the pixel plane. Each
dequeued release point has one owned action, including no-credit/replaced frames;
published frames signal only after final consumption. Removal retains that action
until the GPU is done. Unscheduled-release promises are cleared only after taking
ownership; no timer guesses at ownership of undelivered buffers.

A failed explicit candidate is closed before a new implicit-only candidate is
negotiated, before readiness or display commit. The implicit path snapshots write
fences at arrival; older kernels use DMA-BUF poll readiness rather than assuming
that queue order proves completion. Both paths remain GPU-only.

Request-driven producers are discovered by binding the exact PipeWire node and
reading `node.supports-request`, with a bounded, cancellation-aware probe. Unknown
or ordinary producers keep producer-driven scheduling. A capable producer uses a
lazy consumer driver: requests coalesce, only completed graph cycles permit a new
cycle, and delivered paints set the negotiated cadence. Empty cursor/frame-callback
cycles do not consume a paint interval. Admission suppresses triggers without
credit; a bounded heartbeat retries a lost cycle. State and timers belong to one
stream generation and stop with it. Probe reports include the scheduling mode and
request/trigger counts. This uses PipeWire 0.3.40 or newer; native Mutter scheduling
still requires qualification on a GNOME host.

Before committing a replacement, the loop drains the old consumer, disconnects
it and waits for a bounded PipeWire round trip. The guard can then retire the
old producer without leaving updates addressed to a removed client-node resource.

## Adding a backend

1. Add a variant to `CaptureBackend`, `CaptureSource` and `LiveCapture`, and
   decide where the session selects it. Keep the isolated backend's
   behavior unchanged.
2. Create the frame channel with `admission::capture_channel`. Open the source
   GPU's render node and publish `gpu::capture_context` for it with
   `CaptureSender::set_context` before readiness. Refuse a source on another
   GPU: there is no cross-device path.
3. Run the producer through `start_capture_thread` with a context hold taken
   from the sender. Check the session stop during initialization and return
   instead of reporting readiness. Add a `SessionShutdownReason` (and its
   `status.rs` description) if `CompositorStopped` does not describe it.
4. Produce through admission only: `try_acquire` before any capture work,
   `try_send` with the credit, and hold every source buffer until its
   `consumed` flag is set. Do not add a queue in front of the channel.
5. Send only importable formats, after producer completion, with the source's
   color declaration and content time. Attach overlays only within
   `overlay_caps()`, or compose them.
6. Report readiness with `CaptureCapabilities` and the application display.
   `LiveCapture::reconfigure` returns the effective HDR state, or `Err` for a
   mode the backend cannot apply; PLAY then fails and the session is torn down.
7. Provide an input endpoint or route keyboard and pointer input elsewhere.
8. Test with the mock-backend tests in `capture/tests.rs` as a pattern, add
   hardware tests for the real source, and compare `moonshine-bench` results
   with the isolated backend's baseline.

## Limitations and integration points

The seam covers capture; desktop output and audio administration live in the
separate guard. `CaptureInputEvent` is shared, backend details identify the mode
in management status, and desktop frames carry native completion ownership.
`moonshine-bench` still drives only the isolated backend. Use
`moonshine-desktop-probe` for managed capture diagnostics. Its `--status` mode
queries an already-running guard without starting it or changing outputs. Capture
probes temporarily take over displays and require the guard. Real client
acceptance remains necessary.

## Validation

Automated, without a GPU:

- `session::capture::tests`: readiness, initialization failure, panics,
  cancellation during initialization, readiness timeout, exit after readiness,
  refused start after completion, and a frame outliving its producer; each
  checks the session stop, completion after the thread's thread-local state is
  released, the GPU hold ending with the thread, and resources dropped once.
- `capture::admission::tests` and `capture::frame::tests`: admission credits,
  epochs and source-descriptor ownership.
- `compositor::reconfigure_tests`: only output-mode changes reach the live
  compositor.
- `manager::lifecycle_tests`: the backend-independent manager contract,
  including cancellation at every transition await, failed transitions,
  stale generations, reconnects and repeated cycles.

On GPU hosts, the `compositor::xwayland_tests` hardware tests, `moonshine-bench`
workloads (direct, late-composition, composited, HDR and VRR paths) and
`moonshine-bench --cycles` cover the isolated backend; see
[Benchmarking](BENCHMARKING.md) and [Testing](TESTING.md).
