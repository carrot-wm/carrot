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
use crate::rect::Rect;
use crate::render::renderer::{FrameTarget, RenderOp, Renderer, Texture};
use crate::render::vulkan::VkCore;
use crate::state::State;
use crate::surface::WlSurface;
use crate::util::{AsyncEvent, EitherEvent, Time};
use ash::vk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

pub struct Display {
    pub outputs: RefCell<Vec<Rc<Output>>>,
    dev: Rc<DrmDevice>,
    core: Rc<VkCore>,
    renderer: Rc<Renderer>,
    devnum: u64,
    _pump: SpawnedFuture<()>,
    _presents: RefCell<Vec<SpawnedFuture<()>>>,
    _fanout: SpawnedFuture<()>,
    /// armed once the display lands in state; watches netlink for hotplug
    pub hotplug: RefCell<Option<SpawnedFuture<()>>>,
}

impl Display {
    pub fn output_global(&self, out: &Rc<Output>) -> crate::protocol::output::WlOutputGlobal {
        let refresh = out
            .conn
            .pipe
            .borrow()
            .as_ref()
            .map(|p| p.mode.vrefresh)
            .unwrap_or(60) as i32
            * 1000;
        let (x, y) = out.pos.get();
        crate::protocol::output::WlOutputGlobal {
            name: out.conn.name.clone(),
            x,
            y,
            width: out.width as i32,
            height: out.height as i32,
            refresh_mhz: refresh,
        }
    }

    /// the output whose rect contains the global point
    pub fn output_at(&self, x: i32, y: i32) -> Option<Rc<Output>> {
        self.outputs
            .borrow()
            .iter()
            .find(|o| o.rect().contains(x, y))
            .cloned()
    }
}

impl Display {
    /// pre-SwitchTo teardown: the only moment we know a switch is coming and
    /// still hold master. logind drops master before signaling PauseDevice,
    /// so the pause path is too late.
    pub fn hide_cursor(&self) {
        for out in self.outputs.borrow().iter() {
            out.cursor_locked.set(true);
            out.conn.cursor_hide(&out.dev);
        }
    }

    /// hardware cursor moves bypass rendering
    pub fn move_cursor(&self, x: i32, y: i32) {
        for out in self.outputs.borrow().iter() {
            if out.cursor_locked.get() {
                continue;
            }
            let (ox, oy) = out.pos.get();
            let inside = out.rect().contains(x, y);
            let pipe = out.conn.pipe.borrow();
            let Some(p) = pipe.as_ref() else { continue };
            let Some(cur) = &p.cursor else { continue };
            if inside {
                let (hx, hy) = cur.hotspot.get();
                cur.set_enabled(true);
                cur.set_position(x - ox - hx, y - oy - hy);
            } else {
                cur.set_enabled(false);
            }
            drop(pipe);
            out.conn.cursor_commit(&out.dev);
        }
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

pub struct Output {
    dev: Rc<DrmDevice>,
    pub conn: Rc<Connector>,
    renderer: Rc<Renderer>,
    bufs: [OutBuf; 2],
    front: Cell<usize>,
    pub width: u32,
    pub height: u32,
    /// slot in the display's output list
    pub index: Cell<usize>,
    /// registry name of this output's wl_output global
    pub global_name: Cell<u32>,
    /// per-output wake; the fan-out task mirrors state.damage into these
    pub damage: AsyncEvent,
    /// global origin; outputs tile left to right
    pub pos: Cell<(i32, i32)>,
    /// the workspace this output currently shows
    pub ws: Cell<usize>,
    /// global-space tiling box: our rect minus exclusive zones
    pub usable: Cell<Rect>,
    /// texture + the content generation it was uploaded at, keyed by uid
    /// (surface uid for shm shadows, buffer uid for dmabufs - wire ids
    /// get reused, uids never); shm skips the re-upload while gen holds
    textures: RefCell<HashMap<(ClientId, u64), (Texture, u64)>>,
    /// textures pulled from the cache this frame; destroyed only after
    /// the frame fence proves the last sampler is done
    retired_tex: RefCell<Vec<Texture>>,
    /// card's dev_t, matches logind pause/resume signals
    devnum: u64,
    /// vt elsewhere; render but never commit until resume
    paused: Cell<bool>,
    /// queued motion must not re-arm the cursor while the vt is leaving
    cursor_locked: Cell<bool>,
    /// implicit-sync fences of the dmabufs drawn this frame; the render
    /// submit waits them so client work lands before we sample
    frame_fences: RefCell<Vec<std::os::fd::OwnedFd>>,
    /// a vrr config on an incapable panel complains once, not per frame
    vrr_warned: Cell<bool>,
}

impl Output {
    pub fn rect(&self) -> Rect {
        let (x, y) = self.pos.get();
        Rect::new_sized_saturating(x, y, self.width as i32, self.height as i32)
    }
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
    let cfg_state = state.clone();
    let prefer = move |name: &str| -> Option<(u32, u32, Option<u32>)> {
        cfg_state
            .config
            .borrow()
            .outputs
            .iter()
            .find(|o| o.name == name)
            .and_then(|o| o.mode)
    };
    for path in cards {
        let (dev, core, renderer, devnum) = match init_card(&path, session, &prefer).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("carrot: {}: {e} - trying next card", path.display());
                continue;
            }
        };
        // every connected connector with a pipe becomes an output,
        // tiled left to right in global space
        let conns: Vec<Rc<Connector>> = dev
            .connectors
            .borrow()
            .iter()
            .filter(|c| c.pipe.borrow().is_some())
            .cloned()
            .collect();
        if conns.is_empty() {
            eprintln!("carrot: {}: no connected display - trying next card", path.display());
            continue;
        }
        let mut outputs: Vec<Rc<Output>> = Vec::new();
        let mut x = 0i32;
        for conn in conns {
            let name = conn.name.clone();
            match init_output(&dev, &core, &renderer, conn, devnum) {
                Ok(out) => {
                    let out = Rc::new(out);
                    out.pos.set((x, 0));
                    out.usable.set(out.rect());
                    out.ws.set(outputs.len().min(8));
                    x += out.width as i32;
                    let refresh = out
                        .conn
                        .pipe
                        .borrow()
                        .as_ref()
                        .map(|p| p.mode.vrefresh)
                        .unwrap_or(0);
                    eprintln!(
                        "carrot: output {}: {}x{}@{} at {:?} vrr_capable={}",
                        out.conn.name,
                        out.width,
                        out.height,
                        refresh,
                        out.pos.get(),
                        out.conn.vrr_capable
                    );
                    outputs.push(out);
                }
                Err(e) => eprintln!("carrot: {name}: {e} - skipping connector"),
            }
        }
        if outputs.is_empty() {
            continue;
        }
        // every head in one commit: the driver validates the combined
        // bandwidth, the ladder steps refreshes down until it fits, and a
        // head that can't fit at all is dropped rather than flashing
        // linear dumb-buffer scanout is the display engine's worst case;
        // two high-refresh heads of it starve the FIFO on intel even though
        // the kernel accepts the config. cap unconfigured secondaries.
        if outputs.len() > 1 && outputs.iter().any(|o| o.bufs[0].dumb.is_some()) {
            for o in outputs.iter().skip(1) {
                if prefer(&o.conn.name).is_some() {
                    continue; // explicit config wins, whatever it costs
                }
                let mut hz = o.conn.pipe.borrow().as_ref().map(|p| p.mode.vrefresh).unwrap_or(0);
                while hz > 144 && o.conn.step_down_mode() {
                    hz = o.conn.pipe.borrow().as_ref().map(|p| p.mode.vrefresh).unwrap_or(0);
                }
            }
            let hot = outputs
                .first()
                .and_then(|o| o.conn.pipe.borrow().as_ref().map(|p| p.mode.vrefresh))
                .unwrap_or(0);
            if hot > 240 {
                eprintln!(
                    "carrot: linear scanout at {hot}Hz plus a second head may underrun; set output {{ mode }} to cap one if the screen flashes"
                );
            }
        }
        let mut card_dead = false;
        loop {
            let heads: Vec<(Rc<Connector>, u32)> = outputs
                .iter()
                .map(|o| (o.conn.clone(), o.bufs[0].fb))
                .collect();
            match dev.modeset_heads(&heads) {
                Ok(()) => break,
                Err(e) => {
                    // out of refresh steps: dropping the newest head only
                    // helps when the kernel rejected the combination
                    let bandwidth = matches!(
                        e,
                        crate::drm::device::DrmError::Op("modeset", rustix::io::Errno::INVAL)
                            | crate::drm::device::DrmError::Op("modeset", rustix::io::Errno::NOSPC)
                            | crate::drm::device::DrmError::Op("modeset", rustix::io::Errno::RANGE)
                    );
                    if !bandwidth {
                        eprintln!("carrot: {}: modeset failed: {e} - trying next card", path.display());
                        card_dead = true;
                        break;
                    }
                    match outputs.pop() {
                        Some(o) => eprintln!(
                            "carrot: {}: no mode fits alongside the other heads; head dropped",
                            o.conn.name
                        ),
                        None => break,
                    }
                }
            }
        }
        if card_dead || outputs.is_empty() {
            continue;
        }
        for (i, out) in outputs.iter().enumerate() {
            out.index.set(i);
        }
        // a workspace per output up front, bound to it
        {
            let mut list = state.workspaces.borrow_mut();
            while list.len() < outputs.len().min(9) {
                list.push(Rc::new(crate::tree::workspace::Workspace::default()));
            }
            for i in 0..outputs.len().min(9) {
                list[i].output.set(i);
            }
        }
        let pump = dev.spawn_flip_pump(&state.eng, &state.ring);
        // one logind pause/resume handler per card; it walks the live output
        // list so hotplugged outputs are covered too. the boot set rides
        // along until the display lands in state.
        if let Some(s) = session {
            let boot = outputs.clone();
            let st = state.clone();
            s.on_device(
                devnum,
                Rc::new(move |ev| {
                    let outs: Vec<Rc<Output>> = match st.display.borrow().as_ref() {
                        Some(d) => d.outputs.borrow().clone(),
                        None => boot.clone(),
                    };
                    match ev {
                        // no KMS work: logind already revoked master
                        DeviceEvent::Pause { .. } => {
                            for o in &outs {
                                o.paused.set(true);
                            }
                        }
                        DeviceEvent::Resume { .. } => {
                            let mut heads = Vec::new();
                            for o in &outs {
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
                                heads.push((o.conn.clone(), o.bufs[o.front.get()].fb));
                            }
                            if let Some(o) = outs.first() {
                                if let Err(e) = o.dev.modeset_heads(&heads) {
                                    eprintln!("carrot: resume modeset: {e}");
                                }
                            }
                            st.damage.trigger();
                        }
                        DeviceEvent::Gone { .. } => {
                            for o in &outs {
                                o.paused.set(true);
                            }
                        }
                    }
                }),
            );
        }
        let presents: Vec<SpawnedFuture<()>> = outputs
            .iter()
            .map(|out| {
                let st = state.clone();
                let o = out.clone();
                state.eng.spawn("present", async move {
                    present_loop(&st, &o).await;
                })
            })
            .collect();
        // the dmabuf global speaks for this device from here on
        {
            let rdev = rustix::fs::fstat(dev.fd.as_fd())
                .map(|st| st.st_rdev)
                .unwrap_or(0);
            let mut formats = Vec::new();
            match core.sample_modifiers(vk::Format::B8G8R8A8_UNORM) {
                Ok(mods) => {
                    for &m in &mods {
                        formats.push((crate::format::XRGB8888.drm, m));
                        formats.push((crate::format::ARGB8888.drm, m));
                    }
                }
                Err(e) => eprintln!("carrot: dmabuf modifier probe failed: {e}"),
            }
            if formats.is_empty() {
                formats.push((crate::format::XRGB8888.drm, 0));
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
        // one consumer per AsyncEvent: mirror global damage into each output.
        // the boot set covers the window before the display lands in state;
        // after that the live list picks up hotplugged outputs.
        let fan_outs = outputs.clone();
        let st = state.clone();
        let fanout = state.eng.spawn("damage fanout", async move {
            loop {
                st.damage.triggered().await;
                for o in &fan_outs {
                    o.damage.trigger();
                }
                if let Some(d) = st.display.borrow().as_ref() {
                    for o in d.outputs.borrow().iter().skip(fan_outs.len()) {
                        o.damage.trigger();
                    }
                }
            }
        });
        // union extent: pointer clamping + xdg_output fall back to it
        let uw = outputs.iter().map(|o| o.pos.get().0 + o.width as i32).max().unwrap_or(0);
        let uh = outputs.iter().map(|o| o.pos.get().1 + o.height as i32).max().unwrap_or(0);
        state.output_size.set((uw as u32, uh as u32));
        let display = Display {
            outputs: RefCell::new(outputs),
            dev,
            core,
            renderer,
            devnum,
            _pump: pump,
            _presents: RefCell::new(presents),
            _fanout: fanout,
            hotplug: RefCell::new(None),
        };
        for out in display.outputs.borrow().iter() {
            register_output_global(state, &display, out);
        }
        // paint over the modesets' uninitialized first buffers
        state.damage.trigger();
        return Some(display);
    }
    eprintln!("carrot: no usable output, running headless");
    None
}

/// screenshot capture: compose the output like a present would, render
/// offscreen, read back, crop to the output-local region. rows come back
/// tightly packed (stride = width * 4).
pub fn screencopy(state: &Rc<State>, out_index: usize, region: Rect) -> Option<Vec<u8>> {
    let d = state.display.borrow();
    let out = d.as_ref()?.outputs.borrow().get(out_index)?.clone();
    drop(d);
    let ops = compose(state, &out);
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    let full = match out.renderer.read_frame(out.width, out.height, &ops, waits) {
        Ok(px) => px,
        Err(e) => {
            eprintln!("carrot: screencopy render failed: {e}");
            return None;
        }
    };
    let (rw, rh) = (region.width() as usize, region.height() as usize);
    let (ox, oy) = (region.x1 as usize, region.y1 as usize);
    let src_stride = out.width as usize * 4;
    let mut px = vec![0u8; rw * rh * 4];
    for row in 0..rh {
        let s0 = (oy + row) * src_stride + ox * 4;
        px[row * rw * 4..][..rw * 4].copy_from_slice(&full[s0..s0 + rw * 4]);
    }
    Some(px)
}

/// window capture: compose only the toplevel's surface tree - subsurfaces
/// included, popups/borders/opacity rules not - then read back and crop to
/// its rect. rows come back tightly packed (stride = rect width * 4).
pub fn window_capture(state: &Rc<State>, win: &Rc<crate::tree::Window>) -> Option<Vec<u8>> {
    let surface = win.surface();
    if !surface.mapped.get() {
        return None;
    }
    let ws = crate::tree::workspace_of(state, win)?;
    let out = {
        let d = state.display.borrow();
        let d = d.as_ref()?;
        let outs = d.outputs.borrow();
        outs.get(ws.output.get()).cloned().or_else(|| outs.first().cloned())?
    };
    let rect = win.draw_rect(state);
    if rect.is_empty() {
        return None;
    }
    let geo = win.geometry();
    let mut ops = Vec::new();
    let mut live = Vec::new();
    draw_surface_tree(&out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, 1.0, &mut ops, &mut live);
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    let full = match out.renderer.read_frame(out.width, out.height, &ops, waits) {
        Ok(px) => px,
        Err(e) => {
            eprintln!("carrot: window capture render failed: {e}");
            return None;
        }
    };
    let (rw, rh) = (rect.width() as usize, rect.height() as usize);
    let mut px = vec![0u8; rw * rh * 4];
    // a float can hang off the screen; rows outside the output stay black
    let vis = rect.intersect(out.rect());
    let (gx, gy) = out.pos.get();
    let src_stride = out.width as usize * 4;
    let n = vis.width() as usize * 4;
    for row in vis.y1..vis.y2 {
        let s0 = (row - gy) as usize * src_stride + (vis.x1 - gx) as usize * 4;
        let d0 = (row - rect.y1) as usize * rw * 4 + (vis.x1 - rect.x1) as usize * 4;
        px[d0..d0 + n].copy_from_slice(&full[s0..s0 + n]);
    }
    Some(px)
}

/// output-local full rect + size lookup for the screencopy protocol
pub fn output_geometry(state: &Rc<State>, name: &str) -> Option<(usize, u32, u32)> {
    let d = state.display.borrow();
    let outs = d.as_ref()?.outputs.borrow().clone();
    outs.iter()
        .find(|o| o.conn.name == name)
        .map(|o| (o.index.get(), o.width, o.height))
}

async fn init_card(
    path: &std::path::Path,
    session: Option<&Rc<LogindSession>>,
    prefer: &dyn Fn(&str) -> Option<(u32, u32, Option<u32>)>,
) -> Result<(Rc<DrmDevice>, Rc<VkCore>, Rc<Renderer>, u64), String> {
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
    dev.assign_pipes(prefer).map_err(|e| format!("pipes: {e}"))?;
    let core = Rc::new(VkCore::new(dev.fd.as_fd()).map_err(|e| format!("vulkan: {e}"))?);
    println!("carrot: rendering on {}", core.device_name);
    let renderer = Rc::new(
        Renderer::new(&core, vk::Format::B8G8R8A8_UNORM).map_err(|e| format!("renderer: {e}"))?,
    );
    Ok((dev, core, renderer, devnum))
}

/// register (or re-register) an output's wl_output global
fn register_output_global(state: &Rc<State>, d: &Display, out: &Rc<Output>) {
    let name = state.globals.add(Rc::new(d.output_global(out)));
    out.global_name.set(name);
}

/// arm the netlink watcher; called once the display is stored in state
pub fn start_hotplug(state: &Rc<State>) {
    if state.display.borrow().is_none() {
        return;
    }
    let st = state.clone();
    let task = state.eng.spawn("drm hotplug", async move {
        let fd = match crate::drm::uevent::open() {
            Ok(fd) => Rc::new(fd),
            Err(e) => {
                eprintln!("carrot: hotplug disabled: {e}");
                return;
            }
        };
        // fresh event nodes lag their uevent: udev hasn't applied
        // permissions yet and logind can refuse the take. a worker delays
        // each add a beat and serializes them so duplicates can't race
        let adds: Rc<crate::util::AsyncQueue<String>> = Rc::new(Default::default());
        let aq = adds.clone();
        let ast = st.clone();
        let _adder = st.eng.spawn("input hotplug", async move {
            loop {
                let devname = aq.pop().await;
                let deadline = Time::from_nsec(Time::now().nsec() + 250_000_000);
                let _ = ast.ring.timeout(deadline).await;
                let session = ast.session.borrow().clone();
                let mgr = ast.input.borrow().as_ref().map(|i| i.mgr.clone());
                let (Some(session), Some(mgr)) = (session, mgr) else {
                    continue;
                };
                let path = std::path::PathBuf::from(format!("/dev/{devname}"));
                if let Some(dev) = mgr.add_device(&ast, &session, &path).await {
                    println!("carrot: input: {} added", dev.name);
                }
            }
        });
        let mut buf = vec![0u8; 4096];
        loop {
            let (b, n) = match st.ring.read(&fd, buf).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("carrot: hotplug read failed: {e:?}");
                    return;
                }
            };
            buf = b;
            let msg = &buf[..n];
            if crate::drm::uevent::is_drm_change(msg) {
                rescan(&st);
            } else if let Some((added, devname)) = crate::drm::uevent::input_change(msg) {
                if added {
                    adds.push(devname);
                } else if let Some(devnum) = crate::drm::uevent::devnum(msg) {
                    let session = st.session.borrow().clone();
                    let mgr = st.input.borrow().as_ref().map(|i| i.mgr.clone());
                    if let (Some(session), Some(mgr)) = (session, mgr) {
                        mgr.remove_device(&session, devnum);
                    }
                }
            }
        }
    });
    if let Some(d) = state.display.borrow().as_ref() {
        *d.hotplug.borrow_mut() = Some(task);
    }
}

/// something changed on the card: re-probe every connector, tear down what
/// left, bring up what arrived, then rebuild the global layout
fn rescan(state: &Rc<State>) {
    let dref = state.display.borrow();
    let Some(d) = dref.as_ref() else { return };
    let old: Vec<Rc<Output>> = d.outputs.borrow().clone();

    for conn in d.dev.connectors.borrow().iter() {
        let Ok(info) = sys::connector(d.dev.fd.as_fd(), conn.id.0, true) else {
            continue;
        };
        let now = info.connection == 1;
        conn.connected.set(now);
        if now {
            *conn.modes.borrow_mut() = info.modes;
        }
    }

    // removals first: their crtcs free up for arrivals
    let gone: Vec<usize> = d
        .outputs
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, o)| !o.conn.connected.get())
        .map(|(i, _)| i)
        .collect();
    for &i in gone.iter().rev() {
        let out = d.outputs.borrow_mut().remove(i);
        // the present loop dies with its output: dropping the task cancels
        // it, outside the borrow in case teardown lands back here
        let present = d._presents.borrow_mut().remove(i);
        drop(present);
        state.globals.remove(out.global_name.get());
        crate::protocol::image_copy_capture::output_removed(state, &out.conn.name);
        if let Some(p) = out.conn.pipe.borrow_mut().take() {
            // stop the kernel scanning a dead head before freeing the crtc
            let mut ch = crate::drm::atomic::Change::default();
            out.conn.clear_routing(&mut ch);
            ch.set(p.crtc.id, p.crtc.props.active, 0);
            ch.set(p.crtc.id, p.crtc.props.mode_id, 0);
            ch.set(p.primary.id, p.primary.props.fb_id, 0);
            ch.set(p.primary.id, p.primary.props.crtc_id, 0);
            if let Some(cur) = &p.cursor {
                cur.clear_routing(&mut ch);
            }
            if let Err(e) = ch.commit(
                d.dev.fd.as_fd(),
                crate::drm::atomic::ALLOW_MODESET,
                0,
            ) {
                eprintln!("carrot: head disable failed: {e}");
            }
            p.crtc.connector.set(crate::drm::ObjId(0));
            // free a joiner-slave reservation held next door
            for c in d.dev.crtcs.iter() {
                if c.connector.get() == out.conn.id && c.id != p.crtc.id {
                    c.connector.set(crate::drm::ObjId(0));
                }
            }
        }
        for (_, (t, _)) in out.textures.borrow_mut().drain() {
            out.renderer.destroy_texture(&t);
        }
        for t in out.retired_tex.borrow_mut().drain(..) {
            out.renderer.destroy_texture(&t);
        }
        eprintln!("carrot: output {} disconnected", out.conn.name);
    }

    {
        let cfg = state.config.borrow().clone();
        let prefer = |name: &str| -> Option<(u32, u32, Option<u32>)> {
            cfg.outputs.iter().find(|o| o.name == name).and_then(|o| o.mode)
        };
        if let Err(e) = d.dev.assign_pipes(&prefer) {
            eprintln!("carrot: hotplug pipe assignment: {e}");
        }
    }
    let known: Vec<Rc<Connector>> = d.dev.connectors.borrow().clone();
    for conn in known {
        if conn.pipe.borrow().is_none() {
            continue;
        }
        let exists = d.outputs.borrow().iter().any(|o| Rc::ptr_eq(&o.conn, &conn));
        if exists {
            continue;
        }
        let name = conn.name.clone();
        match init_output(&d.dev, &d.core, &d.renderer, conn, d.devnum) {
            Ok(out) => {
                let out = Rc::new(out);
                register_output_global(state, d, &out);
                let st = state.clone();
                let o = out.clone();
                d._presents.borrow_mut().push(state.eng.spawn("present", async move {
                    present_loop(&st, &o).await;
                }));
                eprintln!("carrot: output {} connected ({}x{})", out.conn.name, out.width, out.height);
                d.outputs.borrow_mut().push(out);
            }
            Err(e) => eprintln!("carrot: {name}: {e} - connector skipped"),
        }
    }
    // one commit over the full head set; the ladder steps the newest down
    {
        let heads: Vec<(Rc<Connector>, u32)> = d
            .outputs
            .borrow()
            .iter()
            .map(|o| (o.conn.clone(), o.bufs[o.front.get()].fb))
            .collect();
        if let Err(e) = d.dev.modeset_heads(&heads) {
            eprintln!("carrot: hotplug modeset: {e}");
        }
    }

    finish_topology(state, d, &old);
}

/// re-tile positions and indexes, remap every index-keyed binding through
/// the identity of the outputs that survived, and re-arm the world
fn finish_topology(state: &Rc<State>, d: &Display, old: &[Rc<Output>]) {
    let outs = d.outputs.borrow();
    let mut x = 0i32;
    for (i, o) in outs.iter().enumerate() {
        o.index.set(i);
        o.pos.set((x, 0));
        o.usable.set(o.rect());
        x += o.width as i32;
    }
    let uw = outs.iter().map(|o| o.rect().x2).max().unwrap_or(1);
    let uh = outs.iter().map(|o| o.rect().y2).max().unwrap_or(1);
    state.output_size.set((uw as u32, uh as u32));

    // old slot -> new slot by identity; the fallen map to slot 0
    let map: Vec<usize> = old
        .iter()
        .map(|o| outs.iter().position(|n| Rc::ptr_eq(n, o)).unwrap_or(0))
        .collect();
    let remap = |slot: usize| map.get(slot).copied().unwrap_or(0);
    for ws in state.workspaces.borrow().iter() {
        ws.output.set(remap(ws.output.get()));
    }
    // layer surfaces stay pinned: the ones whose output left are closed,
    // not remapped (the surface will no longer be shown)
    let layers: Vec<Rc<crate::shell::layer::LayerSurface>> = state.layers.borrow().clone();
    for ls in layers {
        let slot = ls.output.get();
        let survivor = old.get(slot).and_then(|o| outs.iter().position(|n| Rc::ptr_eq(n, o)));
        let Some(new) = survivor else {
            ls.send_closed();
            continue;
        };
        ls.output.set(new);
        // moved bars re-learn their screen
        if new != slot && !ls.surface.destroyed.get() && ls.surface.mapped.get() {
            crate::tree::send_surface_output(state, &ls.surface, new, true);
        }
    }
    state.focused_output.set(remap(state.focused_output.get()).min(outs.len().saturating_sub(1)));

    // every output shows a workspace bound to it; make one when none is
    for o in outs.iter() {
        let ok = state
            .workspaces
            .borrow()
            .get(o.ws.get())
            .is_some_and(|w| w.output.get() == o.index.get());
        if ok {
            continue;
        }
        let mut list = state.workspaces.borrow_mut();
        let found = list.iter().position(|w| w.output.get() == o.index.get());
        let idx = match found {
            Some(i) => i,
            None => {
                let w = crate::tree::workspace::Workspace::default();
                w.output.set(o.index.get());
                list.push(Rc::new(w));
                list.len() - 1
            }
        };
        o.ws.set(idx);
    }
    let active_of_focus = outs
        .get(state.focused_output.get())
        .map(|o| o.ws.get())
        .unwrap_or(0);
    drop(outs);
    state.active_ws.set(active_of_focus);

    crate::shell::layer::arrange(state);
    for ws in crate::tree::visible_workspaces(state) {
        crate::tree::relayout(state, &ws);
    }
    // pull the pointer back onto glass if its output vanished
    if let Some(seat) = state.seat.borrow().clone() {
        let (px, py) = (seat.ptr_x.get(), seat.ptr_y.get());
        let (cx, cy) = clamp_pointer(state, px, py);
        if (cx, cy) != (px, py) {
            seat.warp(state, cx, cy);
        }
    }
    // surviving outputs may have new positions; bound objects re-learn them
    crate::protocol::output::resend_output_state(state);
    state.damage.trigger();
}

/// scanout buffers + modeset for one connector
fn init_output(
    dev: &Rc<DrmDevice>,
    core: &Rc<VkCore>,
    renderer: &Rc<Renderer>,
    conn: Rc<Connector>,
    devnum: u64,
) -> Result<Output, String> {
    let dev = dev.clone();
    let core = core.clone();
    let renderer = renderer.clone();
    let (width, height, primary) = {
        let pipe = conn.pipe.borrow();
        let p = pipe.as_ref().unwrap();
        (
            p.mode.hdisplay as u32,
            p.mode.vdisplay as u32,
            p.primary.clone(),
        )
    };

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

    Ok(Output {
        dev,
        conn,
        renderer,
        bufs,
        front: Cell::new(0),
        width,
        height,
        index: Cell::new(0),
        global_name: Cell::new(0),
        damage: AsyncEvent::default(),
        pos: Cell::new((0, 0)),
        ws: Cell::new(0),
        usable: Cell::new(Rect::default()),
        textures: RefCell::new(HashMap::new()),
        retired_tex: RefCell::new(Vec::new()),
        devnum,
        paused: Cell::new(false),
        cursor_locked: Cell::new(false),
        frame_fences: RefCell::new(Vec::new()),
        vrr_warned: Cell::new(false),
    })
}

async fn present_loop(state: &Rc<State>, out: &Rc<Output>) {
    let mut dirty = false;
    loop {
        EitherEvent(&out.damage, &out.conn.vblank).await;
        let _ = out.conn.vblank.take();
        dirty |= out.damage.take();
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
        state.frames_in_flight.set(state.frames_in_flight.get() + 1);
        buf.undefined.set(false);

        // render fence into the commit
        let sync = frame.export_sync_file(&out.renderer).ok();
        let res = out.conn.flip(
            &out.dev,
            buf.fb,
            sync.as_ref().map(|fd| fd.as_raw_fd()),
        );
        out.conn.vrr_want.set(vrr_wanted(state, out));
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
                // a flip error isn't fatal: log, full damage, retry next trigger
                eprintln!("carrot: {}: flip failed: {e}; retrying", out.conn.name);
                dirty = true;
            }
        }

        // fence fd doubles as the completion signal; drives cleanup + callbacks
        let completed = sync.is_some();
        if let Some(fd) = sync {
            let fd = Rc::new(fd);
            let _ = state.ring.readable(&fd).await;
        }
        // this frame is done sampling: retired textures can go now (kept
        // for the next round if the fence export failed)
        if completed {
            for t in out.retired_tex.borrow_mut().drain(..) {
                out.renderer.destroy_texture(&t);
            }
        }
        // frame done: parked dmabuf attachments release once NO output still
        // has a frame in flight that might sample them
        let inflight = state.frames_in_flight.get().saturating_sub(1);
        state.frames_in_flight.set(inflight);
        if inflight == 0 {
            state.retired.borrow_mut().clear();
        }
        out.renderer.recycle_frame(frame);
        let ms = (Time::now().nsec() / 1_000_000) as u32;
        state.clients.for_each(|c| {
            c.objects.for_each_surface(|s| {
                if s.mapped.get() {
                    s.fire_frame_callbacks(ms);
                }
            });
        });
        // this present only ran because content changed: pending output
        // captures complete against the frame just shown
        crate::protocol::image_copy_capture::output_presented(state, &out.conn.name);
    }
}

/// clamp a global pointer position: x into the union of outputs, y into
/// the output column under x. headless keeps the plain union clamp.
pub fn clamp_pointer(state: &Rc<State>, x: f64, y: f64) -> (f64, f64) {
    let d = state.display.borrow();
    if let Some(d) = d.as_ref() {
        let outs = d.outputs.borrow();
        if !outs.is_empty() {
            let min_x = outs.iter().map(|o| o.rect().x1).min().unwrap_or(0);
            let max_x = outs.iter().map(|o| o.rect().x2).max().unwrap_or(1);
            let x = x.clamp(min_x as f64, (max_x - 1).max(min_x) as f64);
            let col = outs
                .iter()
                .find(|o| {
                    let r = o.rect();
                    (x as i32) >= r.x1 && (x as i32) < r.x2
                })
                .unwrap_or(&outs[0]);
            let r = col.rect();
            let y = y.clamp(r.y1 as f64, (r.y2 - 1).max(r.y1) as f64);
            return (x, y);
        }
    }
    let (w, h) = state.output_size.get();
    (
        x.clamp(0.0, (w.max(1) - 1) as f64),
        y.clamp(0.0, (h.max(1) - 1) as f64),
    )
}

/// per-output vrr policy: off unless configured; "always" holds it on,
/// "automatic" follows a fullscreen window on the active workspace
fn vrr_wanted(state: &Rc<State>, out: &Rc<Output>) -> bool {
    let cfg = state.config.borrow().clone();
    let mode = cfg
        .outputs
        .iter()
        .find(|o| o.name == out.conn.name)
        .and_then(|o| o.vrr.as_deref())
        .unwrap_or("off")
        .to_string();
    if mode == "off" {
        return false;
    }
    if !out.conn.vrr_capable {
        if !out.vrr_warned.replace(true) {
            eprintln!(
                "carrot: {}: vrr \"{mode}\" configured but the panel is not vrr capable",
                out.conn.name
            );
        }
        return false;
    }
    match mode.as_str() {
        "always" => true,
        // automatic: a fullscreen window is what's on glass
        _ => state
            .workspaces
            .borrow()
            .get(out.ws.get())
            .is_some_and(|ws| ws.fullscreen.borrow().is_some()),
    }
}

/// paint order tiled, fullscreen, floats; each window drawn as its surface
/// stack, clipped to the window box so CSD can't leak past the tile. popups
/// sit above their window.
fn compose(state: &Rc<State>, out: &Rc<Output>) -> Vec<RenderOp> {
    let mut ops = Vec::new();
    let mut live: Vec<(ClientId, u64)> = Vec::new();
    let ws = {
        let list = state.workspaces.borrow();
        match list.get(out.ws.get()) {
            Some(w) => w.clone(),
            None => return ops,
        }
    };
    let focused = state
        .seat
        .borrow()
        .as_ref()
        .and_then(|s| s.kb_focus.borrow().clone());
    let fs = ws.fullscreen.borrow().clone();
    let screen = out.rect();
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
        let alpha = win.rule_opacity.get().unwrap_or(1.0);
        draw_surface_tree(out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, alpha, ops, live);
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
    // a drag icon rides the pointer, above everything but the cursor
    if let Some(seat) = state.seat.borrow().clone() {
        if let Some(drag) = seat.data.drag() {
            if let Some(icon) = drag.icon.borrow().clone() {
                let (dx, dy) = drag.icon_off.get();
                let (px, py) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                draw_surface_tree(out, &icon, px + dx, py + dy, screen, 1.0, &mut ops, &mut live);
            }
        }
    }

    // textures for gone buffers don't outlive the frame
    let mut textures = out.textures.borrow_mut();
    let stale: Vec<_> = textures
        .keys()
        .filter(|k| !live.contains(k))
        .copied()
        .collect();
    for k in stale {
        if let Some((t, _)) = textures.remove(&k) {
            // the frame in flight may still sample it; destroy after fence
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
    alpha: f32,
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
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, alpha, ops, live);
        }
        draw_buffer(out, surface, x, y, clip, alpha, ops, live);
        for sub in &stack[below..] {
            let (px, py) = sub.position.get();
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, alpha, ops, live);
        }
    } else {
        drop(children);
        draw_buffer(out, surface, x, y, clip, alpha, ops, live);
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
    draw_surface_tree(out, &p.xdg.surface, px - geo.x1, py - geo.y1, screen, 1.0, ops, live);
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
        if ls.current.get().layer != layer
            || !ls.mapped()
            || ls.output.get() != out.index.get()
        {
            continue;
        }
        let r = ls.rect.get();
        draw_surface_tree(out, &ls.surface, r.x1, r.y1, screen, 1.0, ops, live);
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
    alpha: f32,
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
        // gpu buffers import once and are sampled in place - the client's
        // renders never round-trip through the cpu
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
                    // gen behind the surface's: the first pass uploads
                    textures.insert(key, (t, s.content_gen.get().wrapping_sub(1)));
                }
                Err(e) => {
                    eprintln!("carrot: texture alloc failed: {e}");
                    return;
                }
            }
        }
        // the pixels only move on commit; a compose pass over an
        // unchanged surface samples the texture it already has
        let cur = s.content_gen.get();
        let entry = textures.get_mut(&key).unwrap();
        if entry.1 != cur {
            let shadow = s.shm_shadow.borrow();
            let row = (bw * 4) as usize;
            let need = row * bh as usize;
            let res = if let Some(px) = shadow.as_ref().filter(|p| p.len() >= need) {
                // the commit-time shadow is the source of truth
                out.renderer
                    .upload_texture(&entry.0, |dst| dst[..need].copy_from_slice(&px[..need]))
            } else {
                // no shadow (capture failed): read the client buffer,
                // zero-filling any short row instead of leaking staging
                let stride = buf.stride as usize;
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
    let (gx, gy) = out.pos.get();
    let fx = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
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
        mul: alpha,
        opaque: opaque && alpha >= 1.0,
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
    let (gx, gy) = out.pos.get();
    let fx = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
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
