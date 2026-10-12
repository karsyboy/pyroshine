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

## Updating

Never edit this directory by hand.

```sh
# Change a local fix: edit it in an upstream checkout, `git format-patch` it into
# vendor/pixelforge-patches/ with a four-digit prefix, then regenerate.
scripts/vendor-pixelforge.sh

# Move to a new upstream release (fails if a patch no longer applies).
scripts/vendor-pixelforge.sh v0.10.0
cargo update -p pixelforge   # after updating the tag in moonshine-core/Cargo.toml

# Verify the committed tree is upstream plus the patches, and run its tests.
scripts/vendor-pixelforge.sh --check
scripts/vendor-pixelforge.sh --test
```

When upstream includes a fix, delete its patch and regenerate. Once no patches
remain, delete this directory, `vendor/pixelforge-patches/` and the `[patch]`
entry, and depend on the upstream release directly.

## Validation

`scripts/vendor-pixelforge.sh --test` runs the crate's unit tests, including
the width rule in `src/encoder/resources.rs` and HEVC profile/header regression
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
