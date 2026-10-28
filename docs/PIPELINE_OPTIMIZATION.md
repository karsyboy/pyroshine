# Capture and encode pipeline

This guide describes current admission, ownership and performance constraints.
For the system context see [ARCHITECTURE.md](ARCHITECTURE.md).

## Capture admission and presentation

`capture/admission.rs` provides receiver-driven demand, one owned credit and
one handoff slot. The receiver requests a capture only when useful; the compositor
claims the credit before direct export, fd duplication, scene-element construction
or GLES rendering. Without demand it retains dirty state and skips capture work.

| Consumer | When demand is useful | When admission ends |
| --- | --- | --- |
| PyroWave | Prior frame's encode, packetization and socket send have completed | Actual socket-send completion, or an error/drop |
| Conventional codec | Fewer than two useful frames outstanding, including network work | Final socket submission, transport failure, epoch discard or cancellation |

The source buffer's `consumed` flag is independent. It releases the DMA-BUF only
after GPU reads finish; PyroWave can release the client's buffer before paced
network sending completes. RAII releases admission on early returns/errors.
Timeouts revoke unclaimed demand, not a capture already rendering.

Epoch changes drain queued frames and reject racing old sends/receives. An old
credit's completion cannot replenish the new generation. Receiver disconnect is
terminal. The capacity-one handoff slot is not another admission credit.

The refresh timer is the only capture clock. Each absolute refresh deadline
offers at most one capture, taken before that slot's frame callbacks; a late
wakeup skips whole slots without rebasing the phase or bursting. Reconnects,
epoch resets and codec changes keep this grid; only a refresh-rate change alters
its interval. If admission has no credit at the tick, the slot is deferred.
Demand wakes calloop through a coalesced ping and may complete that deferred
slot only while no client has committed since the tick, i.e. with the scene as
of the deadline. Demand never opens a slot, so a consumer's completion time
cannot become a second sampling clock that captures content rendered in
response to the slot's own callbacks. The next tick discards a slot still
waiting. The refresh timer still dispatches Wayland, services input, releases
buffers and sends frame callbacks even while capture is blocked. Demand wakeups
do not increase callback cadence or resolve presentation feedback; skipped
presentation feedback is discarded, not reported as presented.

## Capture pacing and source timing

Fixed pacing above serves every client by default. It samples whatever was
latched at each refresh deadline, which quantizes an application that does not
run at exactly the stream rate onto the refresh grid: a 90 FPS game on a
120 FPS stream is delivered as a mix of 8.3 and 16.7 ms intervals, and a
vsync'd game that cannot sustain the refresh rate waits for whole ticks.

A client presenting on a VRR display paces playback itself from RTP timestamps
and asks for presentation-driven capture with `clientVrrRequested=1` on
`/launch` or `/resume`. The request belongs to that authorization generation:
a later client that does not send it (any standard Moonlight) gets fixed
pacing, and a pacing change alone reconfigures only the compositor, never the
video pipeline. `[compositor] vrr_capture = "off"` disables it.

With VRR pacing a *flip* is one capture opportunity followed by frame
callbacks and presentation feedback, like a variable-refresh panel whose
maximum refresh is the stream rate:

- A commit that latches a new (or removed) buffer anywhere in a surface tree
  requests a flip, run from a calloop idle callback after the current dispatch.
  Frame-callback-only and subsurface-state commits do not: they would capture
  the old scene and spend the rate on it.
- A credit limiter (`capture::PresentationLimiter`) keeps flips at or below
  the negotiated rate on average. A long interval earns at most one interval of
  credit for a following short one, so uneven but correctly averaged sources
  keep their timing; oversupply is coalesced at the rate; a gap of two
  intervals is a stall that cannot start a catch-up burst. A postponed flip
  fires from a one-shot timer.
- Without a consumer credit a flip still sends its frame callbacks; demand
  then captures the newest scene. Admission credits, the pre-encode drop and
  in-flight bounds are unchanged, so a fast game never raises encode or send
  rates beyond the stream rate.
- The refresh timer becomes an idle heartbeat: it flips only after a full
  interval without one (cursor movement, keepalive, idle callbacks) and never
  consumes limiter credit.

Every exported frame carries its content time (`ExportedFrame::source_time`):
the sampled refresh deadline under fixed pacing; under VRR pacing the latch
time of the newest captured buffer, or the flip time when the limiter held the
flip (the content is then sampled at the flip). The video stream derives the
90 kHz RTP timestamp from it relative to the client epoch (`video::rtp_clock`),
as Sunshine and GameStream do. Timestamps are strictly increasing, never zero,
wrap at 32 bits, and restart near zero when a reconnecting client starts a new
epoch; replays of the last frame (static-scene IDR, PyroWave resend) are stamped
when they are sent. Moonlight clients use RTP only as frame PTS; loss detection
uses sequence and frame numbers. Earlier releases synthesized RTP from the
frame count and negotiated FPS, which made RTP time run faster than real time
whenever captures were skipped.

With `log_stats`, the five-second `Video capture cadence` summary reports
capture spacing (min/p50/p95/p99/max), lateness against the sampled deadline,
refresh versus deferred-slot captures, deferred/superseded/expired slots and how
many client surface commits each capture covered. Average FPS cannot reveal
uneven sampling: steady 120 FPS with captures covering 0 or 2+ commits means
repeated or never-captured application frames. Cursor and overlay surfaces also
commit, so these counts bound rather than equal game-frame repeats/skips; the
`game_*` fields count the actual focused application surface and report when its
commits land after the refresh deadline. A game paced by frame callbacks covers
one commit per capture and commits shortly after each tick.
`same_slot_captures` must stay zero. Under VRR pacing `capture_pacing="vrr"`,
captures are counted as `presentation_captures`, `heartbeat_captures` and
`pending_captures`; lateness is measured from the flip request,
`content_age_*` from the content's latch time to its capture, `limited_flips`
counts flips the rate limiter postponed and `pending_flips` flips that waited
for a consumer credit.

Completed scanout holds must be released and resulting Wayland events flushed
before static-screen skipping. A clean screen can mean the game is waiting for
its swapchain buffers, not that client lifecycle work is unnecessary.

## Direct export and composition

Direct export requires complete-scene eligibility from
[COMPOSITOR.md](COMPOSITOR.md#scene-and-input-decisions). Visible cursors, overlays,
scaling and other scene content can require GLES composition. Rejection diagnostics
record the first blocking condition with fixed counters, avoiding per-frame strings.
Each exported frame also carries that condition and its path in a copyable
`CaptureOrigin` through `FrameStats`, which management telemetry summarizes as the
dashboard's capture path.
Direct export also needs a single-plane RGB buffer the encoders sample (8-bit,
10-bit or FP16, either channel order) with a modifier their Vulkan device imports
([capture buffers](COMPOSITOR.md#ownership-and-frame-handoff)); anything else,
such as a video player's NV12 surface, is composited (`direct_reject_format`).

Composition submits GLES, waits for its SyncPoint, then exports the image to the
Vulkan consumer. PyroWave packet pacing starts before compositor preparation and
rendering; the GLES fence wait and encoding consume the same frame budget rather
than extending it. Completion-based pipeline latency is measured separately.
Failed rendering/completion stops capture rather than publishing or recycling a
buffer with uncertain GPU ownership.

A visible cursor or Steam notification does not by itself force composition;
see [late composition](COMPOSITOR.md#late-composition-cursor-and-steam-notifications).
Commits are latched only once their DMA-BUF finished rendering
([buffer readiness](COMPOSITOR.md#buffer-readiness)), so direct export never
hands the encoder a frame still queued behind a game's GPU work.

Do not relax direct eligibility just because PyroWave has a scaler: compositor
transforms include aspect fitting, offset, borders and sampling filters. Direct
scaling needs an explicit transform contract and pixel comparisons.

## Encoding, caches and queue selection

Conventional conversion uses `pipeline/convert.rs`, the only RGB-to-YCbCr
path of H.264, HEVC and AV1: one compute dispatch writes NV12, P010, 4:4:4 or
4:4:4 10-bit planes into a buffer as whole 32-bit words (4x2 pixels per
invocation, no atomics or buffer clear), optionally compositing the cursor and
a notification, then one buffer-to-image copy fills the encoder input slot.

4:2:0 chroma is generated where the encoder signals it. H.264 and HEVC use
the 2x2 mean, centered in the block, and signal `chroma_sample_loc_type` 1.
AV1 has no code for centered chroma, so it uses left siting (a [1 2 1]/4 filter
across the even column, averaged over both rows; one extra source column per
4x2 block) and signals `CSP_VERTICAL`. Decoders that ignore the signaling
assume left siting, which AV1 now matches. The creation-time self-check
verifies both positions on every subsampled format.

The converter keeps three extents apart. The *frame* is the negotiated stream
size. The *conversion extent* rounds it up to the codec's block size
(macroblock, CTB or superblock); it always fits Pixelforge's input image, whose
coded extent is aligned to that block and the device granularity, and its extra
pixels replicate the nearest frame edge, so alignment padding is deterministic
and cheap to encode while the codec's cropping metadata hides it. The *buffer
extent* rounds up to whole 4x2 blocks: every plane row starts on a word and is
a whole number of words, and the copy regions address the conversion extent
with `bufferRowLength`, so any extent works without partial-word writes and the
samples past it are never copied. H.264 and HEVC cannot crop 4:2:0 to an odd
extent; stream validation rejects those (4:4:4 and AV1 accept them).

The vendored Pixelforge patches also round the input and DPB image widths up
to 64 texels within the GPU's maximum coded width. This keeps NVIDIA's 8-bit
4:4:4 chroma pitch consistent at widths such as 1360, 1440 and 2400: input
chroma is read, and reconstructed chroma written, inside the image. The
picture's coded extent stays aligned to codec blocks and device granularity;
cropping/render metadata retains the negotiated frame size. Additional image
columns are outside the picture. AV1 frames are coded on the implementation's
picture grid (`codedPictureAlignment`, 8x2 on RADV) with the negotiated size as
render size, so the encoder reads the converter's edge padding instead of
filling the grid remainder itself. See
[local dependency maintenance](../vendor/pixelforge/LOCAL_CHANGES.md).

In CBR mode some encoders pad every frame to the per-frame bit budget with
filler data (H.264 NAL type 12, HEVC `FD_NUT`, AV1 `OBU_PADDING`); RADV does so
for all three codecs. With the one-frame virtual buffer that would send every
frame at the negotiated bitrate. The packet consumer removes these units
(`pipeline/padding.rs`) before HDR metadata injection and packetization, so the
transmitted size follows the encoded content; decoders discard padding, and
the remaining bytes are unchanged.

Pixelforge owns profile construction and capability/extent calculations. Setup
queries the exact encoder configuration, including usage hints, then checks
Pyroshine's AV1 minimum-frame policy before allocation. Conversion bounds use
`Encoder::input_image_extent()` from the created encoder, separately from its
`coded_extent()`; GPU fixtures compare both with the queried capabilities.
Encoder driver errors preserve their `vk::Result` and operation context.
`ERROR_DEVICE_LOST` stops the session; rendered error text does not decide
recovery. The existing source-release and IDR recovery rules still apply.

The shader writes each block sample by sample without private arrays or
loops. Every converter first converts a small edge-padded pattern on its queue
and compares it with the CPU reference in `convert::reference` (standard
BT.709/BT.2020/PQ equations, round-to-nearest quantization); a mismatch fails
the stream setup with an error naming the plane and block column, rather than
streaming corrupted color. Edit `pipeline/shaders/convert.comp` and regenerate
the embedded SPIR-V with `scripts/build-shaders.sh`.

The conversion submits to the dedicated compute family by default
(`conversion_queue`), which measured lower game-FPS impact than the graphics
queue alongside a GPU-bound game; `graphics` remains available for A/B
comparison. Pixelforge shares the input image concurrently with that family.
The conversion ends with a CPU fence wait before `Encoder::encode`, whose API
takes no wait semaphore, and releases the source only after that wait. The wait
is bounded at 5 s: a GPU that stops without reporting device loss stops the
stream like device loss instead of hanging the encode thread, and the converter
refuses further work because its submission is still pending. Chaining the
conversion to the encode with a semaphore would need a Pixelforge API change
and separate tracking of the source's release, for at most the fence round trip:
about 50 µs at 4K (`converter_timing`: 98 µs GPU, 151 µs wall).

Conventional encoding uses Pixelforge's asynchronous pipeline with two owned
completion credits spanning submission, readback, packetization, queue residence
and socket submission. The capture thread issues demand only below that bound;
IDR replays use the same gate. The consumer moves each credit into its batch
rather than freeing it at enqueue. RAII also covers queued encoder messages,
packetization errors, sender failure, cancellation and discarded epochs.

VA-API and NVENC keep one input slot (input surface plus coded buffer) per
credit. A slot is held from conversion until its bitstream is read back, inside
one credit, and slots are used in order, so the slot a newly admitted frame
converts into is always free; a further slot was never used.

Two credits let one frame encode while the previous one is read back,
packetized and sent. A third credit cannot add throughput once the encoder or
the link is the bottleneck: its capture waits for one of Pixelforge's two input
slots (inside the encoder's `input_image`), behind the previous encode, or
behind the previous frame's socket send, and ages there. Releasing a credit
wakes the capture thread (`shard_batch::CreditWake`), so demand is raised as
soon as the oldest frame completes rather than at the end of a frame-interval
wait; the capture then lands on the next refresh tick (or, under VRR pacing,
at once). Without that wakeup two credits lose about a quarter of the
saturated frame rate, and with it three credits keep three frames queued in
front of a saturated link. Do not raise the bound to recover throughput without
re-measuring frame age under encoder and link saturation.

`MOONSHINE_CONVENTIONAL_FRAMES_IN_FLIGHT` (1–3) overrides the bound for A/B
measurement on other hardware; it is a diagnostic, not a setting. Benchmark
summaries report frame age, the encoder-slot wait and how many frames were
outstanding at each submission (see [benchmarking](BENCHMARKING.md#output)).

A requested IDR on a static scene (no new capture within a frame interval) is
replayed from the converter, not from the encoder's input image: Pixelforge
rotates two input slots and advances on every encode, so the current slot
holds an older frame or none. The converter's output buffer still holds the
last completed conversion and is copied into the current slot before it is
encoded.
The IDR stays pending across loop iterations until a recovery or a new frame is
actually submitted, including while network credits are exhausted.

Network output storage is bounded by the credits: two frames, or at most three
with the diagnostic override. For a negotiated wire shard
size `S`, the protocol's maximum four unprotected blocks of 1023 data shards
bound it by `N * 4092 * S` bytes for `N` credits; protected blocks have at most 255 total shards.
This is a representability bound, not a desired queue size. TRACE completion
records expose current/high-water output frames and bytes. An indefinitely
blocked socket can make those frames indefinitely old; stop and pause interrupt
socket waits, and pause is acknowledged only after old delivery is disabled.
No arbitrary already-encoded predictive frame is dropped to meet an age target.
Transport loss requests an IDR; epoch discard is followed by the existing reset
and IDR activation contract.

The source DMA-BUF's GPU-read completion remains independent of these network
credits. Do not use kernel submission as source-read completion or proof of
receiver delivery.
Rejecting capture before encoding preserves predictive reference
chains; discarding encoded reference frames can break decoding until an IDR.
PyroWave completes one encode and send before requesting the next capture.
This bounds latency but can limit FPS when capture + encode + paced send consumes
a refresh interval; enlarging queues is not a free throughput improvement. A
frame whose encode outlasted its bitrate-derived send window is therefore paced
to finish before the next capture slot (at up to four times the bitrate) rather
than over a fresh full window.
PyroWave uses a lazily allocated, reusable CLOCK_MONOTONIC timerfd through Tokio
AsyncFd for precise packet deadlines. A mutable send borrow enforces one wait
per timer; cancellation and rearming cannot leak a prior frame's expiry. There
is no spin loop or pacing thread. Timer initialization/wait failure falls back
to Tokio's ordinary timer without failing startup. Unpaced conventional sends
do not allocate or use this timer. GSO and per-datagram fallback share the same
readiness retry and observed `WouldBlock` counters. An observed readiness wait
or a chunk overrun moves the remaining pacing schedule forward; no overdue later
chunks are released together. The maximum within-chunk burst is the
payload-capped GSO cadence (also used without GSO).

Both importers retain DMA-BUF open-file identity and full layout validation.
A recycled numeric fd or reused compositor index is insufficient identity.
Unused PyroWave imports expire; conventional consumers can separately pin imports
while GPU work remains active. See [resource bounds](LONG_SESSION_PERFORMANCE.md#resource-lifetimes).

The conventional pipeline keeps no client buffer imported between frames: after
each conversion's fence it releases the source, its overlay layers and their
converter views, and imports the next frame again (about 15 µs). A buffer that
stays imported makes the device's later submissions, the video encode among
them, wait under implicit synchronization for whatever the game is rendering
into it. With a GPU-bound 4K game on an RX 9070 XT, keeping the game's
swapchain imported held every HEVC 4K60 frame for the game's next frame:
frame age 14 ms, against 5.8 ms importing per frame. The price is mapping each
import into the GPU's address space at submission: about 0.3 ms per frame when
the GPU has headroom (HEVC 4K60 4.5 → 4.8 ms, H.264 1080p60 2.0 → 2.5 ms); the
conversion's own GPU time is unchanged.
PyroWave runs on the graphics queue, where its frames queue behind the game's
work whether or not imports stay alive, so it keeps its cache.

`[stream.video] pyrowave_queue = "auto"` prefers graphics, even when a
compute-only family is available. Explicit `graphics`/`compute` modes support A/B
measurement. Optional selection failure retains a graphics fallback. The logged
value is a preference, not proof of the native library's final queue choice.
Normal queue priority and external-memory synchronization remain in place.
Measure real-game contention before drawing conclusions from cube benchmarks.

Capture follows client demand. A pause (client detached or reconnecting)
clears the stream's media demand until the next epoch is activated: the encoder
then requests no captures and replays nothing, so capture, conversion, encoding
and packetization stop once in-flight work drains, the encoders release their
imports of the client's buffers (which otherwise expire only on later imports,
pinning a swapchain the game replaced while nobody watched), and the audio encoder
returns PCM to the Pulse server without encoding it. The compositor keeps
presenting to the application and Pulse keeps serving it. The encoding thread
sleeps meanwhile (and before `StartB`): `MediaDemand` wakes it when demand
changes and when a reconfiguration, reset, start or stop is queued for it, so a
resumed epoch activates at once instead of at the thread's next poll. After demand
returns, a replay of the last frame (static-scene IDR, PyroWave resend) waits
two frame intervals so a fresh capture can arrive first.

Conventional packetization of frames of 128 KiB or more runs inside
`tokio::task::block_in_place`, so its FEC and encryption work (about 1 ms for
a 1 MB encrypted 4K frame) does not delay other tasks queued on that runtime
worker. Frames are still packetized one at a time in order.

Reed–Solomon parity of a large block (every protected 4K PyroWave block) is
split over four `video-fec-*` threads of a dedicated rayon pool. Rayon's global
pool would use one thread per CPU and wake all of them for each block: on a
16-thread CPU at PyroWave 4:4:4 4K120 that was ~19,000 wakeups/s and twice the
process's CPU time for the same 120 µs packetization. Fewer than four threads
lengthen packetization (two: 160 µs, one: 254 µs).

## Live conventional bitrate and reference recovery

Fixed negotiated bitrate remains the default. Conventional encoders accept an
internal generation-scoped request with requested/accepted bits per second,
staged/applied outcome and first affected encoder display order. The bounded
mailbox coalesces commands; the empty encoding path reads one atomic. Status is
applied only after a successful picture submission, and a failed installation
reports failure and restores the previously accepted rate. Reconnect clears stale
commands and restores the negotiated rate before its new IDR. Display order is
queried from each encoder, since a failed Vulkan attempt can consume an order.

Accepted rates stay within the startup budget (and the Vulkan profile's
`maxBitrate`). This keeps bitstream storage, sequence level and receiver capacity
valid while allowing decreases and increases back to the original ceiling.
Vulkan changes rate control at a video coding boundary without RESET; VA-API
sends RC/HRD parameters on the following picture with reset clear; NVENC submits
a candidate bitrate/VBV configuration with resetEncoder and forceIDR clear and
commits it only after SDK acceptance. Changes neither rebuild the encoder nor
request an IDR. Existing color/epoch changes retain their own IDR requirements.
Conventional UDP transmission has no bitrate pacer to retarget; PyroWave's paced
budget is unaffected and rejects this conventional-only control. This is exposed
through the existing benchmark API and `--rate-change`, with no ABR, operator
setting or new client protocol.

VA-API H.264/HEVC retain up to four reconstructed short-term references, one
active prediction reference and no reorder delay. Storage has an additional free
reconstruction; codec headers and VA picture lists describe the retained DPB.
H.264 uses explicit PicNum selection for an older parent; HEVC carries the retained
negative-POC set and marks only the selected parent as used. Loss taints every
submitted descendant, including queued pictures; recovery chooses the newest
trusted retained parent strictly before that loss. Further loss can taint a
recovery picture. IDR clears trust; unsupported/unknown active-reference capability
keeps the one-reference fallback. AV1 retains its existing keyframe recovery.

NVENC invalidates through the submitted head and records only a successful native
interval. Covered feedback coalesces native calls but still requests a recovery
picture, including on a static desktop. An explicit preset DPB depth bounds safe
anchors; a driver-managed zero depth keeps SDK anchor selection without treating
the 16-call cost bound as retention. Invalid/future requests, exhausted known
anchors or SDK failure request IDR. Neither the default preset nor its reference
depth changes. Loss at the epoch IDR, and wrapped/ambiguous client feedback,
also fall back to IDR.

The moving-picture loss fixtures drop dependent pictures and the first recovery
picture, then compare surviving decoded output with its own source. Set
`MOONSHINE_TEST_RFI_FRAMES=270` at 1280x720 to exercise picture-number/POC wrap.
Continuous codec fixtures also lower, restore and clamp live rates and check that
rate changes add no keyframes. Real Moonlight/Pyrolight packet-loss, long-run VBV
settling and GNOME capture remain separate hardware/client acceptance checks.

## Transport ownership and outcomes

Packetization plans every FEC block first and writes into disjoint views of one
zeroed contiguous frame allocation. Prefix bytes remain outside FEC; encryption
runs in place after parity and metadata. Sending borrows that owned allocation
through completion. Capacity pooling is deferred pending a measured benefit.

`TransportCompletion` distinguishes successful kernel submission, failure,
epoch discard and cancellation, separately from releasing a useful-work credit.
Logical attempted datagrams/UDP payload bytes count once even if readiness
retries or GSO fallback repeat the socket operation. `would_block_events` and
`fallback_chunks` count those additional operations. Submitted counters increase
only after successful complete socket submission. A cancelled partial frame
retains its successful prefix, with its unsent remainder reported as discarded.
Completion notifications never block resource release. Per-datagram failures
are aggregated into batch counters and warnings are limited to once per second.

Benchmark `FrameStats.enqueue` covers queue handoff/residence and `send` covers
socket work; `total` ends at transport completion for every codec.
`wire_bytes` is successfully submitted UDP payload (including FEC and encryption),
not IP/link traffic or client delivery. The sender summary additionally estimates
Ethernet load with IPv6/UDP and framing overhead. Pipeline enqueue summaries
measure earlier handoff separately and do not report transmission throughput.

## Measurement and changes needing more proof

Composited capture waits for its render fence on the compositor thread. On an
RX 9070 XT at 4K60 the wait averaged 0.15–0.5 ms per capture alone and about
10.7 ms (maximum about 14.5 ms) while a GPU-bound 4K game ran, so input and
presentation dispatch can be delayed by most of a frame under contention.
Direct export avoids it. A completion handoff (publishing the frame when an
exported render fence signals, from the event loop) would remove that stall
but must keep the pool slot owned until the fence signals and handle device
loss and teardown; it is not implemented. Measure with
`render_fence_wait_*` in the capture summary.

[Benchmarking](BENCHMARKING.md) defines latency/GPU/throughput denominators;
[runtime diagnostics](LONG_SESSION_PERFORMANCE.md) explains stall signatures.
Server pipeline latency excludes game rendering, compositor CPU fence waits,
client decode and display. GPU utilization alone does not measure codec cost.

Deferred work needs specific evidence before implementation:

| Candidate | Required evidence/contract |
| --- | --- |
| Explicit EGL-to-Vulkan fence transport | Driver support, fd ownership on import failure, semaphore/buffer lifetime, reconfiguration and both encoder paths |
| Direct scaling | Matching crop, origin, borders, sampling and color output |
| RGB conversion/DWT fusion | Equivalent rounding/dithering/HDR output and lower GPU cost across representative devices; a previously measured candidate failed quality/performance checks |
| PyroWave import-cache aliasing | Measured duplicate-fd churn plus preserved identity/layout checks |
| Capture/send overlap | Tightly bounded work without stale scenes, extra admission credits or epoch leakage |

Converter changes need the GPU fixtures on every available vendor (select a
device with `VK_DRIVER_FILES`), plus a decoded stream from a real client:

```sh
MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core converter_matches_reference -- --ignored --nocapture
MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core encode_decode_matches_reference -- --ignored --nocapture
MOONSHINE_TEST_GPU=1 cargo test -p moonshine-core static_recovery_decodes -- --ignored --nocapture
MOONSHINE_TEST_GPU=1 cargo test --release -p moonshine-core converter_timing -- --ignored --nocapture
```

The first compares the dimension matrix (1x1 to 8K, odd and unaligned
extents), every source format, color conversion and range, edge padding, late
layers at the frame edges and the copy into planar images with the CPU
reference; the second encodes, removes rate-control padding as production
does, decodes with `ffmpeg` and checks decoded size, mean error, chroma bias
and the largest per-sample error per codec, chroma format and bit depth. The
per-sample bound catches localized corruption and unencoded padding rows,
which averages hide; the mean error applies to planes of at least one 8x8
transform block (a 2x2 4:2:0 frame has a single chroma sample). On a host that
only runs the packaged daemon, the converter self-check covers the same store
and layout path at startup (the **Color conversion** health check, which also
withdraws the affected profiles) and at stream start: a failure is logged as
`converter self-check failed on this GPU` with the differing plane and block
column.

The encode/decode matrix includes the NVIDIA 8-bit 4:4:4 width regressions at
1360, 1440 and 2400. Run it on NVIDIA hardware when updating the Pixelforge
patches, as one process with the default extents: H.264 4:4:4 at 1440 after
1360 is the sequence whose DPB overrun lost the device. The streaming variant
uses full-range color, low-latency CBR and twelve frames instead of the CQP
quality fixture, so reference corruption that grows over P frames is visible:

```sh
MOONSHINE_TEST_GPU=1 MOONSHINE_TEST_E2E_SAVE_DIR=/tmp/streams \
  cargo test --release -p moonshine-core stream_encode_decode_matches_reference -- --ignored --nocapture
```

`MOONSHINE_TEST_E2E_FRAMES=2880x1920` selects extents and
`MOONSHINE_TEST_E2E_FILTER='H265 Yuv444 Eight'` selects a profile. Saved elementary
streams can be copied to the intended client to test its hardware decoder.
For Intel VAAPI 8-bit 4:4:4, require hardware frames throughout:

```sh
ffmpeg -hwaccel vaapi -hwaccel_device /dev/dri/renderD128 \
  -hwaccel_output_format vaapi -i /tmp/streams/H265-Yuv444-Eight-2880x1920.hevc \
  -vf hwdownload,format=vuyx -f null -
```

Use `format=xv30` for 10-bit 4:4:4 and `format=nv12` for 8-bit 4:2:0.
An FFmpeg exit code alone does not establish hardware decoding: without this
filter it can silently fall back to software after rejecting a profile. HEVC
4:4:4 needs the VPS/SPS Range Extensions constraints that Vulkan StdVideo cannot
express; the vendored patch fills them once before caching the header. Test both
bit depths, plus 4:2:0 and the client reconnect matrix.

To check the actual EGL/GBM capture requests against both encoders:

```sh
MOONSHINE_TEST_GPU=1 MOONSHINE_TEST_PYROWAVE=1 \
  MOONSHINE_PYROWAVE_LIBRARY=/path/to/libpyrowave-shared.so.1 \
  cargo test -p moonshine-core pyrowave_imports_compositor -- --ignored --nocapture
scripts/vendor-pixelforge.sh --check
scripts/vendor-pixelforge.sh --test
```

Use [GPU validation](PYROWAVE.md#validation-matrix), compositor/Steam acceptance
and [reconnect checks](reconnect-validation.md) after pipeline changes. Loopback
runs cannot establish physical network capacity or end-to-end client behavior.
