# Cross-fork block-format evidence

`pyrowave-cross-codec-test` is a Linux GPU harness, separate from production
codec code. It resolves encoder/decoder C API functions from independent shared
libraries with local/deep binding, preventing symbol interposition from making
the comparison accidentally decode with the wrong library. Both codecs borrow
the same Vulkan context through their unchanged C API device structures.

The input is deterministic RGB ramps and chroma edges. SDR uses 8-bit input;
HDR uses packed 10-bit input. Decode outputs are GPU R16 planes read back for
comparison. Each cross-decode must equal its encoder's own self-decode sample
for sample. This verifies block compatibility rather than equality of the two
encoders' independently generated compressed bytes.

```sh
cmake -S . -B build -DPYROWAVE_CROSS_CODEC_TESTS=ON
cmake --build build --target pyrowave-cross-codec-test
python3 tests/cross_codec.py build/pyrowave-cross-codec-test \
  build/libpyrowave-shared.so /path/to/nonary/libpyrowave-shared.so evidence
python3 tests/cross_codec.py build/pyrowave-cross-codec-test \
  build/libpyrowave-shared.so /path/to/nonary/libpyrowave-shared.so matrix --matrix
```

The default preserves small frame fixtures/readbacks. The matrix covers
1920x1080, 2560x1440 and 3840x2160; 60/120 FPS budgets; 50, 100, 200, 400, 600
and 800 Mbps; both directions; SDR/HDR and 420/444. Large binary files are
discarded after comparison; results.json and GPU logs remain. Frame budgets
are aligned bytes per frame, not a sustained frame-rate benchmark or UDP test.
GPU access is required. Run the test against the exact libraries you deploy.

Checked-in `fixtures/native-self` and `fixtures/nonary-self` were generated at
256x144 on AMD Radeon RX 9070 XT / RADV GFX1201, Mesa 26.2.3, Vulkan 1.4.354.
Native code: `e87ba6e` from the authoritative fork (wire-compatible changes on
the pinned e344479 C API 0.7.0), with the existing audited Granite color patches.
Reference: Nonary release/6.1.0-vrr18 vendored Themaister
`186f0393b77f7755953b5ecde994bb1cec2e4155`, C API 0.6.0.
The full matrix passed all 288 comparisons on that GPU.

The reference scaled encoder leaves color sequence bits zero, including HDR;
the native encoder writes HDR color bits. A negotiated record adapter must
normalize that default from session metadata. This belongs to the transport
integration and does not change the codec or native wire-v1 framing.

After merging upstream API 1.0 (bitstream v1 freeze, fork API 1.1), the
default fixture run and the full 288-comparison matrix passed between the merged
library and the pre-merge fork (`4cff786`, API 0.9) on the same RX 9070 XT /
RADV GFX1201, Mesa 26.2.4. The harness resolves the scaled encode entry point by
its 1.0 name and falls back to the pre-1.0 `_synchronous` name.

No live client/server stream, physical HDR display, NIC loss, or reconnect is
established by these fixtures. Those require the deployment validation matrix.

# Overlay (late composition) evidence

`pyrowave-overlay-test` (registered with `-DPYROWAVE_GPU_TESTS=ON`) checks
`pyrowave_encoder_encode_gpu_scaled_overlay` for SDR/R8 and
PQ2020/R16, 4:2:0/4:4:4 and RGBA/BGRA overlays partly outside the input.
A binary-alpha overlay must decode sample-for-sample like the same input
composited on the CPU beforehand, encoded with the same (inert-overlay) scaler
variant; fractional alpha may differ only by the reference's 8-bit rounding.
Layer lists (`pyrowave_encoder_encode_gpu_scaled_layers`) add two-layer cases: an image-view layer (BGRX, alpha
ignored) under a CPU-texel cursor must decode identically to the pre-composited
input at full opacity, and within the 8-bit rounding tolerance at 75% opacity.
The test also bounds the overlay variant against the plain scaler shader
(drivers may make different relaxed-precision choices per specialization) and
rejects scaled input and unsupported overlay formats before submitting work.
Payload placement uses atomics, so compressed bytes are not compared directly.
The bitstream format is unchanged; only encoder input differs.

# Encoder stage benchmark

`pyrowave-stage-bench frames.rgba W H [444|420] [sdr|hdr] [iterations] [budget]`
encodes raw RGBA8 frames through the C API scaled path, prints per-stage GPU
timestamps, then decodes each frame and prints an FNV-1a hash of the decoded
R16 planes. Shader changes meant to be exact must leave every hash unchanged.
It runs the encoder on the compute queue by default so a hang from an
experimental shader resets only a compute ring
(`PYROWAVE_STAGE_BENCH_GRAPHICS=1` selects graphics);
`PYROWAVE_STAGE_BENCH_NO_DECODE=1` times without decoding.

On an RX 9070 XT with 4K natural-image frames at a 416 KB budget, quantization
measured 0.186 ms for 4:4:4 SDR and behaved as bandwidth-bound: aggregating its
payload atomics per subgroup, merging its clustered integer reductions and
replacing its float reconstruction with exact integer shifts each left output
and timing unchanged.
