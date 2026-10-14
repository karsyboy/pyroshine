# Pyroshine changelog

Notable changes in the Pyroshine fork, newest first. Release dates use YYYY-MM-DD.
Earlier Moonshine releases and their original contributor attribution are kept
in the [upstream changelog](UPSTREAM_CHANGELOG.md).

Fork entries below were reconstructed from local tags and their commits. Early
tags did not always match the embedded Cargo version or follow commit order;
those discrepancies are recorded rather than rewriting release history.

Add upcoming changes under **Unreleased**. The release workflow requires a dated,
nonempty entry matching both the tag and workspace version and publishes that
entry as the GitHub release notes. See [release preparation](../CONTRIBUTING.md#publishing-a-release).

## [Unreleased]

## [v0.17.6] - 2026-10-14

### Changed

- Build PyroWave from fork revision `b09e136`, which includes the fragment-path decoder fix merged from upstream and establishes GPU completion on encode failures (see Fixed). The bitstream and C API 1.1.0 are unchanged.
- Lower H.264, HEVC and AV1 latency when the encoder or the network is the bottleneck. The conventional pipeline now keeps at most two frames between encode submission and socket send instead of three, and starts the next capture as soon as a frame completes instead of up to one frame interval later. On an RTX 3080 streaming incompressible 4K content at 240 FPS, median frame age fell from 12.4 to 8.0 ms (99th percentile 14.6 to 9.8 ms) at 2.6% fewer frames; behind a link slower than the stream bitrate, median frame age fell by 40–43% on an RTX 3080 and an RX 9070 XT at an unchanged frame rate. Unsaturated streams and game frame rates are unchanged.
- Redact **Copy diagnostics**. The home directory, user and host names, the server name, client names and IDs, certificate fingerprints, UUIDs and IP addresses are replaced with placeholders, as in support bundles.
- Collect stream statistics while a client streams even when the desktop app is closed, so a support bundle collected afterwards includes them. Summarizing costs one wakeup every 250 ms and one GPU reading per second on a blocking thread; the media path is unchanged.

### Added

- Add `pyroshine diag`, which writes a redacted support bundle (`.tar.gz`) for bug reports on hosts without the desktop app: the diagnostic report, the last two minutes of stream statistics, host and GPU details, the configuration, the service status and log, and `vulkaninfo --summary`. Without a reachable server it collects what it can itself and runs the health check when no server is running. The desktop app saves the same bundle with **Diagnostics → Save support bundle**.
- Show the capture GPU while streaming. The dashboard's **GPU** section reports how busy the GPU and its video encoder are, clocks, temperature, power, video memory, and the GPU time the game and Pyroshine each used, from the kernel's sensors and per-process DRM usage. On NVIDIA it reads the driver's NVML library, which names each side's processes and video memory but samples a process's busy time only while it is busy. **Diagnosis** uses them: a slow encoder names a saturated encode engine or a GPU kept busy by the game, and a fully busy GPU that costs frames is reported with each side's share.

### Fixed

- Never release a captured buffer that PyroWave may still read. A PyroWave frame that failed after its scaling pass was queued (for example when a buffer could not be allocated) left that pass to run with the next frame, after the compositor had reused the buffer; on an RX 9070 XT a test reproduction lost the GPU. A failed wait for the GPU was also reported as success. Such failures now wait for the queued work, and a wait that fails ends the stream without releasing the buffer.
- Route game and application audio to the right speakers. Surround streams whose channel order differs from the sink's (ALSA's 5.1 order puts the rear speakers before center and LFE) and streams with fewer channels (quadraphonic rears played on center and LFE) were mapped by channel index; they now follow the stream's speaker positions, folding missing speakers into their neighbours. Stereo output keeps its ITU-R BS.775 downmix.
- Finish audio drains. An application that drained a stream holding less than one 5 ms tick of audio, or nothing, waited forever (players that drain before closing hung on exit), and a completed drain deleted the stream, so further writes failed. Drains now play the remaining audio, are answered once, and keep the stream; deleting a stream with a drain pending answers the drain with an error, as PulseAudio does. An underrun is reported to the client once instead of every tick.
- Keep a session running when systemd cannot be asked about its application. A failed or timed-out query of the application's state was taken as the application having exited, which ended the session (or rejected the launch) while the game was still running. Queries are retried briefly, and the session follows the application's signals until systemd answers.
- Report a failed application stop as a failure. When the application's termination could not be established during teardown, the last stop was recorded as completed and no problem event was published; it is now an error saying the application could not be confirmed stopped, distinct from a teardown that exceeded its deadline.
- Make stream statistics windows one second long. About half of them lasted 1.25 s because a window ended only at the first 250 ms tick after one second; rates were correct, but the dashboard updated unevenly.

## [v0.17.5] - 2026-10-13

### Added

- Explain why a launch fails. Moonlight and Pyrolight show the cause in their error dialog instead of "check Moonshine logs": a missing command, a failed hook, a UDP port held by another program, a compositor that cannot start, or an application that exits during launch with its exit status or signal, each with what to change. An application that exits cleanly right away is explained as a hand-off to an already-running instance.
- Name what ended a session. The dashboard shows the application's exit status or crash signal and how long after launch it ended, or the encoder failure (GPU device loss, an encoder that cannot be created for the stream) that stopped the stream. A crash or a failing exit within 30 seconds of launch is reported as unexpected.
- List recent problems in the desktop app. **Diagnostics → Recent problems** keeps the last 50 rejected launches, refused stream requests (for example a codec, 4:4:4 or HDR combination the GPU did not pass, with the profiles that work) and unexpected stops, with their causes and fixes. Clients see refused stream requests only as an RTSP status code.
- Diagnose streams on the host. While streaming, the dashboard's **Diagnosis** names what limits the stream (a slow encoder, a game rendering below the stream rate, skipped frames, socket failures, packets the network could not carry, high pipeline latency, composited frames or uneven frame times), with the numbers and what to change. **Settings → Diagnostics → Informational findings** hides the informational ones and keeps warnings.
- Show the capture path. The dashboard reports whether frames are exported directly from the game, directly with the cursor or a notification drawn by the encoder, or composited, and the main reason for composition (for example scaling, the Steam overlay or a CPU-rendered application), plus the encoder backend and GPU.
- Check color conversion at startup. The health check runs the stream converter's known-answer test for each output format a verified profile needs, so a GPU or driver that converts with wrong colors (as behind the green video in [#11](https://github.com/karsyboy/pyroshine/issues/11)) is reported before any client connects, and the affected profiles are no longer offered to clients. A stream stopped by the same test now says the conversion produced wrong colors and asks for a bug report, instead of suggesting a retry.
- Re-run the host checks from the desktop app without restarting Pyroshine. Each failed check shows its fix. While a session streams the GPU probes are not repeated, and the server's own ports are not treated as conflicts. **Copy diagnostics** copies the checks, session, statistics and recent problems as one JSON report.

### Changed

- Update the server's Rust dependencies to their current releases, including the new major versions of aes-gcm, sdp-types, libloading, mdns-sd, opus and dirs. Streaming, pairing and discovery behavior is unchanged.
- Name the `pyroshine` group (NixOS: `moonshine`) in the healthcheck's sleep-inhibit hint instead of the upstream `moonshine` group.
- Propose vendored dependency updates automatically. A daily workflow regenerates Inputtino or Pixelforge on a new upstream revision with the existing patch series and opens a pull request for review, or an issue when a patch no longer applies. Dependabot proposes Cargo, npm and GitHub Actions updates weekly.

### Fixed

- Keep Steam Big Picture's selection highlight after returning from a game. The selected Play button (green), game settings and Steam Input icons could show no highlight, often after exiting a game from the Steam overlay, while controller navigation still worked. The compositor moved keyboard focus from Steam's browser window back to its outer window, and left the overlay below the game for X pointer purposes, so the browser stopped counting as active. Focus inside Big Picture is now kept, and the window that takes input is raised for X as well, as in gamescope.
- Export most applications' frames directly on NVIDIA instead of compositing every frame. Vulkan and X11 applications (including Proton and DXVK games) chose a compressed NVIDIA buffer layout that the encoder cannot read, so only native Wayland OpenGL was exported directly. The compositor now asks applications to prefer the layouts the encoder reads. On an RTX 3080 at 720p this removed about 1.4 ms of capture delay and 0.25 ms of GPU time per frame; AMD (RX 9070 XT) already exported these applications directly.

## [v0.17.4] - 2026-10-12

### Changed

- Use the configured capture GPU for Vulkan encoding on multi-GPU systems without requiring Vulkan adapter reordering. Match the opened DRM render node, with PCI identity fallback, and fail setup if that GPU is unavailable or incompatible.
- Validate stream sizes against Pixelforge's shared encoder capabilities and actual input-image bounds before conversion, preserving the NVIDIA width padding and AV1 minimum-size guard.
- Maintain Inputtino and Pixelforge as pinned sources with reproducible patch series and CI checks for vendor drift and dependency pins.
- Reject odd widths or heights for H.264/HEVC 4:2:0 with a clear error; use an even size, supported 4:4:4, or AV1. Also reject AV1 frames below the GPU's minimum coded size to avoid AMD encoder hangs.
- Reorganize user documentation around Install → Start → Pair → Play, with GUI pairing recommended, native terminal pairing and browser fallback, platform compatibility notes and first-run troubleshooting. Correct healthcheck ordering, request-specific pairing links and configuration guidance without changing server behavior.

### Fixed

- Fix flashing black NVIDIA-encoded HEVC 4:4:4 video on Intel hardware decoders in Pyrolight and Moonlight by declaring the correct 8-bit or 10-bit Main 4:4:4 profile constraints.
- Fix sheared colors in NVIDIA HEVC 8-bit 4:4:4 streams at widths such as 1360, 1440 and 2400 by aligning encoder input-image widths to 64 texels while retaining the negotiated picture size.
- Prevent blank PyroWave video from capture buffers whose DMA-BUF modifiers NVIDIA's Vulkan driver cannot import. Capture uses formats supported by both EGL and Vulkan; unsupported fullscreen buffers and late overlays use GLES composition, and incompatible GBM allocations fail setup with a clear error.
- Correct PyroWave DMA-BUF image creation by supplying its external-memory information only once.
- Preserve Vulkan error codes through encoder operations so device loss reliably ends the session instead of depending on driver error wording.
- Stop sending AMD-encoded streams at the full negotiated bitrate regardless of content. In CBR mode the RADV encoder pads every H.264, HEVC and AV1 frame to the per-frame bit budget with filler data, which made up over 95% of a static desktop's video traffic; the padding is now removed before packetization. Decoded video is unchanged.
- Fix NVIDIA 8-bit 4:4:4 H.264/HEVC streams at widths such as 1360, 1440 and 2400 losing the GPU (`ERROR_DEVICE_LOST`, kernel Xid 31) or showing corrupted chroma blocks that grew until the next keyframe. The encoder wrote reconstructed reference pictures past the end of their images; reference images now use the same 64-texel-aligned width as input images.
- Encode the last row of AV1 streams whose height is odd (for example 1365x767) on AMD instead of a black line visible in clients that ignore AV1's render size. Frames are now coded on the GPU's AV1 picture grid with the replicated frame edge.
- Order each encoder reference-picture write after its preceding layout transition and previous encode, removing a synchronization hazard reported by Vulkan validation on AMD and NVIDIA.
- Address dark green, vertically striped H.264/HEVC/AV1 video reported on an RTX 5070 Ti since v0.17.0-beta-11 ([#11](https://github.com/karsyboy/pyroshine/issues/11)). Replace dynamically indexed pixel arrays in the RGB-to-YCbCr shader with explicit sample writes to avoid corrupted chroma. Confirmation on the affected Blackwell GPU is still pending.
- Keep late cursor and notification composition at every codec-supported stream size, including widths not divisible by four and supported odd sizes. Replicate frame-edge pixels into codec-block padding instead of leaving black-green borders.
- Round YCbCr samples to nearest instead of truncating, keeping full-range neutral chroma at 128 (512 at 10 bits) and removing the slight green bias.
- Read 10-bit ARGB/XRGB2101010 buffers with their correct channel layout in H.264/HEVC/AV1 streams. Composite unsupported fullscreen formats, such as NV12, instead of interpreting them as 8-bit RGB.

### Added

- Add `pyroshine pair` for interactive pairing from the host terminal or SSH. It verifies the requester, submits the PIN through the existing same-user management interface, and confirms the exact request completed. `--name` optionally labels the device; no GUI, browser or token copying is needed.
- Check RGB-to-YCbCr conversion against an independent CPU reference on the selected GPU whenever a converter is created. Incorrect test-pattern output stops stream setup with the affected plane and position instead of allowing corrupted colors through.

## [v0.17.3] - 2026-10-09

### Added

- Choose the audio quality: Pyrolight's **Audio quality** setting requests Standard, High or Maximum per stream (`x-moonshine-audio.quality`), and `[stream.audio] quality` sets it for other clients. Stereo streams at 96, 256 or 512 kbit/s, 5.1 at 256, 512 or 768 kbit/s and 7.1 at 450, 768 or 1024 kbit/s. Moonlight clients previously always received 96 kbit/s stereo; every client decodes the higher bitrates. The default stays Standard.
- Show the client's audio stream on the desktop app's dashboard: an **Audio** card with the channel layout, audio quality and whether the client requested it or the host default applied, the encoded Opus bitrate (after the packet-size limit), the Opus stream layout, sample rate, packet duration and encryption.

### Fixed

- Keep application audio playing after a client reconnects. The embedded PulseAudio server discarded a client's outstanding write credit when a reconnect emptied its stream, so some clients (Chromium-based audio such as Steam Big Picture's UI) ended up a few milliseconds short of the prebuffer and stopped writing. The stream then stayed silent, on any client, until another reconnect. Request accounting now matches PulseAudio across reconnects, underruns, cork/uncork and flushes.

## [v0.17.2] - 2026-10-08

### Added

- Show frame pacing on the desktop app's dashboard while a client streams: whether frames are captured as the game presents them (VRR capture) or on the refresh clock, the game's frame rate and frame times (median, p95, p99, max) as sent to the client, the share of uneven frame times, and the delay from new content to capture.
- Capture each new frame as the application presents it for clients that request VRR presentation (`clientVrrRequested`, sent by Pyrolight with VRR enabled), at up to the stream's frame rate. Games that do not run at exactly the stream rate are no longer quantized onto the refresh grid: a 90 FPS game on a 120 FPS stream was delivered as alternating 8.3/16.7 ms frames and now keeps its 11.1 ms cadence. Standard clients keep fixed refresh pacing; `[compositor] vrr_capture = "off"` disables it.
- Add `--vrr` and `--rtp-trace` to `moonshine-bench`, and honor `MOONSHINE_APPLICATION_UNIT` so a benchmark can run beside a live service without replacing its session.
- Publish `pyroshine-bin` and `pyroshine-ui-bin` in the signed `[pyrowave]` pacman repository (`karsyboy/pyrowave-packages`), so Arch Linux and CachyOS hosts install Pyroshine with pacman and upgrade it with `pacman -Syu`.

### Changed

- Derive RTP timestamps from each frame's content time, as Sunshine does, instead of from the frame count and negotiated frame rate. Skipped captures and games below the stream rate no longer make the RTP timeline run faster than real time, so clients can learn the source cadence.
- Require PyroWave C API 1.1 (fork revision `689854d`), which merges upstream's API 1.0 and frozen bitstream v1. The bundled library is now `libpyrowave-shared.so.1` and the portable installer removes the old `.so.0`. The wire format is unchanged, so existing Pyrolight clients keep working.

### Fixed

- Make the native packages depend on the Vulkan loader (`vulkan-icd-loader`, `libvulkan1`, `vulkan-loader`), which the server loads at runtime for capture and encoding but the packages did not declare.

## [v0.17.1] - 2026-10-07

### Fixed

- Resize the running application's windows when a resuming client requests a different resolution. Previously only the compositor output changed: Steam Big Picture and fullscreen X11 games kept rendering at the previous client's resolution and were scaled into the new stream.

## [v0.17.0] - 2026-10-06

Consolidates the v0.17.0-beta.1 through v0.17.0-beta.15 pre-releases and the
fixes made after them. Before upgrading, read [upgrade migration](NATIVE_PRESENTATION.md#upgrade-migration)
for the removed Vulkan layer, and check application `pre_command`/`post_command`
hooks and listener ports against the stricter configuration checks below.

### Added

- Add the optional `pyroshine-ui` desktop app and package: a tray icon showing whether a client is streaming, the session is retained without a client, or a client waits to pair; pairing notifications with PIN approval and rejection; paired-client naming and revocation; a settings editor for every `config.toml` setting; a stream dashboard with one-second statistics and the compositor's current foreground application; and diagnostics. It manages, but never starts or stops, the service. See [Desktop app](DESKTOP.md).
- Add a local management interface on the service user's D-Bus session bus (`io.github.karsyboy.Pyroshine`), used by the desktop app. Pyroshine runs unchanged without it or without a graphical session. Newly paired clients record an optional operator name and pairing time in `state.toml`.
- Preserve explicitly identified Xbox Elite Series 1/2 (four native grip events), classic Steam Controller and Steam Deck models in automatic gamepad emulation, including motion and Deck touch events when the client supplies them.
- Add `--cycles`, `--reconnect-cycles` and `--cursor static|moving` to `moonshine-bench` for repeated launch/stop and reconnect acceptance on real hardware and for measuring cursor capture paths.
- Report capture cadence with `log_stats`: interval percentiles, lateness against the refresh deadline, refresh versus deferred captures, and client and game commit coverage.

### Changed

- Keep the running application when a client disappears. `[stream].timeout` now only retires a silent client, as a clean disconnect does: held input is released and media paused while the session waits, without a deadline, for a resume. Quitting from Moonlight or the application exiting still ends the session.
- Stop capturing, converting and encoding video and audio while no client is connected to a retained session; the game keeps running, and the first frame after reconnecting is a keyframe of the current scene.
- Applications present on their actual Wayland/XWayland surfaces, and native Wayland color management (including parametric scRGB) owns HDR declarations and mastering metadata. Clean surfaces keep direct DMA-BUF export.
- Convert H.264/HEVC/AV1 input with a Pyroshine-owned compute shader, on the dedicated compute queue by default (`[stream.video] conversion_queue`). 4K NV12 conversion measured 73 µs of GPU time against about 200 µs with Pixelforge's converter on an RX 9070 XT; Pixelforge remains the fallback for widths that are not a multiple of four or odd heights.
- Keep fullscreen games on the direct-export path while the cursor or a Steam notification is visible, compositing them in the conversion shader or PyroWave scaler. At 4K120 HDR 4:4:4 PyroWave with a moving cursor this halved Pyroshine's graphics-engine time.
- Latch a surface commit only once its DMA-BUF has finished rendering, so the encoder no longer stalls behind a GPU-bound game. `MOONSHINE_DISABLE_READY_LATCH=1` restores immediate latching for diagnosis.
- Require PyroWave C API 0.9 (fork revision `4cff786`); the 4:4:4 payload fix is now part of the fork and `nix/patches/pyrowave-444-payload.patch` is removed.
- Packetize FEC blocks into one sender-owned allocation without the output-shard assembly copy, and packetize encrypted frames of 128 KiB or more off the shared runtime worker.
- Give session teardown an end-to-end deadline of 16 seconds (6 for stopping the application). If it is exceeded, new sessions are refused and Pyroshine exits so its service manager can restart it.
- Reject at startup a bind `address` that is not an IP address, TCP or UDP listeners sharing a port, a listener port of 0, a `[stream].timeout` outside 1–86400 seconds, and `pre_command`/`post_command` entries that are empty or whose executable cannot be found (previously silently dropped).
- With video encryption, `max_packet_size` also covers the 32-byte encryption prefix, so the datagram stays at `max_packet_size + 16` bytes as documented.
- Configuration saved from the desktop app is validated first, edits only the changed settings (keeping comments and formatting), refuses to overwrite a file changed since it was loaded, and is replaced atomically. A restart is still required to apply it. The server skips its own pairing notification while the desktop app runs.
- Report hardware-dependent tests as ignored instead of passing without executing; CI runs the pinned PyroWave FFI check explicitly.

### Removed

- Remove the custom Vulkan implicit layer (XWayland bypass and frame limiter), private presentation protocols and XWayland content replacement. Legacy XWayland HDR that relied on interception is unsupported, and `STEAM_GAMESCOPE_DYNAMIC_FPSLIMITER` and `STEAM_GAMESCOPE_HDR_SUPPORTED` are no longer set. Applications whose environment still selects the layer or its variables are refused with a diagnostic.

### Fixed

- Start video and audio reliably when the client's start signal arrives before both pipelines are waiting. Duplicate or early PLAYs, or a PLAY racing a reconnect, no longer stop the retained application or leave the stream paused.
- Finish the whole teardown (application unit, stream workers, sockets, UDP ports and the PulseAudio socket) before reporting a session idle, and only once the application unit's processes are gone; a new launch waits for it. Stopping also cancels a launch, stream start or reconnect in progress, a timed-out launch no longer leaves its unit running, and SIGTERM/SIGINT wait for the application to stop.
- Stop each finished session from leaking its XWayland server, display lock and connections, and take ownership of the XWayland process reliably so its exit is bounded at teardown.
- End the session when a video encoder or GPU device stops working instead of logging a failure for every frame; recoverable failures drop the frame and request an IDR. Captured frames own their DMA-BUF descriptors, so shutdown, resolution changes or buffer release can no longer hand the encoder a closed or reused descriptor.
- Verify that capture and the Vulkan encoder/import use the same GPU before advertising capabilities or launching, with actionable diagnostics for unvalidated cross-device paths.
- Resume a retained session whose client disconnected after PLAY but before the stream started. End the session when a reconnect cannot pause its media or its reconfiguration fails, instead of continuing with partially changed settings.
- Leave the live virtual output untouched on reconnects that change only codec, bitrate, chroma, bit depth or transport, and apply the resolution, refresh rate and HDR mode negotiated in the first ANNOUNCE to the compositor when they differ from the launch request.
- Pause audio on every disconnect and reconnect, and resume it with keys, PCM generation, Opus, RTP/FEC and settings committed together while keeping the application's PulseAudio connections.
- Release held keys, buttons, touch/pen contacts and queued text, neutralize controllers and retire old feedback when the control peer disconnects or is replaced. Virtual controllers stay plugged in across reconnects, which also removes the stutter from Steam raising its overlay after each reconnect.
- Keep controller input and session shutdown responsive while a client is slow to accept rumble, LED, trigger or motion feedback, and stop virtual controllers' background threads before their devices are destroyed.
- Send the current picture when a keyframe is requested on a static screen with H.264, HEVC or AV1, instead of a stale or garbage frame until the scene changes.
- Keep video capture on the virtual display's refresh deadlines when the encoder finishes after a refresh tick; at 2880x1920@120 up to 88% of gameplay captures previously drifted off the grid.
- Encode correct chroma for 8-bit 4:4:4 H.264/HEVC/AV1.
- Hold conventional video admission through UDP completion or discard, including IDR replays; interrupt blocked sends on pause, aggregate transport errors and rebase fallback pacing after socket stalls.
- Validate resolution, refresh rate, packet size, bitrate, audio channel count and packet duration in launch, resume and ANNOUNCE before reconfiguring anything, replying with the rejected value; unsupported audio settings no longer fall back to 5 ms stereo. Malformed SDP lines receive 400 Bad Request and malformed control messages are dropped, instead of panicking or ending the session.
- Bound HTTP, HTTPS and RTSP connections in number, size and duration, so a client stalling its TLS handshake no longer blocks others, and bound and expire pending pairing transactions.
- Answer PulseAudio clients with invalid stream parameters with an error instead of crashing the audio server, remove clients that close mid-request instead of spinning a CPU core, and stop a flooding client from starving others or the capture clock.
- Fit 10 ms high-quality surround audio within Moonlight's 1400-byte receive limit, and stop audio encryption overflowing for key IDs near the 32-bit limit.
- Restore Proton games that minimize themselves for the Steam overlay (for example Grim Dawn, which stayed black): iconic requests are acknowledged, a window regaining keyboard focus is made normal again, and the game keeps focus when Steam's windows take X keyboard focus.
- Stop Steam overlays and notifications that are destroyed without an unmap, or hidden by opacity, from staying painted above the game or keeping its input.
- Keep Steam's controller settings visible over native Wayland Proton games (`PROTON_ENABLE_WAYLAND=1`), and describe native Wayland windows accurately to Steam. Some Proton builds disable Steam Input with their Wayland driver; see [native presentation](NATIVE_PRESENTATION.md#wine-and-proton).
- Stop the cursor flashing while a game moves it with a controller.
- Never let an asynchronous X11 error from a vanished window terminate the server.
- Reconnect the desktop app automatically when its first attach to the service fails, explaining an incompatible or inaccessible service, and follow a different client resuming a retained session in the dashboard and tray.
- Stop `moonshine-bench` runs ending after one minute.

### Security

- Accept pairing approval (`/pin`, `/submit-pin`) only from the host itself. The PIN applies only to the pending request shown, which lists the requester address and certificate fingerprint, and unapproved requests expire after five minutes. A non-loopback `address` gets an additional loopback-only approval listener.
- Bind RTSP negotiation, media endpoint discovery and the control connection to the paired client and its latest `/launch` or `/resume`, with per-session ping and connect payloads for clients supporting Moonlight's session-ID extension. Control messages are accepted only from the peer authenticated with the current session key.
- Stop sending video to a client as soon as another client's `/resume` replaces its authorization. A discovery PING can no longer redirect media before PLAY, and a reconnect PLAY overtaken by a newer `/resume` can no longer activate with mixed keys or settings.
- Require an exact 16-byte `rikey` and 32-bit `rikeyid` for `/launch` and `/resume`, rejecting malformed keys before any state changes. AES-GCM nonces belong to the key, so no (key, nonce) pair is reused across reconfiguration, reconnects or later sessions, and counter exhaustion stops sending instead of wrapping.
- Generate TLS identities with owner-only files and durable, interruption-safe publication, keeping existing administrator-managed identities.
- Commit pairing trust atomically, with durable host-authorized and HTTPS self-revocation that also ends the revoked client's active session.
- Answer unsupported feature-report queries on a virtual DualSense or DualSense Edge with an error instead of reading past the reply buffer, which a local HID reader could use to crash the server.

## [v0.16.15] - 2026-10-01

### Fixed

- Keep capture and application frame callbacks on the same refresh clock through pacing stalls and reconnects, instead of retaining a shifted capture phase after missed slots.
- Pause video delivery on client disconnect and require an acknowledged encoder/transport epoch on every reconnect, preventing old paced PyroWave frames from entering a resumed or changed-codec stream.

## [v0.16.14] - 2026-10-01

### Added

- Explicit Nonary/Vibepollo record transport negotiation using the verified PyroWave block-format family, while retaining the single native wire-v1 frame/packetizer/FEC path and pinned C API 0.7.0.
- Paired HTTPS clients can discover and run a bounded 32 MiB bandwidth probe when no session is active; advertise routed physical Ethernet capacity when known.
- Configurable disabled, legacy Back hold, and recommended Back+Start Guide shortcuts; existing nonzero `hold_ms` configurations retain their legacy behavior.

### Fixed

- Accept Nonary's PyroWave record ANNOUNCE capabilities instead of requiring its client to claim native wire-v1. Reject unknown revisions and contradictory protocol/profile attributes with a compatibility reason.
- Preserve real Select holds with Back+Start shortcuts, extended controller flags, and held inputs during activation-rumble timer completion.
- Reject unsafe Wine/Proton XWayland bypass before top-level acceptance, cache safety behind X11 events, repair XCB child-query layout, and retire stale compositor overrides when falling back to XCB.
- Restore graphics as the automatic PyroWave queue preference; compute remains an explicit option.

## [v0.16.13] - 2026-10-01

### Fixed

- Include compositor work in PyroWave's frame pacing budget and use reusable Linux high-resolution packet timers, restoring the measured saturated composited 4K120 cadence without adding capture credits or frame queues.

### Added

- Add `[stream.video] log_stats` to enable or disable streaming diagnostic summaries and process sampling while preserving benchmark statistics and operational warnings.
- Implemented one-credit capture admission, direct-export rejection counters, GPU timing, import-cache telemetry, and async-compute selection with graphics fallback.

## [v0.16.12] - 2026-09-30

### Fixed

- Release completed direct-scanout buffers and flush Wayland release events before static-screen skipping, preventing swapchain image starvation under encoder load.

### Added

- Report scanout buffer releases and allocated swapchain image counts to diagnose capture stalls and image-count negotiation.

## [v0.16.11] - 2026-09-30

### Fixed

- Clear Tokio writable readiness after raw GSO sends encounter socket backpressure, preventing a retry loop from starving video transmission.
- Expire unused PyroWave DMA-BUF imports and release partial Vulkan import resources on setup errors.

### Added

- Five-second streaming diagnostics for capture resources, pipeline stages, transport backpressure, runtime delay, CPU usage, memory, and open file descriptors.

## [v0.16.10] - 2026-09-30

### Fixed

- Fix the 0.16.9 Vulkan query dispatch regression that crashed Steam's GPU process at Big Picture startup and forced software compositing, degrading streaming performance across encoders.
- Discover instance extensions through the Vulkan loader's global entry point so WSI initialization works when Mesa's device-selection layer is next in the chain.

## [v0.16.9] - 2026-09-30

### Fixed

- Negotiate driver-supported three-image swapchains on the XWayland Vulkan bypass using per-present-mode capabilities and swapchain maintenance, addressing conservative Wayland image-count limits.
- Gate WSI maintenance injection on real extension/feature support and restrict dynamic presentation to declared compatible modes.
- Refactor HDR and YUV 4:4:4 handleing

### Changed

- Deprecate and ignore `MOONSHINE_WSI_MIN_IMAGE_COUNT`; preserve ordinary driver image-count capabilities and report actual allocated swapchain counts.

## [v0.16.8] - 2026-09-30

### Added

- Preserve DualSense Edge identity and its four native extra buttons from compatible Moonlight clients through the virtual PlayStation controller.

### Changed

- Simplify the README and consolidate supporting guides under `docs/`.
- Document every `config.toml` setting and manual build, installation, and upgrade steps.
- Separate fork and upstream release history and require matching changelog notes before publishing releases.

## [v0.16.7] - 2026-09-30

### Fixed

- Capture the complete visible compositor scene, including cursors, overlays, and notifications, while keeping fullscreen direct export when safe.
- Preserve cursor visibility and correctly handle cursor replacement and destruction.
- Improve Steam overlay input routing and virtual controller family selection.

## [v0.16.6] - 2026-09-29

### Fixed

- Reconfigure video and audio on reconnect when codec, resolution, frame rate, HDR, or audio format changes.
- Resume unchanged streams through a fast path while restarting client-visible frame and packet epochs.

## [v0.16.5] - 2026-09-29

### Changed

- Pace UDP GSO transmission by frame size and bitrate and report pacing and send metrics.
- Preserve packetization across GSO chunk boundaries.

This tag points to a commit with embedded workspace version `0.16.4`.

## [v0.16.4] - 2026-09-29

### Added

- Configurable fixed, automatic, or disabled FEC with client feedback and bounds.
- PyroWave benchmark options and wire-byte/packet metrics.

### Fixed

- Respect minimum parity requirements and protocol limits when laying out FEC blocks.
- Rate-limit repeated FEC layout warnings and report PyroWave frames approaching size limits.

## [v0.16.3] - 2026-09-28

### Added

- Per-application `output_scale` for fractional Wayland output scaling.
- Public Pyroshine command, service, installer, and release package names.
- Release publishing from version tags, including portable and native packages.

### Changed

- Launch Plasma through a regular application entry and remove managed desktop-session handling.
- Improve PyroWave buffer handling and packetization efficiency and require hardware Vulkan devices.

### Fixed

- Improve compositor source-size selection and output geometry while preserving the client's physical stream resolution.

This release includes the branding, packaging, and compositor changes after the
commit referenced by v0.16.2, including the earlier legacy-tagged snapshots below.

## [v0.16.2] - 2026-09-28

### Added

- Native PyroWave encoding through the pinned C API, with separate codec/chroma/HDR negotiation and DMA-BUF interoperability checks.

This tag points to the initial PyroWave integration commit `fb5a89a`, earlier
than the v0.16.1.1 and v0.16.1.2 snapshots. Its embedded workspace version is
`0.16.1`; it does not contain all changes made by the later version-bump commit.

## [v0.16.1.2] - 2026-09-28

### Changed

- Remove redundant default initializers from application scanners.

Legacy four-component tag; embedded workspace version remains `0.16.1`.

## [v0.16.1.1] - 2026-09-28

### Added

- Initial fork integration of PyroWave, wire version 1 negotiation, hardware Vulkan checks, and encoding efficiency improvements.
- Pyroshine branding, install assets, and automatic release packaging.

### Changed

- Refactor desktop launching into normal application entries.

Legacy four-component tag; embedded workspace version remains `0.16.1`.
