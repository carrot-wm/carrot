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
use crate::render::renderer::{Frame, FrameTarget, PrePass, PreUpload, RenderOp, Renderer, Texture};
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
            if !out.conn.cursor_commit(&out.dev) {
                // the kernel won't take this plane (joined-pipe quirks, odm);
                // never leave the user cursorless - composite from here on
                self.set_software_cursor(state, true);
                return;
            }
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
    /// callback pacer: fire the sweep at this instant (0 = nothing owed)
    cb_at: Cell<u64>,
    cb_kick: AsyncEvent,
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
    /// offscreen work deferred by compose; the next submit (present or
    /// capture) drains both into its command buffer
    prepasses: RefCell<Vec<PrePass>>,
    preuploads: RefCell<Vec<PreUpload>>,
    /// keys captures sampled recently, with the feed time; the sweep
    /// spares them so cast ticks stop re-importing every frame
    cast_keep: RefCell<HashMap<(ClientId, u64), u64>>,
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
    /// present-compose scratch, reused every frame to spare the allocator
    ops_scratch: RefCell<Vec<RenderOp>>,
    live_scratch: RefCell<Vec<((ClientId, u64), u64)>>,
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
    /// blurred-backdrop cache; levels[0] doubles as the finished result
    pub blur: RefCell<Option<BlurCache>>,
    /// the backdrop changed since the cache was built
    pub blur_dirty: Cell<bool>,
    /// submitted frames whose gpu work may still be running; a side task
    /// waits each render fence and retires them off the present loop
    gpu_done: crate::util::AsyncQueue<CleanupJob>,
    /// late-latch scheduler state
    sched: Sched,
    /// direct-scanout state; the queued flip's zero-copy-ness rides along
    /// for its presentation feedback
    scanout: RefCell<ScanoutState>,
    inflight_zero_copy: Cell<bool>,
}

/// everything whose lifetime ends at a frame's render fence
struct CleanupJob {
    frame: Frame,
    sync: Option<Rc<std::os::fd::OwnedFd>>,
    retired: Vec<Texture>,
    /// when the frame was submitted; the fence-signal instant closes the
    /// gpu-tail sample for the latch scheduler
    submit_ns: u64,
    flight: FlightGuard,
}

/// one submitted frame's slot in the global in-flight count. drop-driven
/// so cancellation can't wedge it: an unplugged output's cleanup task
/// dies mid-await with jobs queued, and a count that never reaches zero
/// would park every replaced dmabuf attachment for the session
struct FlightGuard {
    state: Rc<State>,
}

impl FlightGuard {
    fn new(state: &Rc<State>) -> FlightGuard {
        state.frames_in_flight.set(state.frames_in_flight.get() + 1);
        FlightGuard { state: state.clone() }
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        let inflight = self.state.frames_in_flight.get().saturating_sub(1);
        self.state.frames_in_flight.set(inflight);
        // parked attachments release once NO output still has a frame in
        // flight that might sample them
        if inflight == 0 {
            self.state.retired.borrow_mut().clear();
        }
    }
}

/// the timing model behind late latching: how long a frame costs on the
/// cpu (latch to flip-queued, tracked per path) and on the gpu (submit to
/// fence, compose only), plus an adaptive safety margin. all nanoseconds
struct Sched {
    /// compose path: latch -> flip queued
    ewma_compose: Cell<u64>,
    /// scanout path: latch -> flip queued; no gpu work ever rides this,
    /// so the compose estimates must never price its deadline
    ewma_scanout: Cell<u64>,
    ewma_gpu: Cell<u64>,
    /// AIMD: doubles on a missed vblank, halves when a flip lands a whole
    /// vblank early (overshoot, not success), decays linearly on a met
    /// one. handles transient adversity; the hardware lead lives in cutoff
    margin: Cell<u64>,
    /// learned commit cutoff: how far ahead of the vblank the kernel needs
    /// the ioctl. rises above any lead that missed, decays slowly to keep
    /// probing for a lower edge. the config floor seeds and bounds it
    cutoff: Cell<u64>,
    /// a miss has proven where the edge is; until then the seed sheds fast
    cutoff_settled: Cell<bool>,
    /// clean frames left before the cutoff may probe downward again; a
    /// fresh raise arms this so probe misses stay far apart in the tail
    cutoff_hold: Cell<u32>,
    misses: Cell<u32>,
    clean: Cell<u32>,
    /// enough consecutive misses park the policy on render-at-flip-done
    /// until the pipe proves stable again
    fallback: Cell<bool>,
    /// the glass instant the queued flip aimed for; 0 = nothing aimed
    target_ns: Cell<u64>,
    /// when that flip's ioctl went in; lead = target - this
    aim_ioctl_ns: Cell<u64>,
    /// last flip-done actually accounted, so each is counted once
    last_seq: Cell<u64>,
}

impl Default for Sched {
    fn default() -> Sched {
        Sched {
            // deliberately fat startup estimates; real samples pull them in
            ewma_compose: Cell::new(1_000_000),
            ewma_scanout: Cell::new(100_000),
            ewma_gpu: Cell::new(2_000_000),
            margin: Cell::new(0),
            // the old static floor, now just the starting estimate
            cutoff: Cell::new(500_000),
            cutoff_settled: Cell::new(false),
            cutoff_hold: Cell::new(0),
            misses: Cell::new(0),
            clean: Cell::new(0),
            fallback: Cell::new(false),
            target_ns: Cell::new(0),
            aim_ioctl_ns: Cell::new(0),
            last_seq: Cell::new(0),
        }
    }
}

/// which pipeline the coming latch feeds; scanout skips the gpu, so its
/// deadline sits much closer to the vblank
#[derive(Copy, Clone, PartialEq)]
enum PresentPath {
    Compose,
    Scanout,
}

impl Sched {
    fn ewma(cell: &Cell<u64>, sample: u64) {
        let e = cell.get();
        cell.set(e - e / 8 + sample / 8);
    }

    fn budget_ns(&self, path: PresentPath, floor_ns: u64) -> u64 {
        let cpu = match path {
            PresentPath::Compose => self.ewma_compose.get() + self.ewma_gpu.get(),
            PresentPath::Scanout => self.ewma_scanout.get(),
        };
        cpu + (self.cutoff.get() + self.margin.get()).max(floor_ns)
    }
}

/// where a completed flip landed relative to the vblank it aimed for
#[derive(Copy, Clone, PartialEq, Debug)]
enum Aim {
    /// within half a period of the target: the model is honest
    Clean,
    /// a vblank (or more) late: the budget lied
    Miss,
    /// a vblank early: the aim drifted out and latency doubled silently
    Early,
}

fn score_flip(flip_ns: u64, target_ns: u64, period: u64) -> Aim {
    if flip_ns > target_ns + period / 2 {
        Aim::Miss
    } else if flip_ns + period / 2 < target_ns {
        Aim::Early
    } else {
        Aim::Clean
    }
}

/// the sleep instant and the vblank it aims for. the budget is clamped
/// under one period so a fat estimate degrades to render-at-flip-done
/// instead of silently slipping the aim a whole frame out
fn latch_instants(now: u64, flip_ns: u64, period: u64, budget: u64) -> (u64, u64) {
    let budget = budget.min(period.saturating_sub(MIN_SLACK_NS));
    let k = (now + budget).saturating_sub(flip_ns).div_ceil(period).max(1);
    let target = flip_ns + k * period;
    (target - budget, target)
}

/// online estimate of the kernel's commit deadline. a missed flip proves
/// its lead was short, so the estimate jumps decisively above it; met
/// deadlines walk it back down - fast while the seed is still a guess,
/// slowly once a miss has located the edge, so probe misses stay far out
/// in the tail. early flips say nothing about the hardware
fn cutoff_step(cutoff: u64, aim: Aim, lead_ns: u64, period: u64, settled: bool, held: bool) -> u64 {
    match aim {
        Aim::Miss => {
            // only leads probing the estimate's neighborhood carry
            // information about the hardware edge. a frame that missed
            // with a fat lead (session start, gpu stall) failed for some
            // other reason - the margin doubled on the same miss and
            // covers it. one such miss used to vault the cutoff onto the
            // period/2 cap, where the settled decay held it all session,
            // taxing every latch ~0.75ms
            if lead_ns <= cutoff + CUTOFF_ATTRIB_NS {
                cutoff.max(lead_ns + CUTOFF_BUMP_NS).min(period / 2)
            } else {
                cutoff
            }
        }
        Aim::Clean if held => cutoff,
        Aim::Clean => cutoff.saturating_sub(if settled {
            CUTOFF_DECAY_NS
        } else {
            CUTOFF_DECAY_FAST_NS
        }),
        Aim::Early => cutoff,
    }
}

#[cfg(test)]
mod sched_tests {
    use super::*;

    const P: u64 = 2_083_381;

    #[test]
    fn dropped_flight_guards_never_wedge_the_count() {
        let (state, _client) = crate::client::test_utils::test_client();
        let a = FlightGuard::new(&state);
        let b = FlightGuard::new(&state);
        assert_eq!(state.frames_in_flight.get(), 2);
        // guards decrement by drop alone - a cancelled cleanup task takes
        // this same path, so the count can never wedge above zero
        drop(a);
        assert_eq!(state.frames_in_flight.get(), 1);
        drop(b);
        assert_eq!(state.frames_in_flight.get(), 0);
    }

    #[test]
    fn score_lands_where_it_aims() {
        assert_eq!(score_flip(10 * P, 10 * P, P), Aim::Clean);
        assert_eq!(score_flip(10 * P + P / 2 - 1, 10 * P, P), Aim::Clean);
        assert_eq!(score_flip(11 * P, 10 * P, P), Aim::Miss);
        assert_eq!(score_flip(9 * P, 10 * P, P), Aim::Early);
    }

    #[test]
    fn cutoff_learns_the_commit_deadline() {
        // a probe miss just under the estimate raises it decisively
        let c = cutoff_step(200_000, Aim::Miss, 250_000, P, true, false);
        assert_eq!(c, 250_000 + CUTOFF_BUMP_NS);
        // a miss with a fat lead teaches nothing - startup poison
        assert_eq!(cutoff_step(200_000, Aim::Miss, 1_900_000, P, true, false), 200_000);
        // attributed raises still cap at half a period
        assert_eq!(
            cutoff_step(P / 2, Aim::Miss, P / 2 + 50_000, P, true, false),
            P / 2
        );
        // an unsettled seed sheds fast; a settled edge probes slowly
        assert_eq!(
            cutoff_step(500_000, Aim::Clean, 500_000, P, false, false),
            500_000 - CUTOFF_DECAY_FAST_NS
        );
        assert_eq!(
            cutoff_step(300_000, Aim::Clean, 300_000, P, true, false),
            300_000 - CUTOFF_DECAY_NS
        );
        // a fresh raise holds the estimate: no downward probing yet
        assert_eq!(cutoff_step(300_000, Aim::Clean, 300_000, P, true, true), 300_000);
        // an early flip says nothing about the hardware
        assert_eq!(cutoff_step(300_000, Aim::Early, 300_000, P, true, false), 300_000);
    }

    #[test]
    fn callback_lead_prices_per_pipeline() {
        // windowed: compose budget + grace, nothing else
        assert_eq!(callback_lead(FsContent::Windowed, 900_000, 300_000, P), 1_200_000);
        // fullscreen shm adds the commit-time upload ride
        assert_eq!(
            callback_lead(FsContent::Shm, 900_000, 300_000, P),
            900_000 + UPLOAD_LEAD_NS + 300_000
        );
        // fullscreen dmabuf prices with the scanout budget it was given
        assert_eq!(callback_lead(FsContent::Dmabuf, 600_000, 300_000, P), 900_000);
        // an oversized fullscreen-shm chain wakes at flip-done: only the
        // earliest commit's fence makes the coming scan
        assert_eq!(callback_lead(FsContent::Shm, 1_300_000, 600_000, P), P);
        // oversized windowed chains keep the latest-wake clamp
        assert_eq!(
            callback_lead(FsContent::Windowed, 3_000_000, 600_000, P),
            P - CB_HEADROOM_NS
        );
    }

    #[test]
    fn latch_aims_one_vblank_out() {
        let flip = 100 * P;
        let (latch, target) = latch_instants(flip + 50_000, flip, P, 900_000);
        assert_eq!(target, flip + P);
        assert_eq!(latch, target - 900_000);
        // a poisoned fat budget clamps under one period instead of
        // sliding the aim a whole frame out
        let (latch, target) = latch_instants(flip + 50_000, flip, P, 3 * P);
        assert_eq!(target, flip + P);
        assert!(latch > flip);
        // waking late still aims at a reachable vblank
        let (_, target) = latch_instants(flip + 3 * P + P / 2, flip, P, 900_000);
        assert!(target > flip + 3 * P + P / 2);
    }
}

/// direct-scanout bookkeeping: the client buffer on the primary plane,
/// the one the queued flip is replacing, and the fb cache
#[derive(Default)]
struct ScanoutState {
    /// on glass, or queued to be
    cur: Option<Rc<crate::protocol::shm::WlBuffer>>,
    /// replaced by the in-flight flip; releases at its flip-done
    prev: Option<Rc<crate::protocol::shm::WlBuffer>>,
    /// per-buffer drm framebuffers, keyed by wl_buffer uid
    fbs: HashMap<u64, ScanoutFb>,
    /// gem handles are deduplicated by the kernel per drm fd, so two fbs
    /// over one bo share handles; close only when the last user goes
    handles: HashMap<u32, u32>,
    /// monotonically counts scanout flips; ages cache entries
    tick: u64,
}

struct ScanoutFb {
    fb: u32,
    handles: Vec<u32>,
    last_used: u64,
}

/// cache entries idle this many scanout flips get evicted
const SCANOUT_FB_IDLE: u64 = 600;

/// how many misses in a row park late-latching, and how many met deadlines
/// afterwards un-park it. at 480Hz a frame is ~2ms: parking on 4 was one
/// bad batch, and half a second to forgive kept whole passes degraded
const MISS_PARK: u32 = 16;
const CLEAN_RESTORE: u32 = 30;
/// linear decay of the miss margin per met deadline
const MARGIN_DECAY_NS: u64 = 20_000;
/// a miss proves the lead was short: land the cutoff this far above it
const CUTOFF_BUMP_NS: u64 = 25_000;
/// a miss teaches the cutoff only when its lead was within this of the
/// estimate; farther out, the failure had some other cause
const CUTOFF_ATTRIB_NS: u64 = 100_000;
/// clean frames after a raise before downward probing resumes: one probe
/// miss per ~8.5s at 480Hz instead of several per round
const CUTOFF_HOLD_FRAMES: u32 = 4096;
/// per-met-deadline cutoff decay; ~24us/s at 480Hz, so a probe miss costs
/// one late frame every few hundred - noise beyond the p99
const CUTOFF_DECAY_NS: u64 = 50;
/// until the first miss locates the hardware's edge, the seed sheds at
/// this rate instead - a fat guess costs half a second, not a session.
/// measured: a 213us seed excess held fullscreen shm at half rate forever
const CUTOFF_DECAY_FAST_NS: u64 = 1_000;
/// a deadline closer than this isn't worth a timer round-trip
const MIN_SLACK_NS: u64 = 100_000;
/// extra callback lead when a fullscreen shm surface is up: its commit
/// submits a gpu copy of the whole mode, and the frame's fence must still
/// beat the scanout cutoff. ~14.7MB over pcie plus slack; any fatter and
/// the whole chain stops fitting inside a 480Hz period
const UPLOAD_LEAD_NS: u64 = 650_000;
/// the latest a paced wake may land after flip-done: kick, timer, sweep,
/// and the client's wire round-trip all need this much of the period
const CB_HEADROOM_NS: u64 = 150_000;

pub struct BlurCache {
    /// full res first, then each kawase level at half the previous
    levels: Vec<Texture>,
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
        state.host_import.set(renderer.host_import_supported());
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
                list.push(crate::tree::new_workspace(&state));
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
        // the boot set only covers the window before the display lands in
        // state; from then on the live list is the sole authority - it
        // shrinks and grows with hotplug, so any fixed prefix assumption
        // strands an output added after a removal
        let mut fan_outs = outputs.clone();
        let st = state.clone();
        let fanout = state.eng.spawn("damage fanout", async move {
            loop {
                st.damage.triggered().await;
                if let Some(d) = st.display.borrow().as_ref() {
                    // the boot pins drop with the handoff
                    fan_outs.clear();
                    for o in d.outputs.borrow().iter() {
                        o.damage.trigger();
                    }
                } else {
                    for o in &fan_outs {
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
    let mut ops = Vec::new();
    let mut live = Vec::new();
    compose_ops(state, &out, if cursor { CapCursor::Always } else { CapCursor::Never }, None, &mut ops, &mut live);
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    let uploads = std::mem::take(&mut *out.preuploads.borrow_mut());
    let passes = std::mem::take(&mut *out.prepasses.borrow_mut());
    let full = match out.renderer.read_frame(out.width, out.height, uploads, passes, &ops, waits) {
        Ok(px) => px,
        Err(e) => {
            eprintln!("carrot: screencopy render failed: {e}");
            return None;
        }
    };
    let (rw, rh) = (region.width() as usize, region.height() as usize);
    let src_stride = out.width as usize * 4;
    let mut px = vec![0u8; rw * rh * 4];
    // the region may predate a topology change and outsize the output now
    // in the slot; rows past the live extent stay black instead of
    // slicing past the readback
    let vis = region.intersect(Rect::new_sized_saturating(0, 0, out.width as i32, out.height as i32));
    let n = vis.width() as usize * 4;
    for row in vis.y1..vis.y2 {
        let s0 = row as usize * src_stride + vis.x1 as usize * 4;
        let d0 = (row - region.y1) as usize * rw * 4 + (vis.x1 - region.x1) as usize * 4;
        px[d0..d0 + n].copy_from_slice(&full[s0..s0 + n]);
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
    // rule-hidden: the stream gets an opaque black frame, never pixels
    if win.rule_no_capture.get() {
        let n = rect.width() as usize * rect.height() as usize;
        let mut px = vec![0u8; n * 4];
        for p in px.chunks_exact_mut(4) {
            p[3] = 0xff;
        }
        return Some(px);
    }
    let geo = win.geometry();
    let mut ops = Vec::new();
    let mut live = Vec::new();
    draw_surface_tree(&out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, 1.0, 0.0, &mut ops, &mut live);
    // hidden-window casts land here; spare their textures from the sweep
    note_cast_keys(&out, &live);
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    let uploads = std::mem::take(&mut *out.preuploads.borrow_mut());
    let passes = std::mem::take(&mut *out.prepasses.borrow_mut());
    let full = match out.renderer.read_frame(out.width, out.height, uploads, passes, &ops, waits) {
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
    let mut ops = Vec::new();
    let mut live = Vec::new();
    compose_ops(state, &out, CapCursor::Never, Some(ws_index), &mut ops, &mut live);
    let mut waits = Vec::new();
    for fence in out.frame_fences.borrow_mut().drain(..) {
        if let Ok(sem) = out.renderer.import_wait(fence) {
            waits.push(sem);
        }
    }
    let uploads = std::mem::take(&mut *out.preuploads.borrow_mut());
    let passes = std::mem::take(&mut *out.prepasses.borrow_mut());
    match out.renderer.read_frame(out.width, out.height, uploads, passes, &ops, waits) {
        Ok(px) => Some(px),
        Err(e) => {
            eprintln!("carrot: workspace copy render failed: {e}");
            None
        }
    }
}

/// a live workspace-switch blends two scenes on glass; casts wait it out
pub fn switching(state: &Rc<State>, name: &str) -> bool {
    let d = state.display.borrow();
    let Some(d) = d.as_ref() else { return false };
    let outs = d.outputs.borrow();
    outs.iter()
        .find(|o| o.conn.name == name)
        .is_some_and(|o| o.ws_switch.borrow().is_some())
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
        let res = out.renderer.stage_upload(&tex, |dst| {
            dst[..row * h as usize].copy_from_slice(&pixels[..row * h as usize]);
        });
        match res {
            Ok(up) => out.preuploads.borrow_mut().push(up),
            Err(e) => {
                eprintln!("carrot: cursor upload failed: {e}");
                return;
            }
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
    if renderer.host_import_supported() {
        println!("carrot: sealed shm pools sample in place (host import)");
    }
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
        // the plane's buffer bookkeeping dies with the present loop: retire
        // cur/prev now or their uids stay listed and every later replaced
        // attachment with those uids parks in scanout_hold unreleased
        scanout_reset(state, &out);
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
            out.conn.pipe_torn_down();
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
                let w = crate::tree::new_workspace(state);
                w.output.set(o.index.get());
                list.push(w);
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
        cb_at: Cell::new(0),
        cb_kick: AsyncEvent::default(),
        pos: Cell::new((0, 0)),
        ws: Cell::new(0),
        usable: Cell::new(Rect::default()),
        textures: RefCell::new(HashMap::new()),
        retired_tex: RefCell::new(Vec::new()),
        prepasses: RefCell::new(Vec::new()),
        preuploads: RefCell::new(Vec::new()),
        cast_keep: RefCell::new(HashMap::new()),
        devnum,
        paused: Cell::new(false),
        cursor_locked: Cell::new(false),
        cursor_client_hidden: Cell::new(false),
        theme_cursor: RefCell::new(None),
        rearm: RefCell::new(None),
        frame_fences: RefCell::new(Vec::new()),
        present_fbs: RefCell::new(Vec::new()),
        collect_fbs: Cell::new(false),
        ops_scratch: RefCell::new(Vec::new()),
        live_scratch: RefCell::new(Vec::new()),
        inflight_fbs: RefCell::new(Vec::new()),
        inflight_vsync: Cell::new(true),
        vrr_warned: Cell::new(false),
        cursor_tex: RefCell::new(None),
        cursor_gen: Cell::new(0),
        anim_pending: Cell::new(false),
        ws_switch: RefCell::new(None),
        closing_layers: RefCell::new(Vec::new()),
        blur: RefCell::new(None),
        blur_dirty: Cell::new(true),
        gpu_done: crate::util::AsyncQueue::default(),
        sched: Sched::default(),
        scanout: RefCell::new(ScanoutState::default()),
        inflight_zero_copy: Cell::new(false),
    })
}

/// one refresh period in nanoseconds; 0 when unknown or variable
fn period_ns(out: &Output) -> u64 {
    out.conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| {
            let m = &p.mode;
            m.htotal as u64 * m.vtotal as u64 * 1_000_000_000 / (m.clock.max(1) as u64 * 1000)
        })
        .unwrap_or(0)
}

/// account a completed flip against the deadline it aimed for
fn sched_note_flip(out: &Rc<Output>) {
    let seq = out.conn.seq64.get();
    let sched = &out.sched;
    if seq == 0 || seq == sched.last_seq.replace(seq) {
        return;
    }
    let target = sched.target_ns.replace(0);
    if target == 0 {
        return;
    }
    let period = period_ns(out);
    let (sec, usec) = out.conn.flip_time.get();
    let flip_ns = sec as u64 * 1_000_000_000 + usec as u64 * 1000;
    if period == 0 || flip_ns == 0 {
        return;
    }
    let aim = score_flip(flip_ns, target, period);
    let ioctl = sched.aim_ioctl_ns.replace(0);
    if ioctl != 0 {
        let lead = target.saturating_sub(ioctl);
        let held = sched.cutoff_hold.get() > 0;
        if held && aim == Aim::Clean {
            sched.cutoff_hold.set(sched.cutoff_hold.get() - 1);
        }
        let c = cutoff_step(sched.cutoff.get(), aim, lead, period, sched.cutoff_settled.get(), held);
        if c > sched.cutoff.get() {
            sched.cutoff_settled.set(true);
            sched.cutoff_hold.set(CUTOFF_HOLD_FRAMES);
            crate::trace!("commit cutoff raised to {}us", c / 1000);
        }
        sched.cutoff.set(c);
    }
    match aim {
        Aim::Miss => {
            // the budget lied: back off multiplicatively, and after a
            // burst park the policy entirely
            let m = sched.margin.get().max(MARGIN_DECAY_NS);
            sched.margin.set((m * 2).min(period / 2));
            sched.clean.set(0);
            let misses = sched.misses.get() + 1;
            sched.misses.set(misses);
            if misses >= MISS_PARK && !sched.fallback.replace(true) {
                crate::trace!("late-latch parked after {} misses", misses);
            }
        }
        Aim::Early => {
            // overshoot is not success; pull the aim back in hard
            sched.margin.set(sched.margin.get() / 2);
            sched.clean.set(0);
            crate::trace!("late-latch early flip: margin halved");
        }
        Aim::Clean => {
            sched.misses.set(0);
            sched.margin.set(sched.margin.get().saturating_sub(MARGIN_DECAY_NS));
            let clean = sched.clean.get() + 1;
            sched.clean.set(clean);
            if clean >= CLEAN_RESTORE && sched.fallback.replace(false) {
                crate::trace!("late-latch restored after {} met deadlines", clean);
            }
        }
    }
}

/// the scene-side half of "one client buffer is the whole frame": a bare
/// fullscreen surface, mode-sized and opaque, with nothing drawn over it.
/// scanout and the shm copy path both build on this; anything it rejects
/// falls back to compositing
fn fs_surface_candidate(
    state: &Rc<State>,
    out: &Rc<Output>,
) -> Option<(Rc<WlSurface>, Rc<crate::protocol::shm::WlBuffer>)> {
    // every veto names itself under --features trace
    macro_rules! veto {
        ($why:literal) => {{
            crate::trace!(concat!("scanout veto: ", $why));
            return None;
        }};
    }
    if crate::protocol::session_lock::locked(state) || tearing_wanted(state, out) {
        veto!("locked or tearing");
    }
    if out.anim_pending.get() || out.ws_switch.borrow().is_some() {
        veto!("animation pending");
    }
    // a composited cursor or drag icon needs pixels
    {
        let d = state.display.borrow();
        let d = d.as_ref()?;
        let plane_less = out.conn.pipe.borrow().as_ref().is_none_or(|p| p.cursor.is_none());
        if let Some(seat) = state.seat.borrow().clone() {
            if (d.sw.get() || plane_less)
                && !d.sw_hidden.get()
                && !out.cursor_locked.get()
                && d.sw_image.borrow().is_some()
                && out.rect().contains(seat.ptr_x.get() as i32, seat.ptr_y.get() as i32)
            {
                veto!("software cursor over output");
            }
            if seat.data.drag().is_some_and(|drag| drag.icon.borrow().is_some()) {
                veto!("drag icon");
            }
        }
    }
    let ws = state.workspaces.borrow().get(out.ws.get())?.clone();
    let Some(win) = ws.fullscreen.borrow().clone() else {
        veto!("no fullscreen window");
    };
    let cfg = state.config.borrow();
    if cfg.layout.float_above_fullscreen && !ws.floats.borrow().is_empty() {
        veto!("floats above fullscreen");
    }
    drop(cfg);
    // overlay layers draw above fullscreen
    if state.layers.borrow().iter().any(|ls| {
        ls.current.get().layer == crate::shell::layer::OVERLAY
            && ls.mapped()
            && ls.output.get() == out.index.get()
    }) {
        veto!("overlay layer mapped");
    }
    if win.anims_live(state.anim_clock.now()) || win.rule_opacity.get().unwrap_or(1.0) < 1.0 {
        veto!("window anim or opacity rule");
    }
    let surface = win.surface();
    if !surface.mapped.get() {
        veto!("surface unmapped");
    }
    if surface
        .children
        .borrow()
        .as_ref()
        .is_some_and(|ch| ch.below.iter().chain(ch.above.iter()).any(|e| !e.pending.get()))
    {
        veto!("subsurfaces mapped");
    }
    if let Some(tl) = win.xdg_opt() {
        let mut popups = false;
        tl.xdg.for_each_popup(|p| popups |= p.xdg.surface.mapped.get());
        if popups {
            veto!("popups mapped");
        }
    }
    if surface.transform.get() != crate::surface::Transform::Normal || surface.scale.get() != 1 {
        veto!("transform or scale");
    }
    let Some(buf) = surface.buffer.borrow().as_ref().map(|b| b.buf.clone()) else {
        veto!("no buffer");
    };
    // the buffer must be the whole frame: mode-sized, geometry-aligned,
    // and with no alpha for the plane to blend
    if buf.rect.width() != out.width as i32
        || buf.rect.height() != out.height as i32
        || buf.format.has_alpha()
    {
        veto!("not mode-sized or has alpha");
    }
    // the fullscreen slot draws at the output rect no matter what the
    // layout rect says; only a client-declared geometry inset can offset
    // the buffer within the surface
    if win.geometry() != Rect::new_sized_saturating(0, 0, buf.rect.width(), buf.rect.height()) {
        veto!("geometry mismatch");
    }
    Some((surface, buf))
}

/// a frame that is exactly one client buffer needs no compose at all: the
/// buffer goes on the primary plane and the gpu stays idle
fn scanout_candidate(
    state: &Rc<State>,
    out: &Rc<Output>,
) -> Option<(Rc<WlSurface>, Rc<crate::protocol::shm::WlBuffer>)> {
    macro_rules! veto {
        ($why:literal) => {{
            crate::trace!(concat!("scanout veto: ", $why));
            return None;
        }};
    }
    let (surface, buf) = fs_surface_candidate(state, out)?;
    let Some(img) = buf.dmabuf() else {
        veto!("not a dmabuf");
    };
    // the primary plane has to take this format+modifier as-is
    {
        let pipe = out.conn.pipe.borrow();
        let plane = &pipe.as_ref()?.primary;
        if !plane.supports(buf.format.drm) {
            veto!("plane rejects format");
        }
        let mods = plane.modifiers(buf.format.drm);
        if mods.is_empty() {
            // no IN_FORMATS: only implicit/linear layouts are expressible
            if img.modifier != 0 {
                veto!("plane has no IN_FORMATS, modifier not linear");
            }
        } else if !mods.contains(&img.modifier) {
            veto!("plane rejects modifier");
        }
    }
    Some((surface, buf))
}

/// fullscreen sealed shm: the frame is one gpu copy from the imported
/// pool into the scanout buffer, no render pass. only in-place
/// attachments qualify - a shadowed buffer was already released and the
/// client may be rewriting it
fn shm_fast_candidate(
    state: &Rc<State>,
    out: &Rc<Output>,
) -> Option<(Rc<WlSurface>, vk::Buffer, u64, u32)> {
    macro_rules! veto {
        ($why:literal) => {{
            crate::trace!(concat!("fast veto: ", $why));
            return None;
        }};
    }
    let (surface, buf) = fs_surface_candidate(state, out)?;
    if buf.dmabuf().is_some() {
        // scanout's case, not ours
        return None;
    }
    // byte-identical to the scanout images; anything else composes
    if buf.format.drm != XRGB8888.drm {
        veto!("format not xrgb8888");
    }
    let Some(off) = buf.shm_sealed_pool() else {
        veto!("pool not sealed");
    };
    if surface.shm_shadow.borrow().is_some() {
        veto!("shadowed attachment");
    }
    let w = buf.rect.width() as u32;
    let stride = buf.stride as u32;
    if stride < w * 4 || stride % 4 != 0 {
        veto!("stride unusable");
    }
    let Some(src) = out.renderer.host_buffer_for(off.pool()) else {
        veto!("host import unavailable");
    };
    Some((surface, src, off.pool_offset() as u64, stride / 4))
}

/// commit-side mirror of the fast path: the commit-time texture upload is
/// dead work only when the present loop will actually take the copy path,
/// so this must be exactly the present-side predicate - a looser check
/// moves the upload onto the latch path instead of skipping it
fn fs_fast_bound(state: &Rc<State>, out: &Rc<Output>, s: &crate::surface::WlSurface) -> bool {
    shm_fast_candidate(state, out).is_some_and(|(surface, ..)| surface.uid == s.uid)
}

/// the drm framebuffer for a client buffer, imported once per wl_buffer
fn scanout_fb(out: &Rc<Output>, buf: &Rc<crate::protocol::shm::WlBuffer>) -> Option<u32> {
    use crate::drm::sys;
    let mut st = out.scanout.borrow_mut();
    let tick = st.tick + 1;
    st.tick = tick;
    if let Some(e) = st.fbs.get_mut(&buf.uid) {
        e.last_used = tick;
        return Some(e.fb);
    }
    let img = buf.dmabuf()?;
    let fd = out.dev.fd.as_fd();
    let mut handles = Vec::new();
    let mut pitches = Vec::new();
    let mut offsets = Vec::new();
    for plane in &img.planes {
        match sys::prime_fd_to_handle(fd, plane.fd.as_fd()) {
            Ok(h) => {
                *st.handles.entry(h).or_insert(0) += 1;
                handles.push(h);
                pitches.push(plane.stride);
                offsets.push(plane.offset);
            }
            Err(e) => {
                crate::trace!("scanout: prime import failed: {e}");
                scanout_release_handles(&mut st, fd, &handles);
                return None;
            }
        }
    }
    // IN_FORMATS present means the kernel takes explicit modifiers
    let explicit = out
        .conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| !p.primary.modifiers(buf.format.drm).is_empty())
        .unwrap_or(false);
    let modifier = explicit.then_some(img.modifier);
    let fb = match sys::addfb2(
        fd,
        buf.rect.width() as u32,
        buf.rect.height() as u32,
        buf.format.drm,
        &handles,
        &pitches,
        &offsets,
        modifier,
    ) {
        Ok(fb) => fb,
        Err(e) => {
            crate::trace!("scanout: addfb2 failed: {e}");
            scanout_release_handles(&mut st, fd, &handles);
            return None;
        }
    };
    st.fbs.insert(buf.uid, ScanoutFb { fb, handles, last_used: tick });
    // age out fbs of buffers the client stopped rotating through
    let keep_cur = st.cur.as_ref().map(|b| b.uid);
    let keep_prev = st.prev.as_ref().map(|b| b.uid);
    let stale: Vec<u64> = st
        .fbs
        .iter()
        .filter(|(uid, e)| {
            tick.saturating_sub(e.last_used) > SCANOUT_FB_IDLE
                && Some(**uid) != keep_cur
                && Some(**uid) != keep_prev
        })
        .map(|(uid, _)| *uid)
        .collect();
    for uid in stale {
        if let Some(e) = st.fbs.remove(&uid) {
            let _ = sys::rmfb(fd, e.fb);
            scanout_release_handles(&mut st, fd, &e.handles);
        }
    }
    Some(fb)
}

fn scanout_release_handles(
    st: &mut ScanoutState,
    fd: std::os::fd::BorrowedFd<'_>,
    handles: &[u32],
) {
    for h in handles {
        if let Some(rc) = st.handles.get_mut(h) {
            *rc -= 1;
            if *rc == 0 {
                st.handles.remove(h);
                let _ = crate::drm::sys::gem_close(fd, *h);
            }
        }
    }
}

/// a buffer left the plane: its parked attachment may release now
fn scanout_retire(state: &Rc<State>, buf: &Rc<crate::protocol::shm::WlBuffer>) {
    let mut uids = state.scanout_uids.borrow_mut();
    if let Some(i) = uids.iter().position(|&u| u == buf.uid) {
        uids.swap_remove(i);
    }
    let gone = !uids.contains(&buf.uid);
    drop(uids);
    if gone {
        // dropping the attachment sends the release
        state.scanout_hold.borrow_mut().retain(|a| a.buf.uid != buf.uid);
    }
}

/// drop every plane reference this output holds (vt away, flip errors)
fn scanout_reset(state: &Rc<State>, out: &Rc<Output>) {
    let (cur, prev) = {
        let mut st = out.scanout.borrow_mut();
        (st.cur.take(), st.prev.take())
    };
    for b in [cur, prev].into_iter().flatten() {
        scanout_retire(state, &b);
    }
}

/// the latest instant a latch can start and still make the next vblank,
/// and the vblank it would make. None = no meaningful fixed deadline.
/// a parked policy still aims - met deadlines are what un-park it - but
/// the caller skips the sleep
fn latch_deadline(
    state: &Rc<State>,
    out: &Rc<Output>,
    path: PresentPath,
) -> Option<(u64, u64)> {
    let policy = state.config.borrow().debug.latency_policy;
    if policy == crate::config::LatencyPolicy::Vblank {
        return None;
    }
    if vrr_wanted(state, out) || tearing_wanted(state, out) {
        return None;
    }
    let period = period_ns(out);
    let (sec, usec) = out.conn.flip_time.get();
    let flip_ns = sec as u64 * 1_000_000_000 + usec as u64 * 1000;
    if period == 0 || flip_ns == 0 {
        return None;
    }
    // the learned cutoff carries the hardware lead now; the floor is just
    // a user-settable safety minimum under cutoff + margin
    let floor_ns = state
        .config
        .borrow()
        .debug
        .latch_margin_us
        .map(|us| us as u64 * 1000)
        .unwrap_or(150_000);
    let budget = out.sched.budget_ns(path, floor_ns);
    crate::trace!(
        "budget: compose={}us scanout={}us gpu={}us cutoff={}us margin={}us -> {}us",
        out.sched.ewma_compose.get() / 1000,
        out.sched.ewma_scanout.get() / 1000,
        out.sched.ewma_gpu.get() / 1000,
        out.sched.cutoff.get() / 1000,
        out.sched.margin.get() / 1000,
        budget / 1000
    );
    let now = Time::now().nsec();
    if policy == crate::config::LatencyPolicy::Immediate {
        // hyprland's bet, expressible here: always aim the very next
        // vblank. content with headroom still latches late for freshness;
        // content without gets rendered right now and either makes the
        // scan or slips a frame - no k-decision to say "won't fit"
        let k = now.saturating_sub(flip_ns) / period + 1;
        let target = flip_ns + k * period;
        return Some((target.saturating_sub(budget).max(now), target));
    }
    Some(latch_instants(now, flip_ns, period, budget))
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
        Some(cur) if !cur.anim.settled(now) => crate::config::build_anim(
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
    let _cleanup = {
        let st = state.clone();
        let o = out.clone();
        state
            .eng
            .spawn("present cleanup", async move { gpu_cleanup_loop(&st, &o).await })
    };
    let _pacer = {
        let st = state.clone();
        let o = out.clone();
        state
            .eng
            .spawn("callback pacer", async move { callback_pacer(&st, &o).await })
    };
    let mut dirty = false;
    loop {
        EitherEvent(&out.damage, &out.conn.vblank).await;
        let flipped = out.conn.vblank.take();
        sched_note_flip(out);
        // the completed flip carries the kernel timestamp its feedbacks
        // have been waiting for
        if !out.conn.flip_pending.get() && !out.inflight_fbs.borrow().is_empty() {
            let (sec, usec) = out.conn.flip_time.get();
            let refresh = if out.conn.vrr_want.get() {
                // no fixed period exists under variable refresh
                0
            } else {
                period_ns(out) as u32
            };
            let mut flags = crate::protocol::presentation::FLAG_HW_CLOCK
                | crate::protocol::presentation::FLAG_HW_COMPLETION;
            if out.inflight_vsync.get() {
                flags |= crate::protocol::presentation::FLAG_VSYNC;
            }
            if out.inflight_zero_copy.get() {
                flags |= crate::protocol::presentation::FLAG_ZERO_COPY;
            }
            crate::trace!(
                "fb-present: n={} seq={} flip_t={}.{:06} t={}",
                out.inflight_fbs.borrow().len(),
                out.conn.seq64.get(),
                sec,
                usec,
                Time::now().nsec()
            );
            for fb in out.inflight_fbs.borrow_mut().drain(..) {
                fb.presented(&out.conn.name, sec, usec * 1000, refresh, out.conn.seq64.get(), flags);
            }
        }
        // the completed flip replaced whatever was on the plane before it
        if !out.conn.flip_pending.get() {
            let prev = out.scanout.borrow_mut().prev.take();
            if let Some(p) = prev {
                scanout_retire(state, &p);
            }
            // dead pools' imports go with their last frame
            out.renderer.prune_host_imports();
        }
        // clients wake level with the latch that will consume their next
        // commit, not at flip-done: nothing goes stale in the gap
        if flipped && !out.conn.flip_pending.get() {
            schedule_callbacks(state, out);
        }
        dirty |= out.damage.take();
        dirty |= out.anim_pending.get();
        // render only when dirty AND the pipe is free
        if !dirty || out.conn.flip_pending.get() || out.paused.get() {
            continue;
        }
        // late latch: sleep until just before the last instant that still
        // makes the coming vblank, so the freshest input and commits glass.
        // the probe only prices the deadline; the candidate is re-checked
        // for real after the sleep. vblank policy never sleeps, so it
        // skips the walk entirely
        let path = if state.config.borrow().debug.latency_policy
            != crate::config::LatencyPolicy::Vblank
            && scanout_candidate(state, out).is_some()
        {
            PresentPath::Scanout
        } else {
            PresentPath::Compose
        };
        // the copy path never late-latches: its frame is one client buffer
        // that arrived with the commit, so there is nothing fresher to
        // sleep for, and the copy's fence needs every microsecond ahead
        // of the hardware cutoff
        let fast_bound = path == PresentPath::Compose && shm_fast_candidate(state, out).is_some();
        let mut aim_ns = 0;
        if let Some((deadline, target)) = latch_deadline(state, out, path) {
            aim_ns = target;
            if !fast_bound
                && !out.sched.fallback.get()
                && deadline > Time::now().nsec() + MIN_SLACK_NS
            {
                let _ = state.ring.timeout(Time::from_nsec(deadline)).await;
                // whatever arrived with the timer - input completions,
                // commits - routes before the scene is latched
                state.eng.yield_now().await;
                dirty |= out.damage.take();
                if out.paused.get() {
                    continue;
                }
            }
        }
        dirty = false;
        let latch_ns = Time::now().nsec();

        // one client buffer as the whole frame: onto the plane, no render
        if let Some((surface, buf)) = scanout_candidate(state, out) {
            if let Some(fb) = scanout_fb(out, &buf) {
                let fence = buf.dmabuf().and_then(|img| img.read_fence());
                out.conn.vrr_want.set(vrr_wanted(state, out));
                match out.conn.flip(&out.dev, fb, fence.as_ref().map(|f| f.as_raw_fd())) {
                    Ok(FlipResult::Queued) => {
                        out.inflight_vsync.set(true);
                        out.inflight_zero_copy.set(true);
                        if aim_ns != 0 {
                            let now = Time::now().nsec();
                            out.sched.target_ns.set(aim_ns);
                            out.sched.aim_ioctl_ns.set(now);
                            Sched::ewma(&out.sched.ewma_scanout, now.saturating_sub(latch_ns));
                        }
                        // no compose collected these; they ride directly
                        out.inflight_fbs
                            .borrow_mut()
                            .append(&mut surface.latched_feedbacks.borrow_mut());
                        if !surface.shown.replace(true) {
                            state.shown_surfaces.borrow_mut().push(surface.clone());
                        }
                        {
                            let mut st = out.scanout.borrow_mut();
                            if st.cur.as_ref().is_none_or(|c| c.uid != buf.uid) {
                                state.scanout_uids.borrow_mut().push(buf.uid);
                                st.prev = st.cur.replace(buf.clone());
                            }
                        }
                        // captures compose their own copy of the scene; a
                        // scanout frame still owes them the new-frame kick
                        crate::protocol::image_copy_capture::output_presented(state, &out.conn.name);
                        crate::protocol::session_lock::output_presented(state, &out.conn.name);
                        crate::portal::cast::output_presented(state, &out.conn.name);
                        continue;
                    }
                    Ok(FlipResult::NotPresented) => {
                        dirty = true;
                        out.damage.trigger();
                        continue;
                    }
                    Err(crate::drm::device::DrmError::LostMaster) => {
                        for fb in out.inflight_fbs.borrow_mut().drain(..) {
                            fb.discarded();
                        }
                        scanout_reset(state, out);
                        out.paused.set(true);
                        dirty = true;
                        continue;
                    }
                    Err(e) => {
                        // odd rejection: the composite path still owes a frame
                        crate::trace!("scanout flip failed: {e}");
                    }
                }
            }
        }

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
        let target = FrameTarget {
            image: buf.bo.image,
            view: buf.view,
            width: out.width,
            height: out.height,
            undefined: buf.undefined.get(),
        };

        // fullscreen sealed shm becomes the frame with one copy into the
        // scanout buffer: the flip's fence covers just the copy, so an
        // async commit reaches the plane without paying a render pass
        let frame = if let Some((surface, src, offset, row_texels)) = shm_fast_candidate(state, out)
        {
            crate::trace!("fb-fast: surface #{} t={}", surface.uid, Time::now().nsec());
            out.present_fbs
                .borrow_mut()
                .append(&mut surface.latched_feedbacks.borrow_mut());
            if !surface.shown.replace(true) {
                state.shown_surfaces.borrow_mut().push(surface.clone());
            }
            match out.renderer.copy_frame(src, offset, row_texels, &target, waits) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("carrot: fullscreen copy failed: {e}");
                    continue;
                }
            }
        } else {
            let mut ops = out.ops_scratch.take();
            let mut live = out.live_scratch.take();
            compose(state, out, &mut ops, &mut live);
            // clients' in-flight renders gate our sampling of their dmabufs
            for fence in out.frame_fences.borrow_mut().drain(..) {
                match out.renderer.import_wait(fence) {
                    Ok(sem) => waits.push(sem),
                    Err(e) => eprintln!("carrot: dmabuf fence import failed: {e}"),
                }
            }
            crate::trace!("present: {} ops, paused={}", ops.len(), out.paused.get());
            // one submit: staged uploads, deferred offscreen passes, the scene
            let uploads = std::mem::take(&mut *out.preuploads.borrow_mut());
            let passes = std::mem::take(&mut *out.prepasses.borrow_mut());
            let f = match out.renderer.render_frame(
                uploads,
                passes,
                &target,
                Some([0.1, 0.1, 0.1, 1.0]),
                &ops,
                waits,
            ) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("carrot: render failed: {e}");
                    ops.clear();
                    live.clear();
                    out.ops_scratch.replace(ops);
                    out.live_scratch.replace(live);
                    continue;
                }
            };
            // scratch back, emptied now: ops hold texture refs the frame in
            // flight no longer needs from us
            ops.clear();
            live.clear();
            out.ops_scratch.replace(ops);
            out.live_scratch.replace(live);
            f
        };
        let flight = FlightGuard::new(state);
        buf.undefined.set(false);
        let submit_ns = Time::now().nsec();

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
                crate::trace!(
                    "fb-flip: queued n={} t={}",
                    out.present_fbs.borrow().len(),
                    Time::now().nsec()
                );
                out.inflight_fbs
                    .borrow_mut()
                    .append(&mut out.present_fbs.borrow_mut());
                if aim_ns != 0 {
                    let now = Time::now().nsec();
                    out.sched.target_ns.set(aim_ns);
                    out.sched.aim_ioctl_ns.set(now);
                    // through the flip ioctl, not just to submit: the whole
                    // cpu side of the frame prices the next deadline
                    Sched::ewma(&out.sched.ewma_compose, now.saturating_sub(latch_ns));
                }
                // a composited frame replaces any client buffer on the plane
                out.inflight_zero_copy.set(false);
                let mut st = out.scanout.borrow_mut();
                if let Some(c) = st.cur.take() {
                    st.prev = Some(c);
                }
            }
            Ok(FlipResult::NotPresented) => {
                // the pipe was momentarily busy (a cursor commit can hold
                // it between vblanks) and no event follows: self-wake, or
                // the retry waits on unrelated damage
                dirty = true;
                out.damage.trigger();
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
                scanout_reset(state, out);
                out.paused.set(true);
                dirty = true;
            }
            Err(e) => {
                // a flip error isn't fatal: log, full damage, retry next trigger
                eprintln!("carrot: {}: flip failed: {e}; retrying", out.conn.name);
                dirty = true;
            }
        }

        // everything gated on the render fence retires off-loop
        let retired = if sync.is_some() {
            std::mem::take(&mut *out.retired_tex.borrow_mut())
        } else {
            // no fence to wait: keep them for the next round, like before
            Vec::new()
        };
        out.gpu_done.push(CleanupJob { frame, sync, retired, submit_ns, flight });
    }
}

/// what the next frame's pipeline will be, priced differently per path
#[derive(Copy, Clone, PartialEq, Debug)]
enum FsContent {
    /// composited scene: compose budget only
    Windowed,
    /// fullscreen shm: commit-time gpu copy rides ahead of the frame
    Shm,
    /// fullscreen dmabuf: straight onto the plane, cheapest chain of all
    Dmabuf,
}

/// how far before the vblank clients must wake so their commit makes the
/// latch, priced by the actual pipeline it will feed. a chain that can't
/// fit one period clamps to the latest wake the period allows: firing as
/// late as possible degrades gracefully, where reverting to the flip-done
/// phase cost a flat ~0.6ms and flapped between phases whenever the
/// budget wobbled across the boundary
fn callback_lead(content: FsContent, budget: u64, grace: u64, period: u64) -> u64 {
    let upload = match content {
        FsContent::Shm => UPLOAD_LEAD_NS,
        _ => 0,
    };
    let want = budget + upload + grace;
    // an oversized fullscreen-shm chain wakes its client at flip-done:
    // the frame copy owns most of the period, so only the earliest
    // commit's fence still makes the coming scan - the latest-wake clamp
    // lands ~50us past the hardware cutoff and phase-locks a whole
    // session into one-vblank-late presents
    let cap = if content == FsContent::Shm && want > period {
        period
    } else {
        period.saturating_sub(CB_HEADROOM_NS)
    };
    want.min(cap)
}

/// pick when the just-flipped frame's clients should wake: for the aimed
/// policies, just before the coming latch (minus a grace for the client's
/// own render), so their commits land fresh instead of going stale in the
/// flip-done-to-latch gap. commit at V-lead, glass at V. vblank policy
/// (and chains that can't fit the period) keep the flip-done sweep
fn schedule_callbacks(state: &Rc<State>, out: &Rc<Output>) {
    let (policy, floor_us, grace_us) = {
        let cfg = state.config.borrow();
        (
            cfg.debug.latency_policy,
            cfg.debug.latch_margin_us,
            cfg.debug.callback_grace_us,
        )
    };
    let period = period_ns(out);
    let (sec, usec) = out.conn.flip_time.get();
    let flip_ns = sec as u64 * 1_000_000_000 + usec as u64 * 1000;
    if policy == crate::config::LatencyPolicy::Vblank || period == 0 || flip_ns == 0 {
        fire_callback_sweep(state);
        return;
    }
    let content = state
        .workspaces
        .borrow()
        .get(out.ws.get())
        .and_then(|ws| ws.fullscreen.borrow().clone())
        .and_then(|win| {
            let s = win.surface();
            let dmabuf = s.buffer.borrow().as_ref().map(|a| a.buf.dmabuf().is_some());
            dmabuf.map(|d| if d { FsContent::Dmabuf } else { FsContent::Shm })
        })
        .unwrap_or(FsContent::Windowed);
    let floor_ns = floor_us.map(|us| us as u64 * 1000).unwrap_or(150_000);
    let grace = grace_us.map(|us| us as u64 * 1000).unwrap_or(600_000);
    // scanout frames never touch the gpu: pricing them with the compose
    // budget (whose gpu ewma freezes stale during pure-scanout stretches)
    // overflowed the period and silently unpaced the cheapest path
    let path = match content {
        FsContent::Dmabuf => PresentPath::Scanout,
        _ => PresentPath::Compose,
    };
    let budget = out.sched.budget_ns(path, floor_ns);
    let lead = callback_lead(content, budget, grace, period);
    crate::trace!(
        "cb: {:?} budget={}us lead={}us phase=+{}us",
        content,
        budget / 1000,
        lead / 1000,
        (period.saturating_sub(lead)) / 1000
    );
    out.cb_at.set(flip_ns + period - lead);
    out.cb_kick.trigger();
}

/// waits out each scheduled wake and runs the sweep; one per output,
/// coalescing kicks (the sweep drains a shared list, so overlap is cheap)
async fn callback_pacer(state: &Rc<State>, out: &Rc<Output>) {
    loop {
        out.cb_kick.triggered().await;
        let at = out.cb_at.replace(0);
        if at == 0 {
            continue;
        }
        if at > Time::now().nsec() {
            let _ = state.ring.timeout(Time::from_nsec(at)).await;
        }
        fire_callback_sweep(state);
    }
}

/// frame callbacks for everything some compose (or scanout) marked shown
/// since the last sweep; clients nobody can see stop rendering into the
/// void. fired at flip-done, so a frame's callbacks land at its glass
fn fire_callback_sweep(state: &Rc<State>) {
    // the cursor surface never rides a scene walk but its client still
    // paces animated cursors off frame callbacks
    if let Some(seat) = state.seat.borrow().clone() {
        if let crate::input::seat::CursorState::Surface(s) = &*seat.cursor.borrow() {
            if !s.shown.replace(true) {
                state.shown_surfaces.borrow_mut().push(s.clone());
            }
        }
    }
    // only surfaces some compose (or scanout) marked since the last sweep;
    // nobody walks every client every vblank
    let mut list = state.shown_surfaces.take();
    if list.is_empty() {
        return;
    }
    let ms = (Time::now().nsec() / 1_000_000) as u32;
    for s in list.drain(..) {
        if s.shown.replace(false) && s.mapped.get() {
            s.fire_frame_callbacks(ms);
        }
    }
    // hand the capacity back unless a callback handler raced a new mark in
    let mut slot = state.shown_surfaces.borrow_mut();
    if slot.is_empty() {
        *slot = list;
    }
}

/// tightly pack client shm rows into upload staging; short reads zero-fill
/// like the draw-time path, never leaking staging bytes
pub(crate) fn fill_from_shm(access: &ShmAccess, dst: &mut [u8], bh: u32, row: usize, stride: usize) {
    match access {
        ShmAccess::Ptr(p) => unsafe {
            if stride == row {
                std::ptr::copy_nonoverlapping(*p, dst.as_mut_ptr(), row * bh as usize);
            } else {
                for yy in 0..bh as usize {
                    std::ptr::copy_nonoverlapping(
                        p.add(yy * stride),
                        dst[yy * row..].as_mut_ptr(),
                        row,
                    );
                }
            }
        },
        ShmAccess::Fd { fd, offset } => {
            for yy in 0..bh as usize {
                let dst = &mut dst[yy * row..yy * row + row];
                let mut done = 0;
                while done < row {
                    match rustix::io::pread(fd, &mut dst[done..], (offset + yy * stride + done) as u64)
                    {
                        Ok(0) | Err(_) => {
                            dst[done..].fill(0);
                            break;
                        }
                        Ok(n) => done += n,
                    }
                }
            }
        }
    }
}

/// shm pixels ride from the client buffer straight into upload staging the
/// moment they commit - one copy, off the present's latch path. returns
/// true when every output holding a size-matched texture got the pixels,
/// so the caller can skip the shadow and still release the buffer now.
/// first-draw textures, resizes, and a busy ring report false; those
/// outputs re-source at draw time as before
pub fn upload_on_commit(state: &Rc<State>, s: &crate::surface::WlSurface) -> bool {
    let d = state.display.borrow();
    let Some(d) = d.as_ref() else { return false };
    let buffer = s.buffer.borrow();
    let Some(att) = buffer.as_ref() else { return false };
    let buf = &att.buf;
    if buf.dmabuf().is_some() {
        return false;
    }
    let (bw, bh) = (buf.rect.width() as u32, buf.rect.height() as u32);
    if bw == 0 || bh == 0 {
        return false;
    }
    let Some(access) = buf.shm_access() else {
        return false;
    };
    let row = (bw * 4) as usize;
    let stride = buf.stride as usize;
    if stride < row {
        return false;
    }
    let key = (s.client.id, s.uid);
    let cur = s.content_gen.get();
    let mut uploaded = 0usize;
    let mut missed = false;
    for out in d.outputs.borrow().iter() {
        let mut textures = out.textures.borrow_mut();
        let Some(entry) = textures.get_mut(&key) else {
            continue;
        };
        if entry.0.width != bw || entry.0.height != bh {
            missed = true;
            continue;
        }
        if entry.1 == cur {
            uploaded += 1;
            continue;
        }
        // the fullscreen fast path copies straight from the pool at
        // present; a texture copy here would be dead work riding ahead
        // of it. the stale gen makes a composed frame re-read the pool
        if fs_fast_bound(state, out, s) {
            uploaded += 1;
            continue;
        }
        // sealed pools copy on the gpu straight out of the import; the
        // cpu only records a command buffer. everyone else fills staging
        let res = if let Some((hbuf, poff)) = buf.shm_sealed_pool().and_then(|off| {
            out.renderer
                .host_buffer_for(off.pool())
                .map(|b| (b, off.pool_offset()))
        }) {
            out.renderer
                .upload_now_from(&entry.0, hbuf, poff as u64, (stride / 4) as u32)
        } else {
            out.renderer
                .upload_now(&entry.0, |dst| fill_from_shm(&access, dst, bh, row, stride))
        };
        match res {
            Ok(true) => {
                entry.1 = cur;
                uploaded += 1;
            }
            Ok(false) => missed = true,
            Err(e) => {
                crate::trace!("commit upload failed: {e}");
                missed = true;
            }
        }
    }
    uploaded > 0 && !missed
}

/// the render-fence side of a frame: texture retirement, dmabuf release
/// accounting, frame recycling, and capture completion all wait on the gpu
/// here, off the present loop
async fn gpu_cleanup_loop(state: &Rc<State>, out: &Rc<Output>) {
    loop {
        let CleanupJob { frame, sync, retired, submit_ns, flight } = out.gpu_done.pop().await;
        let completed = sync.is_some();
        if let Some(fd) = &sync {
            let _ = state.ring.readable(fd).await;
            // the gpu-tail sample for the latch scheduler; capped so a
            // stalled queue can't poison the estimate for seconds
            let tail = Time::now().nsec().saturating_sub(submit_ns);
            Sched::ewma(&out.sched.ewma_gpu, tail.min(50_000_000));
        }
        if completed {
            for t in &retired {
                out.renderer.destroy_texture(t);
            }
        }
        // frame done: the guard's drop decrements and, at zero, releases
        // the parked attachments
        drop(flight);
        out.renderer.recycle_frame(frame);
        // pending output captures complete against the frame just shown
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

/// a cast source stays cached this long after its last feed
const CAST_KEEP_NS: u64 = 1_000_000_000;

/// captures mark what they sampled; the present sweep leaves those keys
/// cached so the next tick reuses them
fn note_cast_keys(out: &Rc<Output>, live: &[((ClientId, u64), u64)]) {
    let now = Time::now().nsec();
    let mut keep = out.cast_keep.borrow_mut();
    for (k, _) in live {
        keep.insert(*k, now);
    }
}

/// how a composition treats animation state
#[derive(Clone, Copy, PartialEq)]
enum DrawMode {
    /// the frame going to glass: full effects, prunes, bakes
    Present,
    /// offscreen copy of the live scene: reuse the presents' animation
    /// visuals, never bake or prune
    Capture,
    /// offscreen copy of a hidden scene, pinned to rest: final rects,
    /// no transforms
    Rest,
}

fn compose(
    state: &Rc<State>,
    out: &Rc<Output>,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    // presentation feedbacks are claimed only by display composes, never
    // by captures
    out.collect_fbs.set(true);
    compose_ops(state, out, CapCursor::Present, None, ops, live);
    out.collect_fbs.set(false);
}

fn compose_ops(
    state: &Rc<State>,
    out: &Rc<Output>,
    cursor: CapCursor,
    ws_override: Option<usize>,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    let present = cursor == CapCursor::Present;
    if present {
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
            // a flip that idled goes stale; the next glass is never
            // earlier than now, and a past clock replays settled anims
            (flip_ns + period_ns).max(crate::util::Time::now().nsec())
        };
        state.anim_clock.freeze(target);
        out.anim_pending.set(false);
        out.retired_tex
            .borrow_mut()
            .extend(state.retire_tex.borrow_mut().drain(..));
    }
    // captures sample at the already-frozen clock and must not disturb
    // the present's redraw latch: their scene isn't the one on glass
    let latch = out.anim_pending.get();
    compose_scene(state, out, cursor, ws_override, ops, live);
    if !present {
        out.anim_pending.set(latch);
    }
}

fn compose_scene(
    state: &Rc<State>,
    out: &Rc<Output>,
    cursor: CapCursor,
    ws_override: Option<usize>,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    let mode = if cursor == CapCursor::Present {
        DrawMode::Present
    } else if ws_override.is_some() {
        DrawMode::Rest
    } else {
        DrawMode::Capture
    };
    ops.clear();
    live.clear();
    // a locked session shows nothing but the lock surface; an output
    // without one stays a cleared frame
    if crate::protocol::session_lock::locked(state) {
        let screen = out.rect();
        if let Some(s) = crate::protocol::session_lock::compose_locked(state, &out.conn.name) {
            draw_surface_tree(out, &s, screen.x1, screen.y1, screen, 1.0, 0.0, ops, live);
        }
        if cursor != CapCursor::Never {
            draw_sw_cursor(state, out, ops, cursor == CapCursor::Always);
        }
        if mode != DrawMode::Present {
            return;
        }
        // normal content must not keep textures alive across the lock
        let mut textures = out.textures.borrow_mut();
        let stale: Vec<_> = textures
            .keys()
            .filter(|k| !live.iter().any(|(lk, _)| lk == *k))
            .copied()
            .collect();
        for k in stale {
            if let Some((t, _)) = textures.remove(&k) {
                out.retired_tex.borrow_mut().push(t);
            }
        }
        drop(textures);
        return;
    }
    let ws = {
        let list = state.workspaces.borrow();
        match list.get(ws_override.unwrap_or(out.ws.get())) {
            Some(w) => w.clone(),
            None => return,
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
    ensure_blur_cache(state, out, screen, &cfg);

    // paint order: background, bottom, tiled, fullscreen, floats, top,
    // overlay, layer popups; fullscreen hides everything below itself
    // except overlay
    // rest composes pin layers too: no anim transforms, no prunes
    let lrest = mode == DrawMode::Rest;
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::BACKGROUND, screen, ops, live, lrest);
        draw_layer(state, out, crate::shell::layer::BOTTOM, screen, ops, live, lrest);
    }
    let sw = out.ws_switch.borrow().clone();
    let now = state.anim_clock.now();
    match &sw {
        // a live switch belongs to the glass; rest composes show their
        // workspace in place
        Some(s) if mode != DrawMode::Rest && !s.anim.settled(now) => {
            let p = s.anim.clamped_value(now);
            // full ndc span plus a tenth of it as the gap between scenes
            let step = 2.2 * s.dist;
            let (off_out, off_in) = switch_offsets(p, step, s.sign);
            let (a_out, a_in) = if s.fade { ((1.0 - p) as f32, p as f32) } else { (1.0, 1.0) };
            let from = state.workspaces.borrow().get(s.from_ws).cloned();
            if let Some(from) = from {
                let mark = ops.len();
                ws_scene(state, out, &from, &focused, &cfg, screen, ops, live, mode);
                let d = if s.vert { [0.0, off_out as f32] } else { [off_out as f32, 0.0] };
                apply_batch(&mut ops[mark..], [0.0, 0.0], 1.0, a_out, d, out_dims(out));
            }
            let mark = ops.len();
            ws_scene(state, out, &ws, &focused, &cfg, screen, ops, live, mode);
            let d = if s.vert { [0.0, off_in as f32] } else { [off_in as f32, 0.0] };
            apply_batch(&mut ops[mark..], [0.0, 0.0], 1.0, a_in, d, out_dims(out));
            out.anim_pending.set(true);
        }
        _ => {
            if sw.is_some() && mode == DrawMode::Present {
                *out.ws_switch.borrow_mut() = None;
            }
            ws_scene(state, out, &ws, &focused, &cfg, screen, ops, live, mode);
        }
    }
    if fs.is_none() {
        draw_layer(state, out, crate::shell::layer::TOP, screen, ops, live, lrest);
        draw_closing_list(state, out, &out.closing_layers, ops, mode);
    }
    draw_layer(state, out, crate::shell::layer::OVERLAY, screen, ops, live, lrest);
    draw_layer_popups(state, out, fs.is_some(), screen, ops, live);
    // a drag icon rides the pointer, above everything but the cursor
    if ws_override.is_none() {
        if let Some(seat) = state.seat.borrow().clone() {
            if let Some(drag) = seat.data.drag() {
                if let Some(icon) = drag.icon.borrow().clone() {
                    let (dx, dy) = drag.icon_off.get();
                    let (px, py) = (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
                    draw_surface_tree(out, &icon, px + dx, py + dy, screen, 1.0, 0.0, ops, live);
                }
            }
        }
    }
    if cursor != CapCursor::Never {
        draw_sw_cursor(state, out, ops, cursor == CapCursor::Always);
    }

    // an override composes a side scene: mark its sources so the present
    // sweep keeps them cached between ticks instead of evict/import churn
    if ws_override.is_some() {
        note_cast_keys(out, live);
        return;
    }
    // captures observe; only the present's sweep evicts
    if mode != DrawMode::Present {
        return;
    }
    // textures for gone buffers don't outlive the frame, except cast
    // sources fed within the last second
    let now_ns = Time::now().nsec();
    let mut keep = out.cast_keep.borrow_mut();
    keep.retain(|_, t| now_ns.saturating_sub(*t) < CAST_KEEP_NS);
    let mut textures = out.textures.borrow_mut();
    let stale: Vec<_> = textures
        .keys()
        .filter(|k| !live.iter().any(|(lk, _)| lk == *k) && !keep.contains_key(*k))
        .copied()
        .collect();
    for k in stale {
        if let Some((t, _)) = textures.remove(&k) {
            // the frame in flight may still sample it; destroy after fence
            out.retired_tex.borrow_mut().push(t);
        }
    }
}

/// below children, surface, above children - in index order
fn draw_surface_tree(
    out: &Rc<Output>,
    surface: &Rc<WlSurface>,
    x: i32,
    y: i32,
    clip: Rect,
    alpha: f32,
    round: f32,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    if !surface.mapped.get() {
        return;
    }
    if !surface.shown.replace(true) {
        surface.client.state.shown_surfaces.borrow_mut().push(surface.clone());
    }
    if out.collect_fbs.get() {
        let mut latched = surface.latched_feedbacks.borrow_mut();
        if !latched.is_empty() {
            crate::trace!(
                "fb-collect: surface {} n={} t={}",
                surface.id,
                latched.len(),
                Time::now().nsec()
            );
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
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, alpha, round, ops, live);
        }
        draw_buffer(out, surface, x, y, clip, alpha, round, ops, live);
        for sub in &stack[below..] {
            let (px, py) = sub.position.get();
            draw_surface_tree(out, &sub.surface, x + px, y + py, clip, alpha, round, ops, live);
        }
    } else {
        drop(children);
        draw_buffer(out, surface, x, y, clip, alpha, round, ops, live);
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
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    if !p.xdg.surface.mapped.get() {
        return;
    }
    let (rx, ry) = p.rel.get();
    let (px, py) = (ox + rx, oy + ry);
    let geo = p.xdg.geometry();
    draw_surface_tree(out, &p.xdg.surface, px - geo.x1, py - geo.y1, screen, 1.0, 0.0, ops, live);
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
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    xdg.for_each_popup(|p| draw_popup(state, out, p, ox, oy, screen, ops, live));
}

// one shell layer, mapping order = z within it. rest draws for offscreen
// bakes: no anim transform, no latch arming, no last_batch overwrite
fn draw_layer(
    state: &Rc<State>,
    out: &Rc<Output>,
    layer: u32,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
    rest: bool,
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
        draw_surface_tree(out, &ls.surface, r.x1, r.y1, screen, 1.0, 0.0, ops, live);
        // the backdrop clips to the surface's own alpha, so it can only be
        // built once draw_buffer cached the texture; it slides under the
        // surface here and stays out of the seized batch (the cache view
        // isn't the batch's to keep)
        let mut cmark = mark;
        if layer > crate::shell::layer::BOTTOM {
            if let Some(thr) = layer_blur(&state.config.borrow().clone(), &ls.namespace) {
                if let Some(op) = blur_mask_op(out, &ls.surface, r, thr) {
                    ops.insert(mark, op);
                    cmark += 1;
                }
            }
        }
        if rest {
            continue;
        }
        let anim = ls.anim.borrow().clone();
        if let Some((a, style)) = anim {
            use crate::config::Style;
            let now = state.anim_clock.now();
            if a.settled(now) {
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
                apply_batch(&mut ops[mark..], center_ndc(out, r), scale, alpha, d, out_dims(out));
                out.anim_pending.set(true);
            }
        }
        *ls.last_batch.borrow_mut() = (
            ops[cmark..].to_vec(),
            live[lmark..].to_vec(),
            0..(ops.len() - cmark),
        );
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
    live: &mut Vec<((ClientId, u64), u64)>,
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
    round: f32,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
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
            // sealed pools sample in place: the gpu copies straight out of
            // the imported client pages, no cpu byte ever moves
            if let Some((hbuf, poff)) = buf.shm_sealed_pool().and_then(|off| {
                out.renderer
                    .host_buffer_for(off.pool())
                    .map(|b| (b, off.pool_offset()))
            }) {
                let up = out.renderer.external_pre_upload(
                    &entry.0,
                    hbuf,
                    poff as u64,
                    (buf.stride / 4) as u32,
                );
                out.preuploads.borrow_mut().push(up);
                entry.1 = cur;
            }
        }
        if entry.1 != cur {
            let shadow = s.shm_shadow.borrow();
            let row = (bw * 4) as usize;
            let need = row * bh as usize;
            let res = if let Some(px) = shadow.as_ref().filter(|p| p.len() >= need) {
                // the commit-time shadow is the source of truth
                out.renderer
                    .stage_upload(&entry.0, |dst| dst[..need].copy_from_slice(&px[..need]))
            } else {
                // no shadow (capture failed): read the client buffer,
                // zero-filling any short row instead of leaking staging
                let stride = buf.stride as usize;
                let access = match buf.shm_access() {
                    Some(a) => a,
                    None => return,
                };
                out.renderer.stage_upload(&entry.0, |dst| match access {
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
            match res {
                Ok(up) => out.preuploads.borrow_mut().push(up),
                Err(e) => {
                    eprintln!("carrot: upload failed: {e}");
                    return;
                }
            }
            entry.1 = cur;
        }
    }
    let textures = out.textures.borrow();
    let (tex, _) = textures.get(&key).unwrap();
    live.push((key, tex.uid));
    let (sw, sh) = s.size.get();
    let dst = Rect::new_sized_saturating(x, y, sw, sh);
    let vis = dst.intersect(clip);
    if vis.is_empty() {
        return;
    }
    let (gx, gy) = out.pos.get();
    let fx = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fy = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    let pos = [fx(vis.x1), fy(vis.y1)];
    let size = [
        (vis.width()) as f32 / out.width as f32 * 2.0,
        (vis.height()) as f32 / out.height as f32 * 2.0,
    ];
    let uv_pos = [
        (vis.x1 - dst.x1) as f32 / sw as f32,
        (vis.y1 - dst.y1) as f32 / sh as f32,
    ];
    let uv_size = [
        vis.width() as f32 / sw as f32,
        vis.height() as f32 / sh as f32,
    ];
    if round >= 0.5 {
        // clip corners against the window geometry, not this quad
        ops.push(RenderOp::TexR {
            view: tex.view,
            pos,
            size,
            uv_pos,
            uv_size,
            mul: alpha,
            geo_px: [
                (clip.x1 - gx) as f32,
                (clip.y1 - gy) as f32,
                clip.width() as f32,
                clip.height() as f32,
            ],
            radius: round,
        });
    } else {
        ops.push(RenderOp::Tex {
            view: tex.view,
            pos,
            size,
            uv_pos,
            uv_size,
            mul: alpha,
            opaque: opaque && alpha >= 1.0,
        });
    }
}

/// a layer surface whose namespace hits a blur rule samples the cache;
/// the value is the rule's alpha gate (0 = the whole rect)
fn layer_blur(cfg: &crate::config::Config, ns: &str) -> Option<f32> {
    cfg.decoration.blur.as_ref()?;
    cfg.layer_rules
        .iter()
        .find(|r| r.blur && r.matches.iter().any(|m| m.matches(ns)))
        .map(|r| r.ignore_alpha.unwrap_or(0.0) as f32)
}

fn blur_consumers_visible(state: &Rc<State>, out: &Rc<Output>, cfg: &crate::config::Config) -> bool {
    if cfg.decoration.blur.is_none() {
        return false;
    }
    let ws = state.workspaces.borrow().get(out.ws.get()).cloned();
    let fs = ws.as_ref().and_then(|w| w.fullscreen.borrow().clone());
    for ls in state.layers.borrow().iter() {
        // the cache's own sources can't consume it, and fullscreen hides
        // every band but the overlay
        let layer = ls.current.get().layer;
        if ls.output.get() == out.index.get()
            && ls.mapped()
            && layer > crate::shell::layer::BOTTOM
            && (fs.is_none() || layer == crate::shell::layer::OVERLAY)
            && layer_blur(cfg, &ls.namespace).is_some()
        {
            return true;
        }
    }
    let mut hit = false;
    if let Some(ws) = &ws {
        if fs.is_none() {
            ws.for_each(|w| hit |= w.rule_blur.get());
        } else if cfg.layout.float_above_fullscreen {
            for w in ws.floats.borrow().iter() {
                hit |= w.rule_blur.get();
            }
        }
    }
    hit
}

/// rebuild the blurred backdrop when needed; any failure leaves the cache
/// empty and the consumers draw without it
fn ensure_blur_cache(state: &Rc<State>, out: &Rc<Output>, screen: Rect, cfg: &crate::config::Config) {
    let Some(bc) = &cfg.decoration.blur else {
        if let Some(old) = out.blur.borrow_mut().take() {
            state.retire_tex.borrow_mut().extend(old.levels);
        }
        return;
    };
    if !blur_consumers_visible(state, out, cfg) {
        return;
    }
    // a clean cache is only reusable at the current mode's dimensions
    let fits_now = out.blur.borrow().as_ref().is_some_and(|c| {
        c.levels.first().is_some_and(|t| (t.width, t.height) == (out.width, out.height))
    });
    if !out.blur_dirty.replace(false) && fits_now {
        return;
    }
    let mut ops = Vec::new();
    let mut live = Vec::new();
    draw_layer(state, out, crate::shell::layer::BACKGROUND, screen, &mut ops, &mut live, true);
    draw_layer(state, out, crate::shell::layer::BOTTOM, screen, &mut ops, &mut live, true);
    let passes = bc.passes.clamp(1, 4) as u32;
    let need: Vec<(u32, u32)> = (0..=passes)
        .map(|i| ((out.width >> i).max(1), (out.height >> i).max(1)))
        .collect();
    {
        let mut slot = out.blur.borrow_mut();
        let fits = slot.as_ref().is_some_and(|c| {
            c.levels.len() == need.len()
                && c.levels.iter().zip(&need).all(|(t, n)| (t.width, t.height) == *n)
        });
        if !fits {
            if let Some(old) = slot.take() {
                state.retire_tex.borrow_mut().extend(old.levels);
            }
            let mut levels = Vec::new();
            for (w, h) in &need {
                match out.renderer.create_render_texture(*w, *h) {
                    Ok(t) => levels.push(t),
                    Err(e) => {
                        eprintln!("carrot: blur cache alloc failed: {e}");
                        state.retire_tex.borrow_mut().extend(levels);
                        return;
                    }
                }
            }
            *slot = Some(BlurCache { levels });
        }
    }
    // the whole chain defers into the next submit; each pass samples the
    // previous one through its release barrier, never a cpu wait
    let slot = out.blur.borrow();
    let cache = slot.as_ref().unwrap();
    let mut pp = out.prepasses.borrow_mut();
    let black = Some([0.0, 0.0, 0.0, 1.0]);
    pp.push(PrePass::new(&cache.levels[0], black, ops));
    let sz = bc.size as f32;
    for i in 0..passes as usize {
        let (src, dst) = (&cache.levels[i], &cache.levels[i + 1]);
        let hp = [0.5 / dst.width as f32 * sz, 0.5 / dst.height as f32 * sz];
        let first = i == 0;
        let op = RenderOp::Blur {
            view: src.view,
            halfpixel: hp,
            extra_a: if first { bc.contrast as f32 } else { 1.0 },
            extra_b: if first { bc.brightness as f32 } else { 1.0 },
            up: false,
        };
        pp.push(PrePass::new(dst, black, vec![op]));
    }
    for i in (0..passes as usize).rev() {
        let (src, dst) = (&cache.levels[i + 1], &cache.levels[i]);
        // both directions space taps by the render target's half-pixel;
        // source-derived spacing doubled the upsample kernel per level
        let hp = [0.5 / dst.width as f32 * sz, 0.5 / dst.height as f32 * sz];
        let last = i == 0;
        let op = RenderOp::Blur {
            view: src.view,
            halfpixel: hp,
            extra_a: if last { bc.noise as f32 } else { 0.0 },
            extra_b: 0.0,
            up: true,
        };
        pp.push(PrePass::new(dst, black, vec![op]));
    }
}

/// the backdrop gated by the surface's own alpha; needs the texture
/// draw_buffer cached earlier in this compose. None (no cache, no buffer,
/// import failed) means no backdrop at all - better than a full-rect smudge
fn blur_mask_op(out: &Rc<Output>, s: &Rc<WlSurface>, r: Rect, threshold: f32) -> Option<RenderOp> {
    let slot = out.blur.borrow();
    let cache = slot.as_ref()?;
    let buffer = s.buffer.borrow();
    let att = buffer.as_ref()?;
    let key = if att.buf.dmabuf().is_some() {
        (s.client.id, att.buf.uid)
    } else {
        (s.client.id, s.uid)
    };
    let textures = out.textures.borrow();
    let (tex, _) = textures.get(&key)?;
    let (gx, gy) = out.pos.get();
    let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    Some(RenderOp::BlurMask {
        cache_view: cache.levels.first()?.view,
        surface_view: tex.view,
        pos: [fxp(r.x1), fyp(r.y1)],
        size: [
            r.width() as f32 / out.width as f32 * 2.0,
            r.height() as f32 / out.height as f32 * 2.0,
        ],
        buv_pos: [
            (r.x1 - gx) as f32 / out.width as f32,
            (r.y1 - gy) as f32 / out.height as f32,
        ],
        buv_size: [
            r.width() as f32 / out.width as f32,
            r.height() as f32 / out.height as f32,
        ],
        threshold,
        mul: 1.0,
    })
}

/// solid black over the window's box, rounded like the content it hides
fn push_blackout(out: &Rc<Output>, rect: Rect, round: f32, ops: &mut Vec<RenderOp>) {
    let (gx, gy) = out.pos.get();
    let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    let pos = [fxp(rect.x1), fyp(rect.y1)];
    let size = [
        rect.width() as f32 / out.width as f32 * 2.0,
        rect.height() as f32 / out.height as f32 * 2.0,
    ];
    let black = [0.0, 0.0, 0.0, 1.0];
    if round >= 0.5 {
        // a ring wider than the box is a rounded fill
        ops.push(RenderOp::Border {
            pos,
            size,
            rect_px: [
                (rect.x1 - gx) as f32,
                (rect.y1 - gy) as f32,
                rect.width() as f32,
                rect.height() as f32,
            ],
            radius: round,
            width: rect.width().max(rect.height()) as f32,
            color: black,
        });
    } else {
        ops.push(RenderOp::Fill { pos, size, color: black });
    }
}

/// the blurred backdrop under a rect, rounded to taste
fn push_blur_backdrop(out: &Rc<Output>, r: Rect, round: f32, ops: &mut Vec<RenderOp>) {
    let slot = out.blur.borrow();
    let Some(cache) = slot.as_ref() else { return };
    let (gx, gy) = out.pos.get();
    let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    let pos = [fxp(r.x1), fyp(r.y1)];
    let size = [
        r.width() as f32 / out.width as f32 * 2.0,
        r.height() as f32 / out.height as f32 * 2.0,
    ];
    let uv_pos = [
        (r.x1 - gx) as f32 / out.width as f32,
        (r.y1 - gy) as f32 / out.height as f32,
    ];
    let uv_size = [
        r.width() as f32 / out.width as f32,
        r.height() as f32 / out.height as f32,
    ];
    if round >= 0.5 {
        ops.push(RenderOp::TexR {
            view: cache.levels[0].view,
            pos,
            size,
            uv_pos,
            uv_size,
            mul: 1.0,
            geo_px: [
                (r.x1 - gx) as f32,
                (r.y1 - gy) as f32,
                r.width() as f32,
                r.height() as f32,
            ],
            radius: round,
        });
    } else {
        ops.push(RenderOp::Tex {
            view: cache.levels[0].view,
            pos,
            size,
            uv_pos,
            uv_size,
            mul: 1.0,
            opaque: true,
        });
    }
}

/// one workspace's content: tiled, closings, fullscreen, floats
#[allow(clippy::too_many_arguments)]
fn ws_scene(
    state: &Rc<State>,
    out: &Rc<Output>,
    ws: &Rc<crate::tree::workspace::Workspace>,
    focused: &Option<Rc<WlSurface>>,
    cfg: &crate::config::Config,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
    mode: DrawMode,
) {
    let fs = ws.fullscreen.borrow().clone();
    let mut tiled = Vec::new();
    let mut strip_d = None;
    if fs.is_none() {
        let mark = ops.len();
        ws.tiling.for_each(|win| {
            if let Some(r) = draw_window(state, out, focused, cfg, win, ops, live, mode) {
                tiled.push((win.clone(), r));
            }
        });
        draw_closings(state, out, ws, ops, mode);
        if ws.tiling.mode() == crate::config::LayoutMode::Scrolling && mode != DrawMode::Rest {
            let now = state.anim_clock.now();
            let dx = ws.tiling.strip.draw_offset_px(now);
            if dx.abs() >= 0.5 {
                let d = [(dx / out.width as f64 * 2.0) as f32, 0.0];
                apply_batch(&mut ops[mark..], [0.0, 0.0], 1.0, 1.0, d, out_dims(out));
                out.anim_pending.set(true);
                strip_d = Some(d);
            } else if mode == DrawMode::Present {
                *ws.tiling.strip.view_anim.borrow_mut() = None;
            }
        }
    }
    let mut above = Vec::new();
    if let Some(f) = &fs {
        if let Some(r) = draw_window(state, out, focused, cfg, f, ops, live, mode) {
            above.push((f.clone(), r));
        }
    }
    if fs.is_none() || cfg.layout.float_above_fullscreen {
        for win in ws.floats.borrow().iter() {
            if let Some(r) = draw_window(state, out, focused, cfg, win, ops, live, mode) {
                above.push((win.clone(), r));
            }
        }
    }
    // popups draw once every body has: a menu overhanging its parent
    // covers the neighbor instead of vanishing under it. tiled popups
    // ride the strip translate their parents rode
    let pmark = ops.len();
    for (win, r) in &tiled {
        draw_window_popups(state, out, win, *r, screen, ops, live);
    }
    if let Some(d) = strip_d {
        apply_batch(&mut ops[pmark..], [0.0, 0.0], 1.0, 1.0, d, out_dims(out));
    }
    for (win, r) in &above {
        draw_window_popups(state, out, win, *r, screen, ops, live);
    }
}

/// the lifted popup pass, anchored to the rect the body drew at
fn draw_window_popups(
    state: &Rc<State>,
    out: &Rc<Output>,
    win: &Rc<crate::tree::Window>,
    rect: Rect,
    screen: Rect,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) {
    if let Some(tl) = win.xdg_opt() {
        draw_popups(state, out, &tl.xdg, rect.x1, rect.y1, screen, ops, live);
    }
}

/// re-express screen-space ops in the local ndc of a texture covering `r`
fn remap_local(ops: &mut [RenderOp], out: &Output, r: Rect) {
    let (gx, gy) = out.pos.get();
    let rx = (r.x1 - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let ry = (r.y1 - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    let rw = (r.width() as f32 / out.width as f32 * 2.0).max(1e-6);
    let rh = (r.height() as f32 / out.height as f32 * 2.0).max(1e-6);
    let (px, py) = ((r.x1 - gx) as f32, (r.y1 - gy) as f32);
    let map_pos = |pos: &mut [f32; 2], size: &mut [f32; 2]| {
        pos[0] = (pos[0] - rx) / rw * 2.0 - 1.0;
        pos[1] = (pos[1] - ry) / rh * 2.0 - 1.0;
        size[0] = size[0] / rw * 2.0;
        size[1] = size[1] / rh * 2.0;
    };
    for op in ops {
        match op {
            RenderOp::Fill { pos, size, .. } => map_pos(pos, size),
            RenderOp::Tex { pos, size, .. } => map_pos(pos, size),
            RenderOp::TexR { pos, size, geo_px, .. } => {
                map_pos(pos, size);
                geo_px[0] -= px;
                geo_px[1] -= py;
            }
            RenderOp::Border { pos, size, rect_px, .. } => {
                map_pos(pos, size);
                rect_px[0] -= px;
                rect_px[1] -= py;
            }
            RenderOp::Shadow { pos, size, win_px, .. } => {
                map_pos(pos, size);
                win_px[0] -= px;
                win_px[1] -= py;
            }
            RenderOp::Xfade { pos, size, geo_px, .. } => {
                map_pos(pos, size);
                geo_px[0] -= px;
                geo_px[1] -= py;
            }
            RenderOp::Blur { .. } => {}
            RenderOp::BlurMask { pos, size, .. } => map_pos(pos, size),
        }
    }
}

/// bake/refresh the crossfade textures and emit the mixing quad; false
/// means degrade to a plain draw (the caller snaps the anim)
#[allow(clippy::too_many_arguments)]
fn draw_resize_xfade(
    out: &Rc<Output>,
    win: &Rc<crate::tree::Window>,
    arect: Rect,
    round: f32,
    progress: f64,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
) -> bool {
    let surface = win.surface();
    let target = win.rect.get();
    let geo = win.geometry();
    // current content, drawn fresh at the target size into the live side
    let mut tmp = Vec::new();
    let mut tl = Vec::new();
    draw_surface_tree(
        out,
        &surface,
        target.x1 - geo.x1,
        target.y1 - geo.y1,
        target,
        1.0,
        0.0,
        &mut tmp,
        &mut tl,
    );
    if tmp.is_empty() {
        crate::trace!("rz: no live ops #{}", win.ident);
        return false;
    }
    remap_local(&mut tmp, out, target);
    let mut m = win.anims.borrow_mut();
    let Some(rz) = m.resize.as_mut() else { return false };
    // the old content bakes exactly once, from the last composed batch:
    // surface ops only (decorations draw live), and never from views whose
    // textures were destroyed or replaced since - a key can outlive its
    // texture (shm reallocs and re-imports reuse it), so match uids
    if rz.snapshot.is_none() {
        let (all_ops, keys, srange) = win.last_batch.borrow().clone();
        if all_ops.is_empty() || srange.is_empty() {
            crate::trace!("rz: no last batch #{}", win.ident);
            return false;
        }
        {
            let t = out.textures.borrow();
            if keys
                .iter()
                .any(|(k, uid)| t.get(k).map(|(tex, _)| tex.uid) != Some(*uid))
            {
                crate::trace!("rz: stale batch textures #{}", win.ident);
                return false;
            }
        }
        let mut old_ops = all_ops[srange].to_vec();
        remap_local(&mut old_ops, out, rz.from_rect);
        let (fw, fh) = (rz.from_rect.width().max(1), rz.from_rect.height().max(1));
        let tex = match out.renderer.create_render_texture(fw as u32, fh as u32) {
            Ok(t) => t,
            Err(_) => {
                crate::trace!("rz: snapshot alloc failed #{}", win.ident);
                return false;
            }
        };
        out.prepasses
            .borrow_mut()
            .push(PrePass::new(&tex, Some([0.0; 4]), old_ops));
        crate::trace!("rz: snapshot built #{}", win.ident);
        rz.snapshot = Some(tex);
    }
    // the live side re-renders every frame. the texture is allocated once
    // per fade - the mixing quad stretches 0..1 either way, so a retarget
    // just changes the render's resolution, never the allocation
    if rz.live.is_none() {
        let (tw, th) = (target.width().max(1) as u32, target.height().max(1) as u32);
        match out.renderer.create_render_texture(tw, th) {
            Ok(t) => rz.live = Some(t),
            Err(_) => return false,
        }
    }
    let live_tex = rz.live.as_ref().unwrap();
    out.prepasses
        .borrow_mut()
        .push(PrePass::new(live_tex, Some([0.0; 4]), tmp));
    live.extend(tl);
    push_xfade_op(out, arect, round, progress, rz.snapshot.as_ref().unwrap(), live_tex, ops);
    true
}

/// the mixing quad between two baked sides of a resize
fn push_xfade_op(
    out: &Rc<Output>,
    arect: Rect,
    round: f32,
    progress: f64,
    from: &Texture,
    to: &Texture,
    ops: &mut Vec<RenderOp>,
) {
    let (gx, gy) = out.pos.get();
    let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
    let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
    ops.push(RenderOp::Xfade {
        from_view: from.view,
        to_view: to.view,
        pos: [fxp(arect.x1), fyp(arect.y1)],
        size: [
            arect.width() as f32 / out.width as f32 * 2.0,
            arect.height() as f32 / out.height as f32 * 2.0,
        ],
        progress: progress as f32,
        geo_px: [
            (arect.x1 - gx) as f32,
            (arect.y1 - gy) as f32,
            arect.width() as f32,
            arect.height() as f32,
        ],
        radius: round,
    });
}

#[allow(clippy::too_many_arguments)]
/// one window body (no popups); returns the rect it drew at, the anchor
/// for the later popup pass
fn draw_window(
    state: &Rc<State>,
    out: &Rc<Output>,
    focused: &Option<Rc<WlSurface>>,
    cfg: &crate::config::Config,
    win: &Rc<crate::tree::Window>,
    ops: &mut Vec<RenderOp>,
    live: &mut Vec<((ClientId, u64), u64)>,
    mode: DrawMode,
) -> Option<Rect> {
    let surface = win.surface();
    if !surface.mapped.get() {
        return None;
    }
    let mark = ops.len();
    let lmark = live.len();
    let rect = if mode == DrawMode::Rest {
        win.rect.get()
    } else {
        win.visual_rect(state)
    };
    // a settled crossfade retires its textures; a live one shrinks the
    // drawn rect to the animated size. only presents prune - captures
    // observing a settled anim just draw plainly
    let rz_progress = if mode == DrawMode::Rest {
        None
    } else {
        let now = state.anim_clock.now();
        let mut m = win.anims.borrow_mut();
        match &mut m.resize {
            Some(rz) if rz.anim.settled(now) => {
                if mode == DrawMode::Present {
                    crate::trace!(
                        "rz: settled #{} had_snapshot={} t={}",
                        win.ident,
                        rz.snapshot.is_some(),
                        now
                    );
                    let mut q = state.retire_tex.borrow_mut();
                    q.extend(rz.snapshot.take());
                    q.extend(rz.live.take());
                    m.resize = None;
                    // one more compose drains the retire queue it just fed
                    out.anim_pending.set(true);
                }
                None
            }
            Some(rz) => Some((rz.anim.clamped_value(now), rz.from)),
            None => None,
        }
    };
    let rect = match rz_progress {
        Some((p, from)) => {
            let w = from.0 as f64 + (rect.width() - from.0) as f64 * p;
            let h = from.1 as f64 + (rect.height() - from.1) as f64 * p;
            Rect::new_sized_saturating(
                rect.x1,
                rect.y1,
                (w.round() as i32).max(1),
                (h.round() as i32).max(1),
            )
        }
        None => rect,
    };
    let round = if win.fullscreen.get() {
        0.0
    } else {
        win.rule_rounding
            .get()
            .unwrap_or(cfg.decoration.rounding)
            .max(0) as f32
    };
    // a rule-hidden window leaves the compositor only as a black stand-in
    if mode != DrawMode::Present && win.rule_no_capture.get() {
        push_blackout(out, rect, round, ops);
        return None;
    }
    if win.rule_blur.get() && !win.fullscreen.get() {
        push_blur_backdrop(out, rect, round, ops);
    }
    let cmark = ops.len();
    if !win.fullscreen.get() && win.rule_shadow.get().unwrap_or(true) {
        if let Some(sc) = &cfg.decoration.shadow {
            let (gx, gy) = out.pos.get();
            let (ox, oy) = sc.offset;
            let sr = Rect {
                x1: rect.x1 - sc.size + ox,
                y1: rect.y1 - sc.size + oy,
                x2: rect.x2 + sc.size + ox,
                y2: rect.y2 + sc.size + oy,
            };
            let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
            let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
            ops.push(RenderOp::Shadow {
                pos: [fxp(sr.x1), fyp(sr.y1)],
                size: [
                    sr.width() as f32 / out.width as f32 * 2.0,
                    sr.height() as f32 / out.height as f32 * 2.0,
                ],
                win_px: [
                    (rect.x1 - gx + ox) as f32,
                    (rect.y1 - gy + oy) as f32,
                    rect.width() as f32,
                    rect.height() as f32,
                ],
                radius: round,
                range: sc.size as f32,
                power: sc.power as f32,
                color: sc.color,
            });
        }
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
        push_borders(out, rect, cfg.layout.border.width, round, color, ops);
    }
    let geo = win.geometry();
    let alpha = win.rule_opacity.get().unwrap_or(1.0);
    let smark = ops.len();
    let mut xfaded = false;
    if let Some((p, _)) = rz_progress {
        if mode == DrawMode::Present {
            xfaded = draw_resize_xfade(out, win, rect, round, p, ops, live);
            if !xfaded {
                // degrade: snap the crossfade, draw plainly
                let mut m = win.anims.borrow_mut();
                if let Some(rz) = &mut m.resize {
                    let mut q = state.retire_tex.borrow_mut();
                    q.extend(rz.snapshot.take());
                    q.extend(rz.live.take());
                }
                m.resize = None;
                out.anim_pending.set(true);
            }
        } else {
            // captures reuse whatever the presents already baked; a
            // missing side draws plain, snapping nothing
            let m = win.anims.borrow();
            if let Some(rz) = &m.resize {
                if let (Some(snap), Some(livet)) = (&rz.snapshot, &rz.live) {
                    push_xfade_op(out, rect, round, p, snap, livet, ops);
                    xfaded = true;
                }
            }
        }
    }
    if !xfaded {
        draw_surface_tree(out, &surface, rect.x1 - geo.x1, rect.y1 - geo.y1, rect, alpha, round, ops, live);
    }
    let send = ops.len();
    // captured before dim and the open transform, minus the blur backdrop:
    // close anims and resize snapshots replay clean, untransformed content
    if !xfaded {
        *win.last_batch.borrow_mut() = (
            ops[cmark..].to_vec(),
            live[lmark..].to_vec(),
            (smark - cmark)..(send - cmark),
        );
    }
    let is_focused = focused.as_ref().is_some_and(|f| Rc::ptr_eq(f, &surface));
    let dim_want = if is_focused || win.fullscreen.get() || !win.rule_dim.get().unwrap_or(true) {
        0.0
    } else {
        cfg.decoration.dim_inactive
    };
    let dim = win.dim_now(state, dim_want);
    if dim > 0.004 {
        let (gx, gy) = out.pos.get();
        let fxp = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
        let fyp = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
        let pos = [fxp(rect.x1), fyp(rect.y1)];
        let size = [
            rect.width() as f32 / out.width as f32 * 2.0,
            rect.height() as f32 / out.height as f32 * 2.0,
        ];
        let shade = [0.0, 0.0, 0.0, dim as f32];
        if round >= 0.5 {
            // a ring wider than the box is a rounded fill
            ops.push(RenderOp::Border {
                pos,
                size,
                rect_px: [
                    (rect.x1 - gx) as f32,
                    (rect.y1 - gy) as f32,
                    rect.width() as f32,
                    rect.height() as f32,
                ],
                radius: round,
                width: rect.width().max(rect.height()) as f32,
                color: shade,
            });
        } else {
            ops.push(RenderOp::Fill { pos, size, color: shade });
        }
    }
    let open = win.anims.borrow().open.clone();
    if let Some((a, style)) = open {
        if !win.fullscreen.get() && mode != DrawMode::Rest {
            use crate::config::Style;
            let p = a.clamped_value(state.anim_clock.now());
            let (scale, alpha, d) = match &style {
                Style::Popin { perc } => ((perc + (1.0 - perc) * p) as f32, p as f32, [0.0, 0.0]),
                Style::Fade => (1.0, p as f32, [0.0, 0.0]),
                Style::Slide { dir } => (1.0, 1.0, slide_delta(out, rect, *dir, 1.0 - p)),
                _ => (1.0, 1.0, [0.0, 0.0]),
            };
            apply_batch(&mut ops[mark..], center_ndc(out, rect), scale, alpha, d, out_dims(out));
        }
    }
    // gated last: the dim/border retargets above start their fades inside
    // this draw, and their first frame must arm the redraw latch too
    if win.anims_live(state.anim_clock.now()) {
        out.anim_pending.set(true);
    }
    Some(rect)
}

// -- closing windows: the final batch outlives the surface --

pub struct ClosingWindow {
    pub ops: Vec<RenderOp>,
    /// ownership moved out of out.textures; retired when the anim ends
    pub keep: Vec<Texture>,
    pub rect: Rect,
    pub anim: crate::anim::Anim,
    pub style: crate::config::Style,
    /// the window's capture rule survives it; captures skip the replay
    pub no_capture: bool,
}

/// wrap a cached final batch, taking ownership of its textures. a batch
/// whose textures were evicted - or replaced under the same key - references
/// destroyed views; those must never reach the gpu, so the capture is
/// refused instead (uid mismatch counts as missing)
pub fn seize_batch(
    out: &Rc<Output>,
    batch: (Vec<RenderOp>, Vec<((ClientId, u64), u64)>, std::ops::Range<usize>),
    rect: Rect,
    anim: crate::anim::Anim,
    style: crate::config::Style,
    no_capture: bool,
) -> Option<ClosingWindow> {
    let (ops, keys, _) = batch;
    if ops.is_empty() {
        return None;
    }
    let mut keep = Vec::new();
    let mut taken: Vec<(ClientId, u64)> = Vec::new();
    let mut missing = false;
    {
        let mut textures = out.textures.borrow_mut();
        for (k, uid) in &keys {
            if taken.contains(k) {
                continue;
            }
            match textures.remove(k) {
                Some((t, _)) if t.uid == *uid => {
                    keep.push(t);
                    taken.push(*k);
                }
                // a replacement: the window is going away, retire it too
                Some((t, _)) => {
                    keep.push(t);
                    missing = true;
                }
                None => missing = true,
            }
        }
    }
    if missing {
        out.retired_tex.borrow_mut().extend(keep);
        return None;
    }
    Some(ClosingWindow { ops, keep, rect, anim, style, no_capture })
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
    seize_batch(out, batch, rect, anim, style, win.rule_no_capture.get())
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
    mode: DrawMode,
) {
    draw_closing_list(state, out, &ws.closing, ops, mode);
}

fn draw_closing_list(
    state: &Rc<State>,
    out: &Rc<Output>,
    list: &RefCell<Vec<ClosingWindow>>,
    ops: &mut Vec<RenderOp>,
    mode: DrawMode,
) {
    use crate::config::Style;
    let now = state.anim_clock.now();
    let mut list = list.borrow_mut();
    if mode == DrawMode::Present {
        let mut i = 0;
        while i < list.len() {
            if list[i].anim.settled(now) {
                let cw = list.remove(i);
                out.retired_tex.borrow_mut().extend(cw.keep);
                continue;
            }
            i += 1;
        }
    }
    for c in list.iter() {
        if c.no_capture && mode != DrawMode::Present {
            continue;
        }
        // presence runs 1 -> 0
        let p = c.anim.clamped_value(now);
        let mark = ops.len();
        ops.extend(c.ops.iter().cloned());
        let (scale, alpha, d) = match &c.style {
            Style::Fade => (1.0, p as f32, [0.0, 0.0]),
            Style::Slide { dir } => (1.0, 1.0, slide_delta(out, c.rect, *dir, 1.0 - p)),
            Style::Popin { perc } => ((perc + (1.0 - perc) * p) as f32, p as f32, [0.0, 0.0]),
            _ => ((0.8 + 0.2 * p) as f32, p as f32, [0.0, 0.0]),
        };
        apply_batch(&mut ops[mark..], center_ndc(out, c.rect), scale, alpha, d, out_dims(out));
        out.anim_pending.set(true);
    }
}

// -- op-batch transforms: open/close styles, workspace slides --

/// scale about an ndc center, multiply alpha, then translate; the whole
/// window (borders, subsurfaces, popups) moves as one
fn apply_batch(
    ops: &mut [RenderOp],
    center: [f32; 2],
    scale: f32,
    alpha: f32,
    d: [f32; 2],
    dims: [f32; 2],
) {
    // the rounding geo lives in pixel space; mirror the ndc transform there
    let px_center = [
        (center[0] + 1.0) * 0.5 * dims[0],
        (center[1] + 1.0) * 0.5 * dims[1],
    ];
    let px_d = [d[0] * 0.5 * dims[0], d[1] * 0.5 * dims[1]];
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
            RenderOp::TexR { pos, size, mul, geo_px, radius, .. } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                    geo_px[i] = px_center[i] + (geo_px[i] - px_center[i]) * scale + px_d[i];
                    geo_px[i + 2] *= scale;
                }
                *radius *= scale;
                *mul *= alpha;
            }
            RenderOp::Border { pos, size, rect_px, radius, width, color } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                    rect_px[i] = px_center[i] + (rect_px[i] - px_center[i]) * scale + px_d[i];
                    rect_px[i + 2] *= scale;
                }
                *radius *= scale;
                *width *= scale;
                color[3] *= alpha;
            }
            RenderOp::Shadow { pos, size, win_px, radius, range, color, .. } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                    win_px[i] = px_center[i] + (win_px[i] - px_center[i]) * scale + px_d[i];
                    win_px[i + 2] *= scale;
                }
                *radius *= scale;
                *range *= scale;
                color[3] *= alpha;
            }
            RenderOp::Xfade { pos, size, geo_px, radius, .. } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                    geo_px[i] = px_center[i] + (geo_px[i] - px_center[i]) * scale + px_d[i];
                    geo_px[i + 2] *= scale;
                }
                *radius *= scale;
            }
            RenderOp::BlurMask { pos, size, mul, .. } => {
                for i in 0..2 {
                    pos[i] = center[i] + (pos[i] - center[i]) * scale + d[i];
                    size[i] *= scale;
                }
                *mul *= alpha;
            }
            // pre-pass only; never inside a window batch
            RenderOp::Blur { .. } => {}
        }
    }
}

fn out_dims(out: &Output) -> [f32; 2] {
    [out.width as f32, out.height as f32]
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

/// four fills just outside the window box, or one rounded ring
fn push_borders(
    out: &Rc<Output>,
    r: Rect,
    b: i32,
    round: f32,
    color: [f32; 4],
    ops: &mut Vec<RenderOp>,
) {
    if round >= 0.5 && b > 0 {
        let (gx, gy) = out.pos.get();
        let (ox1, oy1) = (r.x1 - b, r.y1 - b);
        let (ow, oh) = (r.width() + 2 * b, r.height() + 2 * b);
        // quad pads one px past the ring for the aa edge
        let fx = |v: i32| (v - gx) as f32 / out.width as f32 * 2.0 - 1.0;
        let fy = |v: i32| (v - gy) as f32 / out.height as f32 * 2.0 - 1.0;
        ops.push(RenderOp::Border {
            pos: [fx(ox1 - 1), fy(oy1 - 1)],
            size: [
                (ow + 2) as f32 / out.width as f32 * 2.0,
                (oh + 2) as f32 / out.height as f32 * 2.0,
            ],
            rect_px: [(ox1 - gx) as f32, (oy1 - gy) as f32, ow as f32, oh as f32],
            radius: round + b as f32,
            width: b as f32,
            color,
        });
        return;
    }
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
            no_capture: false,
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
        apply_batch(&mut ops, [0.0, 0.0], 0.5, 0.5, [0.25, 0.0], [1000.0, 600.0]);
        let RenderOp::Fill { pos, size, color } = &ops[0] else {
            panic!("op kind changed");
        };
        // -0.5 scaled about 0 is -0.25; x then translates by +0.25
        assert_eq!(*pos, [0.0, -0.25]);
        assert_eq!(*size, [0.5, 0.5]);
        assert_eq!(color[3], 0.5);
    }
}
