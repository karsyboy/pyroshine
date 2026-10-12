# PixelForge Shaders

Precompiled Vulkan compute shaders for GPU-accelerated color format conversion.

## Shader

**color_convert.comp** — RGB → YUV compute shader.
Converts BGRx/RGBx/BGRA/RGBA/ABGR2101010/RGBA16F input to NV12/I420/YUV444/P010/YUV444P10 output using BT.709, BT.2020, or sRGB→BT.2020+PQ color space matrices.

## Compilation

Requires `glslc` from the Vulkan SDK.

```bash
./compile.sh
```

This compiles `color_convert.comp` to `color_convert.spv` (SPIR-V 1.6, Vulkan 1.3, optimized).
The `.spv` file is included in source via `include_bytes!` in `src/converter/pipeline.rs`.
