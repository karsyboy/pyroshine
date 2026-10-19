# GPU and encoder compatibility

Which GPUs and drivers Pyroshine can stream from, what each one encodes, and
how that is decided and checked. The capture GPU is also the encode GPU:
Pyroshine never copies frames to another GPU or through the CPU. For how the
capture device is selected and verified, see
[architecture](ARCHITECTURE.md#startup-and-capability-advertisement); for what
each codec and format does, see the [capability inventory](CAPABILITIES.md).

## Encoders

| Encoder | Codecs | Runs on | Required on the capture GPU |
| --- | --- | --- | --- |
| Vulkan Video (vendored Pixelforge) | H.264, HEVC, AV1 at the profiles the driver supports | the GPU's fixed-function video encoder | Vulkan 1.3 with `VK_KHR_video_queue`, `VK_KHR_video_encode_queue`, a codec extension (`VK_KHR_video_encode_h264`/`h265`/`av1`), a video encode queue and constant-bitrate rate control for the profile |
| PyroWave | PyroWave 4:2:0, 4:4:4, HDR10 | compute shaders | Vulkan 1.3 compute, the pinned PyroWave library, and the same DMA-BUF import as capture |

Every GPU needs Vulkan 1.3 with timeline semaphores, `VK_KHR_external_memory_fd`,
`VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, and
EGL/GLES on the same render node for the compositor. A GPU without Vulkan
Video encode still starts: its Vulkan context is compute-only, PyroWave is
offered if it passes its probe, and no H.264/HEVC/AV1 profile is advertised.
Moonlight clients without PyroWave are then refused at stream setup with the
list of profiles the host has.

Pyroshine does not use VA-API or NVENC. A profile is advertised only when
the startup probe created its encoder with the stream's constant-bitrate
configuration on the capture GPU and the converter's color self-check passed;
nothing is inferred from the GPU vendor or name.

## Support by GPU

**Verified** means run on that hardware and driver with the checks in
[Validation](#validation). **Expected** follows from the driver's source or
documentation but was not run here. **Untested** has neither.

| GPU | Driver | Conventional profiles (Vulkan Video) | PyroWave | Status |
| --- | --- | --- | --- | --- |
| AMD RX 9070 XT (RDNA4) | RADV, Mesa 26.2.4 | H.264; HEVC Main, Main10; AV1 Main 8/10-bit | 4:2:0, 4:4:4, HDR10 | Verified |
| AMD RDNA2, RDNA3 | RADV, Mesa 25.0 or newer | H.264, HEVC; AV1 on RDNA3 | expected | Expected: RADV enables encode by default on VCN 2–4 |
| AMD Polaris, Vega and older (UVD/VCE) | RADV | none (no Vulkan Video encode) | untested | Untested |
| NVIDIA RTX 3080 (Ampere) | NVIDIA 615.78.08 | H.264 incl. High 4:4:4 Predictive; HEVC Main, Main10, RExt 4:4:4 8/10-bit; no AV1 (no AV1 encoder in Ampere) | 4:2:0, 4:4:4, HDR10 | Verified |
| NVIDIA Turing, Ada, Blackwell | NVIDIA 535 or newer | H.264, HEVC; AV1 on Ada and newer | expected | Expected |
| NVIDIA Pascal and older | NVIDIA | unknown | untested | Untested |
| Intel UHD 730 (Alder Lake-S GT1, Gen12) | ANV, Mesa 26.2.4 | none by default. With `ANV_DEBUG=video-encode`: none, because ANV offers only disabled rate control there | 4:2:0 and HDR10 at 1080p60; 4:4:4 reaches 30 FPS | Verified (PyroWave only) |
| Intel Arc B390 (Panther Lake, Xe3) | ANV, Mesa 26.2.4 | none: ANV exposes no Vulkan Video encode on Xe2 or newer, with or without `ANV_DEBUG` | 4:2:0, 4:4:4, HDR10 at 1080p60; 1440p120 reaches ~100 FPS | Verified (PyroWave only) |
| Intel Arc A (DG2), Meteor Lake | ANV | none by default (`ANV_DEBUG=video-encode` is experimental) | expected | Expected (PyroWave only) |
| Intel Lunar Lake, Battlemage (Xe2) | ANV | none (same ANV rule as Xe3) | expected | Expected (PyroWave only) |

## Device and driver limits

- **NVIDIA:** H.264 and HEVC encode at most 120 Mbps (RTX 3080, profile
  `maxBitrate`). A client that asks for more gets a stream encoded at the
  maximum; the server logs `Encoding at this GPU's maximum ... bitrate`.
  H.264 4:4:4 is NVIDIA-only. Compositor capture and late composition handle
  NVIDIA's EGL-only modifiers (see [compositor](COMPOSITOR.md)).
- **AMD:** no conventional 4:4:4 profile; AV1 frames must be at least
  320x128; rate-control filler is removed from the bitstream.
- **Intel:** Mesa's ANV exposes Vulkan Video encode only on Gen9 to Gen12.5
  and only with the `ANV_DEBUG=video-encode` developer option; on Gen12 that
  encoder has no constant-bitrate rate control, so Pyroshine does not use it.
  Intel GPUs therefore stream PyroWave only. Their throughput depends on the
  GPU: a UHD 730 holds 1080p60 4:2:0, an Arc B390 1080p60 in every mode.
- **Any GPU:** the capture and encode GPU are the same device. A multi-GPU
  host streams from the GPU configured in `compositor.gpu`
  (or `MOONSHINE_RENDER_NODE`); another GPU is never used instead.

## Reading a host's support

The health check's **Vulkan** line names the driver and its Vulkan Video
encode support, and **Codecs** lists the advertised profiles and, under
**Not offered**, each missing profile with its reason:

```sh
pyroshine healthcheck
```

For a machine-readable report with every profile's reason:

```sh
moonshine-bench --capabilities --json caps.json
```

Typical reasons are `no queue family supports Vulkan Video encode` (the driver
has no encoder for this GPU), `the driver does not expose VK_KHR_video_encode_av1`,
`ERROR_VIDEO_PROFILE_FORMAT_NOT_SUPPORTED_KHR` (the GPU lacks that chroma or
bit depth) and `has no constant-bitrate rate control`.

## Validation

A row becomes **Verified** after, on that GPU and driver:

1. `moonshine-bench --capabilities` reports the listed profiles and reasons.
2. `scripts/hardware_tests.py run` passes every available GPU test, including
   `encode_decode_matches_reference_on_gpu`, `stream_encode_decode_matches_reference_on_gpu`
   and `encoder_rate_control_limits_on_gpu` for conventional codecs and
   `pyrowave_imports_compositor_dmabufs_on_gpu` for PyroWave (see
   [testing](TESTING.md)).
3. `moonshine-bench` runs each advertised codec at 1080p60 and the
   resolutions claimed, and `--cycles`/`--reconnect-cycles` complete.
4. A real client streams and reconnects ([reconnect validation](reconnect-validation.md)).

A GPU without Vulkan Video encode runs
`capture_device_without_encode_support_never_falls_back_on_gpu` with
`MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE` pointing at it: the compute-only
context must be built on that GPU and no other.
