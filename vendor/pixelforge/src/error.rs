//! Error types for PixelForge.

use thiserror::Error;

/// Main error type for PixelForge operations.
#[derive(Error, Debug)]
pub enum PixelForgeError {
    /// Vulkan instance creation failed.
    #[error("Failed to create Vulkan instance: {0}")]
    InstanceCreation(String),

    /// Vulkan physical device selection failed.
    #[error("No suitable Vulkan physical device found: {0}")]
    NoSuitableDevice(String),

    /// Vulkan logical device creation failed.
    #[error("Failed to create Vulkan device: {0}")]
    DeviceCreation(String),

    /// Video session creation failed.
    #[error("Failed to create video session: {0}")]
    VideoSessionCreation(String),

    /// Video session parameters creation failed.
    #[error("Failed to create video session parameters: {0}")]
    SessionParametersCreation(String),

    /// Memory allocation failed.
    #[error("Memory allocation failed: {0}")]
    MemoryAllocation(String),

    /// Resource creation failed (images, buffers, fences, command pools, etc.).
    #[error("Failed to create resource: {0}")]
    ResourceCreation(String),

    /// Command buffer operation failed.
    #[error("Command buffer error: {0}")]
    CommandBuffer(String),

    /// Invalid input (dimensions, data size, format, etc.).
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Codec not supported.
    #[error("Codec not supported: {0}")]
    CodecNotSupported(String),

    /// Synchronization primitive error.
    #[error("Synchronization error: {0}")]
    Synchronization(String),

    /// Query pool error.
    #[error("Query pool error: {0}")]
    QueryPool(String),

    /// The encoded bitstream overflowed the destination buffer.
    #[error("Encoder bitstream buffer overflow (wrote {written} bytes into {capacity} bytes)")]
    BufferOverflow {
        /// Number of bytes the encoder reported writing.
        written: usize,
        /// Capacity of the bitstream buffer in bytes.
        capacity: usize,
    },

    /// Vulkan failure with the operation and original driver result preserved.
    #[error("{operation}: {result}")]
    VulkanOperation {
        /// Operation that failed; static to avoid allocating on failure paths.
        operation: &'static str,
        /// Original Vulkan result, independent of its display wording.
        result: ash::vk::Result,
    },

    /// Generic Vulkan error.
    #[error("Vulkan error: {0}")]
    Vulkan(ash::vk::Result),
}

impl From<ash::vk::Result> for PixelForgeError {
    fn from(result: ash::vk::Result) -> Self {
        PixelForgeError::Vulkan(result)
    }
}

/// Result type for PixelForge operations.
pub type Result<T> = std::result::Result<T, PixelForgeError>;

impl PixelForgeError {
    /// Wrap a driver failure without discarding its typed result.
    pub fn vulkan(operation: &'static str, result: ash::vk::Result) -> Self {
        Self::VulkanOperation { operation, result }
    }

    /// The original Vulkan result when this error came from the driver.
    /// Non-driver errors return `None`; their text is never parsed.
    pub fn vulkan_result(&self) -> Option<ash::vk::Result> {
        match self {
            Self::Vulkan(result) | Self::VulkanOperation { result, .. } => Some(*result),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk;

    #[test]
    fn preserves_driver_results_across_operations() {
        for operation in [
            "queue submission",
            "fence wait",
            "query readback",
            "session creation",
        ] {
            for result in [
                vk::Result::ERROR_DEVICE_LOST,
                vk::Result::TIMEOUT,
                vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
            ] {
                let error = PixelForgeError::vulkan(operation, result);
                assert_eq!(error.vulkan_result(), Some(result));
                assert!(error.to_string().contains(operation));
            }
        }
        assert_eq!(
            PixelForgeError::from(vk::Result::ERROR_DEVICE_LOST).vulkan_result(),
            Some(vk::Result::ERROR_DEVICE_LOST)
        );
    }

    #[test]
    fn error_text_never_becomes_a_driver_result() {
        assert_eq!(
            PixelForgeError::CommandBuffer("device has been lost".into()).vulkan_result(),
            None
        );
        assert_eq!(
            PixelForgeError::QueryPool("Encoder produced 0 bytes".into()).vulkan_result(),
            None
        );
    }
}
