// per-output display stack: double-buffered scanout (vulkan-native, else
// dumb-import), a present loop gated on flip_pending, fences both ways -
// prev scanout's OUT fence gates the render, render fence rides IN_FENCE_FD
// into the commit. composition in tree z order: tiled, fullscreen, floats.

use crate::allocator::{ScanoutBo, create_scanout_bo, import_bo, import_linear_bo};
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
    /// current cursor image when compositing in software: pixels, w, h
    sw_image: RefCell<Option<(Vec<u8>, u32, u32)>>,
    sw_hot: Cell<(i32, i32)>,
    sw_hidden: Cell<bool>,
    sw: Cell<bool>,
    /// bumps when sw_image changes; outputs rebuild their texture on it
    sw_gen: Cell<u64>,
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
    /// so the pause path is too late. switch_vt is fire-and-forget, so a
    /// rejected switch would strand the cursor locked - the timer re-arms it
    /// if no pause ever arrives.
    pub fn prepare_vt_switch(&self, state: &Rc<State>, vt: u32) {
        for out in self.outputs.borrow().iter() {
            out.cursor_locked.set(true);
            out.conn.cursor_hide(&out.dev);
            let o = out.clone();
            let st = state.clone();
            *out.rearm.borrow_mut() = Some(state.eng.spawn("vt rearm", async move {
                let _ = st.wheel.timeout(500).await;
                if o.paused.get() || !o.cursor_locked.get() {
                    return;
                }
                eprintln!("carrot: vt {vt} switch never paused us, rearming the cursor");
                o.cursor_locked.set(false);
                let sw = st
                    .display
                    .borrow()
                    .as_ref()
                    .is_some_and(|d| d.software_cursor());
                if let Some(p) = o.conn.pipe.borrow().as_ref() {
                    if let Some(cur) = &p.cursor {
                        cur.set_enabled(!sw && !o.cursor_client_hidden.get());
                    }
                }
                o.conn.cursor_commit(&o.dev);
                st.damage.trigger();
            }));
        }
    }

    /// composite the cursor instead of scanning the plane; the escape hatch
    /// for planes that misbehave (joined-pipe modes smudge on xe)
    pub fn set_software_cursor(&self, state: &Rc<State>, on: bool) {
        if self.sw.replace(on) == on {
            return;
        }
        if on {
            // planes off; the frame carries the cursor from here
            for out in self.outputs.borrow().iter() {
                let pipe = out.conn.pipe.borrow();
                if let Some(cur) = pipe.as_ref().and_then(|p| p.cursor.as_ref()) {
                    cur.set_enabled(false);
                }
                drop(pipe);
                out.conn.cursor_commit(&out.dev);
            }
            if self.sw_image.borrow().is_none() {
                let seed = self
                    .outputs
                    .borrow()
                    .first()
                    .and_then(|o| o.theme_cursor.borrow().clone());
                if let Some((px, w, h, hot)) = seed {
                    *self.sw_image.borrow_mut() = Some((px, w, h));
                    self.sw_hot.set(hot);
                }
            }
            eprintln!("carrot: cursor: software compositing");
        }
        state.damage.trigger();
    }

    pub fn software_cursor(&self) -> bool {
        self.sw.get()
    }

    /// hardware cursor moves bypass rendering. only the output under the
    /// pointer shows its plane; the rest disable theirs. in software mode
    /// the frame carries the cursor, so motion just redraws
    pub fn move_cursor(&self, state: &Rc<State>, x: i32, y: i32) {
        if self.sw.get() {
            state.damage.trigger();
            return;
        }
        for out in self.outputs.borrow().iter() {
            if out.cursor_locked.get() {
                continue;
            }
            let inside = out.rect().contains(x, y) && !out.cursor_client_hidden.get();
            let (ox, oy) = out.pos.get();
            let pipe = out.conn.pipe.borrow();
            let Some(p) = pipe.as_ref() else { continue };
            let Some(cur) = &p.cursor else {
                // joined output: the frame carries the cursor
                if inside {
                    state.damage.trigger();
                }
                continue;
            };
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

    /// client-driven hide; vt teardown uses cursor_locked instead
    pub fn set_cursor_hidden(&self, hidden: bool) {
        self.sw_hidden.set(hidden);
        if self.sw.get() {
            return;
        }
        for out in self.outputs.borrow().iter() {
            out.cursor_client_hidden.set(hidden);
            let pipe = out.conn.pipe.borrow();
            let Some(p) = pipe.as_ref() else { continue };
            let Some(cur) = &p.cursor else { continue };
            cur.set_enabled(!hidden && !out.cursor_locked.get());
            drop(pipe);
            out.conn.cursor_commit(&out.dev);
        }
    }

    /// a client cursor surface's pixels onto every plane; the position
    /// decides which plane is actually enabled
    pub fn set_cursor_image(&self, px: &[u8], w: u32, h: u32, hot: (i32, i32)) {
        *self.sw_image.borrow_mut() = Some((px.to_vec(), w, h));
        self.sw_hot.set(hot);
        self.sw_hidden.set(false);
        self.sw_gen.set(self.sw_gen.get() + 1);
        if self.sw.get() {
            return;
        }
        for out in self.outputs.borrow().iter() {
            out.cursor_client_hidden.set(false);
            let pipe = out.conn.pipe.borrow();
            let Some(p) = pipe.as_ref() else { continue };
            let Some(cur) = &p.cursor else { continue };
            cur.write(px, w, h);
            cur.hotspot.set(hot);
            drop(pipe);
            out.conn.cursor_commit(&out.dev);
        }
    }

    /// back to the seeded theme arrow
    pub fn set_cursor_default(&self) {
        {
            let seed = self
                .outputs
                .borrow()
                .first()
                .and_then(|o| o.theme_cursor.borrow().clone());
            if let Some((px, w, h, hot)) = seed {
                *self.sw_image.borrow_mut() = Some((px, w, h));
                self.sw_hot.set(hot);
            }
            self.sw_hidden.set(false);
            self.sw_gen.set(self.sw_gen.get() + 1);
        }
        if self.sw.get() {
            return;
        }
        for out in self.outputs.borrow().iter() {
            let theme = out.theme_cursor.borrow();
            let Some((px, w, h, hot)) = theme.as_ref() else {
                continue;
            };
            let (px, w, h, hot) = (px.clone(), *w, *h, *hot);
            drop(theme);
            out.cursor_client_hidden.set(false);
            let pipe = out.conn.pipe.borrow();
            let Some(p) = pipe.as_ref() else { continue };
            let Some(cur) = &p.cursor else { continue };
            cur.write(&px, w, h);
            cur.hotspot.set(hot);
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
    /// the focused client asked for no cursor (set_cursor null)
    cursor_client_hidden: Cell<bool>,
    /// theme image + hotspot, restored whenever a client cursor goes away
    theme_cursor: RefCell<Option<(Vec<u8>, u32, u32, (i32, i32))>>,
    /// pending unlock for a switch that never pauses us; replacing it (next
    /// switch, or any pause/resume) cancels the stale timer
    rearm: RefCell<Option<SpawnedFuture<()>>>,
    /// implicit-sync fences of the dmabufs drawn this frame; the render
    /// submit waits them so client work lands before we sample
    frame_fences: RefCell<Vec<std::os::fd::OwnedFd>>,
    /// latched feedbacks drained here while composing for the display
    present_fbs: RefCell<Vec<crate::protocol::presentation::Feedback>>,
    collect_fbs: Cell<bool>,
    /// feedbacks riding the queued flip; fired on its completion event
    inflight_fbs: RefCell<Vec<crate::protocol::presentation::Feedback>>,
    inflight_vsync: Cell<bool>,
    /// a vrr config on an incapable panel complains once, not per frame
    vrr_warned: Cell<bool>,
    /// software-cursor texture; rebuilt when the image generation moves
    cursor_tex: RefCell<Option<Texture>>,
    cursor_gen: Cell<u64>,
    /// a live animation was sampled during this output's compose; the
    /// present loop stays dirty until one compose sees none
    pub anim_pending: Cell<bool>,
    /// an in-flight workspace switch; compose draws both scenes offset
    pub ws_switch: RefCell<Option<WsSwitch>>,
    /// unmapped layer surfaces still animating out
    pub closing_layers: RefCell<Vec<ClosingWindow>>,
}

#[derive(Clone)]
pub struct WsSwitch {
    pub from_ws: usize,
    /// 0 -> 1, arrival of the target workspace
    pub anim: crate::anim::Anim,
    pub vert: bool,
    /// slide distance as a fraction of the span; 0 = pure crossfade
    pub dist: f64,
    pub fade: bool,
    /// +1 when the target has the higher index
    pub sign: f64,
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
                    seed_cursor(state, &out);
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
                                o.rearm.take();
                            }
                        }
                        DeviceEvent::Resume { .. } => {
                            let sw = st
                                .display
                                .borrow()
                                .as_ref()
                                .is_some_and(|d| d.software_cursor());
                            let mut heads = Vec::new();
                            for o in &outs {
                                o.paused.set(false);
                                o.cursor_locked.set(false);
                                o.rearm.take();
                                // an in-flight flip never completes when the
                                // vt left; unwedge the gate
                                o.conn.flip_pending.set(false);
                                if let Some(p) = o.conn.pipe.borrow().as_ref() {
                                    if let Some(cur) = &p.cursor {
                                        // software mode keeps planes benched
                                        cur.set_enabled(!sw && !o.cursor_client_hidden.get());
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
            sw_image: RefCell::new(None),
            sw_hot: Cell::new((0, 0)),
            sw_hidden: Cell::new(false),
            sw: Cell::new(false),
            sw_gen: Cell::new(1),
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
        {
            let seed = display
                .outputs
                .borrow()
                .first()
                .and_then(|o| o.theme_cursor.borrow().clone());
            if let Some((px, w, h, hot)) = seed {
                *display.sw_image.borrow_mut() = Some((px, w, h));
                display.sw_hot.set(hot);
            }
        }
        if state.config.borrow().cursor.software {
            display.set_software_cursor(state, true);
        }
        // heads on OTHER cards are a phase-11 (multi-gpu) problem; say so
        // instead of leaving a black monitor unexplained
        warn_other_cards(&path);
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
pub fn screencopy(state: &Rc<State>, out_index: usize, region: Rect, cursor: bool) -> Option<Vec<u8>> {
    let d = state.display.borrow();
    let out = d.as_ref()?.outputs.borrow().get(out_index)?.clone();
    drop(d);
    let ops = compose_ops(state, &out, if cursor { CapCursor::Always } else { CapCursor::Never }, None);
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

/// compose a workspace offscreen - shown or not - and read it back at its
/// assigned output's size. hidden compose never embeds the cursor
pub fn workspace_copy(state: &Rc<State>, ws_index: usize) -> Option<Vec<u8>> {
    let out_slot = state.workspaces.borrow().get(ws_index)?.output.get();
    let out = {
        let d = state.display.borrow();
        let d = d.as_ref()?;
        let outs = d.outputs.borrow();
        outs.get(out_slot).cloned().or_else(|| outs.first().cloned())?
    };
    let ops = compose_ops(state, &out, CapCursor::Never, Some(ws_index));
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    match out.renderer.read_frame(out.width, out.height, &ops, waits) {
        Ok(px) => Some(px),
        Err(e) => {
            eprintln!("carrot: workspace copy render failed: {e}");
            None
        }
    }
}

/// output-local full rect + size lookup for the screencopy protocol
pub fn output_geometry(state: &Rc<State>, name: &str) -> Option<(usize, u32, u32)> {
    let d = state.display.borrow();
    let outs = d.as_ref()?.outputs.borrow().clone();
    outs.iter()
        .find(|o| o.conn.name == name)
        .map(|o| (o.index.get(), o.width, o.height))
}

/// the composited cursor: topmost, on the output under the pointer only
fn draw_sw_cursor(state: &Rc<State>, out: &Rc<Output>, ops: &mut Vec<RenderOp>, force: bool) {
    let d = state.display.borrow();
    let Some(d) = d.as_ref() else { return };
    // joined-pipe outputs have no plane at all; they composite regardless
    // of the global software-cursor setting. force paints for captures even
    // where the live path scans out on a hardware plane; a hidden cursor
    // stays hidden either way
    let plane_less = out
        .conn
        .pipe
        .borrow()
        .as_ref()
        .is_none_or(|p| p.cursor.is_none());
    if !(force || d.sw.get() || plane_less) || d.sw_hidden.get() || out.cursor_locked.get() {
        return;
    }
    let Some(seat) = state.seat.borrow().clone() else { return };
    let (px, py) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
    if !out.rect().contains(px, py) {
        return;
    }
    let image = d.sw_image.borrow();
    let Some((pixels, w, h)) = image.as_ref() else { return };
    let (w, h) = (*w, *h);
    // texture follows the image generation
    if out.cursor_gen.get() != d.sw_gen.get() || out.cursor_tex.borrow().is_none() {
        let tex = match out.renderer.create_texture(w, h, false) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("carrot: cursor texture failed: {e}");
                return;
            }
        };
        let row = (w * 4) as usize;
        let res = out.renderer.upload_texture(&tex, |dst| {
            dst[..row * h as usize].copy_from_slice(&pixels[..row * h as usize]);
        });
        if let Err(e) = res {
            eprintln!("carrot: cursor upload failed: {e}");
            return;
        }
        if let Some(old) = out.cursor_tex.borrow_mut().replace(tex) {
            out.renderer.destroy_texture(&old);
        }
        out.cursor_gen.set(d.sw_gen.get());
    }
    drop(image);
    let (hx, hy) = d.sw_hot.get();
    let (gx, gy) = out.pos.get();
    let x = px - hx - gx;
    let y = py - hy - gy;
    let tex = out.cursor_tex.borrow();
    let Some(tex) = tex.as_ref() else { return };
    let fx = |v: i32| v as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| v as f32 / out.height as f32 * 2.0 - 1.0;
    ops.push(RenderOp::Tex {
        view: tex.view,
        pos: [fx(x), fy(y)],
        size: [
            w as f32 / out.width as f32 * 2.0,
            h as f32 / out.height as f32 * 2.0,
        ],
        uv_pos: [0.0, 0.0],
        uv_size: [1.0, 1.0],
        mul: 1.0,
        opaque: false,
    });
}

/// I915_FORMAT_MOD_4_TILED: fourcc_mod_code(INTEL=1, 9); single-plane,
/// no compression metadata, display-engine friendly
const TILE4: u64 = (1u64 << 56) | 9;

/// kms-side tile4 allocation for one scanout buffer (xe only)
fn xe_tiled_buf(
    dev: &Rc<DrmDevice>,
    core: &Rc<VkCore>,
    renderer: &Rc<Renderer>,
    width: u32,
    height: u32,
    primary: &Rc<crate::drm::device::Plane>,
) -> Result<OutBuf, String> {
    let kms_mods = primary.modifiers(XRGB8888.drm);
    if !kms_mods.is_empty() && !kms_mods.contains(&TILE4) {
        return Err("plane does not advertise tile4".into());
    }
    // tile4 tiles are 128B x 32 rows; anv validates the explicit layout
    let pitch = (width * 4 + 127) & !127;
    let rows = (height + 31) & !31;
    let (placement, page) =
        sys::xe_vram_placement(dev.fd.as_fd()).map_err(|e| format!("mem regions: {e}"))?;
    let size = (pitch as u64 * rows as u64 + page - 1) & !(page - 1);
    let handle = sys::xe_gem_create_scanout(dev.fd.as_fd(), size, placement)
        .map_err(|e| format!("gem create: {e}"))?;
    let fb = match sys::addfb2(
        dev.fd.as_fd(),
        width,
        height,
        XRGB8888.drm,
        &[handle],
        &[pitch],
        &[0],
        Some(TILE4),
    ) {
        Ok(fb) => fb,
        Err(e) => {
            let _ = sys::gem_close(dev.fd.as_fd(), handle);
            return Err(format!("addfb2: {e}"));
        }
    };
    let dmabuf = match sys::prime_handle_to_fd(dev.fd.as_fd(), handle) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = sys::rmfb(dev.fd.as_fd(), fb);
            let _ = sys::gem_close(dev.fd.as_fd(), handle);
            return Err(format!("prime export: {e}"));
        }
    };
    // fb + dmabuf hold their own refs now
    let _ = sys::gem_close(dev.fd.as_fd(), handle);
    let bo = match import_bo(
        core,
        dmabuf,
        width,
        height,
        pitch,
        size,
        vk::Format::B8G8R8A8_UNORM,
        TILE4,
    ) {
        Ok(bo) => bo,
        Err(e) => {
            let _ = sys::rmfb(dev.fd.as_fd(), fb);
            return Err(format!("vulkan import: {e}"));
        }
    };
    let view = renderer
        .create_target_view(bo.image)
        .map_err(|e| format!("view: {e}"))?;
    Ok(OutBuf {
        bo,
        fb,
        view,
        undefined: Cell::new(true),
        dumb: None,
    })
}

/// sysfs peek: connected connectors on cards we did NOT bring up
fn warn_other_cards(active: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir("/sys/class/drm") else { return };
    let active = active.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // card1-DP-2 style entries only, skipping the active card's
        let Some((card, conn)) = name.split_once('-') else { continue };
        if card == active || !card.starts_with("card") {
            continue;
        }
        let status = std::fs::read_to_string(e.path().join("status")).unwrap_or_default();
        if status.trim() == "connected" {
            eprintln!(
                "carrot: {card} {conn} is connected but multi-gpu output is not built yet (phase 11); that monitor stays dark"
            );
        }
    }
}

/// seed the cursor plane: system theme, else built-in arrow
fn seed_cursor(state: &Rc<State>, out: &Rc<Output>) {
    if let Some(p) = out.conn.pipe.borrow().as_ref() {
        if let Some(cur) = &p.cursor {
            match crate::input::cursor_theme::load("left_ptr", &state.config.borrow().cursor) {
                Some(img) => {
                    cur.write(&img.pixels, img.width, img.height);
                    cur.hotspot.set(img.hotspot);
                    *out.theme_cursor.borrow_mut() =
                        Some((img.pixels, img.width, img.height, img.hotspot));
                }
                None => {
                    eprintln!("carrot: no xcursor theme found, using the built-in arrow");
                    let (px, w, h) = default_cursor();
                    cur.write(&px, w, h);
                    *out.theme_cursor.borrow_mut() = Some((px, w, h, (0, 0)));
                }
            }
        }
    }
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
        crate::protocol::session_lock::output_removed(state, &out.conn.name);
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
                seed_cursor(state, &out);
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
    for o in outs.iter() {
        crate::protocol::session_lock::output_resized(state, &o.conn.name);
    }

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
            ls.close_for_output_loss(state, old.get(slot).map(|o| o.conn.name.as_str()));
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
            if let Some(d) = state.display.borrow().as_ref() {
                d.move_cursor(state, cx as i32, cy as i32);
            }
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
        // tier 1.5: kms-allocated tile4, imported into vulkan. linear dumb
        // scanout starves the display fifo once a second head splits the
        // dbuf slices; tiled fetch is what the engine is built for
        if dev.driver == "xe" {
            match xe_tiled_buf(&dev, &core, &renderer, width, height, &primary) {
                Ok(buf) => return Ok(buf),
                Err(e) => {
                    eprintln!("carrot: tiled scanout unavailable ({e}); dumb-linear fallback")
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
        } else if bufs[0].bo.modifier == TILE4 {
            "kms tile4 import"
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
        cursor_client_hidden: Cell::new(false),
        theme_cursor: RefCell::new(None),
        rearm: RefCell::new(None),
        frame_fences: RefCell::new(Vec::new()),
        present_fbs: RefCell::new(Vec::new()),
        collect_fbs: Cell::new(false),
        inflight_fbs: RefCell::new(Vec::new()),
        inflight_vsync: Cell::new(true),
        vrr_warned: Cell::new(false),
        cursor_tex: RefCell::new(None),
        cursor_gen: Cell::new(0),
        anim_pending: Cell::new(false),
        ws_switch: RefCell::new(None),
        closing_layers: RefCell::new(Vec::new()),
    })
}

/// both scenes travel one step apart; out exits opposite the in's entry
fn switch_offsets(p: f64, step: f64, sign: f64) -> (f64, f64) {
    (-p * step * sign, (1.0 - p) * step * sign)
}

pub fn start_ws_switch(state: &Rc<State>, out: &Rc<Output>, from: usize, to: usize) {
    use crate::config::Style;
    let cfg = state.config.borrow().clone();
    let Some(motion) = cfg.animations.motion(crate::config::AnimKind::WorkspaceSwitch) else {
        *out.ws_switch.borrow_mut() = None;
        return;
    };
    let style = cfg.animations.workspace_switch.style.clone();
    let vert = matches!(style, Style::SlideVert | Style::SlideFadeVert { .. })
        || crate::tree::ws_axis_vertical(state);
    let (dist, fade) = match &style {
        Style::Fade => (0.0, true),
        Style::SlideFade { perc } | Style::SlideFadeVert { perc } => (*perc, true),
        _ => (1.0, false),
    };
    let sign = if to > from { 1.0 } else { -1.0 };
    state.anim_clock.touch();
    let now = state.anim_clock.now();
    let mut sw = out.ws_switch.borrow_mut();
    let anim = match &*sw {
        // a switch mid-slide restarts from the current progress
        Some(cur) if !cur.anim.is_done(now) => crate::config::build_anim(
            &state.anim_clock,
            motion,
            &cfg.animations,
            1.0 - cur.anim.clamped_value(now),
            1.0,
            cur.anim.velocity(now),
        ),
        _ => crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 0.0, 1.0, 0.0),
    };
    *sw = Some(WsSwitch { from_ws: from, anim, vert, dist, fade, sign });
}

async fn present_loop(state: &Rc<State>, out: &Rc<Output>) {
    let mut dirty = false;
    loop {
        EitherEvent(&out.damage, &out.conn.vblank).await;
        let _ = out.conn.vblank.take();
        // the completed flip carries the kernel timestamp its feedbacks
        // have been waiting for
        if !out.conn.flip_pending.get() && !out.inflight_fbs.borrow().is_empty() {
            let (sec, usec) = out.conn.flip_time.get();
            let refresh = if out.conn.vrr_want.get() {
                // no fixed period exists under variable refresh
                0
            } else {
                out.conn
                    .pipe
                    .borrow()
                    .as_ref()
                    .map(|p| {
                        let m = &p.mode;
                        (m.htotal as u64 * m.vtotal as u64 * 1_000_000 / m.clock.max(1) as u64)
                            as u32
                    })
                    .unwrap_or(0)
            };
            let mut flags = crate::protocol::presentation::FLAG_HW_CLOCK
                | crate::protocol::presentation::FLAG_HW_COMPLETION;
            if out.inflight_vsync.get() {
                flags |= crate::protocol::presentation::FLAG_VSYNC;
            }
            for fb in out.inflight_fbs.borrow_mut().drain(..) {
                fb.presented(&out.conn.name, sec, usec * 1000, refresh, out.conn.seq64.get(), flags);
            }
        }
        dirty |= out.damage.take();
        dirty |= out.anim_pending.get();
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
        let sync = frame.export_sync_file(&out.renderer).ok().map(Rc::new);
        out.conn.vrr_want.set(vrr_wanted(state, out));
        out.inflight_vsync.set(true);
        let res = if tearing_wanted(state, out) && !out.conn.vrr_dirty() {
            // async commits are FB-only - no fence rides along, so the render
            // has to be finished before the kernel sees the buffer
            if let Some(fd) = &sync {
                let _ = state.ring.readable(fd).await;
            }
            match out.conn.flip_async(&out.dev, buf.fb) {
                // the kernel can reject async for reasons of its own; the
                // frame still has to land
                Err(_) => out
                    .conn
                    .flip(&out.dev, buf.fb, sync.as_ref().map(|fd| fd.as_raw_fd())),
                res => {
                    out.inflight_vsync.set(false);
                    res
                }
            }
        } else {
            out.conn
                .flip(&out.dev, buf.fb, sync.as_ref().map(|fd| fd.as_raw_fd()))
        };
        match res {
            Ok(FlipResult::Queued) => {
                out.front.set(back);
                out.inflight_fbs
                    .borrow_mut()
                    .append(&mut out.present_fbs.borrow_mut());
            }
            Ok(FlipResult::NotPresented) => {
                // retry next wakeup with a fresh frame
                dirty = true;
            }
            Err(crate::drm::device::DrmError::LostMaster) => {
                // vt left between the pause signal and our commit; resume
                // re-modesets and re-damages. abandoned frames never present
                for fb in out.present_fbs.borrow_mut().drain(..) {
                    fb.discarded();
                }
                for fb in out.inflight_fbs.borrow_mut().drain(..) {
                    fb.discarded();
                }
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
        crate::protocol::session_lock::output_presented(state, &out.conn.name);
        crate::portal::cast::output_presented(state, &out.conn.name);
    }
}

/// screens dark or awake without touching the session. off folds every
/// head's disable into one commit; on replays the resume modeset
pub fn dpms(state: &Rc<State>, on: bool) {
    if state.dpms_off.get() != on {
        return;
    }
    let outs: Vec<Rc<Output>> = match state.display.borrow().as_ref() {
        Some(d) => d.outputs.borrow().clone(),
        None => return,
    };
    if !on {
        state.dpms_off.set(true);
        for o in &outs {
            o.paused.set(true);
        }
        if let Some(o) = outs.first() {
            if let Err(e) = o.dev.modeset_heads(&[]) {
                eprintln!("carrot: dpms off: {e}");
            }
        }
        eprintln!("carrot: dpms: screens off");
        return;
    }
    state.dpms_off.set(false);
    let mut heads = Vec::new();
    for o in &outs {
        o.paused.set(false);
        o.conn.flip_pending.set(false);
        heads.push((o.conn.clone(), o.bufs[o.front.get()].fb));
    }
    if let Some(o) = outs.first() {
        if let Err(e) = o.dev.modeset_heads(&heads) {
            eprintln!("carrot: dpms on: {e}");
        }
    }
    eprintln!("carrot: dpms: screens on");
    state.damage.trigger();
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
        .map(|o| o.vrr)
        .unwrap_or(crate::config::Vrr::Off);
    if mode == crate::config::Vrr::Off {
        return false;
    }
    if !out.conn.vrr_capable {
        if !out.vrr_warned.replace(true) {
            eprintln!(
                "carrot: {}: vrr configured but the panel is not vrr capable",
                out.conn.name
            );
        }
        return false;
    }
    match mode {
        crate::config::Vrr::Always => true,
        // automatic: a fullscreen window is what's on glass
        _ => state
            .workspaces
            .borrow()
            .get(out.ws.get())
            .is_some_and(|ws| ws.fullscreen.borrow().is_some()),
    }
}

/// tear only when the config allows it, the device can, and the fullscreen
/// surface asked for async presentation; pending cursor changes ride the
/// sync path so they aren't lost
fn tearing_wanted(state: &Rc<State>, out: &Rc<Output>) -> bool {
    let allowed = state
        .config
        .borrow()
        .outputs
        .iter()
        .find(|o| o.name == out.conn.name)
        .is_some_and(|o| o.allow_tearing);
    if !out.dev.supports_async_flip || !allowed {
        return false;
    }
    if out.conn.cursor_changed() {
        return false;
    }
    let ws = state.workspaces.borrow().get(out.ws.get()).cloned();
    let fs = ws.and_then(|ws| ws.fullscreen.borrow().clone());
    match fs {
        // the client's async hint or a window-rule immediate both qualify
        Some(w) => w.surface().tearing.get() || w.rule_immediate.get(),
        None => false,
    }
}

/// paint order tiled, fullscreen, floats; each window drawn as its surface
/// stack, clipped to the window box so CSD can't leak past the tile. popups
/// sit above their window.
/// how a composition treats the cursor: presents follow the hw/sw split,
/// captures follow the requesting client's overlay flag
#[derive(Clone, Copy, PartialEq)]
enum CapCursor {
    Present,
    Always,
    Never,
}

fn compose(state: &Rc<State>, out: &Rc<Output>) -> Vec<RenderOp> {
    // presentation feedbacks are claimed only by display composes, never
    // by captures
    out.collect_fbs.set(true);
    let ops = compose_ops(state, out, CapCursor::Present, None);
    out.collect_fbs.set(false);
    ops
}

fn compose_ops(
    state: &Rc<State>,
    out: &Rc<Output>,
    cursor: CapCursor,
    ws_override: Option<usize>,
) -> Vec<RenderOp> {
    // animations sample the moment this frame will glass, not "now"
    let (sec, usec) = out.conn.flip_time.get();
    let period_ns = out
        .conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| {
            let m = &p.mode;
            m.htotal as u64 * m.vtotal as u64 * 1_000_000_000 / (m.clock.max(1) as u64 * 1000)
        })
        .unwrap_or(0);
    let flip_ns = sec as u64 * 1_000_000_000 + usec as u64 * 1000;
    let target = if flip_ns == 0 || period_ns == 0 || out.conn.vrr_want.get() {
        crate::util::Time::now().nsec()
    } else {
        flip_ns + period_ns
    };
    state.anim_clock.freeze(target);
    out.anim_pending.set(false);

    let mut ops = Vec::new();
    let mut live: Vec<(ClientId, u64)> = Vec::new();
    // a locked session shows nothing but the lock surface; an output
    // without one stays a cleared frame
    if crate::protocol::session_lock::locked(state) {
        let screen = out.rect();
        if let Some(s) = crate::protocol::session_lock::compose_locked(state, &out.conn.name) {
            draw_surface_tree(out, &s, screen.x1, screen.y1, screen, 1.0, &mut ops, &mut live);
        }
        if cursor != CapCursor::Never {
            draw_sw_cursor(state, out, &mut ops, cursor == CapCursor::Always);
        }
        // normal content must not keep textures alive across the lock
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
        drop(textures);
        return ops;
    }
    let ws = {
        let list = state.workspaces.borrow();
        match list.get(ws_override.unwrap_or(out.ws.get())) {
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

    // paint order: background, bottom, tiled, fullscreen, floats, top,
    // overlay, layer popups; fullscreen hides everything below itself
    // except overlay
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::BACKGROUND, screen, &mut ops, &mut live);
        draw_layer(state, out, crate::shell::layer::BOTTOM, screen, &mut ops, &mut live);
    }
    let sw = out.ws_switch.borrow().clone();
    let now = state.anim_clock.now();
    match &sw {
        Some(s) if !s.anim.is_done(now) => {
            let p = s.anim.clamped_value(now);
            // full ndc span plus a tenth of it as the gap between scenes
            let step = 2.2 * s.dist;
            let (off_out, off_in) = switch_offsets(p, step, s.sign);
            let (a_out, a_in) = if s.fade { ((1.0 - p) as f32, p as f32) } else { (1.0, 1.0) };
            let from = state.workspaces.borrow().get(s.from_ws).cloned();
            if let Some(from) = from {
                let mark = ops.len();
                ws_scene(state, out, &from, &focused, &cfg, screen, &mut ops, &mut live);
                let d = if s.vert { [0.0, off_out as f32] } else { [off_out as f32, 0.0] };
                apply_batch(&mut ops[mark..], [0.0, 0.0], 1.0, a_out, d);
            }
            let mark = ops.len();
            ws_scene(state, out, &ws, &focused, &cfg, screen, &mut ops, &mut live);
            let d = if s.vert { [0.0, off_in as f32] } else { [off_in as f32, 0.0] };
            apply_batch(&mut ops[mark..], [0.0, 0.0], 1.0, a_in, d);
            out.anim_pending.set(true);
        }
        _ => {
            if sw.is_some() {
                *out.ws_switch.borrow_mut() = None;
            }
            ws_scene(state, out, &ws, &focused, &cfg, screen, &mut ops, &mut live);
        }
    }
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::TOP, screen, &mut ops, &mut live);
        draw_closing_list(state, out, &out.closing_layers, &mut ops);
    }
    draw_layer(state, out, crate::shell::layer::OVERLAY, screen, &mut ops, &mut live);
    draw_layer_popups(state, out, fs.is_some(), screen, &mut ops, &mut live);
    // a drag icon rides the pointer, above everything but the cursor
    if ws_override.is_none() {
        if let Some(seat) = state.seat.borrow().clone() {
            if let Some(drag) = seat.data.drag() {
                if let Some(icon) = drag.icon.borrow().clone() {
                    let (dx, dy) = drag.icon_off.get();
                    let (px, py) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                    draw_surface_tree(out, &icon, px + dx, py + dy, screen, 1.0, &mut ops, &mut live);
                }
            }
        }
    }
    if cursor != CapCursor::Never {
        draw_sw_cursor(state, out, &mut ops, cursor == CapCursor::Always);
    }

    // an override composes a side scene; only the present retires textures
    if ws_override.is_some() {
        return ops;
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
    if out.collect_fbs.get() {
        let mut latched = surface.latched_feedbacks.borrow_mut();
        if !latched.is_empty() {
            out.present_fbs.borrow_mut().append(&mut latched);
        }
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

// one shell layer, mapping order = z within it
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
        let mark = ops.len();
        let lmark = live.len();
        draw_surface_tree(out, &ls.surface, r.x1, r.y1, screen, 1.0, ops, live);
        let anim = ls.anim.borrow().clone();
        if let Some((a, style)) = anim {
            use crate::config::Style;
            let now = state.anim_clock.now();
            if a.is_done(now) {
                *ls.anim.borrow_mut() = None;
            } else {
                let p = a.clamped_value(now);
                let (scale, alpha, d) = match &style {
                    Style::Popin { perc } => {
                        ((perc + (1.0 - perc) * p) as f32, p as f32, [0.0, 0.0])
                    }
                    Style::Fade => (1.0, p as f32, [0.0, 0.0]),
                    Style::Slide { dir } => {
                        let dir = dir.or_else(|| ls.slide_dir());
                        (1.0, 1.0, slide_delta(out, r, dir, 1.0 - p))
                    }
                    _ => (1.0, 1.0, [0.0, 0.0]),
                };
                apply_batch(&mut ops[mark..], center_ndc(out, r), scale, alpha, d);
                out.anim_pending.set(true);
            }
        }
        *ls.last_batch.borrow_mut() = (ops[mark..].to_vec(), live[lmark..].to_vec());
    }
}

// layer-parented popups form one band above every layer; a popup of a
// layer hidden by fullscreen hides with its parent
fn draw_layer_popups(
    state: &Rc<State>,
    out: &Rc<Output>,
    fs_active: bool,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    let layers = state.layers.borrow().clone();
    for ls in layers.iter() {
        if !ls.mapped() || ls.output.get() != out.index.get() {
            continue;
        }
        if fs_active && ls.current.get().layer != crate::shell::layer::OVERLAY {
            continue;
        }
        let r = ls.rect.get();
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

/// one workspace's content: tiled, closings, fullscreen, floats
fn ws_scene(
    state: &Rc<State>,
    out: &Rc<Output>,
    ws: &Rc<crate::tree::workspace::Workspace>,
    focused: &Option<Rc<WlSurface>>,
    cfg: &crate::config::Config,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    let fs = ws.fullscreen.borrow().clone();
    if fs.is_none() {
        ws.tiling
            .for_each(|win| draw_window(state, out, focused, cfg, screen, win, ops, live));
        draw_closings(state, out, ws, ops);
    }
    if let Some(f) = &fs {
        draw_window(state, out, focused, cfg, screen, f, ops, live);
    }
    if fs.is_none() || cfg.layout.float_above_fullscreen {
        for win in ws.floats.borrow().iter() {
            draw_window(state, out, focused, cfg, screen, win, ops, live);
        }
    }
}

fn draw_window(
    state: &Rc<State>,
    out: &Rc<Output>,
    focused: &Option<Rc<WlSurface>>,
    cfg: &crate::config::Config,
    screen: Rect,
    win: &Rc<crate::tree::Window>,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<(ClientId, u64)>,
) {
    let surface = win.surface();
    if !surface.mapped.get() {
        return;
    }
    let mark = ops.len();
    let lmark = live.len();
    let rect = win.visual_rect(state);
    if win.anims_live(state.anim_clock.now()) {
        out.anim_pending.set(true);
    }
    if !win.fullscreen.get() {
        let want = match focused {
            Some(f) => {
                if Rc::ptr_eq(f, &surface) {
                    cfg.layout.border.active
                } else {
                    cfg.layout.border.inactive
                }
            }
            None => cfg.layout.border.inactive,
        };
        let color = win.border_color_now(state, want);
        push_borders(out, rect, cfg.layout.border.width, color, ops);
    }
    let geo = win.geometry();
    let alpha = win.rule_opacity.get().unwrap_or(1.0);
    draw_surface_tree(out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, alpha, ops, live);
    if let Some(tl) = win.xdg_opt() {
        draw_popups(state, out, &tl.xdg, rect.x1, rect.y1, screen, ops, live);
    }
    let open = win.anims.borrow().open.clone();
    if let Some((a, style)) = open {
        if !win.fullscreen.get() {
            use crate::config::Style;
            let p = a.clamped_value(state.anim_clock.now());
            let (scale, alpha, d) = match &style {
                Style::Popin { perc } => ((perc + (1.0 - perc) * p) as f32, p as f32, [0.0, 0.0]),
                Style::Fade => (1.0, p as f32, [0.0, 0.0]),
                Style::Slide { dir } => (1.0, 1.0, slide_delta(out, rect, *dir, 1.0 - p)),
                _ => (1.0, 1.0, [0.0, 0.0]),
            };
            apply_batch(&mut ops[mark..], center_ndc(out, rect), scale, alpha, d);
        }
    }
    *win.last_batch.borrow_mut() = (ops[mark..].to_vec(), live[lmark..].to_vec());
}

// -- closing windows: the final batch outlives the surface --

pub struct ClosingWindow {
    pub ops: Vec<RenderOp>,
    /// ownership moved out of out.textures; retired when the anim ends
    pub keep: Vec<Texture>,
    pub rect: Rect,
    pub anim: crate::anim::Anim,
    pub style: crate::config::Style,
}

/// wrap a cached final batch, taking ownership of its textures
pub fn seize_batch(
    out: &Rc<Output>,
    batch: (Vec<RenderOp>, Vec<(ClientId, u64)>),
    rect: Rect,
    anim: crate::anim::Anim,
    style: crate::config::Style,
) -> Option<ClosingWindow> {
    let (ops, keys) = batch;
    if ops.is_empty() {
        return None;
    }
    let mut keep = Vec::new();
    let mut textures = out.textures.borrow_mut();
    for k in &keys {
        if let Some((t, _)) = textures.remove(k) {
            keep.push(t);
        }
    }
    Some(ClosingWindow { ops, keep, rect, anim, style })
}

/// seize the window's cached final batch and its textures
pub fn capture_closing(
    out: &Rc<Output>,
    win: &crate::tree::Window,
    rect: Rect,
    anim: crate::anim::Anim,
    style: crate::config::Style,
) -> Option<ClosingWindow> {
    let batch = std::mem::take(&mut *win.last_batch.borrow_mut());
    seize_batch(out, batch, rect, anim, style)
}

fn cap_evict(list: &mut Vec<ClosingWindow>) -> Option<ClosingWindow> {
    if list.len() >= 8 {
        return Some(list.remove(0));
    }
    None
}

/// bounded stash; the evicted entry's textures go to the retire queue
pub fn push_closing(out: &Rc<Output>, ws: &crate::tree::workspace::Workspace, cw: ClosingWindow) {
    let mut list = ws.closing.borrow_mut();
    if let Some(old) = cap_evict(&mut list) {
        out.retired_tex.borrow_mut().extend(old.keep);
    }
    list.push(cw);
}

pub fn push_closing_layer(out: &Rc<Output>, cw: ClosingWindow) {
    let mut list = out.closing_layers.borrow_mut();
    if let Some(old) = cap_evict(&mut list) {
        out.retired_tex.borrow_mut().extend(old.keep);
    }
    list.push(cw);
}

fn draw_closings(
    state: &Rc<State>,
    out: &Rc<Output>,
    ws: &crate::tree::workspace::Workspace,
    ops: &mut Vec<RenderOp>,
) {
    draw_closing_list(state, out, &ws.closing, ops);
}

fn draw_closing_list(
    state: &Rc<State>,
    out: &Rc<Output>,
    list: &RefCell<Vec<ClosingWindow>>,
    ops: &mut Vec<RenderOp>,
) {
    use crate::config::Style;
    let now = state.anim_clock.now();
    let mut list = list.borrow_mut();
    let mut i = 0;
    while i < list.len() {
        if list[i].anim.is_done(now) {
            let cw = list.remove(i);
            out.retired_tex.borrow_mut().extend(cw.keep);
            continue;
        }
        i += 1;
    }
    for c in list.iter() {
        // presence runs 1 -> 0
        let p = c.anim.clamped_value(now);
        let mark = ops.len();
        ops.extend(c.ops.iter().cloned());
        let (scale, alpha, d) = match &c.style {
            Style::Fade => (1.0, p as f32, [0.0, 0.0]),
            Style::Slide { dir } => (1.0, 1.0, slide_delta(out, c.rect, *dir, 1.0 - p)),
            // popin and everything else: shrink toward 80% while fading
            _ => ((0.8 + 0.2 * p) as f32, p as f32, [0.0, 0.0]),
        };
        apply_batch(&mut ops[mark..], center_ndc(out, c.rect), scale, alpha, d);
        out.anim_pending.set(true);
    }
}

// -- op-batch transforms: open/close styles, workspace slides --

/// scale about an ndc center, multiply alpha, then translate; the whole
/// window (borders, subsurfaces, popups) moves as one
fn apply_batch(ops: &mut [RenderOp], center: [f32; 2], scale: f32, alpha: f32, d: [f32; 2]) {
    for op in ops {
        match op {
            RenderOp::Fill { pos, size, color } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                }
                color[3] *= alpha;
            }
            RenderOp::Tex { pos, size, mul, opaque, .. } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                }
                *mul *= alpha;
                if alpha < 1.0 {
                    *opaque = false;
                }
            }
        }
    }
}

fn center_ndc(out: &Output, r: Rect) -> [f32; 2] {
    let (gx, gy) = out.pos.get();
    [
        ((r.x1 + r.x2) as f32 / 2.0 - gx as f32) / out.width as f32 * 2.0 - 1.0,
        ((r.y1 + r.y2) as f32 / 2.0 - gy as f32) / out.height as f32 * 2.0 - 1.0,
    ]
}

/// the ndc offset that parks the window past a screen edge at remaining=1
/// and decays to zero; no dir picks the nearest edge
fn slide_delta(out: &Output, r: Rect, dir: Option<crate::config::Dir>, remaining: f64) -> [f32; 2] {
    use crate::config::Dir;
    let (gx, gy) = out.pos.get();
    let (w, h) = (out.width as i32, out.height as i32);
    let dir = dir.unwrap_or_else(|| {
        let gaps = [
            (r.y1 - gy, Dir::Up),
            (gy + h - r.y2, Dir::Down),
            (r.x1 - gx, Dir::Left),
            (gx + w - r.x2, Dir::Right),
        ];
        gaps.iter().min_by_key(|(g, _)| *g).map(|(_, d)| *d).unwrap_or(Dir::Up)
    });
    let (px, py) = match dir {
        Dir::Up => (0, -(r.y2 - gy)),
        Dir::Down => (0, gy + h - r.y1),
        Dir::Left => (-(r.x2 - gx), 0),
        Dir::Right => (gx + w - r.x1, 0),
    };
    [
        (px as f64 * remaining / out.width as f64 * 2.0) as f32,
        (py as f64 * remaining / out.height as f64 * 2.0) as f32,
    ]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_offsets_travel_together() {
        // start: incoming parked a full step away, outgoing in place
        let (o, i) = switch_offsets(0.0, 2.2, 1.0);
        assert_eq!((o, i), (0.0, 2.2));
        // end: outgoing fully out the opposite side, incoming settled
        let (o, i) = switch_offsets(1.0, 2.2, 1.0);
        assert_eq!((o, i), (-2.2, 0.0));
        // the gap between them stays one step the whole way
        let (o, i) = switch_offsets(0.37, 2.2, -1.0);
        assert!(((i - o) - -2.2).abs() < 1e-6);
    }

    #[test]
    fn closing_cap_evicts_oldest() {
        let clock = crate::anim::AnimClock::new();
        let mk = |x: i32| ClosingWindow {
            ops: vec![RenderOp::Fill { pos: [0.0; 2], size: [1.0; 2], color: [1.0; 4] }],
            keep: Vec::new(),
            rect: Rect { x1: x, y1: 0, x2: x + 10, y2: 10 },
            anim: crate::anim::Anim::ease(&clock, 1.0, 0.0, 150, crate::anim::Curve::Linear),
            style: crate::config::Style::Fade,
        };
        let mut list: Vec<ClosingWindow> = (0..8).map(mk).collect();
        let evicted = cap_evict(&mut list);
        assert_eq!(evicted.map(|c| c.rect.x1), Some(0), "oldest goes first");
        assert_eq!(list.len(), 7);
        assert!(cap_evict(&mut list).is_none(), "under the cap nothing evicts");
    }

    #[test]
    fn apply_batch_scales_about_center_and_fades() {
        let mut ops = vec![RenderOp::Fill {
            pos: [-0.5, -0.5],
            size: [1.0, 1.0],
            color: [1.0; 4],
        }];
        apply_batch(&mut ops, [0.0, 0.0], 0.5, 0.5, [0.25, 0.0]);
        let RenderOp::Fill { pos, size, color } = &ops[0] else {
            panic!("op kind changed");
        };
        // -0.5 scaled about 0 is -0.25; x then translates by +0.25
        assert_eq!(*pos, [0.0, -0.25]);
        assert_eq!(*size, [0.5, 0.5]);
        assert_eq!(color[3], 0.5);
    }
}
