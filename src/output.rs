// per-output display stack: double-buffered scanout (vulkan-native, else
// dumb-import), a present loop gated on flip_pending, fences both ways -
// prev scanout's OUT fence gates the render, render fence rides IN_FENCE_FD
// into the commit. composition in tree z order: tiled, fullscreen, floats.

use crate::allocator::{ScanoutBo, create_scanout_bo, import_linear_bo};
use crate::client::ClientId;
use crate::clientmem::ShmAccess;
use crate::dbus::{DeviceEvent, LogindSession};
use crate::drm::connector::{Connector, FlipResult};
use crate::drm::device::DrmDevice;
use crate::drm::sys;
use crate::engine::SpawnedFuture;
use crate::format::XRGB8888;
use crate::protocol::ObjectId;
use crate::rect::Rect;
use crate::render::renderer::{FrameTarget, RenderOp, Renderer, Texture};
use crate::render::vulkan::VkCore;
use crate::state::State;
use crate::surface::WlSurface;
use crate::util::{EitherEvent, Time};
use ash::vk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

pub struct Display {
    out: Rc<Output>,
    _pump: SpawnedFuture<()>,
    _present: SpawnedFuture<()>,
}

impl Display {
    pub fn output_global(&self) -> crate::protocol::output::WlOutputGlobal {
        let refresh = self
            .out
            .conn
            .pipe
            .borrow()
            .as_ref()
            .map(|p| p.mode.vrefresh)
            .unwrap_or(60) as i32
            * 1000;
        crate::protocol::output::WlOutputGlobal {
            width: self.out.width as i32,
            height: self.out.height as i32,
            refresh_mhz: refresh,
        }
    }
}

impl Display {
    /// pre-SwitchTo teardown: the only moment we know a switch is coming and
    /// still hold master. logind drops master before signaling PauseDevice,
    /// so the pause path is too late.
    pub fn hide_cursor(&self) {
        self.out.cursor_locked.set(true);
        self.out.conn.cursor_hide(&self.out.dev);
    }

    /// hardware cursor moves bypass rendering
    pub fn move_cursor(&self, x: i32, y: i32) {
        if self.out.cursor_locked.get() {
            return;
        }
        let pipe = self.out.conn.pipe.borrow();
        let Some(p) = pipe.as_ref() else { return };
        let Some(cur) = &p.cursor else { return };
        let (hx, hy) = cur.hotspot.get();
        cur.set_enabled(true);
        cur.set_position(x - hx, y - hy);
        drop(pipe);
        self.out.conn.cursor_commit(&self.out.dev);
    }
}

/// a plain white-on-black arrow so the cursor exists before themes do
fn default_cursor() -> (Vec<u8>, u32, u32) {
    const W: u32 = 12;
    const H: u32 = 19;
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let inside = x <= y && (y < 12 || (x >= 3 && y < 19 - (x - 3).max(0)));
            let edge = inside
                && (x == 0
                    || x == y
                    || y == H as i32 - 1
                    || !(x - 1 <= y)
                    || y == 11 && x > 6);
            let o = ((y as u32 * W + x as u32) * 4) as usize;
            if inside {
                let c = if edge { 0x00 } else { 0xff };
                px[o] = c;
                px[o + 1] = c;
                px[o + 2] = c;
                px[o + 3] = 0xff;
            }
        }
    }
    (px, W, H)
}

struct OutBuf {
    bo: ScanoutBo,
    fb: u32,
    view: vk::ImageView,
    undefined: Cell<bool>,
    dumb: Option<u32>,
}

struct Output {
    dev: Rc<DrmDevice>,
    conn: Rc<Connector>,
    renderer: Rc<Renderer>,
    bufs: [OutBuf; 2],
    front: Cell<usize>,
    width: u32,
    height: u32,
    /// keyed by surface uid; the u64 is the content_gen the texture holds
    textures: RefCell<HashMap<(ClientId, u64), (Texture, u64)>>,
    /// evicted textures still in a submitted frame; drained past the fence
    retired_tex: RefCell<Vec<Texture>>,
    /// client dmabuf read fences to wait on before this frame samples them
    frame_fences: RefCell<Vec<std::os::fd::OwnedFd>>,
    /// card's dev_t, matches logind pause/resume signals
    devnum: u64,
    /// vt elsewhere; render but never commit until resume
    paused: Cell<bool>,
    /// queued motion must not re-arm the cursor while the vt is leaving
    cursor_locked: Cell<bool>,
}

/// bring up a display if a card is reachable, else run headless
pub async fn start(state: &Rc<State>, session: Option<&Rc<LogindSession>>) -> Option<Display> {
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
        Err(_) => Vec::new(),
    };
    cards.sort();
    for path in cards {
        match init_card(state, &path, session).await {
            Ok(out) => {
                println!(
                    "carrot: output up on {} ({}x{})",
                    path.display(),
                    out.width,
                    out.height
                );
                let out = Rc::new(out);
                let pump = out.dev.spawn_flip_pump(&state.eng, &state.ring);
                let st = state.clone();
                let o = out.clone();
                let present = state.eng.spawn("present", async move {
                    present_loop(&st, &o).await;
                });
                // logind pauses the card around vt switches
                if let Some(s) = session {
                    let o = out.clone();
                    let st = state.clone();
                    s.on_device(
                        out.devnum,
                        Rc::new(move |ev| match ev {
                            // no KMS work: logind already revoked master
                            DeviceEvent::Pause { .. } => o.paused.set(true),
                            DeviceEvent::Resume { .. } => {
                                o.paused.set(false);
                                o.cursor_locked.set(false);
                                // an in-flight flip never completes when the
                                // vt left; unwedge the gate
                                o.conn.flip_pending.set(false);
                                if let Some(p) = o.conn.pipe.borrow().as_ref() {
                                    if let Some(cur) = &p.cursor {
                                        cur.set_enabled(true);
                                    }
                                }
                                let fb = o.bufs[o.front.get()].fb;
                                if let Err(e) = o.conn.modeset(&o.dev, fb) {
                                    eprintln!("carrot: resume modeset: {e}");
                                }
                                st.damage.trigger();
                            }
                            DeviceEvent::Gone { .. } => o.paused.set(true),
                        }),
                    );
                }
                // seed the cursor plane: system theme, else built-in arrow
                if let Some(p) = out.conn.pipe.borrow().as_ref() {
                    if let Some(cur) = &p.cursor {
                        match crate::input::cursor_theme::load("left_ptr") {
                            Some(img) => {
                                cur.write(&img.pixels, img.width, img.height);
                                cur.hotspot.set(img.hotspot);
                            }
                            None => {
                                eprintln!(
                                    "carrot: no xcursor theme found, using the built-in arrow"
                                );
                                let (px, w, h) = default_cursor();
                                cur.write(&px, w, h);
                            }
                        }
                    }
                }
                state.output_size.set((out.width, out.height));
                // paint over the modeset's uninitialized first buffer
                state.damage.trigger();
                return Some(Display {
                    out,
                    _pump: pump,
                    _present: present,
                });
            }
            Err(e) => {
                eprintln!("carrot: {}: {e} - trying next card", path.display());
            }
        }
    }
    eprintln!("carrot: no usable output, running headless");
    None
}

async fn init_card(
    state: &Rc<State>,
    path: &std::path::Path,
    session: Option<&Rc<LogindSession>>,
) -> Result<Output, String> {
    let devnum = rustix::fs::stat(path)
        .map_err(|e| format!("stat: {e}"))?
        .st_rdev;
    let dev = match session {
        Some(s) => {
            let (fd, _inactive) = s
                .take_device(devnum)
                .await
                .map_err(|e| format!("TakeDevice: {e}"))?;
            DrmDevice::with_fd(fd).map_err(|e| format!("card init: {e}"))?
        }
        None => DrmDevice::open(path).map_err(|e| format!("open: {e}"))?,
    };
    dev.assign_pipes().map_err(|e| format!("pipes: {e}"))?;
    let conn = dev
        .connectors
        .borrow()
        .iter()
        .find(|c| c.pipe.borrow().is_some())
        .cloned()
        .ok_or("no connected display")?;
    let (width, height, primary) = {
        let pipe = conn.pipe.borrow();
        let p = pipe.as_ref().unwrap();
        (
            p.mode.hdisplay as u32,
            p.mode.vdisplay as u32,
            p.primary.clone(),
        )
    };

    let core = Rc::new(VkCore::new(dev.fd.as_fd()).map_err(|e| format!("vulkan: {e}"))?);
    println!("carrot: rendering on {}", core.device_name);
    let renderer = Rc::new(
        Renderer::new(&core, vk::Format::B8G8R8A8_UNORM).map_err(|e| format!("renderer: {e}"))?,
    );

    // the dmabuf global speaks for this device from here on
    {
        let rdev = rustix::fs::fstat(dev.fd.as_fd())
            .map(|st| st.st_rdev)
            .unwrap_or(0);
        let mut formats = Vec::new();
        match core.sample_modifiers(vk::Format::B8G8R8A8_UNORM) {
            Ok(mods) => {
                for &m in &mods {
                    formats.push((XRGB8888.drm, m));
                    formats.push((crate::format::ARGB8888.drm, m));
                }
            }
            Err(e) => eprintln!("carrot: dmabuf modifier probe failed: {e}"),
        }
        if formats.is_empty() {
            formats.push((XRGB8888.drm, 0));
            formats.push((crate::format::ARGB8888.drm, 0));
        }
        eprintln!(
            "carrot: dmabuf: {} format+modifier pairs, main device {rdev:#x}",
            formats.len()
        );
        *state.dmabuf_info.borrow_mut() = Some(crate::protocol::dmabuf::DmabufInfo {
            main_device: rdev,
            formats,
        });
    }

    let mk_buf = || -> Result<OutBuf, String> {
        // tier 1: vulkan-native, addfb2 arbitrates
        let vk_mods = core
            .scanout_modifiers(vk::Format::B8G8R8A8_UNORM)
            .unwrap_or_default();
        let kms_mods = primary.modifiers(XRGB8888.drm);
        let mut usable: Vec<(u64, u32)> = vk_mods
            .iter()
            .filter(|(m, _)| kms_mods.is_empty() || kms_mods.contains(m))
            .copied()
            .collect();
        while !usable.is_empty() {
            let bo = create_scanout_bo(&core, width, height, vk::Format::B8G8R8A8_UNORM, &usable)
                .map_err(|e| format!("scanout bo: {e}"))?;
            let handles = vec![
                sys::prime_fd_to_handle(dev.fd.as_fd(), bo.fd.as_fd())
                    .map_err(|e| format!("prime import: {e}"))?;
                bo.planes.len()
            ];
            let pitches: Vec<u32> = bo.planes.iter().map(|p| p.pitch as u32).collect();
            let offsets: Vec<u32> = bo.planes.iter().map(|p| p.offset as u32).collect();
            match sys::addfb2(
                dev.fd.as_fd(),
                width,
                height,
                XRGB8888.drm,
                &handles,
                &pitches,
                &offsets,
                Some(bo.modifier),
            ) {
                Ok(fb) => {
                    let _ = sys::gem_close(dev.fd.as_fd(), handles[0]);
                    let view = renderer
                        .create_target_view(bo.image)
                        .map_err(|e| format!("view: {e}"))?;
                    return Ok(OutBuf {
                        bo,
                        fb,
                        view,
                        undefined: Cell::new(true),
                        dumb: None,
                    });
                }
                Err(_) => {
                    let _ = sys::gem_close(dev.fd.as_fd(), handles[0]);
                    let bad = bo.modifier;
                    bo.destroy(&core);
                    usable.retain(|(m, _)| *m != bad);
                }
            }
        }
        // tier 2: dumb buffer imported into vulkan
        let db = sys::create_dumb(dev.fd.as_fd(), width, height, 32)
            .map_err(|e| format!("create_dumb: {e}"))?;
        let fb = sys::addfb2(
            dev.fd.as_fd(),
            width,
            height,
            XRGB8888.drm,
            &[db.handle],
            &[db.pitch],
            &[0],
            None,
        )
        .map_err(|e| format!("dumb addfb2: {e}"))?;
        let dmabuf = sys::prime_handle_to_fd(dev.fd.as_fd(), db.handle)
            .map_err(|e| format!("prime export: {e}"))?;
        let bo = import_linear_bo(
            &core,
            dmabuf,
            width,
            height,
            db.pitch,
            db.size,
            vk::Format::B8G8R8A8_UNORM,
        )
        .map_err(|e| format!("import: {e}"))?;
        let view = renderer
            .create_target_view(bo.image)
            .map_err(|e| format!("view: {e}"))?;
        Ok(OutBuf {
            bo,
            fb,
            view,
            undefined: Cell::new(true),
            dumb: Some(db.handle),
        })
    };
    let bufs = [mk_buf()?, mk_buf()?];
    println!(
        "carrot: scanout tier: {}",
        if bufs[0].dumb.is_some() {
            "dumb-buffer import"
        } else {
            "vulkan-native"
        }
    );

    // light up with buffer 0; render catches up on first damage
    conn.modeset(&dev, bufs[0].fb)
        .map_err(|e| format!("modeset: {e}"))?;

    Ok(Output {
        dev,
        conn,
        renderer,
        bufs,
        front: Cell::new(0),
        width,
        height,
        textures: RefCell::new(HashMap::new()),
        retired_tex: RefCell::new(Vec::new()),
        frame_fences: RefCell::new(Vec::new()),
        devnum,
        paused: Cell::new(false),
        cursor_locked: Cell::new(false),
    })
}

async fn present_loop(state: &Rc<State>, out: &Rc<Output>) {
    let mut dirty = false;
    loop {
        EitherEvent(&state.damage, &out.conn.vblank).await;
        let _ = out.conn.vblank.take();
        dirty |= state.damage.take();
        // render only when dirty AND the pipe is free
        if !dirty || out.conn.flip_pending.get() || out.paused.get() {
            continue;
        }
        dirty = false;

        let back = 1 - out.front.get();
        let buf = &out.bufs[back];

        // previous scanout must finish before we draw over its buffer
        let mut waits = Vec::new();
        if let Some(fence) = out.conn.take_scanout_fence() {
            match out.renderer.import_wait(fence) {
                Ok(sem) => waits.push(sem),
                Err(e) => eprintln!("carrot: out-fence import failed: {e}"),
            }
        }

        let ops = compose(state, out);
        // clients' in-flight renders gate our sampling of their dmabufs
        for fence in out.frame_fences.borrow_mut().drain(..) {
            match out.renderer.import_wait(fence) {
                Ok(sem) => waits.push(sem),
                Err(e) => eprintln!("carrot: dmabuf fence import failed: {e}"),
            }
        }
        crate::trace!("present: {} ops, paused={}", ops.len(), out.paused.get());
        let target = FrameTarget {
            image: buf.bo.image,
            view: buf.view,
            width: out.width,
            height: out.height,
            undefined: buf.undefined.get(),
        };
        let frame = match out
            .renderer
            .render(&target, Some([0.1, 0.1, 0.1, 1.0]), &ops, waits)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("carrot: render failed: {e}");
                continue;
            }
        };
        buf.undefined.set(false);

        // render fence into the commit
        let sync = frame.export_sync_file(&out.renderer).ok();
        let res = out.conn.flip(
            &out.dev,
            buf.fb,
            sync.as_ref().map(|fd| fd.as_raw_fd()),
        );
        match res {
            Ok(FlipResult::Queued) => {
                out.front.set(back);
            }
            Ok(FlipResult::NotPresented) => {
                // retry next wakeup with a fresh frame
                dirty = true;
            }
            Err(crate::drm::device::DrmError::LostMaster) => {
                // vt left between the pause signal and our commit; resume
                // re-modesets and re-damages
                out.paused.set(true);
                dirty = true;
            }
            Err(e) => {
                eprintln!("carrot: flip failed: {e} - display stops, compositor continues");
                out.renderer.recycle_frame(frame);
                return;
            }
        }

        // fence fd doubles as the completion signal; drives cleanup + callbacks
        if let Some(fd) = sync {
            let fd = Rc::new(fd);
            let _ = state.ring.readable(&fd).await;
        }
        out.renderer.recycle_frame(frame);
        // the frame that referenced these has now scanned out
        for t in out.retired_tex.borrow_mut().drain(..) {
            out.renderer.destroy_texture(&t);
        }
        // parked dmabufs are free now the sampling frame is done; drop sends release
        state.retired.borrow_mut().clear();
        let ms = (Time::now().nsec() / 1_000_000) as u32;
        state.clients.for_each(|c| {
            c.objects.for_each_surface(|s| {
                if s.mapped.get() {
                    s.fire_frame_callbacks(ms);
                }
            });
        });
    }
}

/// border colors; config keys once the config layer exists
/// paint order tiled, fullscreen, floats; each window drawn as its surface
/// stack, clipped to the window box so CSD can't leak past the tile. popups
/// sit above their window.
fn compose(state: &Rc<State>, out: &Rc<Output>) -> Vec<RenderOp> {
    let mut ops = Vec::new();
    let mut live: Vec<(ClientId, u64)> = Vec::new();
    let ws = crate::tree::active(state);
    let focused = state
        .seat
        .borrow()
        .as_ref()
        .and_then(|s| s.kb_focus.borrow().clone());
    let fs = ws.fullscreen.borrow().clone();
    let screen = Rect::new_sized_saturating(0, 0, out.width as i32, out.height as i32);
    let cfg = state.config.borrow().clone();

    let draw = |win: &Rc<crate::tree::Window>, ops: &mut Vec<RenderOp>, live: &mut Vec<(ClientId, u64)>| {
        let surface = win.surface();
        if !surface.mapped.get() {
            return;
        }
        let rect = win.draw_rect(state);
        if !win.fullscreen.get() {
            let color = match &focused {
                Some(f) => {
                    if Rc::ptr_eq(f, &surface) {
                        cfg.border_focused
                    } else {
                        cfg.border_unfocused
                    }
                }
                None => cfg.border_unfocused,
            };
            push_borders(out, rect, cfg.border, color, ops);
        }
        let geo = win.geometry();
        draw_surface_tree(out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, ops, live);
        if let Some(tl) = win.xdg_opt() {
            draw_popups(state, out, &tl.xdg, rect.x1, rect.y1, screen, ops, live);
        }
    };

    // paint order: background, bottom, tiled, fullscreen, floats, top,
    // overlay; fullscreen hides everything below itself except overlay
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::BACKGROUND, screen, &mut ops, &mut live);
        draw_layer(state, out, crate::shell::layer::BOTTOM, screen, &mut ops, &mut live);
        ws.tiling.for_each(|win| draw(win, &mut ops, &mut live));
    }
    if let Some(fs) = &fs {
        draw(fs, &mut ops, &mut live);
    }
    if fs.is_none() || cfg.float_above_fullscreen {
        for win in ws.floats.borrow().iter() {
            draw(win, &mut ops, &mut live);
        }
    }
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::TOP, screen, &mut ops, &mut live);
    }
    draw_layer(state, out, crate::shell::layer::OVERLAY, screen, &mut ops, &mut live);

    // a surface gone from the scene keeps its texture until the frame
    // that referenced it clears the fence
    let mut textures = out.textures.borrow_mut();
    let stale: Vec<_> = textures
        .keys()
        .filter(|k| !live.contains(k))
        .copied()
        .collect();
    for k in stale {
        if let Some((t, _)) = textures.remove(&k) {
            out.retired_tex.borrow_mut().push(t);
        }
    }
    ops
}

/// below children, surface, above children - in index order
fn draw_surface_tree(
    out: &Rc<Output>,
    surface: &Rc<WlSurface>,
    x: i32,
    y: i32,
    clip: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    if !surface.mapped.get() {
        return;
    }
    let children = surface.children.borrow();
    if let Some(ch) = &*children {
        let stack: Vec<_> = ch
            .below
            .iter()
            .chain(ch.above.iter())
            .filter(|e| !e.pending.get())
            .map(|e| e.sub.clone())
            .collect();
        let below = ch.below.iter().filter(|e| !e.pending.get()).count();
        drop(children);
        for sub in &stack[..below] {
            let (px, py) = sub.position.get();
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, ops, live);
        }
        draw_buffer(out, surface, x, y, clip, ops, live);
        for sub in &stack[below..] {
            let (px, py) = sub.position.get();
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, ops, live);
        }
    } else {
        drop(children);
        draw_buffer(out, surface, x, y, clip, ops, live);
    }
}

fn draw_popup(
    state: &Rc<State>,
    out: &Rc<Output>,
    p: &Rc<crate::shell::xdg::XdgPopup>,
    ox: i32,
    oy: i32,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    if !p.xdg.surface.mapped.get() {
        return;
    }
    let (rx, ry) = p.rel.get();
    let (px, py) = (ox + rx, oy + ry);
    let geo = p.xdg.geometry();
    draw_surface_tree(out, &p.xdg.surface, px - geo.x1, py - geo.y1, screen, ops, live);
    draw_popups(state, out, &p.xdg, px, py, screen, ops, live);
}

fn draw_popups(
    state: &Rc<State>,
    out: &Rc<Output>,
    xdg: &Rc<crate::shell::xdg::XdgSurface>,
    ox: i32,
    oy: i32,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    xdg.for_each_popup(|p| draw_popup(state, out, p, ox, oy, screen, ops, live));
}

// one shell layer, mapping order = z within it, popups on top of each
fn draw_layer(
    state: &Rc<State>,
    out: &Rc<Output>,
    layer: u32,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    let layers = state.layers.borrow().clone();
    for ls in layers.iter() {
        if ls.current.get().layer != layer || !ls.mapped() {
            continue;
        }
        let r = ls.rect.get();
        draw_surface_tree(out, &ls.surface, r.x1, r.y1, screen, ops, live);
        ls.for_each_popup(|p| draw_popup(state, out, p, r.x1, r.y1, screen, ops, live));
    }
}

/// upload the committed buffer, emit one quad intersected with the clip; uv
/// shrinks by the same fraction so the crop is exact
fn draw_buffer(
    out: &Rc<Output>,
    s: &Rc<WlSurface>,
    x: i32,
    y: i32,
    clip: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    let buffer = s.buffer.borrow();
    let Some(att) = buffer.as_ref() else { return };
    let buf = &att.buf;
    let (bw, bh) = (buf.rect.width() as u32, buf.rect.height() as u32);
    if bw == 0 || bh == 0 {
        return;
    }
    // dmabufs are per-buffer imports; shm shadows belong to the surface
    let key = if buf.dmabuf().is_some() {
        (s.client.id, buf.uid)
    } else {
        (s.client.id, s.uid)
    };
    let opaque = !buf.format.has_alpha();
    if let Some(img) = buf.dmabuf() {
        // gpu buffers import once and sample in place - the client's renders
        // never round-trip through the cpu
        let mut textures = out.textures.borrow_mut();
        let need_new = match textures.get(&key) {
            Some((t, _)) => t.width != bw || t.height != bh,
            None => true,
        };
        if need_new {
            if let Some((old, _)) = textures.remove(&key) {
                out.retired_tex.borrow_mut().push(old);
            }
            match out
                .renderer
                .import_dmabuf(&img.planes, img.modifier, bw, bh, opaque)
            {
                Ok(t) => {
                    textures.insert(key, (t, 0));
                }
                Err(e) => {
                    eprintln!("carrot: dmabuf import failed: {e}");
                    return;
                }
            }
        }
        // wait out the client's pending gpu writes before sampling
        if let Some(fence) = img.read_fence() {
            out.frame_fences.borrow_mut().push(fence);
        }
    } else {
        let mut textures = out.textures.borrow_mut();
        let need_new = match textures.get(&key) {
            Some((t, _)) => t.width != bw || t.height != bh,
            None => true,
        };
        if need_new {
            if let Some((old, _)) = textures.remove(&key) {
                out.retired_tex.borrow_mut().push(old);
            }
            match out.renderer.create_texture(bw, bh, opaque) {
                Ok(t) => {
                    // gen behind the surface's so the first pass uploads
                    textures.insert(key, (t, s.content_gen.get().wrapping_sub(1)));
                }
                Err(e) => {
                    eprintln!("carrot: texture alloc failed: {e}");
                    return;
                }
            }
        }
        // the pixels only move on commit; a compose over an unchanged
        // surface just re-samples the texture it already has
        let cur = s.content_gen.get();
        let entry = textures.get_mut(&key).unwrap();
        if entry.1 != cur {
            let shadow = s.shm_shadow.borrow();
            let row = (bw * 4) as usize;
            let need = row * bh as usize;
            let stride = buf.stride as usize;
            let res = if let Some(px) = shadow.as_ref().filter(|p| p.len() >= need) {
                // the commit-time shadow is the source of truth
                out.renderer
                    .upload_texture(&entry.0, |dst| dst[..need].copy_from_slice(&px[..need]))
            } else {
                // shadow missing (capture failed): fall back to the client
                // buffer, zero-filling short rows instead of leaking staging
                let access = match buf.shm_access() {
                    Some(a) => a,
                    None => return,
                };
                out.renderer.upload_texture(&entry.0, |dst| match access {
                    ShmAccess::Ptr(p) => {
                        for yy in 0..bh as usize {
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    p.add(yy * stride),
                                    dst[yy * row..].as_mut_ptr(),
                                    row,
                                );
                            }
                        }
                    }
                    ShmAccess::Fd { fd, offset } => {
                        for yy in 0..bh as usize {
                            let dst = &mut dst[yy * row..yy * row + row];
                            let mut done = 0;
                            while done < row {
                                match rustix::io::pread(
                                    fd,
                                    &mut dst[done..],
                                    (offset + yy * stride + done) as u64,
                                ) {
                                    Ok(0) | Err(_) => {
                                        dst[done..].fill(0);
                                        break;
                                    }
                                    Ok(n) => done += n,
                                }
                            }
                        }
                    }
                })
            };
            if let Err(e) = res {
                eprintln!("carrot: upload failed: {e}");
                return;
            }
            entry.1 = cur;
        }
    }
    live.push(key);
    let (sw, sh) = s.size.get();
    let dst = Rect::new_sized_saturating(x, y, sw, sh);
    let vis = dst.intersect(clip);
    if vis.is_empty() {
        return;
    }
    let textures = out.textures.borrow();
    let (tex, _) = textures.get(&key).unwrap();
    let fx = |v: i32| v as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| v as f32 / out.height as f32 * 2.0 - 1.0;
    ops.push(RenderOp::Tex {
        view: tex.view,
        pos: [fx(vis.x1), fy(vis.y1)],
        size: [
            (vis.width()) as f32 / out.width as f32 * 2.0,
            (vis.height()) as f32 / out.height as f32 * 2.0,
        ],
        uv_pos: [
            (vis.x1 - dst.x1) as f32 / sw as f32,
            (vis.y1 - dst.y1) as f32 / sh as f32,
        ],
        uv_size: [
            vis.width() as f32 / sw as f32,
            vis.height() as f32 / sh as f32,
        ],
        mul: 1.0,
        opaque,
    });
}

/// four fills just outside the window box
fn push_borders(out: &Rc<Output>, r: Rect, b: i32, color: [f32; 4], ops: &mut Vec<RenderOp>) {
    let sides = [
        Rect { x1: r.x1 - b, y1: r.y1 - b, x2: r.x2 + b, y2: r.y1 },
        Rect { x1: r.x1 - b, y1: r.y2, x2: r.x2 + b, y2: r.y2 + b },
        Rect { x1: r.x1 - b, y1: r.y1, x2: r.x1, y2: r.y2 },
        Rect { x1: r.x2, y1: r.y1, x2: r.x2 + b, y2: r.y2 },
    ];
    let fx = |v: i32| v as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| v as f32 / out.height as f32 * 2.0 - 1.0;
    for s in sides {
        ops.push(RenderOp::Fill {
            pos: [fx(s.x1), fy(s.y1)],
            size: [
                s.width() as f32 / out.width as f32 * 2.0,
                s.height() as f32 / out.height as f32 * 2.0,
            ],
            color,
        });
    }
}
