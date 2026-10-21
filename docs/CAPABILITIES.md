# Capability inventory

What Pyroshine implements, where, and which checks cover it. Use it to find the
owning code and the test that must keep passing before changing a feature, and
to see which features still depend on manual client validation. Paths are
relative to `moonshine-core/src/` unless marked otherwise.

Status:

- **Implemented**: complete code path with automated tests and a hardware check.
- **Partial**: works with stated limits, or only part of the path is covered.
- **Unsupported**: deliberately not offered; negotiation or the probe rejects it.
- **Unverified**: implemented, but no automated or scripted hardware check
  exercises it; only a real client can confirm it.

Hardware checks name the commands in [testing](TESTING.md): `hw:<test>` is a
test run by `scripts/hardware_tests.py`, `bench` is a
[benchmark workload](BENCHMARKING.md) and `cycles` the benchmark's lifecycle
cycles. GPU-dependent features are advertised only when the startup probe
succeeds on the capture GPU, so a GPU without them rejects them rather than
failing mid-stream.

## Codecs and formats

Conventional codecs are encoded with Vulkan Video through the vendored
Pixelforge, or with VA-API or NVENC for profiles Vulkan Video cannot encode
(or when `stream.video.encoder` selects them); `session/stream/video/pipeline/`
`backend.rs` is the shared interface. The startup probe (`healthcheck.rs`
`check_codecs`, `pipeline/encode_caps.rs`, `vaapi::probe`, `nvenc::probe`)
creates each profile's encoder with the stream's constant-bitrate
configuration and advertises only those that succeed and pass the converter
self-check, recording which encoder will encode it; every other profile keeps
its reason. `healthcheck::supports_video_format` is the single
negotiation gate (`CODEC_PROFILES`). Supported GPUs and drivers are listed in
[GPU compatibility](GPU_COMPATIBILITY.md).

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| H.264 8-bit 4:2:0 | Implemented | `pipeline/mod.rs`, `pipeline/convert.rs` | `pipeline` (61), `convert/tests.rs` layout/reference | `hw:encode_decode_matches_reference_on_gpu`, `hw:stream_encode_decode_matches_reference_on_gpu`, `bench` |
| Constant-bitrate rate control and the profile's maximum bitrate | Implemented: profiles without CBR are not advertised; the encoder bitrate is limited to `maxBitrate` | `pipeline/encode_caps.rs` `cbr_target`, Pixelforge patch 0010 | `encode_caps::tests`, Pixelforge `unsupported_rate_control_and_excess_bitrate_are_rejected` | `hw:encoder_rate_control_limits_on_gpu`; validation layers clean at 150 Mbps on an RTX 3080 (120 Mbps maximum) |
| H.264 4:4:4 (High 4:4:4 Predictive) | Implemented where the GPU advertises it | same | same | same; NVIDIA advertises it, RADV does not |
| H.264 10-bit | Unsupported | `healthcheck.rs` `supports_video_format` | `healthcheck::tests` | — |
| HEVC Main, Main10 | Implemented | same | same | same |
| HEVC RExt 4:4:4 8/10-bit | Implemented where advertised | same | same | same |
| AV1 Main 8/10-bit | Implemented where advertised (needs an AV1 encoder: e.g. RDNA3+, Ada+) | same; Pixelforge patch 0007 (coded picture grid) | same | same |
| AV1 High 4:4:4 8/10-bit | Implemented where advertised | same | same | same |
| PyroWave 4:2:0, 4:4:4, HDR10 | Implemented with the pinned library | `session/stream/video/pyrowave.rs`, `pyrowave_protocol.rs` | `pyrowave` (12), ABI check in CI | `hw:ffi_loads_pinned_api`, `hw:pyrowave_imports_compositor_dmabufs_on_gpu`, `bench` |
| SDR 8-bit | Implemented | `session/stream/video/format.rs` | `format`, `rtsp::tests` | `bench` |
| SDR 10-bit (`x-moonshine-video[0].bitDepth`) | Implemented for HEVC and AV1 | `rtsp.rs` `negotiated_video_axes`, `format.rs` | `rtsp::tests` | `bench` (`hevc-1080p60-10bit`) |
| HDR10 (BT.2020/PQ) | Implemented; only native color descriptions declare HDR | `compositor/color_management.rs`, `pipeline/hdr_sei.rs`, `format.rs` `validate` | `color_management`, `hdr_sei`, `format` | `hw:native_color_draw_preserves_pq_and_scales_linear_light`, `hw:invalid_native_color_constructors_…`, `bench` (`*-hdr10`); client display of HDR is unverified |
| Full and limited range | Implemented (PyroWave always full) | `format.rs`, `pipeline/convert.rs` | `convert/tests.rs` | `hw:converter_matches_reference_on_gpu` |
| Odd and unaligned extents | Implemented where the codec can crop | `pipeline/convert.rs`, Pixelforge patches 0001, 0003, 0006 | `video::tests::unaligned_extents_validate_…` | `hw:encode_decode_matches_reference_on_gpu` |
| Encoder selection (`stream.video.encoder`) | Implemented: per-profile choice fixed at startup, explicit choices never fall back | `pipeline/backend.rs`, `healthcheck.rs` `check_codecs`, `pipeline/mod.rs` `select_backend` | `backend::tests`, `healthcheck::tests`, `pipeline` | `bench --capabilities --encoder …` on RX 9070 XT, RTX 3080, UHD 730, Arc B390 |
| VA-API encoding (H.264; HEVC Main, Main10, 4:4:4 8/10-bit; AV1 Main 8/10-bit) | Implemented where the driver encodes the profile | `pipeline/vaapi/` (packed headers in `h264.rs`, `hevc.rs`, `av1.rs`; DMA-BUF surface import), packed 4:4:4 conversion in `convert.rs` | `vaapi` (bit writer, headers, AV1 offsets, profile map, bitrate-aware level/tier, corrupt coded buffers, entry-point fallback, slot completion after failed submissions), `padding` (iHD zero padding) | `hw:vaapi_encode_decode_matches_reference_on_gpu` on Arc B390 (7 profiles), UHD 730 (5), RX 9070 XT (5) |
| NVENC encoding (H.264 incl. 4:4:4; HEVC Main, Main10, 4:4:4 8-bit; AV1 Main 8/10-bit) | Implemented; HEVC 4:4:4 10-bit not implemented | `pipeline/nvenc/` (CUDA import of Vulkan buffers, reference invalidation) | `backend::tests`, `nvenc::tests` (color changes pending until accepted), `convert::tests` (external buffer ownership) | `hw:nvenc_encode_decode_matches_reference_on_gpu` on RTX 3080 (5 profiles; AV1 needs Ada or newer) |

## GPU vendors and device selection

There is no vendor-specific capability table in the code: every vendor goes
through the same Vulkan Video probe, and vendor differences are handled where
they occur. The verified hardware is listed in [GPU compatibility](GPU_COMPATIBILITY.md).

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| AMD (RADV) | Implemented | probe as above; CBR filler removal `pipeline/padding.rs`; AV1 grid patch 0007 | `padding` | all `hw:` GPU tests, `bench`, `cycles` |
| NVIDIA (proprietary) | Implemented | 64-texel input/DPB widths (patches 0001, 0006); NVML telemetry `gpu_sensors/nvml.rs` | `gpu_sensors::nvml::tests` | all `hw:` GPU tests, `hw:reads_an_nvidia_gpu`, `bench` (encoder utilization via `nvidia-smi`), `cycles` |
| Intel (ANV, iHD) | Implemented: conventional codecs through VA-API (needs `intel-media-driver`), PyroWave through Vulkan compute | compute-only capture context (`gpu.rs`, Pixelforge patch 0009); `pipeline/vaapi/` | Pixelforge `encode_queue_flag_without_extensions_is_not_encode_support`, `vaapi` | ANV exposes no Vulkan Video encode on Xe2/Xe3 and only with `ANV_DEBUG=video-encode` (without CBR) on Gen12. UHD 730 and Arc B390: `hw:vaapi_encode_decode_matches_reference_on_gpu`, `hw:pyrowave_imports_compositor_dmabufs_on_gpu`, PyroWave's own round-trip tests, `bench`; no real client checked |
| Capture GPU by DRM/PCI identity, no fallback | Implemented | `gpu.rs`, Pixelforge patch 0005 | `gpu::tests` | `hw:capture_device_selection_on_gpu`, `hw:capture_device_without_encode_support_never_falls_back_on_gpu` |
| Capture GPU without Vulkan Video encode | Implemented: compute-only context on the same device; PyroWave, and VA-API/NVENC where available | `gpu.rs` `capture_context`, `encode_status`; Pixelforge patch 0009 | `gpu::tests`, `healthcheck::tests` | `hw:capture_device_without_encode_support_never_falls_back_on_gpu` (UHD 730 beside an RTX 3080; Arc B390 alone) |
| GPU telemetry (busy, engines, power) | Implemented; NVIDIA via NVML only (no DRM fdinfo) | `gpu_sensors/` | `gpu_sensors` | `hw:reads_this_hosts_gpus` |

## Capture and pacing

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| Direct DMA-BUF export of the application's buffer | Implemented | `compositor/capture.rs` (`DirectReject`), `compositor/state.rs`, `capture/frame.rs` | `compositor::capture::tests`, `frame::tests` | `bench` (`capture.direct`), `hw:gpu_import_after_source_owner_teardown_and_fd_reuse` |
| Late composition (cursor, Steam notification drawn by the converter) | Implemented | `compositor/cursor.rs`, `pipeline/convert.rs` | `cursor::*_tests`, `convert/tests.rs` | `bench` (`hevc-1080p60-cursor`, `capture.direct_late`) |
| Composited GLES fallback | Implemented | `compositor/state.rs`, `compositor/scaling.rs` | `scaling::tests`, `handlers::tests` | `bench` (`*-composited`) |
| Fixed-rate capture | Implemented | `capture/mod.rs` `CapturePacing::Fixed`, `compositor/capture.rs`, `capture/admission.rs` | `capture::cadence_tests`, `admission::tests` | `bench` (pacing) |
| VRR-aware (presentation-driven) capture | Implemented; client-side VRR presentation is unverified here | `CapturePacing::Vrr`, `PresentationLimiter` | `cadence_tests`, `lifecycle_tests::capture_pacing_follows_each_generations_vrr_request` | `bench --vrr` (`hevc-1080p120-vrr`), `cycles --vrr` |
| Bounded capture admission and GPU completion | Implemented | `capture/admission.rs`, `pipeline/mod.rs` credits | `admission::tests`, `pipeline` | `bench` (frame age, outstanding) |

## Transport

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| Video packetization (RTP + NV header) | Implemented | `session/stream/video/packetizer.rs`, `shard_batch.rs` | `packetizer` (byte-exact fingerprints), `shard_batch` | `bench` |
| Video encryption (AES-GCM, per-generation keys) | Implemented | `packetizer.rs`, `session/keys.rs` | `packetizer`, `keys::tests` | `bench --encrypt-video` |
| Video FEC (off/fixed/auto) | Implemented | `session/stream/video/fec.rs` | `fec` | `bench --fec-mode auto` |
| GSO batching and send pacing | Implemented | `gso_socket.rs`, `pacing_timer.rs` | `gso_socket` (21) | `bench` (transport summary) |
| Endpoint discovery bound to the authorized generation | Implemented | `session/authorization.rs`, `video/mod.rs` | `authorization::tests`, `video::tests::endpoint_discovery_is_bound_…` | `cycles` |
| Audio encryption and FEC | Implemented | `session/stream/audio/` | `audio::tests` | — (needs a client) |
| Physical-network behavior (loss, congestion, client decode) | Unverified | — | — | client [reconnect matrix](reconnect-validation.md) |

## Session, application, input and audio lifecycle

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| Launch, stream start, stop, relaunch | Implemented | `session/manager.rs`, `session/lifecycle.rs` | `lifecycle_tests` (100 full cycles, every transition held/cancelled/failed) | `cycles` |
| Reconnect, unchanged and with changed codec/resolution/FPS/bitrate/HDR/chroma/audio | Implemented | `manager.rs` resume plans, `video/mod.rs` epochs | `lifecycle_tests::hundred_reconnect_cycles_alternate_every_negotiated_axis` | `cycles --reconnect-cycles` |
| Stale generations and completed sessions cannot affect newer ones | Implemented | `manager.rs` (`is_current`, epoch-scoped teardown), `authorization.rs` | `stale_generation_cannot_commit`, `completed_session_cannot_affect_its_successor`, `stale_peer_cannot_detach_its_replacement` | — |
| Cancellation during startup, service shutdown | Implemented | `manager.rs` | `stop_cancels_every_transition_await`, `service_shutdown_during_every_transition_completes` | — |
| Capture backend start, readiness, failure and release | Implemented | `session/capture/mod.rs` (`start_capture_thread`, `CaptureSource`, `LiveCapture`) | `session::capture::tests` (mock backends: init failure, panic, cancellation, ready timeout, exit after readiness, refused start, GPU hold released after thread-local state) | `compositor::xwayland_tests` (`hw:`), `cycles` |
| Managed desktop takeover on existing Wayland session | Implemented: native KDE/GNOME adapters, SDR, physical-output disable/restore, grace and remote audio | `desktop/`, `capture/portal/`, `audio/desktop.rs` | `desktop::guard::tests`, `desktop::grace::tests`, `lifecycle_tests::desktop_mode`, protected management routes and Dashboard tests | Physical KDE/GNOME acceptance outstanding; [matrix](DESKTOP_MODE.md#validation) |
| Encoder initialization failure, encoder/device loss | Implemented (teardown) | `pipeline/failure.rs`, watchdog in `manager.rs` | `failure_of_each_transition_…`, `watchdog_triggered_cleanup_completes`, `pipeline::encoder_failures_are_classified_by_recovery`, `device_lost_is_detected_by_driver_result` | — (no safe way to induce device loss) |
| Application unit stop exactly once; failed or hung stops not reported idle | Implemented | `session/application.rs`, `manager.rs` | `every_application_unit_is_stopped_exactly_once`, `failed_application_stop_is_not_reported_idle`, `application::tests` | `hw:transient_unit_stop_establishes_termination`, `hw:launch_failures_and_exits_report_the_exit_status`, `cycles` |
| Release of workers, sockets, XWayland, DMA-BUFs after stop | Implemented | `lifecycle.rs` worker guards, `compositor/xwayland_process.rs` | `teardown_waits_for_delayed_workers_…`, `xwayland_process::tests`, socket tests in `video`/`audio` | `cycles` (port, unit, children, DMA-BUF descriptors), `hw:gpu_import_after_source_owner_teardown_and_fd_reuse` |
| Retained application across disconnects | Implemented | `manager.rs`, `status.rs` | `retained_session_reports_current_authorized_client`, `foreground_changes_are_session_scoped_…`, `control::tests::clean_disconnect_detaches_…`, `silent_peer_is_detached_without_stopping_the_session` | `cycles --reconnect-cycles` |
| Controllers persist across reconnects | Implemented | `session/stream/control/input/` (`ownership.rs`, `gamepad.rs`) | `peer_replacement_keeps_the_device_but_revokes_state_and_feedback`, `input_without_arrival_rebinds_a_retained_device`, `owner_reset_orders_…` | `hw:native_devices_join_their_threads_on_destruction`; client controllers unverified here |
| Audio capture (embedded Pulse server) and reconnect | Implemented | `session/stream/audio/` | `audio::tests` (epochs, ordered activation, 100 real sessions), `pulse_server` | `hw:real_pulse_clients_play_every_layout_and_may_vanish` |
| UDP bandwidth calibration (authenticated, frame-shaped, production sender) | Implemented | `calibration/` | `calibration::tests` (loopback runs, forged/foreign hellos, consent, stop, preemption, admission, simulated bottleneck), `recommend::tests`, `wire::tests` (shared HMAC vector), `bandwidth_tests::udp_probe_negotiation_and_complete_run_over_https` | Simulated with netem in a network namespace; real LAN 2.5 GbE host ↔ 5 GbE client with Pyrolight and `moonshine-udp-probe` (host NIC limit reported as host-limited); Wi-Fi not measured |
| Optional frame limiting of launched applications (Proton DXVK/vkd3d-proton or MangoHud) | Implemented, off by default | `session/frame_limit.rs`, `session/application.rs` | `frame_limit::tests` (resolution, user values win, one limiter, MangoHud detection and fallback), `config::tests`, `application::tests` (disabled adds nothing), schema tests | `bench --frame-limit` with vkcube MAILBOX through MangoHud on RX 9070 XT (fixed and VRR capture, reconnect cycles); the Proton path and real games not measured |
| Application catalog: automatic Steam/Lutris/Heroic/desktop discovery, stable IDs, visibility | Implemented | `app_scanner/` (`service.rs`, `catalog.rs`, `identity.rs`, `watch.rs`) | `app_scanner::service::tests` (installs, removals, renames, library folders, unavailable storage, corrupt metadata, restarts, debouncing), `identity::tests` (legacy IDs, persistence, interrupted saves), `catalog::tests`, `webserver::security_tests::catalog_refresh_during_an_active_stream_leaves_it_untouched` | `moonshine-library --follow` against real launchers; Lutris and Heroic on real installs unverified here |

## Management

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| Web management interface: API, SSE, static files | Implemented | `management/web/`, `management/ops.rs`, `management/hub.rs`, `web/` | `management::web::tests` (real listener), `web::http::tests`, `web::assets::tests`, `hub::tests`, frontend `npm test` | Live against an isolated instance: setup, pairing approval, settings conflict, restart recovery, phone layout (Chromium); Firefox sign-in page only |
| Administrator authentication and HTTP protections | Implemented | `management/auth.rs`, `management/web/http.rs` | `auth::tests` (setup codes, hashing, expiry, revocation, lockout, damaged file), `web::tests` (CSRF, Origin, Host, body limits, rate limit, sign-out) | Live probes: rebinding Host, forged cookie, cross-site and form posts, path traversal |
| Session history | Implemented | `management/history.rs` | `history::tests` (recorder, retention, pagination, interrupted sessions, damaged database, unwritable storage) | Not yet recorded from a real stream |
| Release notifications | Implemented, off by default | `management/updates.rs` | `updates::tests` (semver precedence, channels, link validation, failures, install detection) | GitHub request not exercised in tests |
| Desktop tray | Implemented | `src/tray/` | `tray` tests (routes, addresses, activation tokens, session detection, launcher session identity and restart backoff, attach supervision, icons) | KDE Plasma 6 Wayland: icon state, notification replacement, portal launch, hide, crash and server-restart recovery; End Session with a live stream and GNOME not checked |

## Known gaps

- The benchmark has no Moonlight client: client decode, display latency, ENet
  control, controller feedback and audio playback need the
  [reconnect validation matrix](reconnect-validation.md) with a real client.
- GPU device loss and encoder failures are covered by classification and
  teardown tests, not by inducing a real device loss.
- Intel hosts encode H.264/HEVC/AV1 through VA-API (Mesa's ANV offers no
  usable Vulkan Video encoder; see [GPU compatibility](GPU_COMPATIBILITY.md)).
  The VA-API and NVENC encoders are covered by GPU encode/decode round trips
  and benchmarks; a real client has not streamed from them, nor from an
  Intel host.
