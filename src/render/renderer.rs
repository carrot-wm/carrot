// renderer core. a frame: acquire target from the display side, one
// dynamic-rendering pass of quads, release it, one queue_submit2. blending
// only where alpha exists.
//
// INVARIANT: views handed to Tex ops must already be SHADER_READ_ONLY_OPTIMAL.

use crate::render::shaders;
use crate::render::vulkan::{RenderError, VkCore};
use ash::vk;
use std::cell::{Cell, RefCell};
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;

// -- push constants; layouts mirror the hand-built spir-v --

#[repr(C)]
struct FillPush {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
struct TexPush {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    mul: f32,
}

#[repr(C)]
struct BorderPush {
    pos: [f32; 2],
    size: [f32; 2],
    rect_px: [f32; 4],
    radius: f32,
    width: f32,
    aa: f32,
    _pad: f32,
    color: [f32; 4],
}

#[repr(C)]
struct ShadowPush {
    pos: [f32; 2],
    size: [f32; 2],
    win_px: [f32; 4],
    radius: f32,
    range: f32,
    power: f32,
    aa: f32,
    color: [f32; 4],
}

#[repr(C)]
struct XfadePush {
    pos: [f32; 2],
    size: [f32; 2],
    progress: f32,
    radius: f32,
    aa: f32,
    _pad: f32,
    geo_pos: [f32; 2],
    geo_size: [f32; 2],
}

#[repr(C)]
struct BlurPush {
    pos: [f32; 2],
    size: [f32; 2],
    halfpixel: [f32; 2],
    extra_a: f32,
    extra_b: f32,
}

#[repr(C)]
struct TexRPush {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    mul: f32,
    _pad: f32, // spir-v vec2 members sit on 8-byte offsets
    geo_pos: [f32; 2],
    geo_size: [f32; 2],
    radius: f32,
    aa: f32,
}

fn push_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast(), size_of::<T>()) }
}

// -- ops --

/// all coordinates are final vulkan NDC; the shaders are passthroughs
#[derive(Clone)]
pub enum RenderOp {
    Fill {
        pos: [f32; 2],
        size: [f32; 2],
        color: [f32; 4],
    },
    Tex {
        view: vk::ImageView,
        pos: [f32; 2],
        size: [f32; 2],
        uv_pos: [f32; 2],
        uv_size: [f32; 2],
        mul: f32,
        opaque: bool,
    },
    /// a rounded ring in one quad; rect/radius in output-local pixels.
    /// width past half the rect turns the ring into a rounded fill
    Border {
        pos: [f32; 2],
        size: [f32; 2],
        rect_px: [f32; 4],
        radius: f32,
        width: f32,
        color: [f32; 4],
    },
    /// distance-falloff halo around a rounded window body
    Shadow {
        pos: [f32; 2],
        size: [f32; 2],
        win_px: [f32; 4],
        radius: f32,
        range: f32,
        power: f32,
        color: [f32; 4],
    },
    /// one kawase step over the whole target; down: a=contrast b=brightness,
    /// up: a=noise
    Blur {
        view: vk::ImageView,
        halfpixel: [f32; 2],
        extra_a: f32,
        extra_b: f32,
        up: bool,
    },
    /// two textures stretched to the quad, mixed by progress, clipped to
    /// the rounded geometry; the resize crossfade
    Xfade {
        from_view: vk::ImageView,
        to_view: vk::ImageView,
        pos: [f32; 2],
        size: [f32; 2],
        progress: f32,
        geo_px: [f32; 4],
        radius: f32,
    },
    /// tex clipped to a rounded rect; geo/radius in output-local pixels
    TexR {
        view: vk::ImageView,
        pos: [f32; 2],
        size: [f32; 2],
        uv_pos: [f32; 2],
        uv_size: [f32; 2],
        mul: f32,
        geo_px: [f32; 4],
        radius: f32,
    },
}

impl RenderOp {
    fn blends(&self) -> bool {
        match self {
            RenderOp::Fill { color, .. } => color[3] < 1.0,
            RenderOp::Tex { opaque, mul, .. } => !opaque || *mul < 1.0,
            RenderOp::TexR { .. } => true,
            RenderOp::Border { .. } => true,
            RenderOp::Shadow { .. } => true,
            RenderOp::Xfade { .. } => true,
            RenderOp::Blur { .. } => false,
        }
    }
}

/// who consumes the target after the pass
#[derive(Copy, Clone, PartialEq)]
pub enum TargetRelease {
    /// scanout: foreign-queue handoff, GENERAL
    Scanout,
    /// our own sampler: stays on the graphics queue, SHADER_READ_ONLY
    Sampled,
}

pub struct FrameTarget {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
    /// first ever use: acquire from UNDEFINED, not GENERAL
    pub undefined: bool,
}

/// a submitted frame. wait() is for bring-up; the present loop exports the
/// fence instead and never blocks.
pub struct Frame {
    cb: vk::CommandBuffer,
    fence: vk::Fence,
    waits: Vec<vk::Semaphore>,
}

impl Frame {
    pub fn export_sync_file(&self, r: &Renderer) -> Result<OwnedFd, RenderError> {
        let info = vk::FenceGetFdInfoKHR::default()
            .fence(self.fence)
            .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
        let fd = unsafe { r.core.ext_fence_fd.get_fence_fd(&info) }?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn wait(self, r: &Renderer) -> Result<(), RenderError> {
        unsafe {
            r.core
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)?;
        }
        r.recycle(self);
        Ok(())
    }
}

// -- renderer --

struct Pipelines {
    fill_opaque: vk::Pipeline,
    fill_blend: vk::Pipeline,
    tex_opaque: vk::Pipeline,
    tex_blend: vk::Pipeline,
    texr_blend: vk::Pipeline,
    border_blend: vk::Pipeline,
    shadow_blend: vk::Pipeline,
    xfade_blend: vk::Pipeline,
    blur_down: vk::Pipeline,
    blur_up: vk::Pipeline,
}

pub struct Renderer {
    pub core: Rc<VkCore>,
    format: vk::Format,
    sampler: vk::Sampler,
    tex_set_layout: vk::DescriptorSetLayout,
    tex_layout: vk::PipelineLayout,
    texr_layout: vk::PipelineLayout,
    xfade_set_layout: vk::DescriptorSetLayout,
    xfade_layout: vk::PipelineLayout,
    blur_layout: vk::PipelineLayout,
    border_layout: vk::PipelineLayout,
    shadow_layout: vk::PipelineLayout,
    fill_layout: vk::PipelineLayout,
    pipes: Pipelines,
    pool: vk::CommandPool,
    free_cbs: RefCell<Vec<vk::CommandBuffer>>,
    tex_uid: Cell<u64>,
}

impl Renderer {
    pub fn new(core: &Rc<VkCore>, format: vk::Format) -> Result<Renderer, RenderError> {
        let dev = &core.device;

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.25);
        let sampler = unsafe { dev.create_sampler(&sampler_info, None) }?;

        let samplers = [sampler];
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers);
        let bindings = [binding];
        let set_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
            .bindings(&bindings);
        let tex_set_layout = unsafe { dev.create_descriptor_set_layout(&set_info, None) }?;

        let fill_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<FillPush>() as u32)];
        let fill_layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&fill_range);
        let fill_layout = unsafe { dev.create_pipeline_layout(&fill_layout_info, None) }?;

        let tex_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<TexPush>() as u32)];
        let set_layouts = [tex_set_layout];
        let tex_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&tex_range);
        let tex_layout = unsafe { dev.create_pipeline_layout(&tex_layout_info, None) }?;

        let texr_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<TexRPush>() as u32)];
        let texr_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&texr_range);
        let texr_layout = unsafe { dev.create_pipeline_layout(&texr_layout_info, None) }?;

        // two combined samplers for the crossfade
        let b0 = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers);
        let b1 = vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers);
        let xf_bindings = [b0, b1];
        let xf_set_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
            .bindings(&xf_bindings);
        let xfade_set_layout = unsafe { dev.create_descriptor_set_layout(&xf_set_info, None) }?;
        let xfade_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<XfadePush>() as u32)];
        let xf_set_layouts = [xfade_set_layout];
        let xfade_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&xf_set_layouts)
            .push_constant_ranges(&xfade_range);
        let xfade_layout = unsafe { dev.create_pipeline_layout(&xfade_layout_info, None) }?;

        let blur_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<BlurPush>() as u32)];
        let blur_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&blur_range);
        let blur_layout = unsafe { dev.create_pipeline_layout(&blur_layout_info, None) }?;

        let border_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<BorderPush>() as u32)];
        let border_layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&border_range);
        let border_layout = unsafe { dev.create_pipeline_layout(&border_layout_info, None) }?;

        let shadow_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .size(size_of::<ShadowPush>() as u32)];
        let shadow_layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&shadow_range);
        let shadow_layout = unsafe { dev.create_pipeline_layout(&shadow_layout_info, None) }?;

        let fill_module = create_module(dev, shaders::FILL)?;
        let tex_module = create_module(dev, shaders::TEX)?;
        let texr_module = create_module(dev, shaders::TEXR)?;
        let border_module = create_module(dev, shaders::BORDER)?;
        let shadow_module = create_module(dev, shaders::SHADOW)?;
        let xfade_module = create_module(dev, shaders::XFADE)?;
        let blur_down_module = create_module(dev, shaders::BLUR_DOWN)?;
        let blur_up_module = create_module(dev, shaders::BLUR_UP)?;
        let pipes = Pipelines {
            fill_opaque: create_pipeline(dev, format, fill_module, fill_layout, false)?,
            fill_blend: create_pipeline(dev, format, fill_module, fill_layout, true)?,
            tex_opaque: create_pipeline(dev, format, tex_module, tex_layout, false)?,
            tex_blend: create_pipeline(dev, format, tex_module, tex_layout, true)?,
            texr_blend: create_pipeline(dev, format, texr_module, texr_layout, true)?,
            border_blend: create_pipeline(dev, format, border_module, border_layout, true)?,
            shadow_blend: create_pipeline(dev, format, shadow_module, shadow_layout, true)?,
            xfade_blend: create_pipeline(dev, format, xfade_module, xfade_layout, true)?,
            blur_down: create_pipeline(dev, format, blur_down_module, blur_layout, false)?,
            blur_up: create_pipeline(dev, format, blur_up_module, blur_layout, false)?,
        };
        unsafe {
            dev.destroy_shader_module(fill_module, None);
            dev.destroy_shader_module(tex_module, None);
            dev.destroy_shader_module(texr_module, None);
            dev.destroy_shader_module(border_module, None);
            dev.destroy_shader_module(shadow_module, None);
            dev.destroy_shader_module(xfade_module, None);
            dev.destroy_shader_module(blur_down_module, None);
            dev.destroy_shader_module(blur_up_module, None);
        }

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(
                vk::CommandPoolCreateFlags::TRANSIENT
                    | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
            )
            .queue_family_index(core.queue_family);
        let pool = unsafe { dev.create_command_pool(&pool_info, None) }?;

        Ok(Renderer {
            core: core.clone(),
            format,
            sampler,
            tex_set_layout,
            tex_layout,
            texr_layout,
            xfade_set_layout,
            xfade_layout,
            blur_layout,
            border_layout,
            shadow_layout,
            fill_layout,
            pipes,
            pool,
            free_cbs: RefCell::new(Vec::new()),
            tex_uid: Cell::new(0),
        })
    }

    /// textures are cached under stable keys; the uid tells a replacement
    /// apart from the texture a stored batch actually sampled
    fn next_tex_uid(&self) -> u64 {
        let u = self.tex_uid.get();
        self.tex_uid.set(u + 1);
        u
    }

    /// sync_file (previous scanout's OUT fence) -> submit wait; temporary
    /// import restores the semaphore after one use.
    pub fn import_wait(&self, fd: OwnedFd) -> Result<vk::Semaphore, RenderError> {
        use std::os::fd::IntoRawFd;
        let sem = unsafe {
            self.core
                .device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        }?;
        let info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(fd.into_raw_fd());
        if let Err(e) = unsafe { self.core.ext_semaphore_fd.import_semaphore_fd(&info) } {
            unsafe { self.core.device.destroy_semaphore(sem, None) };
            return Err(e.into());
        }
        Ok(sem)
    }

    pub fn render(
        &self,
        target: &FrameTarget,
        clear: Option<[f32; 4]>,
        ops: &[RenderOp],
        waits: Vec<vk::Semaphore>,
    ) -> Result<Frame, RenderError> {
        self.render_release(target, clear, ops, waits, TargetRelease::Scanout)
    }

    pub fn render_release(
        &self,
        target: &FrameTarget,
        clear: Option<[f32; 4]>,
        ops: &[RenderOp],
        waits: Vec<vk::Semaphore>,
        release_mode: TargetRelease,
    ) -> Result<Frame, RenderError> {
        let dev = &self.core.device;
        let cb = self.take_cb()?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { dev.begin_command_buffer(cb, &begin) }?;

        // take the target from the display side; sampled targets are ours
        // already and come back from their last SHADER_READ_ONLY use
        let (src_qf, dst_qf, old_owned) = match release_mode {
            TargetRelease::Scanout => (
                vk::QUEUE_FAMILY_FOREIGN_EXT,
                self.core.queue_family,
                vk::ImageLayout::GENERAL,
            ),
            TargetRelease::Sampled => (
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            ),
        };
        let acquire = image_barrier(target.image)
            .src_queue_family_index(src_qf)
            .dst_queue_family_index(dst_qf)
            .old_layout(if target.undefined {
                vk::ImageLayout::UNDEFINED
            } else {
                old_owned
            })
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
            );
        barrier2(dev, cb, &[acquire]);

        let mut attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        if let Some(c) = clear {
            attachment = attachment
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: c },
                });
        }
        let attachments = [attachment];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: target.width,
                height: target.height,
            },
        };
        let rendering = vk::RenderingInfo::default()
            .render_area(area)
            .layer_count(1)
            .color_attachments(&attachments);
        unsafe {
            dev.cmd_begin_rendering(cb, &rendering);
            dev.cmd_set_viewport(
                cb,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: target.width as f32,
                    height: target.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            dev.cmd_set_scissor(cb, 0, &[area]);
        }

        let mut bound = vk::Pipeline::null();
        for op in ops {
            let (pipe, layout) = match (op, op.blends()) {
                (RenderOp::Fill { .. }, false) => (self.pipes.fill_opaque, self.fill_layout),
                (RenderOp::Fill { .. }, true) => (self.pipes.fill_blend, self.fill_layout),
                (RenderOp::Tex { .. }, false) => (self.pipes.tex_opaque, self.tex_layout),
                (RenderOp::Tex { .. }, true) => (self.pipes.tex_blend, self.tex_layout),
                (RenderOp::TexR { .. }, _) => (self.pipes.texr_blend, self.texr_layout),
                (RenderOp::Border { .. }, _) => (self.pipes.border_blend, self.border_layout),
                (RenderOp::Shadow { .. }, _) => (self.pipes.shadow_blend, self.shadow_layout),
                (RenderOp::Xfade { .. }, _) => (self.pipes.xfade_blend, self.xfade_layout),
                (RenderOp::Blur { up, .. }, _) => (
                    if *up { self.pipes.blur_up } else { self.pipes.blur_down },
                    self.blur_layout,
                ),
            };
            unsafe {
                if pipe != bound {
                    dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe);
                    bound = pipe;
                }
                match op {
                    RenderOp::Fill { pos, size, color } => {
                        // the blend is premultiplied-over; straight alpha
                        // would glow additively during fades
                        let a = color[3];
                        let pc = FillPush {
                            pos: *pos,
                            size: *size,
                            color: [color[0] * a, color[1] * a, color[2] * a, a],
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::Tex {
                        view,
                        pos,
                        size,
                        uv_pos,
                        uv_size,
                        mul,
                        ..
                    } => {
                        let image_info = vk::DescriptorImageInfo::default()
                            .sampler(self.sampler)
                            .image_view(*view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                        let infos = [image_info];
                        let write = vk::WriteDescriptorSet::default()
                            .dst_binding(0)
                            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .image_info(&infos);
                        self.core.ext_push_desc.cmd_push_descriptor_set(
                            cb,
                            vk::PipelineBindPoint::GRAPHICS,
                            layout,
                            0,
                            &[write],
                        );
                        let pc = TexPush {
                            pos: *pos,
                            size: *size,
                            uv_pos: *uv_pos,
                            uv_size: *uv_size,
                            mul: *mul,
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::Blur { view, halfpixel, extra_a, extra_b, .. } => {
                        let image_info = vk::DescriptorImageInfo::default()
                            .sampler(self.sampler)
                            .image_view(*view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                        let infos = [image_info];
                        let write = vk::WriteDescriptorSet::default()
                            .dst_binding(0)
                            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .image_info(&infos);
                        self.core.ext_push_desc.cmd_push_descriptor_set(
                            cb,
                            vk::PipelineBindPoint::GRAPHICS,
                            layout,
                            0,
                            &[write],
                        );
                        let pc = BlurPush {
                            pos: [-1.0, -1.0],
                            size: [2.0, 2.0],
                            halfpixel: *halfpixel,
                            extra_a: *extra_a,
                            extra_b: *extra_b,
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::Xfade { from_view, to_view, pos, size, progress, geo_px, radius } => {
                        let infos0 = [vk::DescriptorImageInfo::default()
                            .sampler(self.sampler)
                            .image_view(*from_view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                        let infos1 = [vk::DescriptorImageInfo::default()
                            .sampler(self.sampler)
                            .image_view(*to_view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                        let writes = [
                            vk::WriteDescriptorSet::default()
                                .dst_binding(0)
                                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                                .image_info(&infos0),
                            vk::WriteDescriptorSet::default()
                                .dst_binding(1)
                                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                                .image_info(&infos1),
                        ];
                        self.core.ext_push_desc.cmd_push_descriptor_set(
                            cb,
                            vk::PipelineBindPoint::GRAPHICS,
                            layout,
                            0,
                            &writes,
                        );
                        let pc = XfadePush {
                            pos: *pos,
                            size: *size,
                            progress: *progress,
                            radius: radius.max(1.0),
                            aa: 0.7,
                            _pad: 0.0,
                            geo_pos: [geo_px[0], geo_px[1]],
                            geo_size: [geo_px[2], geo_px[3]],
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::Border { pos, size, rect_px, radius, width, color } => {
                        let a = color[3];
                        let pc = BorderPush {
                            pos: *pos,
                            size: *size,
                            rect_px: *rect_px,
                            // below one pixel the aa band eats the whole
                            // interior and coverage collapses to ~0.5
                            radius: radius.max(1.0),
                            width: *width,
                            aa: 0.7,
                            _pad: 0.0,
                            color: [color[0] * a, color[1] * a, color[2] * a, a],
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::Shadow { pos, size, win_px, radius, range, power, color } => {
                        let pc = ShadowPush {
                            pos: *pos,
                            size: *size,
                            win_px: *win_px,
                            radius: radius.max(1.0),
                            range: range.max(1.0),
                            power: *power,
                            aa: 0.7,
                            color: *color,
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                    RenderOp::TexR {
                        view,
                        pos,
                        size,
                        uv_pos,
                        uv_size,
                        mul,
                        geo_px,
                        radius,
                    } => {
                        let image_info = vk::DescriptorImageInfo::default()
                            .sampler(self.sampler)
                            .image_view(*view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                        let infos = [image_info];
                        let write = vk::WriteDescriptorSet::default()
                            .dst_binding(0)
                            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .image_info(&infos);
                        self.core.ext_push_desc.cmd_push_descriptor_set(
                            cb,
                            vk::PipelineBindPoint::GRAPHICS,
                            layout,
                            0,
                            &[write],
                        );
                        let pc = TexRPush {
                            pos: *pos,
                            size: *size,
                            uv_pos: *uv_pos,
                            uv_size: *uv_size,
                            mul: *mul,
                            _pad: 0.0,
                            geo_pos: [geo_px[0], geo_px[1]],
                            geo_size: [geo_px[2], geo_px[3]],
                            radius: radius.max(1.0),
                            aa: 0.7,
                        };
                        dev.cmd_push_constants(
                            cb,
                            layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            push_bytes(&pc),
                        );
                    }
                }
                dev.cmd_draw(cb, 4, 1, 0, 0);
            }
        }

        unsafe { dev.cmd_end_rendering(cb) };

        // hand it back: to the display side, or to our own samplers
        let release = match release_mode {
            TargetRelease::Scanout => image_barrier(target.image)
                .src_queue_family_index(self.core.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(
                    vk::AccessFlags2::COLOR_ATTACHMENT_WRITE
                        | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
                ),
            TargetRelease::Sampled => image_barrier(target.image)
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ),
        };
        barrier2(dev, cb, &[release]);
        unsafe { dev.end_command_buffer(cb) }?;

        // exportable fence so the commit path can take a sync_file
        let mut export = vk::ExportFenceCreateInfo::default()
            .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
        let fence_info = vk::FenceCreateInfo::default().push_next(&mut export);
        let fence = unsafe { dev.create_fence(&fence_info, None) }?;

        let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let wait_infos: Vec<_> = waits
            .iter()
            .map(|&s| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(s)
                    .stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            })
            .collect();
        let submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&cbs)
            .wait_semaphore_infos(&wait_infos);
        let res = unsafe { dev.queue_submit2(self.core.queue, &[submit], fence) };
        if let Err(e) = res {
            unsafe {
                dev.destroy_fence(fence, None);
                for s in &waits {
                    dev.destroy_semaphore(*s, None);
                }
            }
            self.free_cbs.borrow_mut().push(cb);
            return Err(e.into());
        }
        Ok(Frame { cb, fence, waits })
    }

    /// diagnostic: pull the target's pixels to the cpu. blocking, allocates
    /// per call - never on the frame path.
    pub fn readback(&self, target: &FrameTarget) -> Result<Vec<u8>, RenderError> {
        let dev = &self.core.device;
        let size = (target.width * target.height * 4) as u64;
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST);
        let buf = unsafe { dev.create_buffer(&buf_info, None) }?;
        let reqs = unsafe { dev.get_buffer_memory_requirements(buf) };
        let mem_type = self.core.find_memory_type(
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let mem = unsafe { dev.allocate_memory(&alloc, None) }?;
        unsafe { dev.bind_buffer_memory(buf, mem, 0) }?;

        let cb = self.take_cb()?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { dev.begin_command_buffer(cb, &begin) }?;
        // plain layout transitions, no ownership dance - debug read, not a
        // scanout handoff
        let acquire = image_barrier(target.image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ);
        barrier2(dev, cb, &[acquire]);
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: target.width,
                height: target.height,
                depth: 1,
            });
        unsafe {
            dev.cmd_copy_image_to_buffer(
                cb,
                target.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buf,
                &[region],
            );
        }
        let release = image_barrier(target.image)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_READ);
        barrier2(dev, cb, &[release]);
        unsafe { dev.end_command_buffer(cb) }?;

        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }?;
        let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = vk::SubmitInfo2::default().command_buffer_infos(&cbs);
        let out = unsafe {
            dev.queue_submit2(self.core.queue, &[submit], fence)
                .and_then(|_| dev.wait_for_fences(&[fence], true, u64::MAX))
                .map(|_| {
                    let ptr = dev
                        .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                        .unwrap() as *const u8;
                    let v = std::slice::from_raw_parts(ptr, size as usize).to_vec();
                    dev.unmap_memory(mem);
                    v
                })
        };
        unsafe {
            dev.destroy_fence(fence, None);
            dev.destroy_buffer(buf, None);
            dev.free_memory(mem, None);
        }
        self.free_cbs.borrow_mut().push(cb);
        out.map_err(RenderError::from)
    }

    pub fn create_target_view(&self, image: vk::Image) -> Result<vk::ImageView, RenderError> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        Ok(unsafe { self.core.device.create_image_view(&info, None) }?)
    }

    fn take_cb(&self) -> Result<vk::CommandBuffer, RenderError> {
        if let Some(cb) = self.free_cbs.borrow_mut().pop() {
            unsafe {
                self.core
                    .device
                    .reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            }
            return Ok(cb);
        }
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        Ok(unsafe { self.core.device.allocate_command_buffers(&alloc) }?[0])
    }

    /// async path: call once the frame's fence is known signaled
    pub fn recycle_frame(&self, frame: Frame) {
        self.recycle(frame);
    }

    fn recycle(&self, frame: Frame) {
        unsafe {
            self.core.device.destroy_fence(frame.fence, None);
            for s in &frame.waits {
                self.core.device.destroy_semaphore(*s, None);
            }
        }
        self.free_cbs.borrow_mut().push(frame.cb);
    }

    // -- textures (shm upload path) --

    pub fn create_texture(&self, w: u32, h: u32, opaque: bool) -> Result<Texture, RenderError> {
        let dev = &self.core.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(self.format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { dev.create_image(&info, None) }?;
        let image_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image(image, None) });
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        let mem_type = self
            .core
            .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { dev.allocate_memory(&alloc, None) }?;
        let mem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(memory, None) });
        unsafe { dev.bind_image_memory(image, memory, 0) }?;
        // xrgb clients leave alpha undefined; pin it to one so premultiplied
        // sampling can't make the window invisible.
        let alpha = if opaque {
            vk::ComponentSwizzle::ONE
        } else {
            vk::ComponentSwizzle::IDENTITY
        };
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: alpha,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { dev.create_image_view(&view_info, None) }?;
        std::mem::forget(mem_guard);
        std::mem::forget(image_guard);
        Ok(Texture {
            image,
            memory,
            view,
            width: w,
            height: h,
            undefined: std::cell::Cell::new(true),
            uid: self.next_tex_uid(),
        })
    }

    /// a texture the renderer can draw into and later sample
    pub fn create_render_texture(&self, w: u32, h: u32) -> Result<Texture, RenderError> {
        let dev = &self.core.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(self.format)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { dev.create_image(&info, None) }?;
        let image_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image(image, None) });
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        let mem_type = self
            .core
            .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { dev.allocate_memory(&alloc, None) }?;
        let mem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(memory, None) });
        unsafe { dev.bind_image_memory(image, memory, 0) }?;
        // attachment views take no swizzle; identity serves sampling too
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { dev.create_image_view(&view_info, None) }?;
        std::mem::forget(mem_guard);
        std::mem::forget(image_guard);
        Ok(Texture {
            image,
            memory,
            view,
            width: w,
            height: h,
            undefined: std::cell::Cell::new(true),
            uid: self.next_tex_uid(),
        })
    }

    /// draw an op list into a render texture; it comes out sampleable
    pub fn render_into(
        &self,
        tex: &Texture,
        clear: Option<[f32; 4]>,
        ops: &[RenderOp],
        waits: Vec<vk::Semaphore>,
    ) -> Result<Frame, RenderError> {
        let target = FrameTarget {
            image: tex.image,
            view: tex.view,
            width: tex.width,
            height: tex.height,
            undefined: tex.undefined.get(),
        };
        let frame = self.render_release(&target, clear, ops, waits, TargetRelease::Sampled)?;
        tex.undefined.set(false);
        Ok(frame)
    }

    pub fn destroy_texture(&self, tex: &Texture) {
        unsafe {
            self.core.device.destroy_image_view(tex.view, None);
            self.core.device.destroy_image(tex.image, None);
            self.core.device.free_memory(tex.memory, None);
        }
    }

    /// wrap a client dmabuf as a sampled texture. the import happens once
    /// per wl_buffer; every later commit samples in place.
    pub fn import_dmabuf(
        &self,
        planes: &[crate::protocol::shm::DmabufPlane],
        modifier: u64,
        w: u32,
        h: u32,
        opaque: bool,
    ) -> Result<Texture, RenderError> {
        use std::os::fd::AsRawFd;
        let dev = &self.core.device;
        let first = planes
            .first()
            .ok_or_else(|| RenderError::Load("dmabuf with no planes".into()))?;
        let fd = first
            .fd
            .try_clone()
            .map_err(|e| RenderError::Load(format!("dmabuf dup: {e}")))?;
        let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))
            .map_err(|e| RenderError::Load(format!("dmabuf seek: {e}")))?;
        let layouts: Vec<vk::SubresourceLayout> = planes
            .iter()
            .map(|p| {
                vk::SubresourceLayout::default()
                    .offset(p.offset as u64)
                    .row_pitch(p.stride as u64)
            })
            .collect();
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&layouts);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(self.format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut explicit)
            .push_next(&mut ext);
        let image = unsafe { dev.create_image(&info, None) }?;
        let image_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image(image, None) });

        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            self.core.ext_mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
        }?;
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        let mem_type = self.core.find_memory_type(
            reqs.memory_type_bits & fd_props.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd.as_raw_fd());
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(mem_type)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let memory = match unsafe { dev.allocate_memory(&alloc, None) } {
            Ok(m) => {
                // the device owns the fd now
                std::mem::forget(fd);
                m
            }
            Err(e) => return Err(e.into()),
        };
        let mem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(memory, None) });
        unsafe { dev.bind_image_memory(image, memory, 0) }?;

        let alpha = if opaque {
            vk::ComponentSwizzle::ONE
        } else {
            vk::ComponentSwizzle::IDENTITY
        };
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: alpha,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { dev.create_image_view(&view_info, None) }?;
        let view_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image_view(view, None) });

        // the draw pass expects SHADER_READ_ONLY; linear has no metadata so a
        // one-time transition stays valid across the client's rewrites
        self.transition_sampled(image)?;

        std::mem::forget(view_guard);
        std::mem::forget(mem_guard);
        std::mem::forget(image_guard);
        Ok(Texture {
            image,
            memory,
            view,
            width: w,
            height: h,
            undefined: std::cell::Cell::new(false),
            uid: self.next_tex_uid(),
        })
    }

    /// render ops into an offscreen image and read the pixels back as
    /// tightly packed rows. screenshot-grade: fully synchronous.
    pub fn read_frame(
        &self,
        w: u32,
        h: u32,
        ops: &[RenderOp],
        waits: Vec<vk::Semaphore>,
    ) -> Result<Vec<u8>, RenderError> {
        let dev = &self.core.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(self.format)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { dev.create_image(&info, None) }?;
        let img_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image(image, None) });
        let reqs = unsafe { dev.get_image_memory_requirements(image) };
        let mem_type = self
            .core
            .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { dev.allocate_memory(&alloc, None) }?;
        let mem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(memory, None) });
        unsafe { dev.bind_image_memory(image, memory, 0) }?;
        let view = self.create_target_view(image)?;
        let view_guard = crate::util::OnDrop(|| unsafe { dev.destroy_image_view(view, None) });

        let target = FrameTarget {
            image,
            view,
            width: w,
            height: h,
            undefined: true,
        };
        let frame = self.render(&target, Some([0.0, 0.0, 0.0, 1.0]), ops, waits)?;
        frame.wait(self)?;

        // staging buffer, host visible
        let size = (w as u64) * (h as u64) * 4;
        let binfo = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { dev.create_buffer(&binfo, None) }?;
        let buf_guard = crate::util::OnDrop(|| unsafe { dev.destroy_buffer(buffer, None) });
        let breqs = unsafe { dev.get_buffer_memory_requirements(buffer) };
        let btype = self.core.find_memory_type(
            breqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let balloc = vk::MemoryAllocateInfo::default()
            .allocation_size(breqs.size)
            .memory_type_index(btype);
        let bmem = unsafe { dev.allocate_memory(&balloc, None) }?;
        let bmem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(bmem, None) });
        unsafe { dev.bind_buffer_memory(buffer, bmem, 0) }?;

        // one-shot copy; render released the target to GENERAL
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::TRANSIENT)
            .queue_family_index(self.core.queue_family);
        let pool = unsafe { dev.create_command_pool(&pool_info, None) }?;
        let pool_guard = crate::util::OnDrop(|| unsafe { dev.destroy_command_pool(pool, None) });
        let cba = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { dev.allocate_command_buffers(&cba) }?[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            dev.begin_command_buffer(cb, &begin)?;
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
            dev.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::GENERAL, buffer, &[region]);
            dev.end_command_buffer(cb)?;
            let cbs = [cb];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs);
            dev.queue_submit(self.core.queue, &[submit], vk::Fence::null())?;
            dev.queue_wait_idle(self.core.queue)?;
        }

        let mut out = vec![0u8; size as usize];
        unsafe {
            let ptr = dev.map_memory(bmem, 0, size, vk::MemoryMapFlags::empty())?;
            std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), out.as_mut_ptr(), size as usize);
            dev.unmap_memory(bmem);
        }
        drop(pool_guard);
        drop(bmem_guard);
        drop(buf_guard);
        drop(view_guard);
        drop(mem_guard);
        drop(img_guard);
        Ok(out)
    }

    /// synchronous one-shot layout transition; import-time work, not the
    /// frame loop
    fn transition_sampled(&self, image: vk::Image) -> Result<(), RenderError> {
        let dev = &self.core.device;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::TRANSIENT)
            .queue_family_index(self.core.queue_family);
        let pool = unsafe { dev.create_command_pool(&pool_info, None) }?;
        let pool_guard = crate::util::OnDrop(|| unsafe { dev.destroy_command_pool(pool, None) });
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { dev.allocate_command_buffers(&alloc) }?[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { dev.begin_command_buffer(cb, &begin) }?;
        let barrier = image_barrier(image)
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        barrier2(dev, cb, &[barrier]);
        unsafe { dev.end_command_buffer(cb) }?;
        let cbs = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        unsafe {
            dev.queue_submit(self.core.queue, &[submit], vk::Fence::null())?;
            dev.queue_wait_idle(self.core.queue)?;
        }
        drop(pool_guard);
        Ok(())
    }

    /// blocking staging upload; `fill` writes tightly packed rows into the
    /// staging slice. bring-up grade.
    pub fn upload_texture(
        &self,
        tex: &Texture,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<(), RenderError> {
        let dev = &self.core.device;
        let size = (tex.width * tex.height * 4) as u64;
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC);
        let buf = unsafe { dev.create_buffer(&buf_info, None) }?;
        let buf_guard = crate::util::OnDrop(|| unsafe { dev.destroy_buffer(buf, None) });
        let reqs = unsafe { dev.get_buffer_memory_requirements(buf) };
        let mem_type = self.core.find_memory_type(
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let mem = unsafe { dev.allocate_memory(&alloc, None) }?;
        let mem_guard = crate::util::OnDrop(|| unsafe { dev.free_memory(mem, None) });
        unsafe {
            dev.bind_buffer_memory(buf, mem, 0)?;
            let ptr = dev.map_memory(mem, 0, size, vk::MemoryMapFlags::empty())? as *mut u8;
            fill(std::slice::from_raw_parts_mut(ptr, size as usize));
            dev.unmap_memory(mem);
        }

        let cb = self.take_cb()?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { dev.begin_command_buffer(cb, &begin) }?;
        let to_dst = image_barrier(tex.image)
            .old_layout(if tex.undefined.get() {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            })
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE);
        barrier2(dev, cb, &[to_dst]);
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: tex.width,
                height: tex.height,
                depth: 1,
            });
        unsafe {
            dev.cmd_copy_buffer_to_image(
                cb,
                buf,
                tex.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
        }
        let to_sample = image_barrier(tex.image)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ);
        barrier2(dev, cb, &[to_sample]);
        unsafe { dev.end_command_buffer(cb) }?;

        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }?;
        let cbs = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = vk::SubmitInfo2::default().command_buffer_infos(&cbs);
        let res = unsafe {
            dev.queue_submit2(self.core.queue, &[submit], fence)
                .and_then(|_| dev.wait_for_fences(&[fence], true, u64::MAX))
        };
        unsafe { dev.destroy_fence(fence, None) };
        self.free_cbs.borrow_mut().push(cb);
        drop(buf_guard);
        drop(mem_guard);
        res?;
        tex.undefined.set(false);
        Ok(())
    }
}

pub struct Texture {
    pub image: vk::Image,
    memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
    undefined: std::cell::Cell<bool>,
    /// never reused; view handles can be, so identity checks go through this
    pub uid: u64,
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let dev = &self.core.device;
        unsafe {
            let _ = dev.device_wait_idle();
            dev.destroy_pipeline(self.pipes.fill_opaque, None);
            dev.destroy_pipeline(self.pipes.fill_blend, None);
            dev.destroy_pipeline(self.pipes.tex_opaque, None);
            dev.destroy_pipeline(self.pipes.tex_blend, None);
            dev.destroy_pipeline_layout(self.fill_layout, None);
            dev.destroy_pipeline_layout(self.tex_layout, None);
            dev.destroy_descriptor_set_layout(self.tex_set_layout, None);
            dev.destroy_sampler(self.sampler, None);
            dev.destroy_command_pool(self.pool, None);
        }
    }
}

// -- headless diagnostic (`carrot render-probe`) --

/// end to end without drm master, per card: dumb buffer -> vulkan import ->
/// quads -> readback -> pixel check. proves shaders, blend, barriers and the
/// tier-2 scanout write path.
pub fn probe() -> i32 {
    use crate::drm::sys;
    use rustix::fs::{Mode, OFlags, open};
    use std::os::fd::AsFd;

    let mut cards: Vec<_> = match std::fs::read_dir("/dev/dri") {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            eprintln!("cannot read /dev/dri: {e}");
            return 1;
        }
    };
    cards.sort();
    let mut failed = false;
    for path in cards {
        println!("=== {} ===", path.display());
        match probe_card(&path) {
            Ok(()) => println!("PASS"),
            Err(e) => {
                println!("FAIL: {e}");
                failed = true;
            }
        }
    }
    failed as i32
}

fn probe_card(path: &std::path::Path) -> Result<(), String> {
    use crate::allocator::import_linear_bo;
    use crate::drm::sys;
    use rustix::fs::{Mode, OFlags, open};
    use std::os::fd::AsFd;

    const W: u32 = 256;
    const H: u32 = 256;

    let card = open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("open: {e}"))?;
    let core = Rc::new(VkCore::new(card.as_fd()).map_err(|e| format!("vulkan: {e}"))?);
    println!("vulkan device: {}", core.device_name);
    let renderer =
        Renderer::new(&core, vk::Format::B8G8R8A8_UNORM).map_err(|e| format!("renderer: {e}"))?;

    let db = sys::create_dumb(card.as_fd(), W, H, 32).map_err(|e| format!("create_dumb: {e}"))?;
    let dmabuf =
        sys::prime_handle_to_fd(card.as_fd(), db.handle).map_err(|e| format!("prime: {e}"))?;
    let bo = import_linear_bo(&core, dmabuf, W, H, db.pitch, db.size, vk::Format::B8G8R8A8_UNORM)
        .map_err(|e| format!("import: {e}"))?;
    let view = renderer
        .create_target_view(bo.image)
        .map_err(|e| format!("view: {e}"))?;
    let target = FrameTarget {
        image: bo.image,
        view,
        width: W,
        height: H,
        undefined: true,
    };

    // blue clear, red left half
    let ops = [RenderOp::Fill {
        pos: [-1.0, -1.0],
        size: [1.0, 2.0],
        color: [1.0, 0.0, 0.0, 1.0],
    }];
    let frame = renderer
        .render(&target, Some([0.0, 0.0, 1.0, 1.0]), &ops, Vec::new())
        .map_err(|e| format!("render: {e}"))?;
    frame.wait(&renderer).map_err(|e| format!("wait: {e}"))?;

    let px = renderer
        .readback(&target)
        .map_err(|e| format!("readback: {e}"))?;
    let at = |x: u32, y: u32| {
        let o = ((y * W + x) * 4) as usize;
        (px[o], px[o + 1], px[o + 2], px[o + 3])
    };
    // B8G8R8A8: bytes are b,g,r,a
    let left = at(64, 128);
    let right = at(192, 128);
    if left != (0, 0, 255, 255) {
        return Err(format!("left half should be red, got {left:?}"));
    }
    if right != (255, 0, 0, 255) {
        return Err(format!("right half should be blue, got {right:?}"));
    }
    println!("readback ok (left red, right blue)");

    // cross-check through the dumb mapping - the bytes the display engine
    // would scan out
    let offset = sys::map_dumb(card.as_fd(), db.handle).map_err(|e| format!("map_dumb: {e}"))?;
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            db.size as usize,
            rustix::mm::ProtFlags::READ,
            rustix::mm::MapFlags::SHARED,
            card.as_fd(),
            offset,
        )
        .map_err(|e| format!("mmap: {e}"))?
    } as *const u8;
    let scan_at = |x: u32, y: u32| unsafe {
        let p = map.add((y * db.pitch + x * 4) as usize);
        (*p, *p.add(1), *p.add(2), *p.add(3))
    };
    let scan_left = scan_at(64, 128);
    let scan_right = scan_at(192, 128);
    unsafe {
        let _ = rustix::mm::munmap(map.cast_mut().cast(), db.size as usize);
    }
    if scan_left != (0, 0, 255, 255) || scan_right != (255, 0, 0, 255) {
        return Err(format!(
            "dumb mapping disagrees: left {scan_left:?}, right {scan_right:?}"
        ));
    }
    println!("scanout bytes ok");

    unsafe { core.device.destroy_image_view(view, None) };
    bo.destroy(&core);
    drop(renderer);
    let _ = sys::destroy_dumb(card.as_fd(), db.handle);
    Ok(())
}

// -- helpers --

fn image_barrier<'a>(image: vk::Image) -> vk::ImageMemoryBarrier2<'a> {
    vk::ImageMemoryBarrier2::default()
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )
}

fn barrier2(dev: &ash::Device, cb: vk::CommandBuffer, barriers: &[vk::ImageMemoryBarrier2]) {
    let dep = vk::DependencyInfo::default().image_memory_barriers(barriers);
    unsafe { dev.cmd_pipeline_barrier2(cb, &dep) };
}

fn create_module(dev: &ash::Device, words: &[u32]) -> Result<vk::ShaderModule, RenderError> {
    let info = vk::ShaderModuleCreateInfo::default().code(words);
    Ok(unsafe { dev.create_shader_module(&info, None) }?)
}

fn create_pipeline(
    dev: &ash::Device,
    format: vk::Format,
    module: vk::ShaderModule,
    layout: vk::PipelineLayout,
    blend: bool,
) -> Result<vk::Pipeline, RenderError> {
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(c"vs_main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(module)
            .name(c"fs_main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // premultiplied over: ONE / ONE_MINUS_SRC_ALPHA on color and alpha
    let mut attachment = vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    if blend {
        attachment = attachment
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD);
    }
    let attachments = [attachment];
    let blend_state = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
    let dynamic =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&[
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
        ]);
    let formats = [format];
    let mut rendering =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend_state)
        .dynamic_state(&dynamic)
        .layout(layout)
        .push_next(&mut rendering);
    let pipes = unsafe {
        dev.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
            .map_err(|(_, e)| e)
    }?;
    Ok(pipes[0])
}
