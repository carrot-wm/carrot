// kernel kms abi - opcodes, repr(C) structs, thin wrappers. no libdrm;
// this file is the entire surface between carrot and the drm uapi.

use rustix::io::Errno;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, ioctl, opcode};
use std::ffi::c_void;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};

// -- ioctl plumbing --

struct DrmIoctl<'a, T> {
    opcode: Opcode,
    data: &'a mut T,
}

unsafe impl<T> Ioctl for DrmIoctl<'_, T> {
    type Output = ();
    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        self.opcode
    }

    fn as_ptr(&mut self) -> *mut c_void {
        (self.data as *mut T).cast()
    }

    unsafe fn output_from_ptr(_: IoctlOutput, _: *mut c_void) -> rustix::io::Result<()> {
        Ok(())
    }
}

fn drm_ioctl<T>(fd: BorrowedFd<'_>, opcode: Opcode, data: &mut T) -> Result<(), Errno> {
    loop {
        match unsafe { ioctl(fd, DrmIoctl { opcode, data }) } {
            Err(Errno::INTR | Errno::AGAIN) => continue,
            r => return r,
        }
    }
}

const fn iowr<T>(nr: u8) -> Opcode {
    opcode::read_write::<T>(b'd', nr)
}

const fn iow<T>(nr: u8) -> Opcode {
    opcode::write::<T>(b'd', nr)
}

// -- caps --

pub const CAP_CURSOR_WIDTH: u64 = 0x8;
pub const CAP_CURSOR_HEIGHT: u64 = 0x9;
pub const CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;
pub const CLIENT_CAP_ATOMIC: u64 = 3;

#[repr(C)]
#[derive(Default)]
struct drm_get_cap {
    capability: u64,
    value: u64,
}

pub fn get_cap(fd: BorrowedFd<'_>, cap: u64) -> Result<u64, Errno> {
    let mut d = drm_get_cap {
        capability: cap,
        ..Default::default()
    };
    drm_ioctl(fd, iowr::<drm_get_cap>(0x0c), &mut d)?;
    Ok(d.value)
}

#[repr(C)]
struct drm_set_client_cap {
    capability: u64,
    value: u64,
}

pub fn set_client_cap(fd: BorrowedFd<'_>, cap: u64, value: u64) -> Result<(), Errno> {
    let mut d = drm_set_client_cap {
        capability: cap,
        value,
    };
    drm_ioctl(fd, iow::<drm_set_client_cap>(0x0d), &mut d)
}

// -- resources --

#[repr(C)]
#[derive(Default)]
struct drm_mode_card_res {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

pub struct CardResources {
    pub crtcs: Vec<u32>,
    pub connectors: Vec<u32>,
    pub encoders: Vec<u32>,
}

pub fn resources(fd: BorrowedFd<'_>) -> Result<CardResources, Errno> {
    let mut crtcs = Vec::new();
    let mut connectors = Vec::new();
    let mut encoders = Vec::new();
    loop {
        let mut d = drm_mode_card_res {
            crtc_id_ptr: crtcs.as_mut_ptr() as u64,
            connector_id_ptr: connectors.as_mut_ptr() as u64,
            encoder_id_ptr: encoders.as_mut_ptr() as u64,
            count_crtcs: crtcs.capacity() as u32,
            count_connectors: connectors.capacity() as u32,
            count_encoders: encoders.capacity() as u32,
            ..Default::default()
        };
        drm_ioctl(fd, iowr::<drm_mode_card_res>(0xa0), &mut d)?;
        let fits = d.count_crtcs as usize <= crtcs.capacity()
            && d.count_connectors as usize <= connectors.capacity()
            && d.count_encoders as usize <= encoders.capacity();
        if fits {
            unsafe {
                crtcs.set_len(d.count_crtcs as usize);
                connectors.set_len(d.count_connectors as usize);
                encoders.set_len(d.count_encoders as usize);
            }
            return Ok(CardResources {
                crtcs,
                connectors,
                encoders,
            });
        }
        crtcs.reserve(d.count_crtcs as usize);
        connectors.reserve(d.count_connectors as usize);
        encoders.reserve(d.count_encoders as usize);
    }
}

// -- modes --

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub ty: u32,
    pub name: [u8; 32],
}

/// kernel name buffers: fixed 32 bytes, nul-terminated unless full
fn fixed_name(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

impl ModeInfo {
    pub fn name(&self) -> String {
        fixed_name(&self.name)
    }
}

// -- connectors / encoders --

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_connector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

pub struct ConnectorInfo {
    pub id: u32,
    /// 1 = connected
    pub connection: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub encoders: Vec<u32>,
    pub modes: Vec<ModeInfo>,
}

pub fn connector(fd: BorrowedFd<'_>, id: u32, force_probe: bool) -> Result<ConnectorInfo, Errno> {
    let mut encoders: Vec<u32> = Vec::new();
    let mut modes: Vec<ModeInfo> = Vec::new();
    let mut first = true;
    loop {
        // zero mode count means "re-probe the link"; the no-probe path needs a
        // nonzero count pointing at real memory the kernel can copy through
        if modes.capacity() == 0 {
            modes.reserve(1);
        }
        let mut d = drm_mode_get_connector {
            connector_id: id,
            encoders_ptr: encoders.as_mut_ptr() as u64,
            modes_ptr: modes.as_mut_ptr() as u64,
            count_encoders: encoders.capacity() as u32,
            count_modes: if first && force_probe {
                0
            } else {
                modes.capacity() as u32
            },
            ..Default::default()
        };
        if first && force_probe {
            // dedicated probe pass; results discarded
            drm_ioctl(fd, iowr::<drm_mode_get_connector>(0xa7), &mut d)?;
            first = false;
            modes.reserve(d.count_modes as usize);
            encoders.reserve(d.count_encoders as usize);
            continue;
        }
        drm_ioctl(fd, iowr::<drm_mode_get_connector>(0xa7), &mut d)?;
        let fits = d.count_encoders as usize <= encoders.capacity()
            && d.count_modes as usize <= modes.capacity();
        if fits {
            unsafe {
                encoders.set_len(d.count_encoders as usize);
                modes.set_len(d.count_modes as usize);
            }
            return Ok(ConnectorInfo {
                id,
                connection: d.connection,
                connector_type: d.connector_type,
                connector_type_id: d.connector_type_id,
                encoders,
                modes,
            });
        }
        encoders.reserve(d.count_encoders as usize);
        modes.reserve(d.count_modes as usize);
        first = false;
    }
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_encoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

pub fn encoder_possible_crtcs(fd: BorrowedFd<'_>, id: u32) -> Result<u32, Errno> {
    let mut d = drm_mode_get_encoder {
        encoder_id: id,
        ..Default::default()
    };
    drm_ioctl(fd, iowr::<drm_mode_get_encoder>(0xa6), &mut d)?;
    Ok(d.possible_crtcs)
}

// -- planes --

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_plane_res {
    plane_id_ptr: u64,
    count_planes: u32,
}

pub fn plane_resources(fd: BorrowedFd<'_>) -> Result<Vec<u32>, Errno> {
    let mut planes: Vec<u32> = Vec::new();
    loop {
        let mut d = drm_mode_get_plane_res {
            plane_id_ptr: planes.as_mut_ptr() as u64,
            count_planes: planes.capacity() as u32,
        };
        drm_ioctl(fd, iowr::<drm_mode_get_plane_res>(0xb5), &mut d)?;
        if d.count_planes as usize <= planes.capacity() {
            unsafe { planes.set_len(d.count_planes as usize) };
            return Ok(planes);
        }
        planes.reserve(d.count_planes as usize);
    }
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_plane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

pub struct PlaneInfo {
    pub id: u32,
    pub possible_crtcs: u32,
    pub formats: Vec<u32>,
}

pub fn plane(fd: BorrowedFd<'_>, id: u32) -> Result<PlaneInfo, Errno> {
    let mut formats: Vec<u32> = Vec::new();
    loop {
        let mut d = drm_mode_get_plane {
            plane_id: id,
            format_type_ptr: formats.as_mut_ptr() as u64,
            count_format_types: formats.capacity() as u32,
            ..Default::default()
        };
        drm_ioctl(fd, iowr::<drm_mode_get_plane>(0xb6), &mut d)?;
        if d.count_format_types as usize <= formats.capacity() {
            unsafe { formats.set_len(d.count_format_types as usize) };
            return Ok(PlaneInfo {
                id,
                possible_crtcs: d.possible_crtcs,
                formats,
            });
        }
        formats.reserve(d.count_format_types as usize);
    }
}

// -- properties --

pub const OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const OBJECT_PLANE: u32 = 0xeeee_eeee;

#[repr(C)]
#[derive(Default)]
struct drm_mode_obj_get_properties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

pub fn object_properties(
    fd: BorrowedFd<'_>,
    obj_id: u32,
    obj_type: u32,
) -> Result<Vec<(u32, u64)>, Errno> {
    let mut props: Vec<u32> = Vec::new();
    let mut values: Vec<u64> = Vec::new();
    loop {
        let mut d = drm_mode_obj_get_properties {
            props_ptr: props.as_mut_ptr() as u64,
            prop_values_ptr: values.as_mut_ptr() as u64,
            count_props: props.capacity().min(values.capacity()) as u32,
            obj_id,
            obj_type,
        };
        drm_ioctl(fd, iowr::<drm_mode_obj_get_properties>(0xb9), &mut d)?;
        if d.count_props as usize <= props.capacity().min(values.capacity()) {
            unsafe {
                props.set_len(d.count_props as usize);
                values.set_len(d.count_props as usize);
            }
            return Ok(props.into_iter().zip(values).collect());
        }
        props.reserve(d.count_props as usize);
        values.reserve(d.count_props as usize);
    }
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_property {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct PropertyEnum {
    pub value: u64,
    pub name: [u8; 32],
}

pub struct PropertyMeta {
    pub name: String,
    pub flags: u32,
    /// for RANGE props: [min, max]
    pub values: Vec<u64>,
    pub enums: Vec<PropertyEnum>,
}

impl PropertyEnum {
    pub fn name(&self) -> String {
        fixed_name(&self.name)
    }

    pub fn value(&self) -> u64 {
        self.value
    }
}

pub fn property_meta(fd: BorrowedFd<'_>, prop_id: u32) -> Result<PropertyMeta, Errno> {
    let mut values: Vec<u64> = Vec::new();
    let mut enums: Vec<PropertyEnum> = Vec::new();
    loop {
        let mut d = drm_mode_get_property {
            prop_id,
            values_ptr: values.as_mut_ptr() as u64,
            enum_blob_ptr: enums.as_mut_ptr() as u64,
            count_values: values.capacity() as u32,
            count_enum_blobs: enums.capacity() as u32,
            ..Default::default()
        };
        drm_ioctl(fd, iowr::<drm_mode_get_property>(0xaa), &mut d)?;
        let fits = d.count_values as usize <= values.capacity()
            && d.count_enum_blobs as usize <= enums.capacity();
        if fits {
            unsafe {
                values.set_len(d.count_values as usize);
                enums.set_len(d.count_enum_blobs as usize);
            }
            return Ok(PropertyMeta {
                name: fixed_name(&d.name),
                flags: d.flags,
                values,
                enums,
            });
        }
        values.reserve(d.count_values as usize);
        enums.reserve(d.count_enum_blobs as usize);
    }
}

// -- blobs --

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_blob {
    blob_id: u32,
    length: u32,
    data: u64,
}

pub fn get_blob(fd: BorrowedFd<'_>, blob_id: u32) -> Result<Vec<u8>, Errno> {
    let mut data: Vec<u8> = Vec::new();
    loop {
        let mut d = drm_mode_get_blob {
            blob_id,
            length: data.capacity() as u32,
            data: data.as_mut_ptr() as u64,
        };
        drm_ioctl(fd, iowr::<drm_mode_get_blob>(0xac), &mut d)?;
        if d.length as usize <= data.capacity() {
            unsafe { data.set_len(d.length as usize) };
            return Ok(data);
        }
        data.reserve(d.length as usize);
    }
}

#[repr(C)]
struct drm_mode_create_blob {
    data: u64,
    length: u32,
    blob_id: u32,
}

pub fn create_blob(fd: BorrowedFd<'_>, data: &[u8]) -> Result<u32, Errno> {
    let mut d = drm_mode_create_blob {
        data: data.as_ptr() as u64,
        length: data.len() as u32,
        blob_id: 0,
    };
    drm_ioctl(fd, iowr::<drm_mode_create_blob>(0xbd), &mut d)?;
    Ok(d.blob_id)
}

#[repr(C)]
struct drm_mode_destroy_blob {
    blob_id: u32,
}

pub fn destroy_blob(fd: BorrowedFd<'_>, blob_id: u32) -> Result<(), Errno> {
    let mut d = drm_mode_destroy_blob { blob_id };
    drm_ioctl(fd, iowr::<drm_mode_destroy_blob>(0xbe), &mut d)
}

// -- framebuffers --

pub const FB_MODIFIERS: u32 = 1 << 1;
pub const INVALID_MODIFIER: u64 = 0x00ff_ffff_ffff_ffff;

#[repr(C)]
#[derive(Default)]
struct drm_mode_fb_cmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[allow(clippy::too_many_arguments)]
pub fn addfb2(
    fd: BorrowedFd<'_>,
    width: u32,
    height: u32,
    fourcc: u32,
    handles: &[u32],
    pitches: &[u32],
    offsets: &[u32],
    modifier: Option<u64>,
) -> Result<u32, Errno> {
    let mut d = drm_mode_fb_cmd2 {
        width,
        height,
        pixel_format: fourcc,
        ..Default::default()
    };
    for i in 0..handles.len().min(4) {
        d.handles[i] = handles[i];
        d.pitches[i] = pitches[i];
        d.offsets[i] = offsets[i];
        if let Some(m) = modifier {
            d.modifier[i] = m;
            d.flags |= FB_MODIFIERS;
        }
    }
    drm_ioctl(fd, iowr::<drm_mode_fb_cmd2>(0xb8), &mut d)?;
    Ok(d.fb_id)
}

pub fn rmfb(fd: BorrowedFd<'_>, fb_id: u32) -> Result<(), Errno> {
    let mut id: u32 = fb_id;
    drm_ioctl(fd, iowr::<u32>(0xaf), &mut id)
}

// -- dumb buffers --
// cursor plane material; also handy as a known-good addfb2 source

#[repr(C)]
#[derive(Default)]
struct drm_mode_create_dumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

pub struct DumbBuffer {
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

pub fn create_dumb(
    fd: BorrowedFd<'_>,
    width: u32,
    height: u32,
    bpp: u32,
) -> Result<DumbBuffer, Errno> {
    let mut d = drm_mode_create_dumb {
        height,
        width,
        bpp,
        ..Default::default()
    };
    drm_ioctl(fd, iowr::<drm_mode_create_dumb>(0xb2), &mut d)?;
    Ok(DumbBuffer {
        handle: d.handle,
        pitch: d.pitch,
        size: d.size,
    })
}

pub fn map_dumb(fd: BorrowedFd<'_>, handle: u32) -> Result<u64, Errno> {
    #[repr(C)]
    #[derive(Default)]
    struct drm_mode_map_dumb {
        handle: u32,
        pad: u32,
        offset: u64,
    }
    let mut d = drm_mode_map_dumb {
        handle,
        ..Default::default()
    };
    drm_ioctl(fd, iowr::<drm_mode_map_dumb>(0xb3), &mut d)?;
    Ok(d.offset)
}

pub fn destroy_dumb(fd: BorrowedFd<'_>, handle: u32) -> Result<(), Errno> {
    #[repr(C)]
    struct drm_mode_destroy_dumb {
        handle: u32,
    }
    let mut d = drm_mode_destroy_dumb { handle };
    drm_ioctl(fd, iowr::<drm_mode_destroy_dumb>(0xb4), &mut d)
}

// -- gem / prime --

#[repr(C)]
struct drm_prime_handle {
    handle: u32,
    flags: u32,
    fd: i32,
}

pub fn prime_fd_to_handle(fd: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> Result<u32, Errno> {
    use std::os::fd::AsRawFd;
    let mut d = drm_prime_handle {
        handle: 0,
        flags: 0,
        fd: dmabuf.as_raw_fd(),
    };
    drm_ioctl(fd, iowr::<drm_prime_handle>(0x2e), &mut d)?;
    Ok(d.handle)
}

pub fn prime_handle_to_fd(fd: BorrowedFd<'_>, handle: u32) -> Result<OwnedFd, Errno> {
    use rustix::fs::OFlags;
    let mut d = drm_prime_handle {
        handle,
        // DRM_CLOEXEC | DRM_RDWR are literally the O_ flags
        flags: (OFlags::CLOEXEC | OFlags::RDWR).bits(),
        fd: -1,
    };
    drm_ioctl(fd, iowr::<drm_prime_handle>(0x2d), &mut d)?;
    Ok(unsafe { OwnedFd::from_raw_fd(d.fd) })
}

#[repr(C)]
struct drm_gem_close {
    handle: u32,
    pad: u32,
}

pub fn gem_close(fd: BorrowedFd<'_>, handle: u32) -> Result<(), Errno> {
    let mut d = drm_gem_close { handle, pad: 0 };
    drm_ioctl(fd, iow::<drm_gem_close>(0x09), &mut d)
}

// -- atomic --

#[repr(C)]
struct drm_mode_atomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

#[allow(clippy::too_many_arguments)]
pub fn atomic(
    fd: BorrowedFd<'_>,
    flags: u32,
    objects: &[u32],
    lengths: &[u32],
    props: &[u32],
    values: &[u64],
    user_data: u64,
) -> Result<(), Errno> {
    debug_assert_eq!(lengths.iter().sum::<u32>() as usize, props.len());
    if objects.is_empty() {
        return Ok(());
    }
    let mut d = drm_mode_atomic {
        flags,
        count_objs: objects.len() as u32,
        objs_ptr: objects.as_ptr() as u64,
        count_props_ptr: lengths.as_ptr() as u64,
        props_ptr: props.as_ptr() as u64,
        prop_values_ptr: values.as_ptr() as u64,
        reserved: 0,
        user_data,
    };
    drm_ioctl(fd, iowr::<drm_mode_atomic>(0xbc), &mut d)
}

// -- IN_FORMATS blob --

#[repr(C)]
struct drm_format_modifier_blob {
    version: u32,
    flags: u32,
    count_formats: u32,
    formats_offset: u32,
    count_modifiers: u32,
    modifiers_offset: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct drm_format_modifier {
    formats: u64,
    offset: u32,
    pad: u32,
    modifier: u64,
}

/// per-plane format -> supported modifiers, from the IN_FORMATS property
pub fn parse_in_formats(blob: &[u8]) -> Vec<(u32, Vec<u64>)> {
    let hdr_len = size_of::<drm_format_modifier_blob>();
    if blob.len() < hdr_len {
        return Vec::new();
    }
    // get_blob buffers are only align-1; never form a reference here
    let hdr =
        unsafe { (blob.as_ptr() as *const drm_format_modifier_blob).read_unaligned() };
    if hdr.version != 1 {
        return Vec::new();
    }
    let f_off = hdr.formats_offset as usize;
    let m_off = hdr.modifiers_offset as usize;
    let f_end = f_off + hdr.count_formats as usize * 4;
    let m_end = m_off + hdr.count_modifiers as usize * size_of::<drm_format_modifier>();
    if f_end > blob.len() || m_end > blob.len() {
        return Vec::new();
    }
    let mut formats: Vec<(u32, Vec<u64>)> = (0..hdr.count_formats as usize)
        .map(|i| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&blob[f_off + i * 4..f_off + i * 4 + 4]);
            (u32::from_ne_bytes(b), Vec::new())
        })
        .collect();
    for i in 0..hdr.count_modifiers as usize {
        let at = m_off + i * size_of::<drm_format_modifier>();
        // read unaligned; blob buffers are align-1
        let m = unsafe { (blob.as_ptr().add(at) as *const drm_format_modifier).read_unaligned() };
        for bit in 0..64 {
            if m.formats & (1 << bit) != 0 {
                let idx = m.offset as usize + bit;
                if let Some((_, mods)) = formats.get_mut(idx) {
                    mods.push(m.modifier);
                }
            }
        }
    }
    formats
}

// -- events --

pub const EVENT_FLIP_COMPLETE: u32 = 0x02;

#[repr(C)]
struct drm_event {
    ty: u32,
    length: u32,
}

pub struct FlipComplete {
    pub crtc_id: u32,
    pub sequence: u32,
    pub tv_sec: u32,
    pub tv_usec: u32,
}

/// parse packed drm events, yielding flip completions
pub fn parse_flip_events(buf: &[u8]) -> Vec<FlipComplete> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + size_of::<drm_event>() <= buf.len() {
        let ev = unsafe { (buf.as_ptr().add(off) as *const drm_event).read_unaligned() };
        if ev.length as usize == 0 || off + ev.length as usize > buf.len() {
            break;
        }
        if ev.ty == EVENT_FLIP_COMPLETE && ev.length as usize >= 32 {
            // drm_event_vblank: base(8) user_data(8) tv_sec tv_usec sequence crtc_id
            let f = |at: usize| {
                let mut b = [0u8; 4];
                b.copy_from_slice(&buf[off + at..off + at + 4]);
                u32::from_ne_bytes(b)
            };
            out.push(FlipComplete {
                tv_sec: f(16),
                tv_usec: f(20),
                sequence: f(24),
                crtc_id: f(28),
            });
        }
        off += ev.length as usize;
    }
    out
}
