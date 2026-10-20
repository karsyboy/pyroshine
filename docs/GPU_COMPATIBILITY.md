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
| VA-API | H.264 High; HEVC Main, Main 10, Main 4:4:4 8/10-bit; AV1 Main 8/10-bit | the GPU's fixed-function video encoder | `libva.so.2` and `libva-drm.so.2`, a VA-API driver with an encoder for the profile on the capture GPU's render node (Intel: `intel-media-driver`/iHD; AMD: Mesa), constant-bitrate rate control and packed headers, and `VK_EXT_queue_family_foreign` on the Vulkan side |
| NVENC | H.264 High and High 4:4:4; HEVC Main, Main 10, RExt 4:4:4 8-bit; AV1 Main 8/10-bit | NVIDIA's video encoder | the NVIDIA driver's `libcuda.so.1` and `libnvidia-encode.so.1` (NVENC API 12.1 or newer) |
| PyroWave | PyroWave 4:2:0, 4:4:4, HDR10 | compute shaders | Vulkan 1.3 compute, the pinned PyroWave library, and the same DMA-BUF import as capture |

The conventional encoders share the capture, color conversion and everything
after the encoded packet (padding removal, HDR metadata, packetization, FEC,
pacing). Frames stay on the GPU: the converter's output is copied on the GPU
into the encoder's input, a VA surface imported into Vulkan as DMA-BUF or a
Vulkan buffer imported into CUDA. VA-API and NVENC are loaded at runtime only
when a profile needs them; a host without them is unaffected.

`stream.video.encoder` chooses among them ([configuration](CONFIGURATION.md)).
`auto` (the default) uses Vulkan Video for every profile it encodes on the
capture GPU, then VA-API, then NVENC for the profiles it cannot; `vulkan`,
`vaapi` and `nvenc` use only that encoder. Each profile's encoder is decided
by the startup probe and fixed for the server's lifetime: a stream never
changes encoder, and an explicit choice never falls back to another one.

Every GPU needs Vulkan 1.3 with timeline semaphores, `VK_KHR_external_memory_fd`,
`VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, and
EGL/GLES on the same render node for the compositor. A GPU without Vulkan
Video encode still starts: its Vulkan context is compute-only, PyroWave is
offered if it passes its probe, and H.264/HEVC/AV1 are offered only through
VA-API or NVENC. A client that asks for a profile the host lacks is refused
at stream setup with the list of profiles the host has.

A profile is advertised only when the startup probe created its encoder with
the stream's constant-bitrate configuration on the capture GPU (VA-API and
NVENC also encode one frame) and the converter's color self-check passed for
that encoder's input format; nothing is inferred from the GPU vendor or name.

## Support by GPU

Each encoder on a GPU passes through separate validation stages, listed in
[Validation](#validation):

- **Encode tested:** the capability report and the GPU tests (encode/decode
  round trips against the CPU reference) passed on that hardware and driver.
- **Session tested:** benchmark sessions and their full and reconnect cycles
  ran with that encoder, and a real Moonlight or Pyrolight client streamed and
  reconnected with changed and unchanged settings.

**Expected** rows follow from the driver's source or documentation but were
not run here; **untested** rows have neither. A stage not yet run is listed as
pending, never inferred from another stage.

The conventional column lists what `auto` advertises and which encoder
encodes it; other encoders tested on that GPU follow in parentheses.

| GPU | Drivers | Conventional profiles (`auto`) | PyroWave | Encode tested | Session tested |
| --- | --- | --- | --- | --- | --- |
| AMD RX 9070 XT (RDNA4) | RADV and radeonsi, Mesa 26.2.4 | Vulkan Video: H.264; HEVC Main, Main10; AV1 Main 8/10-bit (VA-API: the same profiles, AV1 only at sizes in multiples of 8) | 4:2:0, 4:4:4, HDR10 | Vulkan Video, VA-API, PyroWave | Pending for every encoder |
| AMD RDNA2, RDNA3 | RADV, Mesa 25.0 or newer | Vulkan Video: H.264, HEVC; AV1 on RDNA3 | expected | Expected: RADV enables encode by default on VCN 2–4 | Pending |
| AMD Polaris, Vega and older (UVD/VCE) | RADV, radeonsi | VA-API where Mesa's VCE/UVD encoder offers constant bitrate | untested | Untested | Untested |
| NVIDIA RTX 3080 (Ampere) | NVIDIA 615.78.08 | Vulkan Video: H.264 incl. High 4:4:4 Predictive; HEVC Main, Main10, RExt 4:4:4 8/10-bit; no AV1 (no AV1 encoder in Ampere) (NVENC: the same except HEVC 4:4:4 10-bit) | 4:2:0, 4:4:4, HDR10 | Vulkan Video, NVENC, PyroWave | Pending for every encoder |
| NVIDIA Turing, Ada, Blackwell | NVIDIA 535 or newer | Vulkan Video: H.264, HEVC; AV1 on Ada and newer | expected | Expected | Pending |
| NVIDIA Pascal and older | NVIDIA | NVENC where Vulkan Video encode is missing | untested | Untested | Untested |
| Intel UHD 730 (Alder Lake-S GT1, Gen12) | ANV, Mesa 26.2.4; iHD 26.3.5 | VA-API: H.264; HEVC Main, Main10, Main 4:4:4 8/10-bit (ANV's Vulkan Video encoder, behind `ANV_DEBUG=video-encode`, has no constant-bitrate control) | 4:2:0 and HDR10 at 1080p60; 4:4:4 reaches 30 FPS | VA-API, PyroWave | Pending |
| Intel Arc B390 (Panther Lake, Xe3) | ANV, Mesa 26.2.4; iHD 26.3.5 | VA-API: H.264; HEVC Main, Main10, Main 4:4:4 8/10-bit; AV1 Main 8/10-bit (ANV has no Vulkan Video encode on Xe2 or newer) | 4:2:0, 4:4:4, HDR10 at 1080p60; 1440p120 reaches ~100 FPS | VA-API, PyroWave | Pending |
| Intel Arc A (DG2), Meteor Lake, Lunar Lake, Battlemage | ANV; iHD | VA-API: the profiles iHD encodes (H.264, HEVC; AV1 on DG2 and newer) | expected | Expected | Pending |

The encode tests use short smooth sequences: they establish input packing,
cropping, inter frames and requested key frames, not loss recovery on a
client, HDR signaling, sustained rate control or throughput at every
advertised resolution.

## Device and driver limits

- **NVIDIA:** Vulkan Video H.264 and HEVC encode at most 120 Mbps on an
  RTX 3080 (the profile's `maxBitrate`; other GPUs report their own). A client
  that asks for more gets a Vulkan Video stream encoded at the maximum; the
  server logs `Encoding at this GPU's maximum ... bitrate`. This is a Vulkan
  Video limit, not an NVENC one.
  H.264 4:4:4 is NVIDIA-only. Compositor capture and late composition handle
  NVIDIA's EGL-only modifiers (see [compositor](COMPOSITOR.md)).
- **AMD:** no conventional 4:4:4 profile; AV1 frames must be at least
  320x128; rate-control filler is removed from the bitstream.
- **Intel:** Mesa's ANV exposes Vulkan Video encode only on Gen9 to Gen12.5
  and only with the `ANV_DEBUG=video-encode` developer option; on Gen12 that
  encoder has no constant-bitrate rate control, so Pyroshine does not use it.
  Intel GPUs encode H.264/HEVC/AV1 through VA-API instead, which needs
  Intel's media driver (`intel-media-driver`, VA driver `iHD`); without it they
  stream PyroWave only. PyroWave throughput depends on the GPU: a UHD 730
  holds 1080p60 4:2:0, an Arc B390 1080p60 in every mode. iHD pads
  constant-bitrate frames with zero bytes, which are removed before sending.
- **VA-API:** HEVC 4:4:4 and AV1 depend on the driver (iHD encodes both on
  the GPUs above; Mesa's radeonsi encodes neither 4:4:4 nor, on the RX 9070
  XT, AV1 at sizes that are not multiples of 8, which it would code larger
  than negotiated). VA-API AV1 codes a single tile, which limits frames to
  4096 pixels wide and 4096x2304 pixels of area (4K included). The signaled
  level (and tier, for HEVC and AV1) covers the stream's frame size, frame
  rate and bitrate, Main tier first. The low-power encoder is preferred and
  the slice encoder used when low-power cannot encode the profile (for
  example older Intel GPUs without the HuC firmware its rate control needs).
- **NVENC:** HEVC 4:4:4 10-bit is not implemented (Vulkan Video encodes it on
  NVIDIA). The encoder uses NVIDIA's lowest-latency preset with infinite GOP
  and recovers from loss with reference invalidation.
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

Profiles encoded by VA-API or NVENC carry the encoder's name, for example
`HEVC Main 10 (VA-API)`. With `auto`, a missing profile lists each encoder's
reason. Typical reasons are `no queue family supports Vulkan Video encode`
(the driver has no encoder for this GPU), `the driver does not expose
VK_KHR_video_encode_av1`, `ERROR_VIDEO_PROFILE_FORMAT_NOT_SUPPORTED_KHR` (the
GPU lacks that chroma or bit depth), `has no constant-bitrate rate control`,
`libva.so.2: ... cannot open shared object file` or `no VA-API driver for
/dev/dri/renderD128` (libva or its driver is missing), and
`libnvidia-encode.so.1: ...` (no NVIDIA driver).

## Validation

An encoder becomes **encode tested** on a GPU and driver after:

1. `moonshine-bench --capabilities` (with `--encoder` for each encoder)
   reports the listed profiles and reasons.
2. `scripts/hardware_tests.py run` passes every available GPU test, including
   `encode_decode_matches_reference_on_gpu`, `stream_encode_decode_matches_reference_on_gpu`
   and `encoder_rate_control_limits_on_gpu` for Vulkan Video,
   `vaapi_encode_decode_matches_reference_on_gpu` and
   `nvenc_encode_decode_matches_reference_on_gpu` for the other encoders
   (decoded with `ffmpeg` and compared with the CPU reference, including an IDR
   request and a reference invalidation mid-stream), and
   `pyrowave_imports_compositor_dmabufs_on_gpu` for PyroWave (see
   [testing](TESTING.md)).

It becomes **session tested** after:

3. `moonshine-bench` runs each advertised codec at 1080p60 and the
   resolutions claimed, and `--cycles`/`--reconnect-cycles` complete with
   `--encoder` set to each encoder (each cycle records the encoder it used).
4. A real client streams and reconnects with it, with changed and unchanged
   settings and, for HDR profiles, SDR/HDR transitions
   ([reconnect validation](reconnect-validation.md)).

A GPU without Vulkan Video encode runs
`capture_device_without_encode_support_never_falls_back_on_gpu` with
`MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE` pointing at it: the compute-only
context must be built on that GPU and no other.
