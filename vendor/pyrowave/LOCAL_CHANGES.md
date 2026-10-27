# Local PyroWave patches

Base: https://github.com/Themaister/pyrowave, the revision recorded in
`../pyrowave-patches/UPSTREAM` (`7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b`).
The files the standalone library build and its tests need (CMake, the C API and
codec sources, `shaders/`, `bitstream/`, `pkg-config/`, `tests/`, the Granite
checkout script) are copied with their MIT license. Metal, Android, evaluation
data and the development tools are left out. This directory is generated: it is
exactly that revision plus the numbered patches in `../pyrowave-patches/`.

Granite is not vendored. The patched `checkout_granite.sh` clones Themaister's
Granite at the pinned commit (with Volk and Vulkan-Headers) and applies the
Granite patches in `patches/`; Nix fetches the same revisions and applies the
same files. Pyroshine builds the tree with `scripts/build-pyrowave.sh` and
`nix/pyrowave.nix`. Pyrolight builds its decoder from this directory at a
pinned Pyroshine commit.

The series replaces the former `karsyboy/pyrowave` fork, whose own commits on
top of upstream it reproduces (patches 0001 to 0005), plus the Granite patch
Pyroshine applied on top of the fork (0006) and two fixes (0007, 0008).

## Changes from upstream

| Patch | Change | Why |
| --- | --- | --- |
| `0001-decoder-validate-coefficient-packets.patch` | Validate every byte the GPU coefficient unpacker consumes before a packet is queued; reject a sequence header announcing more blocks than the frame holds; copy headers instead of dereferencing unaligned packet memory. The bitstream structures move to `pyrowave_bitstream.hpp`. | The unpacker trusts plane counts and significance bits inside a packet, so a malformed or truncated network packet could make it read past the payload. Clients decode packets from the network. |
| `0002-encoder-size-444-payload-buffer.patch` | The temporary payload buffer is twice as large for 4:4:4, computed in 64 bits. | It is written before rate control trims the frame and was sized for 4:2:0, while 4:4:4 carries twice the coefficient samples. |
| `0003-granite-scaler-overlays-and-sdr-normalization.patch` | `checkout_granite.sh` applies `granite-sdr-normalization.patch` and `granite-scaler-overlay.patch`; `shaders/slangmosh_scaler.hpp` is regenerated from the patched scaler. | PQ input converted for SDR output is normalized to SDR reference white instead of left in nits. Up to two premultiplied layers are composited 1:1 before any transfer function, for late cursor and Steam notification composition without re-rendering the scene. |
| `0004-c-api-1.1-color-metadata-overlays-completion.patch` | C API 1.1.0: `pyrowave_encoder_set_color_metadata`, `pyrowave_encoder_encode_gpu_scaled_overlay`/`_layers`, checked fence waits in every readback call, a failed scaled encode waits for its queued scaler work, `packetize` bounds checks, and a readback GPU stage. | Upstream always writes zero color fields in the sequence header. Pyroshine composites late layers through the scaler. Granite's fence wait reports nothing on failure, so without the checks a lost device looked like success and a failed frame could release a buffer the GPU still read (seen on an RX 9070 XT). Pyroshine requires exactly 1.1.0. |
| `0005-tests-gpu-fixtures-and-stage-benchmark.patch` | CPU packet-validation CTest; GPU overlay and color-pipeline tests (`-DPYROWAVE_GPU_TESTS=ON`); cross-revision codec fixtures (`-DPYROWAVE_CROSS_CODEC_TESTS=ON`, see `tests/README.md`); `pyrowave-stage-bench`. | Regression coverage for patches 0001 to 0004 and bitstream compatibility across builds. |
| `0006-granite-bt2408-reference-white.patch` | Add `granite-sdr-reference-white-203.patch`, applied last: SDR white is 203 nits when converting between SDR and PQ. | BT.2408 reference white, which Pyroshine's compositor and conventional converter use; upstream Granite uses 200. Host code only, so `slangmosh_scaler.hpp` is unchanged. |
| `0007-c-api-p010-range-metadata.patch` | The scaled path writes the limited-range bit only for RGB input, now excluding P010 as well as NV12. | Both are scaled without color conversion and keep their own range; P010 arrived upstream after patch 0004. |
| `0008-granite-rerunnable-patching-in-nested-work-trees.patch` | `checkout_granite.sh` restores Granite's pinned files, then applies each Granite patch inside Granite's checkout (`git apply -p2`). | A re-run failed (the 203-nit patch edits lines the normalization patch adds, so a patched tree neither takes nor reverses it), and inside another work tree (`deps/PyroWave` in Pyrolight, Pyroshine's `target/`) `git apply` resolved the paths against that tree and skipped them, leaving Granite unpatched. |

Candidates for upstream: 0001, 0002 and the checked-completion part of 0004 fix
upstream defects with no API change. The color-metadata setter fills fields the
bitstream specification already defines. The overlay layers and the Granite
patches are Pyroshine features that upstream may not want. Drop a patch once
upstream provides its behavior; when only the overlay and Granite patches
remain, the series is still needed, because Pyroshine's late composition uses
them.

## Updating

Never edit this directory by hand. Follow the shared
[maintenance workflow](../README.md) for patch creation, upstream review,
regeneration and drift checks.

```sh
scripts/vendor-pyrowave.sh
scripts/vendor-pyrowave.sh --check
scripts/vendor-pyrowave.sh --test
```

A patch that changes a Granite shader must regenerate `shaders/slangmosh_scaler.hpp`
with `slangmosh` (see `slangmosh.sh`) in the same patch. Upstream regenerates
`shaders/slangmosh.hpp` itself; take its copy. When the base moves, check that
upstream did not change the C API 1.0 functions the patches build on, and that
Pyrolight's pinned decoder still accepts the bitstream.

## Validation

`scripts/vendor-pyrowave.sh --test` builds the tree with
`scripts/build-pyrowave.sh` and runs the CPU packet-validation test. GPU checks
reuse the copy that script prepares (with Granite checked out and patched), so
the generated tree stays clean:

```sh
scripts/build-pyrowave.sh /tmp/pw /tmp/pw/install
cmake -S /tmp/pw/src -B /tmp/pw/gpu -DCMAKE_BUILD_TYPE=Release \
  -DPYROWAVE_GPU_TESTS=ON -DPYROWAVE_CROSS_CODEC_TESTS=ON
cmake --build /tmp/pw/gpu --parallel
(cd /tmp/pw/gpu && ctest --output-on-failure && ./pyrowave-c-test && ./pyrowave-c-interop-test)
```

`pyrowave-cross-codec-test` decodes `tests/fixtures/` and encodes and decodes
across a previous build, which confirms that the bitstream stays compatible.
Pyroshine's FFI load test (`ffi_loads_pinned_api`) checks the installed library;
the stream checks are in [PyroWave](../../docs/PYROWAVE.md).
