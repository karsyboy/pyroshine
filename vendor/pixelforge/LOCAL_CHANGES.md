# Local Pixelforge patches

Base: https://github.com/hgaiser/pixelforge, the revision recorded in
`../pixelforge-patches/UPSTREAM` (v0.9.1, `2ee4d7ac6e470cc34270e48d6e9d6f3fc1d7c379`).
The crate files Pyroshine builds (`Cargo.toml`, `src/`, `shader/`, license, README,
changelog) are copied with their BSD-2-Clause license. This directory is generated:
it is exactly that revision plus the numbered patches in `../pixelforge-patches/`.

`moonshine-core` keeps depending on the upstream Git tag; the workspace `[patch]` in
the root `Cargo.toml` substitutes this directory, so `Cargo.lock` changes only the
source of `pixelforge`. Nix builds it as part of the source tree.

## Changes from upstream

| Patch | Change | Why |
| --- | --- | --- |
| `0001-encoder-64-align-input-image-widths-for-NVIDIA-8-bit.patch` | Encoder input images are 64 texels wide-aligned (within `maxCodedExtent`); pictures keep the coded width through `codedExtent`. | NVIDIA's encoder reads the chroma plane of 8-bit 2-plane 4:4:4 inputs with twice the 64-aligned luma pitch; at widths with `width % 64` in 1..=32 (1360, 1440, 2400, ...) the decoded chroma was sheared. Verified on an RTX 3080 (driver 615.78). |
| `0002-h265-complete-main-444-profile-constraints.patch` | Complete the VPS/SPS Range Extensions bit-depth and lower-bit-rate constraints for HEVC 4:4:4 before caching the headers. | Vulkan StdVideo does not expose these flags; NVIDIA writes zeros, so FFmpeg cannot select Intel VAAPI Main 4:4:4 hardware decoding. Normalizing the headers enables 8/10-bit decoding without changing coded pictures. |
| `0003-encoder-capabilities-and-image-extents.patch` | Share profile/capability construction across codec setup and public queries; expose coded and allocated input-image extents. | Pyroshine uses the actual image bounds without duplicating profiles or the maintained NVIDIA padding rule. Extent arithmetic rejects overflow and device-limit violations before allocating resources. |
| `0004-preserve-typed-vulkan-errors.patch` | Preserve driver result codes with operation context and expose `vulkan_result()`. | Device-loss classification uses `ERROR_DEVICE_LOST`, independent of error wording. Fault-injection tests cover submission, fence wait and encode feedback. |
| `0005-select-capture-device-by-drm-and-pci-identity.patch` | Add a DRM/PCI selector to context construction and clean up the instance on failed setup. | Select the opened capture GPU, with no fallback to another GPU when support or identity is unavailable. Pyroshine independently verifies the resulting device. |
| `0006-encoder-allocate-DPB-images-at-the-input-image-width.patch` | DPB images use the 64-aligned input-image width; pictures keep the coded extent. | NVIDIA writes reconstructed 8-bit 4:4:4 chroma with the same mis-computed pitch as patch 0001. At coded width the writes overran the DPB image: reference corruption growing over P frames (chroma errors of ~200 codes at 1440x768 H.264 4:4:4) and, when the overrun reached unmapped memory, an NVENC MMU fault (Xid 31) and `ERROR_DEVICE_LOST`. Verified on an RTX 3080 (driver 615.78). |
| `0007-av1-code-frames-on-the-coded-picture-grid.patch` | AV1 `max_frame_*` and picture `codedExtent` use the requested size rounded up to `codedPictureAlignment` (within the coded extent); `render_size` keeps the requested size. Exposes `EncoderCapabilities::av1_frame_extent`. | Implementations code AV1 on that grid but read the source only within `codedExtent`. RADV (8x2) coded 1365x767 as 1368x768 with a black last row, which clients that ignore `render_size` display. |
| `0008-encoder-order-reconstructed-writes-after-DPB-setup-barrier.patch` | The setup slot's DPB barrier makes prior writes available and the transition visible to later reads and writes. | Its empty access masks left the reconstructed-picture write unordered after the layout transition and the slot's previous encode (`SYNC-HAZARD-WRITE-AFTER-WRITE` on RADV and NVIDIA). |

## Updating

Never edit this directory by hand. Follow the shared
[maintenance workflow](../README.md) for patch creation, upstream review,
regeneration, Cargo/Nix coordination and drift checks.

```sh
scripts/vendor-pixelforge.sh
scripts/vendor-pixelforge.sh --check
scripts/vendor-pixelforge.sh --test
```

Remove a patch only when upstream provides its behavior, retaining the width
and HEVC profile contracts described above.

## Validation

`scripts/vendor-pixelforge.sh --test` runs the crate's unit tests, including
profile/capability chains, coded/image bounds, typed driver failures, strict device
identity selection, the width rule in `src/encoder/resources.rs` and HEVC profile/header regression
tests in `src/encoder/h265/profile.rs`, from a temporary copy. The
hardware check is the conventional encode/decode fixture on an NVIDIA GPU, whose
default extents include affected widths:

```sh
MOONSHINE_TEST_GPU=1 cargo test --release -p moonshine-core encode_decode_matches -- --ignored --nocapture
```

For HEVC 4:4:4 compatibility, save streams from the streaming CBR fixture
(`stream_encode_decode_matches_reference_on_gpu`, with
`MOONSHINE_TEST_E2E_SAVE_DIR=/tmp/streams`) and decode them on the intended client
GPU. Require VAAPI output and a `hwdownload` filter to prevent FFmpeg from hiding
a hardware failure with software fallback. Check both 8-bit and 10-bit, and
repeat 4:2:0, initial connection and reconnect in Pyrolight/Moonlight. See
[the pipeline validation guide](../../docs/PIPELINE_OPTIMIZATION.md#measurement-and-changes-needing-more-proof).

Validate selection on each GPU host with the opened render node (use a different
path when needed):

```sh
MOONSHINE_TEST_GPU=1 MOONSHINE_TEST_RENDER_NODE=/dev/dri/renderD128 \
  cargo test -p moonshine-core capture_device_selection_on_gpu -- --ignored --nocapture
```

The converter/encode fixtures use that same selected capture context. Run them
on AMD and NVIDIA, including the streaming configuration, input-slot rotation
and static recovery. Client initial connection/reconnect, SDR/HDR and both
encoder families remain hardware acceptance checks. The selector does not
create a cross-device capture path.

On a multi-GPU host where one adapter lacks Vulkan Video encode, verify that
requesting it fails without selecting the encoding adapter:

```sh
MOONSHINE_TEST_GPU=1 MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE=/dev/dri/renderD129 \
  cargo test -p moonshine-core capture_device_without_encode_support -- --ignored --nocapture
```

The encode/decode matrix shares a capture context across cases, like production,
and its streaming fixture uses 100 Mbps (within the RTX 3080's 120 Mbps H.264
profile limit). Run it as one process with the default extents on NVIDIA: the
H.264 4:4:4 1360-then-1440 sequence is the one that lost the device before patch
0006, and the twelve-frame streaming run exposes reference corruption. Check
`journalctl -k` for NVRM Xid messages afterwards. Run the GPU tests with
synchronization validation; they are expected to report no messages:

```sh
VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation VK_KHRONOS_VALIDATION_VALIDATE_SYNC=1 \
  MOONSHINE_TEST_GPU=1 cargo test --release -p moonshine-core gpu -- --ignored --test-threads 1
```
