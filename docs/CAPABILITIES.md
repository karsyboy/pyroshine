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
Pixelforge (`session/stream/video/pipeline/`); the startup probe
(`healthcheck.rs` `check_codecs`, `pipeline/encode_caps.rs`) creates each
profile's encoder and advertises only those that succeed and pass the
converter self-check. `healthcheck::supports_video_format` is the single
negotiation gate (`CODEC_PROFILES`).

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| H.264 8-bit 4:2:0 | Implemented | `pipeline/mod.rs`, `pipeline/convert.rs` | `pipeline` (61), `convert/tests.rs` layout/reference | `hw:encode_decode_matches_reference_on_gpu`, `hw:stream_encode_decode_matches_reference_on_gpu`, `bench` |
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

## GPU vendors and device selection

There is no vendor-specific capability table: every vendor goes through the
same Vulkan Video probe, and vendor differences are handled where they occur.

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| AMD (RADV) | Implemented | probe as above; CBR filler removal `pipeline/padding.rs`; AV1 grid patch 0007 | `padding` | all `hw:` GPU tests, `bench`, `cycles` |
| NVIDIA (proprietary) | Implemented | 64-texel input/DPB widths (patches 0001, 0006); NVML telemetry `gpu_sensors/nvml.rs` | `gpu_sensors::nvml::tests` | all `hw:` GPU tests, `hw:reads_an_nvidia_gpu`, `bench` (encoder utilization via `nvidia-smi`), `cycles` |
| Intel (ANV) | Partial | same probe | — | ANV exposes Vulkan Video encode only with `ANV_DEBUG=video-encode` (experimental), so a default Intel host advertises no conventional codec; not part of routine validation |
| Capture GPU by DRM/PCI identity, no fallback | Implemented | `gpu.rs`, Pixelforge patch 0005 | `gpu::tests` | `hw:capture_device_selection_on_gpu`, `hw:capture_device_without_encode_support_never_falls_back_on_gpu` |
| GPU telemetry (busy, engines, power) | Implemented; NVIDIA via NVML only (no DRM fdinfo) | `gpu_sensors/` | `gpu_sensors` | `hw:reads_this_hosts_gpus` |

## Capture and pacing

| Capability | Status | Code | Automated tests | Hardware check |
| --- | --- | --- | --- | --- |
| Direct DMA-BUF export of the application's buffer | Implemented | `compositor/capture.rs` (`DirectReject`), `compositor/state.rs`, `compositor/frame.rs` | `capture::tests`, `frame::tests` | `bench` (`capture.direct`), `hw:gpu_import_after_source_owner_teardown_and_fd_reuse` |
| Late composition (cursor, Steam notification drawn by the converter) | Implemented | `compositor/cursor.rs`, `pipeline/convert.rs` | `cursor::*_tests`, `convert/tests.rs` | `bench` (`hevc-1080p60-cursor`, `capture.direct_late`) |
| Composited GLES fallback | Implemented | `compositor/state.rs`, `compositor/scaling.rs` | `scaling::tests`, `handlers::tests` | `bench` (`*-composited`) |
| Fixed-rate capture | Implemented | `compositor/capture.rs` `CapturePacing::Fixed`, `admission.rs` | `capture::cadence_tests`, `admission::tests` | `bench` (pacing) |
| VRR-aware (presentation-driven) capture | Implemented; client-side VRR presentation is unverified here | `CapturePacing::Vrr`, `PresentationLimiter` | `cadence_tests`, `lifecycle_tests::capture_pacing_follows_each_generations_vrr_request` | `bench --vrr` (`hevc-1080p120-vrr`), `cycles --vrr` |
| Bounded capture admission and GPU completion | Implemented | `compositor/admission.rs`, `pipeline/mod.rs` credits | `admission::tests`, `pipeline` | `bench` (frame age, outstanding) |

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
| Encoder initialization failure, encoder/device loss | Implemented (teardown) | `pipeline/failure.rs`, watchdog in `manager.rs` | `failure_of_each_transition_…`, `watchdog_triggered_cleanup_completes`, `pipeline::encoder_failures_are_classified_by_recovery`, `device_lost_is_detected_by_driver_result` | — (no safe way to induce device loss) |
| Application unit stop exactly once; failed or hung stops not reported idle | Implemented | `session/application.rs`, `manager.rs` | `every_application_unit_is_stopped_exactly_once`, `failed_application_stop_is_not_reported_idle`, `application::tests` | `hw:transient_unit_stop_establishes_termination`, `hw:launch_failures_and_exits_report_the_exit_status`, `cycles` |
| Release of workers, sockets, XWayland, DMA-BUFs after stop | Implemented | `lifecycle.rs` worker guards, `compositor/xwayland_process.rs` | `teardown_waits_for_delayed_workers_…`, `xwayland_process::tests`, socket tests in `video`/`audio` | `cycles` (port, unit, children, DMA-BUF descriptors), `hw:gpu_import_after_source_owner_teardown_and_fd_reuse` |
| Retained application across disconnects | Implemented | `manager.rs`, `status.rs` | `retained_session_reports_current_authorized_client`, `foreground_changes_are_session_scoped_…`, `control::tests::clean_disconnect_detaches_…`, `silent_peer_is_detached_without_stopping_the_session` | `cycles --reconnect-cycles` |
| Controllers persist across reconnects | Implemented | `session/stream/control/input/` (`ownership.rs`, `gamepad.rs`) | `peer_replacement_keeps_the_device_but_revokes_state_and_feedback`, `input_without_arrival_rebinds_a_retained_device`, `owner_reset_orders_…` | `hw:native_devices_join_their_threads_on_destruction`; client controllers unverified here |
| Audio capture (embedded Pulse server) and reconnect | Implemented | `session/stream/audio/` | `audio::tests` (epochs, ordered activation, 100 real sessions), `pulse_server` | `hw:real_pulse_clients_play_every_layout_and_may_vanish` |
| UDP bandwidth calibration (authenticated, frame-shaped, production sender) | Implemented | `calibration/` | `calibration::tests` (loopback runs, forged/foreign hellos, consent, stop, preemption, admission, simulated bottleneck), `recommend::tests`, `wire::tests` (shared HMAC vector), `bandwidth_tests::udp_probe_negotiation_and_complete_run_over_https` | Simulated with netem in a network namespace; a real LAN or Wi-Fi path not yet measured |
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
- Intel hosts are not routinely validated (ANV encode is experimental).
