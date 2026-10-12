use crate::encoder::{BitDepth, PixelFormat};
use crate::error::{PixelForgeError, Result};
use crate::vulkan::VideoContext;
use ash::vk::TaggedStructure;
use ash::vk::{self, Handle};
use std::ptr;

/// Minimum bitstream buffer size.
pub(crate) const MIN_BITSTREAM_BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Compute greatest common divisor of two values.
pub(crate) fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let tmp = a % b;
        a = b;
        b = tmp;
    }
    a
}

/// Compute least common multiple of two values.
pub(crate) fn lcm(a: u32, b: u32) -> u32 {
    if a == 0 || b == 0 {
        0
    } else {
        (a / gcd(a, b)).saturating_mul(b)
    }
}

/// Align a value up to the next multiple of the given alignment.
pub(crate) fn align_up(value: u32, alignment: u32) -> u32 {
    if alignment <= 1 {
        value
    } else {
        value.div_ceil(alignment) * alignment
    }
}

pub(crate) fn query_supported_video_formats(
    context: &VideoContext,
    profile_info: &vk::VideoProfileInfoKHR,
    image_usage: vk::ImageUsageFlags,
) -> Result<Vec<vk::Format>> {
    let video_queue_fn = ash::khr::video_queue::Instance::load(context.entry(), context.instance());

    // Vulkan expects a profile list in the pNext chain.
    let profiles = [*profile_info];
    let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);

    let format_info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(image_usage)
        .push(&mut profile_list);

    let physical_device = context.physical_device();
    let mut count = 0u32;
    let result = unsafe {
        (video_queue_fn
            .fp()
            .get_physical_device_video_format_properties_khr)(
            physical_device,
            &format_info,
            &mut count,
            ptr::null_mut(),
        )
    };

    if result != vk::Result::SUCCESS {
        return Err(PixelForgeError::NoSuitableDevice(format!(
            "Failed to query video format properties for usage {:?}: {:?}",
            image_usage, result
        )));
    }

    if count == 0 {
        return Ok(Vec::new());
    }

    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let result = unsafe {
        (video_queue_fn
            .fp()
            .get_physical_device_video_format_properties_khr)(
            physical_device,
            &format_info,
            &mut count,
            props.as_mut_ptr(),
        )
    };

    if result != vk::Result::SUCCESS {
        return Err(PixelForgeError::NoSuitableDevice(format!(
            "Failed to enumerate video format properties for usage {:?}: {:?}",
            image_usage, result
        )));
    }

    props.truncate(count as usize);
    Ok(props.into_iter().map(|p| p.format).collect())
}

/// Get the Vulkan format for a given pixel format and bit depth.
///
/// Supports YUV420 and YUV444 in 8-bit and 10-bit.
/// For YUV444, uses 2-plane (semi-planar) formats from VK_EXT_ycbcr_2plane_444_formats
/// which are supported by NVIDIA hardware for video encoding.
pub(crate) fn get_video_format(pixel_format: PixelFormat, bit_depth: BitDepth) -> vk::Format {
    match (pixel_format, bit_depth) {
        (PixelFormat::Yuv420, BitDepth::Eight) => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        (PixelFormat::Yuv420, BitDepth::Ten) => {
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
        }
        // Use 2-plane semi-planar formats for YUV444 (supported by NVIDIA for video encoding).
        (PixelFormat::Yuv444, BitDepth::Eight) => vk::Format::G8_B8R8_2PLANE_444_UNORM,
        (PixelFormat::Yuv444, BitDepth::Ten) => {
            vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16
        }
        // TODO: Add support for YUV422 formats.
        _ => unimplemented!(
            "Unsupported pixel format / bit depth combination: {:?} / {:?}",
            pixel_format,
            bit_depth
        ),
    }
}

/// Create a buffer that requires device addresses (SHADER_DEVICE_ADDRESS usage).
///
/// This allocates memory with `VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT` so that
/// `get_buffer_device_address` returns a valid address.
pub(crate) fn create_buffer_with_device_address(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
    properties: vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(|e| PixelForgeError::ResourceCreation(format!("buffer creation: {}", e)))?;

    let mem_requirements = unsafe { device.get_buffer_memory_requirements(buffer) };

    let memory_type_index = find_memory_type(
        memory_properties,
        mem_requirements.memory_type_bits,
        properties,
    )
    .ok_or_else(|| {
        PixelForgeError::MemoryAllocation(format!(
            "No suitable memory type for buffer with properties {:?}",
            properties
        ))
    })?;

    let mut alloc_flags_info =
        vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_requirements.size)
        .memory_type_index(memory_type_index)
        .push(&mut alloc_flags_info);

    let memory = match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(PixelForgeError::MemoryAllocation(e.to_string()));
        }
    };

    match unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        Ok(()) => Ok((buffer, memory)),
        Err(e) => {
            unsafe {
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
            }
            Err(PixelForgeError::MemoryAllocation(e.to_string()))
        }
    }
}

pub(crate) fn find_memory_type(
    memory_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    properties: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..memory_props.memory_type_count).find(|&i| {
        (type_filter & (1 << i)) != 0
            && memory_props.memory_types[i as usize]
                .property_flags
                .contains(properties)
    })
}

pub(crate) fn create_bitstream_buffer(
    context: &VideoContext,
    size: usize,
    profile_info: &vk::VideoProfileInfoKHR,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let profiles = [*profile_info];
    let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);

    let create_info = vk::BufferCreateInfo::default()
        .size(size as vk::DeviceSize)
        .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .push(&mut profile_list);

    let buffer = unsafe { context.device().create_buffer(&create_info, None) }
        .map_err(|e| PixelForgeError::ResourceCreation(format!("buffer creation: {}", e)))?;

    let mem_requirements = unsafe { context.device().get_buffer_memory_requirements(buffer) };

    let memory_type_index = find_memory_type(
        context.memory_properties(),
        mem_requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or_else(|| {
        PixelForgeError::MemoryAllocation("No suitable memory type for buffer".to_string())
    })?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_requirements.size)
        .memory_type_index(memory_type_index);

    let memory = unsafe { context.device().allocate_memory(&alloc_info, None) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    unsafe { context.device().bind_buffer_memory(buffer, memory, 0) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    Ok((buffer, memory))
}

pub(crate) fn create_timeline_semaphore(context: &VideoContext) -> Result<vk::Semaphore> {
    let mut type_info = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let create_info = vk::SemaphoreCreateInfo::default().push(&mut type_info);

    unsafe { context.device().create_semaphore(&create_info, None) }
        .map_err(|e| PixelForgeError::Synchronization(e.to_string()))
}

pub(crate) fn create_encode_feedback_query_pool(
    context: &VideoContext,
    profile_info: &mut vk::VideoProfileInfoKHR,
) -> Result<vk::QueryPool> {
    let mut encode_feedback_create = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
        .encode_feedback_flags(
            vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
        );

    let query_pool_create_info = unsafe {
        vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
            .query_count(1)
            .extend(profile_info)
            .push(&mut encode_feedback_create)
    };

    unsafe {
        context
            .device()
            .create_query_pool(&query_pool_create_info, None)
    }
    .map_err(|e| PixelForgeError::QueryPool(e.to_string()))
}

pub(crate) fn create_encode_timestamp_query_pool(context: &VideoContext) -> Result<vk::QueryPool> {
    let timestamp_query_pool_create_info = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::TIMESTAMP)
        .query_count(2); // start and end

    unsafe {
        context
            .device()
            .create_query_pool(&timestamp_query_pool_create_info, None)
    }
    .map_err(|e| PixelForgeError::QueryPool(e.to_string()))
}

/// Create an image for video encoding (input or DPB).
///
/// This creates a VkImage suitable for use with a video encoder.
/// For DPB images, the usage is VIDEO_ENCODE_DPB_KHR.
/// For input images, the usage is VIDEO_ENCODE_SRC_KHR | TRANSFER_DST.
///
/// # Arguments
/// * `context` - The Vulkan video context
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `format` - The Vulkan format to use for the image
/// * `is_dpb` - If true, create a DPB image; if false, create an input image
/// * `profile_info` - Video profile info for the encoder session
pub(crate) fn create_image(
    context: &VideoContext,
    width: u32,
    height: u32,
    format: vk::Format,
    is_dpb: bool,
    profile_info: &vk::VideoProfileInfoKHR,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let usage = if is_dpb {
        vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR
    } else {
        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST
    };

    // For input (non-DPB) images, use CONCURRENT sharing mode when multiple
    // queue families need access. The image may be accessed by:
    // - The video encode queue (for encoding)
    // - The transfer queue (for InputImage upload)
    // - The compute queue (for ColorConverter buffer-to-image copy)
    let mut queue_families = Vec::new();
    let sharing_mode = if !is_dpb {
        if let Some(encode_family) = context.video_encode_queue_family() {
            queue_families.push(encode_family);
            let transfer_family = context.transfer_queue_family();
            if !queue_families.contains(&transfer_family) {
                queue_families.push(transfer_family);
            }
            let compute_family = context.compute_queue_family();
            if !queue_families.contains(&compute_family) {
                queue_families.push(compute_family);
            }
        }
        if queue_families.len() > 1 {
            vk::SharingMode::CONCURRENT
        } else {
            queue_families.clear();
            vk::SharingMode::EXCLUSIVE
        }
    } else {
        vk::SharingMode::EXCLUSIVE
    };

    let profiles = [*profile_info];
    let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);

    let create_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .sharing_mode(sharing_mode)
        .queue_family_indices(&queue_families)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push(&mut profile_list);

    let image = unsafe { context.device().create_image(&create_info, None) }
        .map_err(|e| PixelForgeError::ResourceCreation(format!("image creation: {}", e)))?;

    let mem_requirements = unsafe { context.device().get_image_memory_requirements(image) };

    let memory_type_index = find_memory_type(
        context.memory_properties(),
        mem_requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .ok_or_else(|| {
        PixelForgeError::MemoryAllocation("No suitable memory type for image".to_string())
    })?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_requirements.size)
        .memory_type_index(memory_type_index);

    let memory = unsafe { context.device().allocate_memory(&alloc_info, None) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    unsafe { context.device().bind_image_memory(image, memory, 0) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    let view_create_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::IDENTITY,
        })
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });

    let view = unsafe { context.device().create_image_view(&view_create_info, None) }
        .map_err(|e| PixelForgeError::ResourceCreation(format!("image view creation: {}", e)))?;

    Ok((image, memory, view))
}
/// Allocate and bind memory for a video session.
///
/// Returns the allocated device memory handles.
pub(crate) fn allocate_session_memory(
    context: &VideoContext,
    session: vk::VideoSessionKHR,
    video_queue_fn: &ash::khr::video_queue::Device,
) -> Result<Vec<vk::DeviceMemory>> {
    // Query memory requirements count.
    let mut memory_requirements_count = 0u32;
    let result = unsafe {
        (video_queue_fn
            .fp()
            .get_video_session_memory_requirements_khr)(
            context.device().handle(),
            session,
            &mut memory_requirements_count,
            ptr::null_mut(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(PixelForgeError::MemoryAllocation(format!("{:?}", result)));
    }

    // Query actual requirements.
    let mut memory_requirements =
        vec![vk::VideoSessionMemoryRequirementsKHR::default(); memory_requirements_count as usize];
    let result = unsafe {
        (video_queue_fn
            .fp()
            .get_video_session_memory_requirements_khr)(
            context.device().handle(),
            session,
            &mut memory_requirements_count,
            memory_requirements.as_mut_ptr(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(PixelForgeError::MemoryAllocation(format!("{:?}", result)));
    }

    // Allocate and bind memory for each requirement.
    let mut session_memory = Vec::new();
    let mut bind_infos = Vec::new();

    for req in &memory_requirements {
        let memory_type_index = find_memory_type(
            context.memory_properties(),
            req.memory_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            find_memory_type(
                context.memory_properties(),
                req.memory_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        })
        .ok_or_else(|| {
            PixelForgeError::MemoryAllocation(format!(
                "No suitable memory type for video session (type_bits: 0x{:x})",
                req.memory_requirements.memory_type_bits
            ))
        })?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(req.memory_requirements.size)
            .memory_type_index(memory_type_index);

        let memory = unsafe { context.device().allocate_memory(&alloc_info, None) }
            .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

        bind_infos.push(
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(req.memory_bind_index)
                .memory(memory)
                .memory_offset(0)
                .memory_size(req.memory_requirements.size),
        );

        session_memory.push(memory);
    }

    // Bind all memory to the session.
    let result = unsafe {
        (video_queue_fn.fp().bind_video_session_memory_khr)(
            context.device().handle(),
            session,
            bind_infos.len() as u32,
            bind_infos.as_ptr(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(PixelForgeError::MemoryAllocation(format!("{:?}", result)));
    }

    Ok(session_memory)
}

/// Command resources for encoding operations.
pub(crate) struct CommandResources {
    /// Command pool for encode commands.
    pub command_pool: vk::CommandPool,
    /// Command pool for upload/transfer commands (may differ from command_pool when
    /// the encode queue does not support transfer operations).
    pub upload_command_pool: vk::CommandPool,
    /// Command buffer for upload operations.
    pub upload_command_buffer: vk::CommandBuffer,
    /// Fence for upload synchronization.
    pub upload_fence: vk::Fence,
}

/// Create command resources for encoding.
///
/// `encode_queue_family` is the queue family used for video encode commands.
/// `upload_queue_family` is the queue family used for transfer (upload) commands.
/// They may be the same if the encode queue supports transfer operations.
pub(crate) fn create_command_resources(
    context: &VideoContext,
    encode_queue_family: u32,
    upload_queue_family: u32,
) -> Result<CommandResources> {
    // Create command pool for encode commands.
    let pool_create_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(encode_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);

    let command_pool = unsafe {
        context
            .device()
            .create_command_pool(&pool_create_info, None)
    }
    .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    // Create command pool for upload commands (may be the same family).
    let upload_command_pool = if upload_queue_family == encode_queue_family {
        command_pool
    } else {
        let upload_pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(upload_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        unsafe {
            context
                .device()
                .create_command_pool(&upload_pool_info, None)
        }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?
    };

    // Allocate upload command buffer from the upload pool.
    let upload_alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(upload_command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);

    let upload_command_buffers = unsafe {
        context
            .device()
            .allocate_command_buffers(&upload_alloc_info)
    }
    .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;
    let upload_command_buffer = upload_command_buffers[0];

    // Upload fence is created unsignaled. Per-slot encode fences are created by
    // the encode pipeline (see `encoder::pipeline`).
    let fence_create_info = vk::FenceCreateInfo::default();
    let upload_fence = unsafe { context.device().create_fence(&fence_create_info, None) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    Ok(CommandResources {
        command_pool,
        upload_command_pool,
        upload_command_buffer,
        upload_fence,
    })
}

/// Create DPB images for video encoding.
///
/// When `use_layered` is true (required when the driver does not support
/// `VK_VIDEO_CAPABILITY_SEPARATE_REFERENCE_IMAGES_BIT_KHR`), a single
/// `VkImage` with `array_layers = count` is created and one `VkImageView`
/// per layer is returned.  The image and memory vectors will have a single
/// entry while the view vector will have `count` entries.
///
/// When `use_layered` is false the previous behaviour is preserved: one
/// separate image/memory/view per DPB slot.
pub(crate) fn create_dpb_images(
    context: &VideoContext,
    width: u32,
    height: u32,
    format: vk::Format,
    count: usize,
    profile_info: &vk::VideoProfileInfoKHR,
    use_layered: bool,
) -> Result<(Vec<vk::Image>, Vec<vk::DeviceMemory>, Vec<vk::ImageView>)> {
    if use_layered {
        // Create a single image with `count` array layers.
        let profiles = [*profile_info];
        let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);

        let create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(count as u32)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push(&mut profile_list);

        let image = unsafe { context.device().create_image(&create_info, None) }
            .map_err(|e| PixelForgeError::ResourceCreation(format!("layered DPB image: {}", e)))?;

        let mem_requirements = unsafe { context.device().get_image_memory_requirements(image) };

        let memory_type_index = find_memory_type(
            context.memory_properties(),
            mem_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            PixelForgeError::MemoryAllocation(
                "No suitable memory type for layered DPB image".to_string(),
            )
        })?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index);

        let memory = unsafe { context.device().allocate_memory(&alloc_info, None) }
            .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

        unsafe { context.device().bind_image_memory(image, memory, 0) }
            .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

        // Create one view per array layer.
        let mut dpb_image_views = Vec::with_capacity(count);
        for layer in 0..count as u32 {
            let view_create_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .components(vk::ComponentMapping {
                    r: vk::ComponentSwizzle::IDENTITY,
                    g: vk::ComponentSwizzle::IDENTITY,
                    b: vk::ComponentSwizzle::IDENTITY,
                    a: vk::ComponentSwizzle::IDENTITY,
                })
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: layer,
                    layer_count: 1,
                });

            let view = unsafe { context.device().create_image_view(&view_create_info, None) }
                .map_err(|e| {
                    PixelForgeError::ResourceCreation(format!(
                        "layered DPB image view layer {}: {}",
                        layer, e
                    ))
                })?;
            dpb_image_views.push(view);
        }

        Ok((vec![image], vec![memory], dpb_image_views))
    } else {
        let mut dpb_images = Vec::with_capacity(count);
        let mut dpb_image_memories = Vec::with_capacity(count);
        let mut dpb_image_views = Vec::with_capacity(count);

        for _ in 0..count {
            let (dpb_image, dpb_image_memory, dpb_image_view) =
                create_image(context, width, height, format, true, profile_info)?;
            dpb_images.push(dpb_image);
            dpb_image_memories.push(dpb_image_memory);
            dpb_image_views.push(dpb_image_view);
        }

        Ok((dpb_images, dpb_image_memories, dpb_image_views))
    }
}

/// Map a bitstream buffer for persistent access.
pub(crate) fn map_bitstream_buffer(
    context: &VideoContext,
    memory: vk::DeviceMemory,
    size: usize,
) -> Result<*mut u8> {
    let ptr = unsafe {
        context.device().map_memory(
            memory,
            0,
            size as vk::DeviceSize,
            vk::MemoryMapFlags::empty(),
        )
    }
    .map_err(|e| {
        PixelForgeError::MemoryAllocation(format!("Failed to map bitstream buffer: {}", e))
    })? as *mut u8;

    Ok(ptr)
}

/// Parameters for clearing the input image at initialization.
pub(crate) struct ClearImageParams {
    pub command_buffer: vk::CommandBuffer,
    pub fence: vk::Fence,
    pub queue: vk::Queue,
    pub image: vk::Image,
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub bit_depth: BitDepth,
}

/// Clear the input image by filling it with zeros via a staging buffer.
///
/// This must be called once after creating the input image to ensure
/// the padding region (between the user dimensions and the aligned coded
/// extent) contains defined values. Without this, the first frame's
/// padding is undefined, which can cause encoding artifacts on strict
/// drivers.
pub(crate) fn clear_input_image(context: &VideoContext, params: &ClearImageParams) -> Result<()> {
    let device = context.device();
    let bytes_per_component: u32 = match params.bit_depth {
        BitDepth::Eight => 1,
        BitDepth::Ten => 2,
    };

    // Calculate per-plane sizes.
    // For YUV444, align Y plane size to 4 bytes so the UV plane buffer offset
    // meets VkBufferImageCopy::bufferOffset alignment requirements.
    // YUV420/422 dimensions are always even, so alignment is naturally satisfied.
    let plane0_raw = (params.width * params.height * bytes_per_component) as usize;
    let plane0_size = match params.pixel_format {
        PixelFormat::Yuv444 => crate::align4(plane0_raw) as u32,
        _ => plane0_raw as u32,
    };
    let plane1_size = match params.pixel_format {
        // YUV 4:2:0 (e.g., NV12): UV plane is half width, half height, 2 components per pixel.
        PixelFormat::Yuv420 => (params.width / 2) * (params.height / 2) * 2 * bytes_per_component,
        // YUV 4:2:2: UV plane is half width, full height, 2 components per pixel.
        PixelFormat::Yuv422 => (params.width / 2) * params.height * 2 * bytes_per_component,
        // YUV 4:4:4 (e.g., NV24): UV plane is full width, full height, 2 components per pixel.
        PixelFormat::Yuv444 => params.width * params.height * 2 * bytes_per_component,
    };
    let total_size = (plane0_size + plane1_size) as vk::DeviceSize;

    // Create a staging buffer filled with zeros.
    let buffer_create_info = vk::BufferCreateInfo::default()
        .size(total_size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let staging_buffer = unsafe { device.create_buffer(&buffer_create_info, None) }
        .map_err(|e| PixelForgeError::ResourceCreation(format!("staging buffer: {}", e)))?;

    let mem_requirements = unsafe { device.get_buffer_memory_requirements(staging_buffer) };
    let memory_type_index = find_memory_type(
        context.memory_properties(),
        mem_requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or_else(|| {
        PixelForgeError::MemoryAllocation("No suitable memory type for staging buffer".to_string())
    })?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_requirements.size)
        .memory_type_index(memory_type_index);

    let staging_memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    unsafe { device.bind_buffer_memory(staging_buffer, staging_memory, 0) }
        .map_err(|e| PixelForgeError::MemoryAllocation(e.to_string()))?;

    // Map and zero-fill.
    let data_ptr =
        unsafe { device.map_memory(staging_memory, 0, total_size, vk::MemoryMapFlags::empty()) }
            .map_err(|e| PixelForgeError::MemoryAllocation(format!("map staging buffer: {}", e)))?;
    unsafe { ptr::write_bytes(data_ptr as *mut u8, 0, total_size as usize) };
    unsafe { device.unmap_memory(staging_memory) };

    // Record commands.
    unsafe {
        device.reset_command_buffer(params.command_buffer, vk::CommandBufferResetFlags::empty())
    }
    .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { device.begin_command_buffer(params.command_buffer, &begin_info) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    // Transition image from UNDEFINED to TRANSFER_DST.
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

    unsafe {
        device.cmd_pipeline_barrier(
            params.command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }

    // Copy from staging buffer to image planes.
    let (uv_width, uv_height) = match params.pixel_format {
        PixelFormat::Yuv420 => (params.width / 2, params.height / 2),
        PixelFormat::Yuv444 => (params.width, params.height),
        _ => (params.width / 2, params.height / 2),
    };

    let copy_regions = [
        vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::PLANE_0,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
            image_extent: vk::Extent3D {
                width: params.width,
                height: params.height,
                depth: 1,
            },
        },
        vk::BufferImageCopy {
            buffer_offset: plane0_size as vk::DeviceSize,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::PLANE_1,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
            image_extent: vk::Extent3D {
                width: uv_width,
                height: uv_height,
                depth: 1,
            },
        },
    ];

    unsafe {
        device.cmd_copy_buffer_to_image(
            params.command_buffer,
            staging_buffer,
            params.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &copy_regions,
        );
    }

    // Transition image to VIDEO_ENCODE_SRC.
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::empty());

    unsafe {
        device.cmd_pipeline_barrier(
            params.command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }

    unsafe { device.end_command_buffer(params.command_buffer) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    // Submit and wait.
    let submit_info =
        vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&params.command_buffer));
    unsafe { device.reset_fences(&[params.fence]) }
        .map_err(|e| PixelForgeError::CommandBuffer(format!("reset fence: {}", e)))?;
    unsafe { device.queue_submit(params.queue, &[submit_info], params.fence) }
        .map_err(|e| PixelForgeError::CommandBuffer(format!("submit clear: {}", e)))?;
    unsafe { device.wait_for_fences(&[params.fence], true, u64::MAX) }
        .map_err(|e| PixelForgeError::CommandBuffer(format!("wait clear: {}", e)))?;
    unsafe { device.reset_fences(&[params.fence]) }
        .map_err(|e| PixelForgeError::CommandBuffer(format!("reset fence after clear: {}", e)))?;

    // Clean up staging buffer.
    unsafe {
        device.destroy_buffer(staging_buffer, None);
        device.free_memory(staging_memory, None);
    }

    Ok(())
}

/// Parameters for uploading an image to the encoder's input image.
pub(crate) struct UploadParams {
    /// The command buffer to use for the upload.
    pub upload_command_buffer: vk::CommandBuffer,
    /// The fence to use for synchronization.
    pub upload_fence: vk::Fence,
    /// The source image to copy from.
    pub src_image: vk::Image,
    /// The destination image to copy to.
    pub dst_image: vk::Image,
    /// The width of the image.
    pub width: u32,
    /// The height of the image.
    pub height: u32,
    /// The pixel format of the image.
    pub pixel_format: PixelFormat,
    /// The current layout of the input image.
    pub input_image_layout: vk::ImageLayout,
    /// The queue to submit transfer operations to.
    pub upload_queue: vk::Queue,
}

/// Upload an image to the encoder's input image via GPU-to-GPU copy.
///
/// This function handles:
/// - Resetting and beginning the command buffer
/// - Transitioning source image from GENERAL to TRANSFER_SRC
/// - Transitioning destination image from `input_image_layout` to TRANSFER_DST
/// - Copying Y and UV planes (NV12 format)
/// - Transitioning destination image to VIDEO_ENCODE_SRC
/// - Transitioning source image back to GENERAL
/// - Submitting the command buffer and waiting for completion
///
/// Returns Ok(()) on success, or an error if any Vulkan operation fails.
pub(crate) fn upload_image_to_input(
    context: &crate::vulkan::VideoContext,
    params: &UploadParams,
) -> Result<()> {
    let device = context.device();

    unsafe {
        device.reset_command_buffer(
            params.upload_command_buffer,
            vk::CommandBufferResetFlags::empty(),
        )
    }
    .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { device.begin_command_buffer(params.upload_command_buffer, &begin_info) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    // Transition source image from GENERAL to TRANSFER_SRC.
    let src_barrier = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.src_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ);

    // Transition destination image to TRANSFER_DST.
    let dst_barrier = vk::ImageMemoryBarrier::default()
        .old_layout(params.input_image_layout)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.dst_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

    unsafe {
        device.cmd_pipeline_barrier(
            params.upload_command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[src_barrier, dst_barrier],
        );
    }

    // Copy image to image using per-plane copy regions (NV12 format).
    // Copy Y plane (plane 0).
    let y_copy_region = vk::ImageCopy {
        src_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::PLANE_0,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        src_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        dst_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::PLANE_0,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        dst_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        extent: vk::Extent3D {
            width: params.width,
            height: params.height,
            depth: 1,
        },
    };

    // Copy UV plane (plane 1).
    let (uv_width, uv_height) = match params.pixel_format {
        PixelFormat::Yuv420 => (params.width / 2, params.height / 2),
        PixelFormat::Yuv444 => (params.width, params.height),
        _ => (params.width / 2, params.height / 2),
    };

    let uv_copy_region = vk::ImageCopy {
        src_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::PLANE_1,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        src_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        dst_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::PLANE_1,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        dst_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        extent: vk::Extent3D {
            width: uv_width,
            height: uv_height,
            depth: 1,
        },
    };

    unsafe {
        device.cmd_copy_image(
            params.upload_command_buffer,
            params.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            params.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[y_copy_region, uv_copy_region],
        );
    }

    // Transition destination image to VIDEO_ENCODE_SRC.
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.dst_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::empty());

    // Also transition source image back to GENERAL for reuse.
    let src_barrier_back = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(params.src_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::TRANSFER_READ)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);

    unsafe {
        device.cmd_pipeline_barrier(
            params.upload_command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier, src_barrier_back],
        );
    }

    unsafe { device.end_command_buffer(params.upload_command_buffer) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    let submit_info = vk::SubmitInfo::default()
        .command_buffers(std::slice::from_ref(&params.upload_command_buffer));

    unsafe { device.queue_submit(params.upload_queue, &[submit_info], params.upload_fence) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    unsafe { device.wait_for_fences(&[params.upload_fence], true, u64::MAX) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    unsafe { device.reset_fences(&[params.upload_fence]) }
        .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

    Ok(())
}

/// Record DPB image barriers for encode.
///
/// Transitions the setup DPB slot from UNDEFINED to VIDEO_ENCODE_DPB and
/// adds execution barriers for reference slot images.
///
/// # Safety
///
/// The command buffer must be in recording state.
pub(crate) unsafe fn record_dpb_barriers(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    dpb_images: &[vk::Image],
    use_layered_dpb: bool,
    current_dpb_slot: u8,
    reference_dpb_slots: &[u8],
    setup_slot_active: bool,
) {
    let dpb_image = if use_layered_dpb {
        dpb_images[0]
    } else {
        dpb_images[current_dpb_slot as usize]
    };
    let dpb_base_array_layer = if use_layered_dpb {
        current_dpb_slot as u32
    } else {
        0
    };

    // Use UNDEFINED only on first use of a DPB slot; after that it is already
    // in VIDEO_ENCODE_DPB_KHR and transitioning from UNDEFINED would discard
    // the contents, which is invalid/UB.
    let setup_old_layout = if setup_slot_active {
        vk::ImageLayout::VIDEO_ENCODE_DPB_KHR
    } else {
        vk::ImageLayout::UNDEFINED
    };

    let dpb_barrier = vk::ImageMemoryBarrier::default()
        .old_layout(setup_old_layout)
        .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(dpb_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: dpb_base_array_layer,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::empty());

    let mut all_barriers = vec![dpb_barrier];

    for &ref_slot in reference_dpb_slots {
        let (ref_image, ref_layer) = if use_layered_dpb {
            (dpb_images[0], ref_slot as u32)
        } else {
            (dpb_images[ref_slot as usize], 0u32)
        };
        all_barriers.push(
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(ref_image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: ref_layer,
                    layer_count: 1,
                })
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ),
        );
    }

    unsafe {
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &all_barriers,
        );
    }
}

/// Prepare an encode command buffer for recording.
///
/// Resets the command buffer, begins recording with ONE_TIME_SUBMIT, and resets
/// the query pool. This is the common preamble for all encode operations.
///
/// # Safety
///
/// The command buffer must not be in use by the GPU.
pub(crate) unsafe fn prepare_encode_command_buffer(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    query_pool: vk::QueryPool,
) -> Result<()> {
    unsafe {
        device
            .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
            .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;
    }

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe {
        device
            .begin_command_buffer(command_buffer, &begin_info)
            .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;
    }

    unsafe {
        device.cmd_reset_query_pool(command_buffer, query_pool, 0, 1);
    }

    Ok(())
}

/// Record a post-encode DPB synchronization barrier.
///
/// Ensures the DPB image write from the encode operation is visible to subsequent
/// reads (e.g. as a reference frame for the next encode).
///
/// # Safety
///
/// The command buffer must be in recording state.
pub(crate) unsafe fn record_post_encode_dpb_barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    dpb_images: &[vk::Image],
    use_layered_dpb: bool,
    current_dpb_slot: u8,
) {
    let (post_dpb_image, post_dpb_layer) = if use_layered_dpb {
        (dpb_images[0], current_dpb_slot as u32)
    } else {
        (dpb_images[current_dpb_slot as usize], 0)
    };

    let dpb_sync_barrier = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(post_dpb_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: post_dpb_layer,
            layer_count: 1,
        })
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ);

    unsafe {
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[dpb_sync_barrier],
        );
    }
}

/// Submit an encode command buffer without waiting for completion.
///
/// Timeline semaphore waits/signals are used to keep shared DPB access ordered
/// across pipelined slots while leaving bitstream readback delayed. Uses
/// Synchronization2 (`queue_submit2`) so the timeline wait/signal are scoped to
/// the `VIDEO_ENCODE` stage rather than `ALL_COMMANDS`, avoiding unnecessary
/// over-synchronization.
///
/// # Safety
///
/// The command buffer must have been ended.
pub(crate) unsafe fn submit_encode_only(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    encode_queue: vk::Queue,
    wait_timeline: Option<(vk::Semaphore, u64)>,
    signal_timeline: Option<(vk::Semaphore, u64)>,
) -> Result<()> {
    let command_buffer_info = vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer);
    let command_buffer_infos = [command_buffer_info];

    let wait_infos: Vec<vk::SemaphoreSubmitInfo> = wait_timeline
        .into_iter()
        .map(|(semaphore, value)| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
        })
        .collect();
    let signal_infos: Vec<vk::SemaphoreSubmitInfo> = signal_timeline
        .into_iter()
        .map(|(semaphore, value)| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
        })
        .collect();

    let submit_info = vk::SubmitInfo2::default()
        .wait_semaphore_infos(&wait_infos)
        .command_buffer_infos(&command_buffer_infos)
        .signal_semaphore_infos(&signal_infos);

    unsafe {
        device
            .reset_fences(&[fence])
            .map_err(|e| PixelForgeError::Synchronization(e.to_string()))?;
    }

    unsafe {
        device
            .queue_submit2(encode_queue, &[submit_info], fence)
            .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;
    }

    Ok(())
}

/// Wait for a submitted encode and copy its bitstream data.
///
/// # Safety
///
/// The bitstream buffer pointer must be valid and persistently mapped.
/// Wait for the encode to finish, then append its bitstream directly onto `dst`
/// (which already holds any codec header). Appending in place means the encoded
/// bytes are copied out of the mapped buffer exactly once — there is no
/// intermediate owned `Vec`.
///
/// `bitstream_buffer_size` is the mapped buffer's capacity; if the reported
/// `bytes_written` (+ `offset`) exceeds it, return [`PixelForgeError::BufferOverflow`]
/// instead of reading out of bounds.
pub(crate) unsafe fn wait_and_read_bitstream(
    device: &ash::Device,
    fence: vk::Fence,
    query_pool: vk::QueryPool,
    bitstream_buffer_ptr: *const u8,
    bitstream_buffer_size: usize,
    dst: &mut Vec<u8>,
) -> Result<()> {
    unsafe {
        device
            .wait_for_fences(&[fence], true, u64::MAX)
            .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;
    }

    // Read query results (offset + bytes_written).
    #[repr(C)]
    struct QueryResult {
        offset: u32,
        bytes_written: u32,
    }

    let mut query_results = [QueryResult {
        offset: 0,
        bytes_written: 0,
    }];

    unsafe {
        device
            .get_query_pool_results(
                query_pool,
                0,
                &mut query_results,
                vk::QueryResultFlags::WAIT,
            )
            .map_err(|e| PixelForgeError::QueryPool(e.to_string()))?;
    }

    let offset = query_results[0].offset as usize;
    let size = query_results[0].bytes_written as usize;

    if size == 0 {
        return Err(PixelForgeError::QueryPool(
            "Encoder produced 0 bytes".to_string(),
        ));
    }

    tracing::debug!("Encoded frame: offset={}, size={}", offset, size);

    // Guard against writing past the destination range: `bytes_written > capacity` is the only overflow signal.
    if offset.saturating_add(size) > bitstream_buffer_size {
        return Err(PixelForgeError::BufferOverflow {
            written: offset + size,
            capacity: bitstream_buffer_size,
        });
    }

    let src = unsafe { std::slice::from_raw_parts(bitstream_buffer_ptr.add(offset), size) };
    dst.extend_from_slice(src);

    Ok(())
}

/// The per-codec Vulkan resources torn down on `Drop`, other than the encode
/// pipeline (which the engine owns and destroys). Bundled so the identical
/// teardown lives in one place; see [`destroy_encoder_resources`].
pub(crate) struct EncoderTeardown<'a> {
    pub command_pool: vk::CommandPool,
    pub upload_command_pool: vk::CommandPool,
    pub upload_fence: vk::Fence,
    pub dpb_images: &'a [vk::Image],
    pub dpb_image_views: &'a [vk::ImageView],
    pub dpb_image_memories: &'a [vk::DeviceMemory],
    pub session: vk::VideoSessionKHR,
    pub session_params: vk::VideoSessionParametersKHR,
    pub session_memory: &'a [vk::DeviceMemory],
}

/// Destroy the common encoder resources (command pools, upload fence, DPB
/// images, and the video session). Identical across codecs.
///
/// # Safety
///
/// All queues that may reference these resources must be idle.
pub(crate) unsafe fn destroy_encoder_resources(
    device: &ash::Device,
    video_queue_fn: &ash::khr::video_queue::Device,
    res: &EncoderTeardown,
) {
    unsafe {
        device.destroy_fence(res.upload_fence, None);
        device.destroy_command_pool(res.command_pool, None);
        if res.upload_command_pool != res.command_pool {
            device.destroy_command_pool(res.upload_command_pool, None);
        }

        for &view in res.dpb_image_views {
            device.destroy_image_view(view, None);
        }
        for &image in res.dpb_images {
            device.destroy_image(image, None);
        }
        for &memory in res.dpb_image_memories {
            device.free_memory(memory, None);
        }

        if res.session_params != vk::VideoSessionParametersKHR::null() {
            (video_queue_fn.fp().destroy_video_session_parameters_khr)(
                device.handle(),
                res.session_params,
                std::ptr::null(),
            );
        }
        (video_queue_fn.fp().destroy_video_session_khr)(
            device.handle(),
            res.session,
            std::ptr::null(),
        );
        for &memory in res.session_memory {
            device.free_memory(memory, None);
        }
    }
}

/// Retrieve driver-encoded video session parameters (H.264 SPS/PPS, H.265
/// VPS/SPS/PPS, or AV1 sequence header) into a byte buffer, retrying on
/// `INCOMPLETE`.
///
/// The codec-specific `*GetInfoKHR` and feedback structs are built by the caller
/// and chained into `get_info`/`feedback`; this owns only the identical query
/// loop. A preallocated buffer is always provided because some drivers misbehave
/// on a size-only query (`pData == NULL`), notably for 4:4:4 profiles.
pub(crate) fn get_encoded_session_params(
    context: &VideoContext,
    video_encode_fn: &ash::khr::video_encode_queue::Device,
    get_info: &vk::VideoEncodeSessionParametersGetInfoKHR,
    feedback: &mut vk::VideoEncodeSessionParametersFeedbackInfoKHR,
) -> Result<Vec<u8>> {
    let mut data = vec![0u8; 4096];
    let mut data_size: usize = data.len();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let result = unsafe {
            (video_encode_fn
                .fp()
                .get_encoded_video_session_parameters_khr)(
                context.device().handle(),
                get_info,
                feedback,
                &mut data_size,
                data.as_mut_ptr() as *mut std::ffi::c_void,
            )
        };

        match result {
            vk::Result::SUCCESS => {
                if data_size == 0 {
                    return Err(PixelForgeError::SessionParametersCreation(
                        "Encoded session parameters size is 0".to_string(),
                    ));
                }
                data.truncate(data_size);
                return Ok(data);
            }
            // Driver indicates the buffer was too small; resize to the reported
            // required size (or grow conservatively if it is not provided).
            vk::Result::INCOMPLETE if attempts < 3 => {
                let new_size = data_size.max(data.len() * 2).max(1);
                data.resize(new_size, 0);
                data_size = data.len();
            }
            err => {
                return Err(PixelForgeError::SessionParametersCreation(format!(
                    "Failed to get encoded session parameters: {err:?}"
                )));
            }
        }
    }
}

/// Resets the given query pool and writes starting timestamp command.
///
/// No-op when `query_pool` is null (i.e. the encode queue family does not
/// support timestamp queries).
pub(crate) fn reset_start_timestamp(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    query_pool: vk::QueryPool,
) {
    if query_pool.is_null() {
        return;
    }
    unsafe {
        device.cmd_reset_query_pool(command_buffer, query_pool, 0, 2);
        device.cmd_write_timestamp(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            query_pool,
            0, // query index 0 = start
        );
    }
}

/// Writes ending timestamp command to the given query pool.
///
/// No-op when `query_pool` is null (i.e. the encode queue family does not
/// support timestamp queries).
pub(crate) fn end_timestamp(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    query_pool: vk::QueryPool,
) {
    if query_pool.is_null() {
        return;
    }
    unsafe {
        device.cmd_write_timestamp(
            command_buffer,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            query_pool,
            1, // query index 1 = end
        );
    }
}

/// Queries the given query pool for recorded timestamps, returning their difference.
///
/// Returns `None` when `query_pool` is null (i.e. the encode queue family does
/// not support timestamp queries).
///
/// # Safety
/// This must only be called if both `reset_start_timestamp` and `end_timestamp`
///  were previously written and executed for the given query pool.
pub(crate) unsafe fn query_timestamp_diff(
    device: &ash::Device,
    query_pool: vk::QueryPool,
    mut timestamps: [u64; 2],
    timestamp_period: f32,
) -> Option<u64> {
    if query_pool.is_null() {
        return None;
    }
    let mut encode_time_ns: Option<u64> = None;
    let result = unsafe {
        device.get_query_pool_results(
            query_pool,
            0,               // first_query
            &mut timestamps, // data slice (length 2)
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::TYPE_64,
        )
    };
    if result.is_ok() {
        encode_time_ns = Some(((timestamps[1] - timestamps[0]) as f32 * timestamp_period) as u64);
    }
    encode_time_ns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gcd() {
        assert_eq!(gcd(12, 8), 4);
        assert_eq!(gcd(8, 12), 4);
        assert_eq!(gcd(16, 16), 16);
        assert_eq!(gcd(7, 3), 1);
        assert_eq!(gcd(0, 5), 5);
        assert_eq!(gcd(5, 0), 5);
    }

    #[test]
    fn test_lcm() {
        assert_eq!(lcm(32, 64), 64);
        assert_eq!(lcm(16, 12), 48);
        assert_eq!(lcm(4, 6), 12);
        assert_eq!(lcm(0, 5), 0);
        assert_eq!(lcm(5, 0), 0);
        assert_eq!(lcm(7, 7), 7);
    }

    #[test]
    fn input_images_are_64_aligned_within_the_device_maximum() {
        use crate::encoder::codec::input_image_width;
        // Widths whose 8-bit 4:4:4 chroma NVIDIA misread at the coded width.
        for (coded, input) in [(1360, 1408), (1440, 1472), (2400, 2432), (160, 192)] {
            assert_eq!(input_image_width(coded, 8192), input);
        }
        assert_eq!(input_image_width(1920, 8192), 1920);
        assert_eq!(input_image_width(4080, 4096), 4096);
        // Never wider than the device allows, never narrower than coded.
        assert_eq!(input_image_width(4080, 4080), 4080);
    }

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(130, 64), 192);
        assert_eq!(align_up(128, 64), 128);
        assert_eq!(align_up(1, 64), 64);
        assert_eq!(align_up(0, 64), 0);
        assert_eq!(align_up(100, 1), 100);
        assert_eq!(align_up(100, 0), 100);
        // AMD-realistic case: align 320 to lcm(32, 64) = 64.
        assert_eq!(align_up(320, lcm(32, 64)), 320);
        // AMD-realistic case: align 130 to lcm(32, 16) = 32.
        assert_eq!(align_up(130, lcm(32, 16)), 160);
    }
}
