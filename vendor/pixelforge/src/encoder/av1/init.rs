use super::{Av1, MIN_BITSTREAM_BUFFER_SIZE};

use crate::encoder::codec::{
    CodecEncoder, CommonInitRequest, build_encoder_common, query_video_caps,
};
use crate::encoder::{ColorDescription, EncodeConfig};
use crate::error::{PixelForgeError, Result};
use crate::vulkan::VideoContext;
use tracing::info;

impl Av1 {
    /// Create a new AV1 encoder.
    pub fn create(context: VideoContext, config: EncodeConfig) -> Result<CodecEncoder<Self>> {
        // Reachable from safe caller code via `with_b_frames`, so report it
        // rather than panicking inside the library.
        if config.b_frame_count != 0 {
            return Err(PixelForgeError::InvalidInput(format!(
                "AV1 encode: B-frames are not yet supported; set b_frame_count = 0 (got {})",
                config.b_frame_count
            )));
        }

        info!(
            "Creating AV1 encoder: {}x{}, pixel_format={:?}",
            config.dimensions.width, config.dimensions.height, config.pixel_format
        );

        let mut profile = super::super::capabilities::EncodeProfile::new(&config)?;
        let profile_info = profile.info();

        let bitstream_buffer_size = MIN_BITSTREAM_BUFFER_SIZE
            .max(config.dimensions.width as usize * config.dimensions.height as usize);

        let caps = query_video_caps(&context, &profile_info)?;

        let init = build_encoder_common(&CommonInitRequest {
            context: &context,
            config: &config,
            profile_info: &profile_info,
            caps: &caps,
            // AV1 has NUM_REF_FRAMES = 8 DPB slots, and `refresh_frame_flags`
            // is an 8-bit syntax element. `build_encoder_common` adds one setup
            // slot on top of the active count, so cap active refs at 7 to keep
            // every DPB slot index in 0..=7 (the range the [u8; 8] order-hint
            // array and `1 << slot` refresh mask can represent).
            max_active_refs_cap: 7,
            bitstream_buffer_size,
            // Allow the shared infrastructure to fall back to a layered DPB
            // when the driver does not support separate reference images
            // (required for AMD RADV).
            allow_layered_dpb: true,
        })?;
        let active_reference_count = init.active_reference_count;
        let common = init.common;
        let frame_extent = common
            .capabilities
            .av1_frame_extent(config.dimensions.width, config.dimensions.height)?;

        let codec = Av1 {
            frame_num: 0,
            order_hint: 0,
            header_data: None,
            references: Vec::new(),
            active_reference_count,
            frame_extent,
        };

        let mut encoder = CodecEncoder { common, codec };
        let color_desc = config
            .color_description
            .unwrap_or(ColorDescription::bt709());
        encoder.common.session_params = encoder
            .codec
            .build_session_params(&encoder.common, &color_desc)?;

        info!("AV1 encoder created successfully");
        Ok(encoder)
    }
}
