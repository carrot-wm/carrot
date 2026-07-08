// vulkan-native buffer allocation: VK_EXT_image_drm_format_modifier images,
// PRIME export for scanout. no gbm - carrot links no C library.

use crate::render::vulkan::{RenderError, VkCore};
use crate::util::OnDrop;
use ash::vk;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct BoPlane {
    pub offset: u64,
    pub pitch: u64,
}

pub struct ScanoutBo {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    /// exported dma-buf; every plane references this one fd
    pub fd: OwnedFd,
    pub modifier: u64,
    pub planes: Vec<BoPlane>,
    pub width: u32,
    pub height: u32,
}

impl ScanoutBo {
    pub fn destroy(&self, core: &VkCore) {
        unsafe {
            core.device.destroy_image(self.image, None);
            core.device.free_memory(self.memory, None);
        }
    }
}

/// candidates = (modifier, plane count) pairs the plane and device both
/// accept; the driver picks one
pub fn create_scanout_bo(
    core: &VkCore,
    width: u32,
    height: u32,
    format: vk::Format,
    candidates: &[(u64, u32)],
) -> Result<ScanoutBo, RenderError> {
    let mods: Vec<u64> = candidates.iter().map(|(m, _)| *m).collect();
    let mut list =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
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
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut list)
        .push_next(&mut ext);
    let image = unsafe { core.device.create_image(&info, None) }?;
    // disarmed at the end; until then every ? frees what exists so far
    let image_guard = OnDrop(|| unsafe { core.device.destroy_image(image, None) });

    let mut mod_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
    unsafe {
        core.ext_modifier
            .get_image_drm_format_modifier_properties(image, &mut mod_props)
    }?;
    let modifier = mod_props.drm_format_modifier;
    let plane_count = candidates
        .iter()
        .find(|(m, _)| *m == modifier)
        .map(|(_, p)| *p)
        .unwrap_or(1);

    let reqs = unsafe { core.device.get_image_memory_requirements(image) };
    let mem_type = core.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut export = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type)
        .push_next(&mut dedicated)
        .push_next(&mut export);
    let memory = unsafe { core.device.allocate_memory(&alloc, None) }?;
    let memory_guard = OnDrop(|| unsafe { core.device.free_memory(memory, None) });
    unsafe { core.device.bind_image_memory(image, memory, 0) }?;

    let fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw = unsafe { core.ext_mem_fd.get_memory_fd(&fd_info) }?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    std::mem::forget(memory_guard);
    std::mem::forget(image_guard);

    let aspects = [
        vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
    ];
    let mut planes = Vec::new();
    for aspect in aspects.iter().take(plane_count as usize) {
        let sub = vk::ImageSubresource::default().aspect_mask(*aspect);
        let layout = unsafe { core.device.get_image_subresource_layout(image, sub) };
        planes.push(BoPlane {
            offset: layout.offset,
            pitch: layout.row_pitch,
        });
    }

    Ok(ScanoutBo {
        image,
        memory,
        fd,
        modifier,
        planes,
        width,
        height,
    })
}

/// wrap a linear dma-buf (a kms dumb buffer) in a vulkan image the renderer
/// can draw into. fallback for drivers that refuse to scan out vulkan memory
/// - anv only marks its own wsi allocations displayable, so on intel xe the
/// native path bounces off addfb2 and this is how pixels reach the screen.
pub fn import_linear_bo(
    core: &VkCore,
    dmabuf: OwnedFd,
    width: u32,
    height: u32,
    pitch: u32,
    size: u64,
    format: vk::Format,
) -> Result<ScanoutBo, RenderError> {
    import_bo(core, dmabuf, width, height, pitch, size, format, 0)
}

/// like import_linear_bo but with the modifier the bo was really made with;
/// single memory plane only (tile4 qualifies, ccs does not)
pub fn import_bo(
    core: &VkCore,
    dmabuf: OwnedFd,
    width: u32,
    height: u32,
    pitch: u32,
    size: u64,
    format: vk::Format,
    modifier: u64,
) -> Result<ScanoutBo, RenderError> {
    let layouts = [vk::SubresourceLayout::default().row_pitch(pitch as u64)];
    let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&layouts);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
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
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut explicit)
        .push_next(&mut ext);
    let image = unsafe { core.device.create_image(&info, None) }?;
    let image_guard = OnDrop(|| unsafe { core.device.destroy_image(image, None) });

    // device consumes the fd on success; give it a dup, ScanoutBo keeps the original
    let vk_fd = dmabuf
        .try_clone()
        .map_err(|e| RenderError::Load(e.to_string()))?;
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    unsafe {
        core.ext_mem_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            vk_fd.as_raw_fd(),
            &mut fd_props,
        )
    }?;
    // type must satisfy both the image and the imported fd
    let reqs = unsafe { core.device.get_image_memory_requirements(image) };
    let mem_type = core.find_memory_type(
        reqs.memory_type_bits & fd_props.memory_type_bits,
        vk::MemoryPropertyFlags::empty(),
    )?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(vk_fd.as_raw_fd());
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(size)
        .memory_type_index(mem_type)
        .push_next(&mut import)
        .push_next(&mut dedicated);
    let memory = match unsafe { core.device.allocate_memory(&alloc, None) } {
        Ok(m) => {
            // fd now belongs to the device
            std::mem::forget(vk_fd);
            m
        }
        Err(e) => return Err(e.into()),
    };
    // freeing the memory also releases the imported fd
    let memory_guard = OnDrop(|| unsafe { core.device.free_memory(memory, None) });
    unsafe { core.device.bind_image_memory(image, memory, 0) }?;
    std::mem::forget(memory_guard);
    std::mem::forget(image_guard);

    Ok(ScanoutBo {
        image,
        memory,
        fd: dmabuf,
        modifier,
        planes: vec![BoPlane {
            offset: 0,
            pitch: pitch as u64,
        }],
        width,
        height,
    })
}

/// clear the image to a solid color, hand it to the foreign queue.
/// synchronous - allocation-time work, not the frame loop.
pub fn fill_solid(core: &VkCore, image: vk::Image, color: [f32; 4]) -> Result<(), RenderError> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::TRANSIENT)
        .queue_family_index(core.queue_family);
    let pool = unsafe { core.device.create_command_pool(&pool_info, None) }?;
    let res = fill_solid_(core, image, color, pool);
    unsafe { core.device.destroy_command_pool(pool, None) };
    res
}

fn fill_solid_(
    core: &VkCore,
    image: vk::Image,
    color: [f32; 4],
    pool: vk::CommandPool,
) -> Result<(), RenderError> {
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { core.device.allocate_command_buffers(&alloc) }?[0];
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { core.device.begin_command_buffer(cb, &begin) }?;

    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let to_dst = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range);
    unsafe {
        core.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        );
        let value = vk::ClearColorValue { float32: color };
        core.device
            .cmd_clear_color_image(cb, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &value, &[range]);
    }
    // hand ownership to kms before it scans out
    let release = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::empty())
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(core.queue_family)
        .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        .image(image)
        .subresource_range(range);
    unsafe {
        core.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[release],
        );
        core.device.end_command_buffer(cb)?;
        let cbs = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        core.device
            .queue_submit(core.queue, &[submit], vk::Fence::null())?;
        core.device.queue_wait_idle(core.queue)?;
    }
    Ok(())
}
