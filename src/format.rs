// pixel formats. render-side fields arrive with the renderer; for now the
// table carries what wl_shm needs.

pub struct Format {
    pub name: &'static str,
    pub drm: u32,
    /// wl_shm enum value where it differs from the fourcc
    pub wl_id: Option<u32>,
    /// NOT the drm sense of bpp - dumb-buffer ioctls want bits
    pub bytes_per_pixel: u32,
}

impl Format {
    pub fn has_alpha(&self) -> bool {
        self.drm == ARGB8888.drm
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

pub static ARGB8888: Format = Format {
    name: "argb8888",
    drm: fourcc(b"AR24"),
    wl_id: Some(0),
    bytes_per_pixel: 4,
};

pub static XRGB8888: Format = Format {
    name: "xrgb8888",
    drm: fourcc(b"XR24"),
    wl_id: Some(1),
    bytes_per_pixel: 4,
};

/// advertised through wl_shm; the renderer narrows it by real support later
static SHM_FORMATS: [&Format; 2] = [&ARGB8888, &XRGB8888];

pub fn shm_formats() -> &'static [&'static Format] {
    &SHM_FORMATS
}

/// wl_shm speaks 0/1 for the two mandatory formats and fourcc for the rest
pub fn map_wayland_format_id(id: u32) -> u32 {
    match id {
        0 => ARGB8888.drm,
        1 => XRGB8888.drm,
        other => other,
    }
}

pub fn shm_format_by_wl(id: u32) -> Option<&'static Format> {
    let drm = map_wayland_format_id(id);
    shm_formats().iter().find(|f| f.drm == drm).copied()
}
