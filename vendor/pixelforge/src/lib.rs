//! A Vulkan-based video encoding library for Rust, supporting H.264, H.265, and AV1 codecs.
//!
//! # Features
//!
//! - **Hardware-accelerated** video encoding using Vulkan Video extensions.
//! - **Multiple codec support**: H.264/AVC, H.265/HEVC, AV1.
//! - **GPU color conversion**: RGB/BGR → YUV via Vulkan compute shaders (BT.709, BT.2020, sRGB→BT.2020+PQ, scRGB-linear→BT.2020+PQ).
//! - **HDR support**: 10-bit encoding (P010, YUV444P10), PQ transfer function, BT.2020 color space.
//! - **GPU-native API**: Encode directly from Vulkan images (`vk::Image`).
//! - **Flexible configuration**: Rate control (CBR, VBR, CQP), quality levels, GOP settings.
//! - **Multiple input formats**: BGRx, RGBx, BGRA, RGBA, ABGR2101010 (10-bit packed), RGBA16F (FP16).
//! - **Utility helpers**: [`InputImage`] for easy YUV data upload to GPU.
//! - **Optional DMA-BUF support**: Zero-copy image import from external processes (Linux only).
//!
//! > **Note**: B-frame support is not yet implemented. Setting `b_frame_count > 0` will panic.
//!
//! # Supported Codecs
//!
//! | Codec | Encode |
//! |-------|--------|
//! | H.264/AVC | ✓ |
//! | H.265/HEVC | ✓ |
//! | AV1 | ✓ |
//!
//! # Requirements
//!
//! - A GPU with Vulkan video encoding support (e.g., NVIDIA RTX series, AMD RDNA2+, Intel Arc)
//!
//! # Installation
//!
//! Add this to your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! pixelforge = "0.1"
//! ```
//!
//! ## Optional Features
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `dmabuf` | Enable DMA-BUF support for zero-copy image import from external processes (Linux only). Adds Vulkan extensions: `VK_KHR_external_memory`, `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`. |
//!
//! To enable DMA-BUF support:
//!
//! ```toml
//! [dependencies]
//! pixelforge = { version = "0.1", features = ["dmabuf"] }
//! ```
//!
//! # Quick Start
//!
//! ## Query Capabilities
//!
//! ```rust,no_run
//! use pixelforge::{Codec, VideoContextBuilder};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let context = VideoContextBuilder::new()
//!         .app_name("My App")
//!         .build()?;
//!
//!     for codec in [Codec::H264, Codec::H265, Codec::AV1] {
//!         println!("{:?}: encode={}",
//!             codec,
//!             context.supports_encode(codec)
//!         );
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Encoding Video
//!
//! ```rust,no_run
//! use pixelforge::{
//!     Codec, EncodeBitDepth, EncodeConfig, Encoder, InputImage, PixelFormat, RateControlMode,
//!     VideoContextBuilder,
//! };
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let context = VideoContextBuilder::new()
//!         .app_name("Encoder Example")
//!         .require_encode(Codec::H264)
//!         .build()?;
//!
//!     let config = EncodeConfig::h264(1920, 1080)
//!         .with_rate_control(RateControlMode::Vbr)
//!         .with_target_bitrate(5_000_000)
//!         .with_frame_rate(30, 1)
//!         .with_gop_size(60);
//!
//!     // Create an InputImage helper for uploading YUV data to the GPU.
//!     let mut input_image = InputImage::new(
//!         context.clone(),
//!         Codec::H264,
//!         1920,
//!         1080,
//!         EncodeBitDepth::Eight,
//!         PixelFormat::Yuv420,
//!     )?;
//!     let mut encoder = Encoder::new(context, config)?;
//!
//!     // For each frame: upload YUV data and encode.
//!     // let yuv_data: &[u8] = ...;  // YUV420 frame data
//!     // input_image.upload_yuv420(yuv_data)?;
//!     // let packets = encoder.encode(input_image.image())?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Color Conversion (RGB → YUV)
//!
//! PixelForge includes a GPU compute shader for converting RGB input to YUV output, supporting multiple color spaces:
//!
//! | Color Space | Description |
//! |-------------|-------------|
//! | `Bt709` | Standard SDR (BT.709 coefficients) |
//! | `Bt2020` | HDR passthrough (BT.2020 coefficients, PQ-encoded input) |
//! | `SrgbToBt2020Pq` | SDR-in-HDR (sRGB → linear → BT.2020 gamut → PQ OETF) |
//! | `Bt709LinearToBt2020Pq` | scRGB HDR (linear BT.709 → BT.2020 gamut → PQ OETF). `sdr_reference_white_nits` sets the interpretation of 1.0; per the scRGB spec (IEC 61966-2-2), 80 nits. |
//!
//! Supported input formats: BGRx, RGBx, BGRA, RGBA, ABGR2101010 (10-bit packed), RGBA16F (FP16).
//! Supported output formats: NV12 (8-bit), I420 (8-bit), YUV444 (8-bit), P010 (10-bit), YUV444P10 (10-bit).
//!
//! ```rust,no_run
//! use pixelforge::{ColorConverter, ColorConverterConfig, ColorSpace, InputFormat, OutputFormat, VideoContextBuilder};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let context = VideoContextBuilder::new()
//!     .app_name("Color Converter")
//!     .build()?;
//!
//! let mut config = ColorConverterConfig::new(1920, 1080, InputFormat::BGRx, OutputFormat::NV12);
//! config.color_space = ColorSpace::SrgbToBt2020Pq;
//!
//! let mut converter = ColorConverter::new(context.clone(), config)?;
//! // converter.convert(input_image, output_buffer)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Benchmarking
//!
//! Run the encode latency benchmark with:
//!
//! ```text
//! cargo run --example encode_bench
//! ```
//!
//! # Examples
//!
//! Run the examples with:
//!
//! ```text
//! # Query codec capabilities
//! cargo run --example query_capabilities
//!
//! # H.264 encoding example
//! cargo run --example encode_h264
//!
//! # H.265 encoding example
//! cargo run --example encode_h265
//!
//! # AV1 encoding example
//! cargo run --example encode_av1
//!
//! # Verify all codecs and formats
//! cargo run --example verify_all
//! ```
//!
//! # Shader Development
//!
//! The color conversion shader is precompiled to SPIR-V and embedded at build time.
//! See [shader/README.md](shader/README.md) for details on editing and recompiling shaders.
//!
//! # TODO's
//!
//! 1. [] Decoding.
//! 1. [] B-frames support.
//!
//! # Contributing
//!
//! Contributions are welcome! Please feel free to submit a Pull Request.
//!
//! # Acknowledgement
//!
//! This project was heavily inspired by the [vk_video_samples](https://github.com/nvpro-samples/vk_video_samples)
//! repository by NVIDIA, which provided invaluable reference for Vulkan Video encoding.

pub mod converter;
pub mod encoder;
pub mod error;
pub mod image;
pub mod vulkan;

/// Align a byte size up to a multiple of 4.
///
/// Required for `VkBufferImageCopy::bufferOffset` to meet the texel block alignment
/// of multi-component plane formats (e.g. R8G8, R16G16).
pub(crate) const fn align4(size: usize) -> usize {
    (size + 3) & !3
}

pub use converter::{ColorConverter, ColorConverterConfig, ColorSpace, InputFormat, OutputFormat};
pub use encoder::{
    BitDepth as EncodeBitDepth, Codec, ColorDescription, DEFAULT_FRAME_RATE, DEFAULT_GOP_SIZE,
    DEFAULT_H264_QP, DEFAULT_H265_QP, DEFAULT_MAX_BITRATE, DEFAULT_MAX_REFERENCE_FRAMES,
    DEFAULT_TARGET_BITRATE, EncodeConfig, EncodeContentHint, EncodeFuture, EncodeUsageHint,
    EncodedPacket, Encoder, EncoderTuningMode, FrameType, PixelFormat, RateControlMode,
};
pub use error::PixelForgeError;
pub use image::InputImage;
pub use vulkan::VideoContextBuilder;

/// Re-export VideoContext for convenience.
pub use vulkan::VideoContext;
