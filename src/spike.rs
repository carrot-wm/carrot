// scanout de-risk spike: vulkan-native buffers straight to the display, no
// gbm in the path. run from a tty as `carrot spike-scanout`; paints each
// connected display red/blue for ~2s per card and reports PASS/FAIL.

use crate::allocator::{ScanoutBo, create_scanout_bo, fill_solid, import_linear_bo};
use crate::drm::{ObjId, PropId, atomic, sys};
use crate::format::XRGB8888;
use crate::render::vulkan::VkCore;
use ash::vk;
use rustix::fs::{Mode, OFlags, open};
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;

type R<T> = Result<T, String>;

enum Outcome {
    Pass,
    Skip(String),
}

pub fn run() -> i32 {
    let mut cards: Vec<PathBuf> = match std::fs::read_dir("/dev/dri") {
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
    if cards.is_empty() {
        eprintln!("no drm cards found");
        return 1;
    }
    let mut passed = 0;
    let mut failed = 0;
    for path in &cards {
        println!("=== {} ===", path.display());
        match run_card(path) {
            Ok(Outcome::Pass) => {
                println!("PASS: {}", path.display());
                passed += 1;
            }
            Ok(Outcome::Skip(why)) => println!("SKIP: {}: {why}", path.display()),
            Err(e) => {
                println!("FAIL: {}: {e}", path.display());
                failed += 1;
            }
        }
    }
    println!("--- {passed} passed, {failed} failed ---");
    if failed > 0 || passed == 0 { 1 } else { 0 }
}

struct Props {
    ids: HashMap<String, (u32, u64)>,
}

impl Props {
    fn of(fd: std::os::fd::BorrowedFd<'_>, obj: u32, ty: u32) -> R<Props> {
        let raw = sys::object_properties(fd, obj, ty)
            .map_err(|e| format!("object {obj} properties: {e}"))?;
        let mut ids = HashMap::new();
        for (prop, value) in raw {
            let meta =
                sys::property_meta(fd, prop).map_err(|e| format!("property {prop} meta: {e}"))?;
            ids.insert(meta.name, (prop, value));
        }
        Ok(Props { ids })
    }

    fn id(&self, name: &str) -> R<PropId> {
        self.ids
            .get(name)
            .map(|(id, _)| PropId(*id))
            .ok_or_else(|| format!("missing property {name}"))
    }

    fn value(&self, name: &str) -> Option<u64> {
        self.ids.get(name).map(|(_, v)| *v)
    }
}

fn run_card(path: &PathBuf) -> R<Outcome> {
    let card: OwnedFd = open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("open: {e}"))?;
    let fd = card.as_fd();

    if sys::set_client_cap(fd, sys::CLIENT_CAP_ATOMIC, 2).is_err() {
        sys::set_client_cap(fd, sys::CLIENT_CAP_ATOMIC, 1)
            .map_err(|e| format!("no atomic modesetting: {e}"))?;
    }
    let res = match sys::resources(fd) {
        Ok(r) => r,
        Err(rustix::io::Errno::OPNOTSUPP) => {
            return Ok(Outcome::Skip("render-only node, no kms".into()));
        }
        Err(e) => return Err(format!("resources: {e}")),
    };

    // connected connector with modes
    let mut picked = None;
    for &conn_id in &res.connectors {
        let info =
            sys::connector(fd, conn_id, true).map_err(|e| format!("connector {conn_id}: {e}"))?;
        if info.connection == 1 && !info.modes.is_empty() {
            picked = Some(info);
            break;
        }
    }
    let Some(conn) = picked else {
        return Ok(Outcome::Skip("no connected display".into()));
    };
    let mode = conn.modes[0];
    let (w, h) = (mode.hdisplay as u32, mode.vdisplay as u32);
    println!(
        "connector {} mode {} {}x{}@{}",
        conn.id,
        mode.name(),
        w,
        h,
        mode.vrefresh
    );

    // crtc reachable through the connector's encoders
    let mut crtc = None;
    'outer: for &enc in &conn.encoders {
        let mask =
            sys::encoder_possible_crtcs(fd, enc).map_err(|e| format!("encoder {enc}: {e}"))?;
        for (idx, &c) in res.crtcs.iter().enumerate() {
            if mask & (1 << idx) != 0 {
                crtc = Some((idx, c));
                break 'outer;
            }
        }
    }
    let Some((crtc_idx, crtc_id)) = crtc else {
        return Err("no usable crtc".into());
    };

    // primary plane for that crtc
    let mut primary = None;
    for plane_id in sys::plane_resources(fd).map_err(|e| format!("planes: {e}"))? {
        let p = sys::plane(fd, plane_id).map_err(|e| format!("plane {plane_id}: {e}"))?;
        if p.possible_crtcs & (1 << crtc_idx) == 0 {
            continue;
        }
        let props = Props::of(fd, plane_id, sys::OBJECT_PLANE)?;
        let Some(ty) = props.value("type") else { continue };
        let meta = sys::property_meta(fd, props.id("type")?.0)
            .map_err(|e| format!("plane type meta: {e}"))?;
        let is_primary = meta
            .enums
            .iter()
            .any(|e| e.value == ty && e.name() == "Primary");
        if is_primary {
            primary = Some((plane_id, props));
            break;
        }
    }
    let Some((plane_id, plane_props)) = primary else {
        return Err("no primary plane".into());
    };

    // kms-side modifiers for xrgb8888
    let kms_mods: Vec<u64> = match plane_props.value("IN_FORMATS") {
        Some(blob_id) if blob_id != 0 => {
            let blob =
                sys::get_blob(fd, blob_id as u32).map_err(|e| format!("IN_FORMATS: {e}"))?;
            sys::parse_in_formats(&blob)
                .into_iter()
                .find(|(f, _)| *f == XRGB8888.drm)
                .map(|(_, m)| m)
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let core = VkCore::new(fd).map_err(|e| format!("vulkan: {e}"))?;
    println!("vulkan device: {}", core.device_name);
    let vk_mods = core
        .scanout_modifiers(vk::Format::B8G8R8A8_UNORM)
        .map_err(|e| format!("modifier probe: {e}"))?;
    let candidates: Vec<(u64, u32)> = if kms_mods.is_empty() {
        // no IN_FORMATS: try everything vulkan can export
        vk_mods.clone()
    } else {
        vk_mods
            .iter()
            .filter(|(m, _)| kms_mods.contains(m))
            .copied()
            .collect()
    };
    if candidates.is_empty() {
        return Err(format!(
            "no common modifier (kms: {kms_mods:x?}, vulkan: {:x?})",
            vk_mods.iter().map(|(m, _)| *m).collect::<Vec<_>>()
        ));
    }
    println!("modifier candidates: {:x?}", candidates.iter().map(|(m, _)| *m).collect::<Vec<_>>());

    // two solid-color buffers. tier 1 is vulkan-native allocation; if addfb2
    // rejects everything (anv only marks its own wsi allocations displayable,
    // so intel xe lands here) tier 2 flips it: kms dumb buffers imported into
    // vulkan.
    let mut dumb_handles: Vec<u32> = Vec::new();
    let (bo_a, fb_a, bo_b, fb_b) = match native_bufs(fd, &core, w, h, &candidates) {
        Ok(x) => {
            println!("scanout tier: vulkan-native, modifier {:#x}", x.0.modifier);
            x
        }
        Err(e) => {
            println!("vulkan-native tier failed: {e}");
            diagnose_addfb(fd, &core, w, h);
            println!("falling back to dumb-buffer tier");
            let x = dumb_bufs(fd, &core, w, h, &mut dumb_handles)?;
            println!("scanout tier: dumb-buffer import (linear)");
            x
        }
    };
    println!("addfb2 ok (fb {fb_a}, {fb_b})");
    fill_solid(&core, bo_a.image, [0.8, 0.1, 0.1, 1.0]).map_err(|e| format!("fill a: {e}"))?;
    fill_solid(&core, bo_b.image, [0.1, 0.1, 0.8, 1.0]).map_err(|e| format!("fill b: {e}"))?;

    // modeset onto bo a
    let conn_props = Props::of(fd, conn.id, sys::OBJECT_CONNECTOR)?;
    let crtc_props = Props::of(fd, crtc_id, sys::OBJECT_CRTC)?;
    let mode_bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const mode) as *const u8,
            std::mem::size_of::<sys::ModeInfo>(),
        )
    };
    let mode_blob = sys::create_blob(fd, mode_bytes).map_err(|e| format!("mode blob: {e}"))?;
    let conn_obj = ObjId(conn.id);
    let crtc_obj = ObjId(crtc_id);
    let plane_obj = ObjId(plane_id);
    let mut ch = atomic::Change::default();
    ch.set(conn_obj, conn_props.id("CRTC_ID")?, crtc_id as u64);
    ch.set(crtc_obj, crtc_props.id("ACTIVE")?, 1);
    ch.set(crtc_obj, crtc_props.id("MODE_ID")?, mode_blob as u64);
    ch.set(plane_obj, plane_props.id("FB_ID")?, fb_a as u64);
    ch.set(plane_obj, plane_props.id("CRTC_ID")?, crtc_id as u64);
    ch.set(plane_obj, plane_props.id("SRC_X")?, 0);
    ch.set(plane_obj, plane_props.id("SRC_Y")?, 0);
    ch.set(plane_obj, plane_props.id("SRC_W")?, (w as u64) << 16);
    ch.set(plane_obj, plane_props.id("SRC_H")?, (h as u64) << 16);
    ch.set(plane_obj, plane_props.id("CRTC_X")?, 0);
    ch.set(plane_obj, plane_props.id("CRTC_Y")?, 0);
    ch.set(plane_obj, plane_props.id("CRTC_W")?, w as u64);
    ch.set(plane_obj, plane_props.id("CRTC_H")?, h as u64);
    ch.commit(fd, atomic::ALLOW_MODESET, 0)
        .map_err(|e| format!("modeset commit: {e}"))?;
    println!("modeset ok, flipping for ~2s");

    // alternate buffers, one flip per vblank, ~2 seconds worth
    let flips = (mode.vrefresh.max(30) as u32) * 2;
    let fb_id_prop = plane_props.id("FB_ID")?;
    let mut buf = [0u8; 1024];
    for i in 0..flips {
        let fb = if i % 2 == 0 { fb_b } else { fb_a };
        let mut flip = atomic::Change::default();
        flip.set(plane_obj, fb_id_prop, fb as u64);
        // the flip event fires at vblank but the kernel commit worker finishes
        // its tail work a hair later; at 480hz we can lap it. EBUSY is
        // backpressure, not failure - wait it out.
        let mut spins = 0u32;
        loop {
            match flip.commit(fd, atomic::NONBLOCK | atomic::PAGE_FLIP_EVENT, 0) {
                Ok(()) => break,
                Err(rustix::io::Errno::BUSY) if spins < 10_000 => {
                    spins += 1;
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
                Err(e) => return Err(format!("flip {i}: {e}")),
            }
        }
        loop {
            let n = rustix::io::read(fd, &mut buf).map_err(|e| format!("event read: {e}"))?;
            if !sys::parse_flip_events(&buf[..n]).is_empty() {
                break;
            }
        }
    }
    println!("{flips} flips completed");

    // teardown; the console stays black until a vt switch
    let _ = sys::rmfb(fd, fb_a);
    let _ = sys::rmfb(fd, fb_b);
    let _ = sys::destroy_blob(fd, mode_blob);
    bo_a.destroy(&core);
    bo_b.destroy(&core);
    for h in dumb_handles {
        let _ = sys::destroy_dumb(fd, h);
    }
    Ok(Outcome::Pass)
}

fn native_bufs(
    fd: std::os::fd::BorrowedFd<'_>,
    core: &VkCore,
    w: u32,
    h: u32,
    candidates: &[(u64, u32)],
) -> R<(ScanoutBo, u32, ScanoutBo, u32)> {
    // addfb2 is the real arbiter of what scans out - IN_FORMATS can advertise
    // modifiers the kernel still rejects for a given bo - so drop what it
    // refuses and retry with the rest
    let mut usable = candidates.to_vec();
    let (bo_a, fb_a) = loop {
        let bo = create_scanout_bo(core, w, h, vk::Format::B8G8R8A8_UNORM, &usable)
            .map_err(|e| format!("allocate bo a: {e}"))?;
        println!(
            "bo: modifier {:#x}, {} plane(s), pitch {}, offset {}",
            bo.modifier,
            bo.planes.len(),
            bo.planes[0].pitch,
            bo.planes[0].offset
        );
        match add_fb(fd, &bo) {
            Ok(fb) => break (bo, fb),
            Err(e) => {
                println!("  {e} - dropping modifier, retrying");
                let bad = bo.modifier;
                bo.destroy(core);
                usable.retain(|(m, _)| *m != bad);
                if usable.is_empty() {
                    return Err("addfb2 rejected every candidate modifier".into());
                }
            }
        }
    };
    let winner = [(bo_a.modifier, bo_a.planes.len() as u32)];
    let bo_b = create_scanout_bo(core, w, h, vk::Format::B8G8R8A8_UNORM, &winner)
        .map_err(|e| format!("allocate bo b: {e}"))?;
    let fb_b = add_fb(fd, &bo_b)?;
    Ok((bo_a, fb_a, bo_b, fb_b))
}

fn dumb_bufs(
    fd: std::os::fd::BorrowedFd<'_>,
    core: &VkCore,
    w: u32,
    h: u32,
    handles: &mut Vec<u32>,
) -> R<(ScanoutBo, u32, ScanoutBo, u32)> {
    let mut mk = || -> R<(ScanoutBo, u32)> {
        let db = sys::create_dumb(fd, w, h, 32).map_err(|e| format!("create_dumb: {e}"))?;
        handles.push(db.handle);
        let fb = sys::addfb2(fd, w, h, XRGB8888.drm, &[db.handle], &[db.pitch], &[0], None)
            .map_err(|e| format!("dumb addfb2: {e}"))?;
        let dmabuf =
            sys::prime_handle_to_fd(fd, db.handle).map_err(|e| format!("prime export: {e}"))?;
        let bo = import_linear_bo(
            core,
            dmabuf,
            w,
            h,
            db.pitch,
            db.size,
            vk::Format::B8G8R8A8_UNORM,
        )
        .map_err(|e| format!("vulkan import: {e}"))?;
        Ok((bo, fb))
    };
    let (bo_a, fb_a) = mk()?;
    let (bo_b, fb_b) = mk()?;
    Ok((bo_a, fb_a, bo_b, fb_b))
}

fn add_fb(fd: std::os::fd::BorrowedFd<'_>, bo: &ScanoutBo) -> R<u32> {
    let handle = sys::prime_fd_to_handle(fd, bo.fd.as_fd())
        .map_err(|e| format!("prime fd to handle: {e}"))?;
    let n = bo.planes.len();
    let handles: Vec<u32> = vec![handle; n];
    let pitches: Vec<u32> = bo.planes.iter().map(|p| p.pitch as u32).collect();
    let offsets: Vec<u32> = bo.planes.iter().map(|p| p.offset as u32).collect();
    let fb = sys::addfb2(
        fd,
        bo.width,
        bo.height,
        XRGB8888.drm,
        &handles,
        &pitches,
        &offsets,
        Some(bo.modifier),
    )
    .map_err(|e| format!("addfb2 (modifier {:#x}): {e}", bo.modifier))?;
    let _ = sys::gem_close(fd, handle);
    Ok(fb)
}

/// every modifier bounced - find which side is lying. a dumb buffer through
/// the same call tests our plumbing (the kernel marks those scanout-capable);
/// a linear vulkan bo through the legacy no-modifier path isolates the
/// FB_MODIFIERS flag from the bo.
fn diagnose_addfb(fd: std::os::fd::BorrowedFd<'_>, core: &VkCore, w: u32, h: u32) {
    match sys::create_dumb(fd, w, h, 32) {
        Ok(db) => {
            match sys::addfb2(fd, w, h, XRGB8888.drm, &[db.handle], &[db.pitch], &[0], None) {
                Ok(fb) => {
                    println!("diag: dumb buffer addfb2 ok - plumbing is fine, kms rejects the vulkan bo itself");
                    let _ = sys::rmfb(fd, fb);
                }
                Err(e) => {
                    println!("diag: dumb buffer addfb2 fails too ({e}) - the addfb2 call is broken");
                }
            }
            let _ = sys::destroy_dumb(fd, db.handle);
        }
        Err(e) => println!("diag: create_dumb failed: {e}"),
    }
    let bo = match create_scanout_bo(core, w, h, vk::Format::B8G8R8A8_UNORM, &[(0, 1)]) {
        Ok(bo) => bo,
        Err(e) => {
            println!("diag: linear bo alloc failed: {e}");
            return;
        }
    };
    match sys::prime_fd_to_handle(fd, bo.fd.as_fd()) {
        Ok(handle) => {
            let pitch = bo.planes[0].pitch as u32;
            let offset = bo.planes[0].offset as u32;
            match sys::addfb2(fd, w, h, XRGB8888.drm, &[handle], &[pitch], &[offset], None) {
                Ok(fb) => {
                    println!("diag: linear vulkan bo passes WITHOUT the modifier flag - FB_MODIFIERS path is the problem");
                    let _ = sys::rmfb(fd, fb);
                }
                Err(e) => {
                    println!("diag: linear vulkan bo rejected on the legacy path too ({e}) - kms won't scan out this allocation at all");
                }
            }
            let _ = sys::gem_close(fd, handle);
        }
        Err(e) => println!("diag: prime import failed: {e}"),
    }
    bo.destroy(core);
}
