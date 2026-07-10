// VkCore, the device half: instance, physical device matched to a drm node
// by devnum, one graphics queue, and the per-format modifier probe.

use ash::vk;
use rustix::fs::{fstat, makedev};
use std::ffi::CStr;
use std::fmt;
use std::os::fd::BorrowedFd;

#[derive(Debug)]
pub enum RenderError {
    Load(String),
    NoDevice,
    MissingExt(&'static str),
    NoGraphicsQueue,
    NoMemoryType,
    Vk(vk::Result),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Load(e) => write!(f, "loading the vulkan loader failed: {e}"),
            RenderError::NoDevice => write!(f, "no vulkan device matches this drm node"),
            RenderError::MissingExt(e) => write!(f, "the device lacks {e}"),
            RenderError::NoGraphicsQueue => write!(f, "the device has no graphics queue"),
            RenderError::NoMemoryType => write!(f, "no suitable memory type"),
            RenderError::Vk(r) => write!(f, "vulkan error: {r:?}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<vk::Result> for RenderError {
    fn from(r: vk::Result) -> Self {
        RenderError::Vk(r)
    }
}

const REQUIRED_EXTS: &[&CStr] = &[
    ash::khr::external_fence_fd::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::khr::external_semaphore_fd::NAME,
    ash::khr::push_descriptor::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

pub struct VkCore {
    // field order is drop order: device before instance, entry last
    pub ext_mem_fd: ash::khr::external_memory_fd::Device,
    pub ext_modifier: ash::ext::image_drm_format_modifier::Device,
    pub ext_fence_fd: ash::khr::external_fence_fd::Device,
    pub ext_semaphore_fd: ash::khr::external_semaphore_fd::Device,
    pub ext_push_desc: ash::khr::push_descriptor::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub pdev: vk::PhysicalDevice,
    pub device_name: String,
    pub device: ash::Device,
    pub instance: ash::Instance,
    pub entry: ash::Entry,
}

impl Drop for VkCore {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn has_ext(exts: &[vk::ExtensionProperties], name: &CStr) -> bool {
    exts.iter().any(|e| {
        e.extension_name_as_c_str()
            .map(|n| n == name)
            .unwrap_or(false)
    })
}

impl VkCore {
    pub fn new(card: BorrowedFd<'_>) -> Result<VkCore, RenderError> {
        let rdev = fstat(card).map_err(|e| RenderError::Load(e.to_string()))?.st_rdev;
        let entry: ash::Entry = Err(RenderError::Load("icd loader not wired yet".into()))?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = unsafe { entry.create_instance(&info, None) }?;
        let pdevs = unsafe { instance.enumerate_physical_devices() }?;
        for pdev in pdevs {
            let exts = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
            if !has_ext(&exts, ash::ext::physical_device_drm::NAME) {
                continue;
            }
            let mut drm_props = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm_props);
            unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
            // props2 holds a &mut into drm_props; read everything out before
            // touching drm_props again
            let api_version = props2.properties.api_version;
            let device_name = props2
                .properties
                .device_name_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into());
            if api_version < vk::API_VERSION_1_3 {
                continue;
            }
            let matches = (drm_props.has_primary != 0
                && makedev(drm_props.primary_major as u32, drm_props.primary_minor as u32) == rdev)
                || (drm_props.has_render != 0
                    && makedev(drm_props.render_major as u32, drm_props.render_minor as u32)
                        == rdev);
            if !matches {
                continue;
            }
            // our card; missing requirements are now hard errors
            for req in REQUIRED_EXTS {
                if !has_ext(&exts, req) {
                    return Err(RenderError::MissingExt(
                        req.to_str().unwrap_or("unknown extension"),
                    ));
                }
            }
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let queue_family = families
                .iter()
                .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .ok_or(RenderError::NoGraphicsQueue)? as u32;
            let prio = [1.0f32];
            let queue_info = vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&prio);
            let ext_ptrs: Vec<*const std::ffi::c_char> =
                REQUIRED_EXTS.iter().map(|c| c.as_ptr()).collect();
            let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(true)
                .dynamic_rendering(true);
            let queue_infos = [queue_info];
            let device_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&ext_ptrs)
                .push_next(&mut features13);
            let device = unsafe { instance.create_device(pdev, &device_info, None) }?;
            let queue = unsafe { device.get_device_queue(queue_family, 0) };
            let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
            return Ok(VkCore {
                ext_mem_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                ext_modifier: ash::ext::image_drm_format_modifier::Device::new(
                    &instance, &device,
                ),
                ext_fence_fd: ash::khr::external_fence_fd::Device::new(&instance, &device),
                ext_semaphore_fd: ash::khr::external_semaphore_fd::Device::new(
                    &instance, &device,
                ),
                ext_push_desc: ash::khr::push_descriptor::Device::new(&instance, &device),
                queue,
                queue_family,
                mem_props,
                pdev,
                device_name,
                device,
                instance,
                entry,
            });
        }
        unsafe { instance.destroy_instance(None) };
        Err(RenderError::NoDevice)
    }

    /// modifiers this device can allocate + export for scanout of `format`,
    /// with plane counts
    pub fn scanout_modifiers(&self, format: vk::Format) -> Result<Vec<(u64, u32)>, RenderError> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            self.instance
                .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
        };
        let count = list.drm_format_modifier_count as usize;
        let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut props);
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            self.instance
                .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
        };
        let filled = list.drm_format_modifier_count as usize;
        props.truncate(filled);

        let mut out = Vec::new();
        for p in &props {
            if !p
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
            {
                continue;
            }
            let mut mod_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(p.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(
                    vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .push_next(&mut mod_info)
                .push_next(&mut ext_info);
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let mut ifp2 = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            let ok = unsafe {
                self.instance
                    .get_physical_device_image_format_properties2(self.pdev, &info, &mut ifp2)
            };
            if ok.is_ok()
                && ext_props
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
            {
                out.push((
                    p.drm_format_modifier,
                    p.drm_format_modifier_plane_count,
                ));
            }
        }
        Ok(out)
    }

    /// modifiers this device can import + sample for `format` - what the
    /// dmabuf global advertises to gpu clients
    pub fn sample_modifiers(&self, format: vk::Format) -> Result<Vec<u64>, RenderError> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            self.instance
                .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
        };
        let count = list.drm_format_modifier_count as usize;
        let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut props);
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            self.instance
                .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
        };
        let filled = list.drm_format_modifier_count as usize;
        props.truncate(filled);

        let mut out = Vec::new();
        for p in &props {
            if !p
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            {
                continue;
            }
            let mut mod_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(p.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut mod_info)
                .push_next(&mut ext_info);
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let mut ifp2 = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            let ok = unsafe {
                self.instance
                    .get_physical_device_image_format_properties2(self.pdev, &info, &mut ifp2)
            };
            if ok.is_ok()
                && ext_props
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            {
                out.push(p.drm_format_modifier);
            }
        }
        Ok(out)
    }

    pub fn find_memory_type(
        &self,
        bits: u32,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<u32, RenderError> {
        for i in 0..self.mem_props.memory_type_count {
            if bits & (1 << i) != 0
                && self.mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
            {
                return Ok(i);
            }
        }
        Err(RenderError::NoMemoryType)
    }
}
