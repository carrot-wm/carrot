// dwindle tree + workspaces + the floating stack.
// nodes have stable identity - no vec indices. z-order is
// fullscreen > floats > tiled (float_above_fullscreen swaps the first two).
// new windows split whatever is under the cursor, not the focused one.

pub mod dwindle;
pub mod float;
pub mod layout;
pub mod scrolling;
pub mod workspace;

use crate::config::Dir;
use crate::rect::Rect;
use crate::shell::xdg::XdgToplevel;
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use workspace::Workspace;

// gaps, borders, colors and binds all live in state.config now

pub fn output_extent(state: &State) -> (i32, i32) {
    let (w, h) = state.output_size.get();
    (w as i32, h as i32)
}

/// global rect of the output holding this workspace, or output_extent when headless
pub fn workspace_output_rect(state: &State, ws: &Workspace) -> Rect {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(out) = d.outputs.borrow().get(ws.output.get()) {
            return out.rect();
        }
    }
    let (w, h) = output_extent(state);
    Rect::new_sized_saturating(0, 0, w.max(1), h.max(1))
}

/// every workspace currently on glass: one per output, else the active one
pub fn visible_workspaces(state: &Rc<State>) -> Vec<Rc<Workspace>> {
    if let Some(d) = state.display.borrow().as_ref() {
        let outs = d.outputs.borrow();
        if !outs.is_empty() {
            let list = state.workspaces.borrow();
            let mut seen = Vec::new();
            for o in outs.iter() {
                if let Some(ws) = list.get(o.ws.get()) {
                    if !seen.iter().any(|w: &Rc<Workspace>| Rc::ptr_eq(w, ws)) {
                        seen.push(ws.clone());
                    }
                }
            }
            if !seen.is_empty() {
                return seen;
            }
        }
    }
    vec![active(state)]
}

/// the workspace shown at a global point, else the active one
pub fn workspace_at(state: &Rc<State>, x: i32, y: i32) -> Rc<Workspace> {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(out) = d.output_at(x, y) {
            if let Some(ws) = state.workspaces.borrow().get(out.ws.get()) {
                return ws.clone();
            }
        }
    }
    active(state)
}

/// tell the client which output its surface is on; wl_output objects are
/// per bind, matched by connector name
pub fn send_surface_output(state: &Rc<State>, surface: &crate::surface::WlSurface, slot: usize, enter: bool) {
    let name = {
        let d = state.display.borrow();
        let Some(d) = d.as_ref() else { return };
        let outs = d.outputs.borrow();
        let Some(o) = outs.get(slot) else { return };
        o.conn.name.clone()
    };
    send_surface_output_named(surface, &name, enter);
}

/// same, addressed by connector name: bound wl_output objects outlive the
/// global, so an output that already left the layout is still reachable
pub fn send_surface_output_named(surface: &crate::surface::WlSurface, name: &str, enter: bool) {
    use crate::protocol::interfaces::wl_surface;
    surface.client.objects.for_each_output(|o| {
        if o.name == name {
            let sid = surface.id;
            let oid = o.id;
            surface.client.event(|w| {
                if enter {
                    wl_surface::enter::send(w, sid, oid);
                } else {
                    wl_surface::leave::send(w, sid, oid);
                }
            });
        }
    });
}

/// keybind-driven focus onto another output lands the cursor there too:
/// center of the workspace's focused-most window, else the output center
fn warp_to_workspace(state: &Rc<State>, ws: &Workspace) {
    // painted rects: the fullscreen slot draws at the output rect, and
    // the cursor should land where the window actually is
    let r = ws
        .fullscreen
        .borrow()
        .as_ref()
        .map(|w| w.draw_rect(state))
        .or_else(|| ws.tiling.first().map(|w| w.rect.get()))
        .or_else(|| ws.top_float().map(|w| w.rect.get()))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| workspace_output_rect(state, ws));
    let (cx, cy) = ((r.x1 + r.x2) / 2, (r.y1 + r.y2) / 2);
    if let Some(seat) = state.seat.borrow().clone() {
        seat.place_pointer(state, cx as f64, cy as f64);
    }
}

/// pointer crossed onto another output: focus follows, and the active
/// workspace becomes whatever that output is showing
pub fn note_pointer_output(state: &Rc<State>, x: f64, y: f64) {
    let hit = {
        let d = state.display.borrow();
        let Some(d) = d.as_ref() else { return };
        let Some(out) = d.output_at(x as i32, y as i32) else { return };
        (out.index.get(), out.ws.get())
    };
    if state.focused_output.replace(hit.0) != hit.0 {
        state.active_ws.set(hit.1);
    }
}

pub enum WindowKind {
    Xdg(Rc<XdgToplevel>),
    X11(Rc<crate::xwayland::XWindow>),
}

pub struct Window {
    pub kind: WindowKind,
    /// stable identity for the window's lifetime; uids never get reused
    pub ident: u64,
    /// assigned box, gaps/border applied - the TARGET; animations chase it
    pub rect: Cell<Rect>,
    pub node: RefCell<Weak<dwindle::Node>>,
    pub floating: Cell<bool>,
    pub fullscreen: Cell<bool>,
    /// window-rule `immediate`: tearing without the client's async hint
    pub rule_immediate: Cell<bool>,
    /// window-rule `opacity`, multiplied into every sampled quad
    pub rule_opacity: Cell<Option<f32>>,
    pub rule_rounding: Cell<Option<i32>>,
    pub rule_shadow: Cell<Option<bool>>,
    pub rule_dim: Cell<Option<bool>>,
    pub rule_blur: Cell<bool>,
    /// captures and casts get a black stand-in instead of this window
    pub rule_no_capture: Cell<bool>,
    pub anims: RefCell<WinAnims>,
    /// the ops + texture keys/uids this window produced at its last compose,
    /// plus the subrange holding only the surface-tree ops; the close
    /// animation seizes the whole batch, the resize snapshot bakes just
    /// the surface slice
    pub last_batch: RefCell<(
        Vec<crate::render::renderer::RenderOp>,
        Vec<((crate::client::ClientId, u64), u64)>,
        std::ops::Range<usize>,
    )>,
}

// -- window animations: the visuals chasing the target rect --

#[derive(Default)]
pub struct WinAnims {
    pub move_: Option<MoveAnim>,
    pub open: Option<(crate::anim::Anim, crate::config::Style)>,
    pub border: Option<BorderAnim>,
    /// settled dim rides here too; never pruned
    pub dim: Option<crate::anim::Anim>,
    pub resize: Option<ResizeAnim>,
}

/// old content crossfades into new across the animated geometry; the
/// textures are baked/rendered at compose time and retired through state
pub struct ResizeAnim {
    pub anim: crate::anim::Anim, // 0 -> 1
    pub from: (i32, i32),
    pub from_rect: Rect,
    pub snapshot: Option<crate::render::renderer::Texture>,
    pub live: Option<crate::render::renderer::Texture>,
}

/// draw offset = (dx, dy) * anim.value(); the anim runs 1 -> 0
pub struct MoveAnim {
    pub dx: f64,
    pub dy: f64,
    pub anim: crate::anim::Anim,
}

pub struct BorderAnim {
    pub from: [f32; 4],
    pub to: [f32; 4],
    pub anim: crate::anim::Anim,
}

impl Window {
    pub fn new(state: &State, kind: WindowKind) -> Window {
        Window {
            kind,
            ident: state.next_uid(),
            rect: Cell::new(Rect::default()),
            node: RefCell::new(Weak::new()),
            floating: Cell::new(false),
            fullscreen: Cell::new(false),
            rule_immediate: Cell::new(false),
            rule_opacity: Cell::new(None),
            rule_rounding: Cell::new(None),
            rule_shadow: Cell::new(None),
            rule_dim: Cell::new(None),
            rule_blur: Cell::new(false),
            rule_no_capture: Cell::new(false),
            anims: RefCell::new(WinAnims::default()),
            last_batch: RefCell::new((Vec::new(), Vec::new(), 0..0)),
        }
    }

    pub fn xdg_opt(&self) -> Option<&Rc<XdgToplevel>> {
        match &self.kind {
            WindowKind::Xdg(tl) => Some(tl),
            _ => None,
        }
    }

    pub fn x11_opt(&self) -> Option<&Rc<crate::xwayland::XWindow>> {
        match &self.kind {
            WindowKind::X11(xw) => Some(xw),
            _ => None,
        }
    }

    pub fn surface(&self) -> Rc<WlSurface> {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.xdg.surface.clone(),
            WindowKind::X11(xw) => xw.surface().expect("x window in tree without a surface"),
        }
    }

    // x windows have no client-declared geometry; the surface is the window
    pub fn geometry(&self) -> Rect {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.xdg.geometry(),
            WindowKind::X11(_) => {
                let (w, h) = self.surface().size.get();
                Rect::new_sized_saturating(0, 0, w, h)
            }
        }
    }

    pub fn title(&self) -> String {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.title.borrow().clone(),
            WindowKind::X11(xw) => xw.title.borrow().clone(),
        }
    }

    pub fn app_id(&self) -> String {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.app_id.borrow().clone(),
            WindowKind::X11(xw) => xw.class.borrow().clone(),
        }
    }

    pub fn set_fullscreen_state(&self, on: bool) {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.set_fullscreen_state(on),
            WindowKind::X11(_) => {}
        }
    }

    pub fn wants_fullscreen(&self) -> bool {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.wants_fullscreen(),
            WindowKind::X11(_) => false,
        }
    }

    /// where this window paints
    pub fn draw_rect(&self, state: &State) -> Rect {
        if self.fullscreen.get() {
            fullscreen_rect(state, self)
        } else {
            self.rect.get()
        }
    }

    /// target rect write + move-anim bookkeeping; every layout path uses this
    pub fn set_rect_animated(&self, state: &State, r: Rect) {
        self.set_rect_animated_bias(state, r, 0.0);
    }

    /// view_dx: apparent horizontal movement owed to a strip scroll; the
    /// view animation carries that share, move anims only the remainder
    pub fn set_rect_animated_bias(&self, state: &State, r: Rect, view_dx: f64) {
        let old = self.rect.replace(r);
        if old == r {
            return;
        }
        // a live grab tracks 1:1: no anims spawn, in-flight ones drop so
        // nothing chases the pointer
        if state.grab_active.get() {
            let mut m = self.anims.borrow_mut();
            m.move_ = None;
            if let Some(rz) = &mut m.resize {
                let mut q = state.retire_tex.borrow_mut();
                q.extend(rz.snapshot.take());
                q.extend(rz.live.take());
            }
            m.resize = None;
            return;
        }
        let first = old == Rect::default();
        let cfg = state.config.borrow().clone();
        // size change beyond the threshold starts (or retargets) the
        // crossfade; the textures get built at compose time
        let (dw, dh) = (old.width() - r.width(), old.height() - r.height());
        if !first
            && !self.fullscreen.get()
            && (dw.abs() > 10 || dh.abs() > 10)
            && self.surface().mapped.get()
        {
            if let Some(motion) = cfg.animations.motion(crate::config::AnimKind::WindowResize) {
                state.anim_clock.touch();
                let now = state.anim_clock.now();
                let mut m = self.anims.borrow_mut();
                match &mut m.resize {
                    Some(rz) => {
                        let p = rz.anim.clamped_value(now);
                        crate::trace!(
                            "rz: retarget #{} p={:.2} {}x{} -> {}x{} t={}",
                            self.ident,
                            p,
                            old.width(),
                            old.height(),
                            r.width(),
                            r.height(),
                            now
                        );
                        let cw = rz.from.0 as f64 + (old.width() - rz.from.0) as f64 * p;
                        let ch = rz.from.1 as f64 + (old.height() - rz.from.1) as f64 * p;
                        rz.from = (cw.round() as i32, ch.round() as i32);
                        rz.anim = crate::config::build_anim(
                            &state.anim_clock,
                            motion,
                            &cfg.animations,
                            0.0,
                            1.0,
                            rz.anim.velocity(now),
                        );
                    }
                    None => {
                        crate::trace!(
                            "rz: start #{} {}x{} -> {}x{} t={}",
                            self.ident,
                            old.width(),
                            old.height(),
                            r.width(),
                            r.height(),
                            now
                        );
                        m.resize = Some(ResizeAnim {
                            anim: crate::config::build_anim(
                                &state.anim_clock,
                                motion,
                                &cfg.animations,
                                0.0,
                                1.0,
                                0.0,
                            ),
                            from: (old.width(), old.height()),
                            from_rect: old,
                            snapshot: None,
                            live: None,
                        });
                    }
                }
            }
        }
        let (dx, dy) = ((old.x1 - r.x1) as f64 - view_dx, (old.y1 - r.y1) as f64);
        if first || (dx == 0.0 && dy == 0.0) || self.fullscreen.get() {
            return;
        }
        let Some(motion) = cfg.animations.motion(crate::config::AnimKind::WindowMove) else {
            self.anims.borrow_mut().move_ = None;
            return;
        };
        state.anim_clock.touch();
        let now = state.anim_clock.now();
        let mut anims = self.anims.borrow_mut();
        // retarget folds the current visual offset into the new from-delta;
        // the scalar velocity is per-span, so rescale it to the new span or
        // the handoff speed jumps with the delta magnitude
        let (cdx, cdy, vel) = match &anims.move_ {
            Some(m) => {
                let v = m.anim.value(now);
                let (cdx, cdy) = (m.dx * v + dx, m.dy * v + dy);
                let old = (m.dx * m.dx + m.dy * m.dy).sqrt();
                let new = (cdx * cdx + cdy * cdy).sqrt();
                let vel = if new > 1e-6 { m.anim.velocity(now) * old / new } else { 0.0 };
                (cdx, cdy, vel)
            }
            None => (dx, dy, 0.0),
        };
        anims.move_ = Some(MoveAnim {
            dx: cdx,
            dy: cdy,
            anim: crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 1.0, 0.0, vel),
        });
    }

    /// where this window paints THIS frame: the target plus animated offset
    pub fn visual_rect(&self, state: &State) -> Rect {
        let base = self.draw_rect(state);
        let m = self.anims.borrow();
        match &m.move_ {
            Some(mv) => {
                let v = mv.anim.value(state.anim_clock.now());
                base.move_((mv.dx * v).round() as i32, (mv.dy * v).round() as i32)
            }
            None => base,
        }
    }

    /// prune finished animations, report whether any remain. move draws
    /// the raw overshooting value, so it lives until the envelope rests;
    /// everything else draws clamped and is static once settled
    pub fn anims_live(&self, now: u64) -> bool {
        let mut m = self.anims.borrow_mut();
        if m.move_.as_ref().is_some_and(|mv| mv.anim.is_done(now)) {
            m.move_ = None;
        }
        // resize is pruned by the draw path, which can retire its textures
        if m.open.as_ref().is_some_and(|(a, _)| a.settled(now)) {
            m.open = None;
        }
        if m.border.as_ref().is_some_and(|b| b.anim.settled(now)) {
            m.border = None;
        }
        m.move_.is_some()
            || m.open.is_some()
            || m.border.is_some()
            || m.dim.as_ref().is_some_and(|a| !a.settled(now))
            || m.resize.as_ref().is_some_and(|r| !r.anim.settled(now))
    }

    /// drop every animation; grabs and no-anim paths stay 1:1
    pub fn anims_snap(&self, state: &State) {
        self.retire_gpu(state);
        *self.anims.borrow_mut() = WinAnims::default();
    }

    /// hand any held gpu textures to the deferred retire queue
    pub fn retire_gpu(&self, state: &State) {
        let mut m = self.anims.borrow_mut();
        if let Some(rz) = &mut m.resize {
            let mut q = state.retire_tex.borrow_mut();
            q.extend(rz.snapshot.take());
            q.extend(rz.live.take());
        }
    }

    /// drop only the move animation; a grab must not chase its own pointer
    pub fn move_snap(&self) {
        self.anims.borrow_mut().move_ = None;
    }

    /// this frame's dim strength, easing toward `want` on focus flips
    pub fn dim_now(&self, state: &State, want: f64) -> f64 {
        let now = state.anim_clock.now();
        let cfg = state.config.borrow().clone();
        let mut m = self.anims.borrow_mut();
        let Some(a) = &mut m.dim else {
            m.dim = Some(crate::anim::Anim::ease(
                &state.anim_clock,
                want,
                want,
                0,
                crate::anim::Curve::Linear,
            ));
            return want;
        };
        if (a.to() - want).abs() > 1e-3 {
            match cfg.animations.motion(crate::config::AnimKind::BorderColor) {
                Some(motion) => {
                    let cur = a.clamped_value(now);
                    state.anim_clock.touch();
                    *a = crate::config::build_anim(
                        &state.anim_clock,
                        motion,
                        &cfg.animations,
                        cur,
                        want,
                        0.0,
                    );
                }
                None => {
                    *a = crate::anim::Anim::ease(
                        &state.anim_clock,
                        want,
                        want,
                        0,
                        crate::anim::Curve::Linear,
                    );
                }
            }
        }
        a.clamped_value(now).clamp(0.0, 1.0)
    }

    /// the border color this frame, easing in oklab toward `want`
    pub fn border_color_now(&self, state: &State, want: [f32; 4]) -> [f32; 4] {
        let now = state.anim_clock.now();
        let cfg = state.config.borrow().clone();
        let mut m = self.anims.borrow_mut();
        let Some(b) = &mut m.border else {
            // first paint settles instantly; only changes animate
            m.border = Some(BorderAnim {
                from: want,
                to: want,
                anim: crate::anim::Anim::ease(&state.anim_clock, 1.0, 1.0, 0, crate::anim::Curve::Linear),
            });
            return want;
        };
        if b.to != want {
            match cfg.animations.motion(crate::config::AnimKind::BorderColor) {
                Some(motion) => {
                    // restart from the color currently on glass
                    let cur = crate::anim::lerp_oklab(b.from, b.to, b.anim.clamped_value(now));
                    state.anim_clock.touch();
                    b.from = cur;
                    b.to = want;
                    b.anim =
                        crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 0.0, 1.0, 0.0);
                }
                None => {
                    b.from = want;
                    b.to = want;
                }
            }
        }
        crate::anim::lerp_oklab(b.from, b.to, b.anim.clamped_value(now))
    }

    pub fn configure_rect(&self) {
        let r = self.rect.get();
        match &self.kind {
            WindowKind::Xdg(tl) => tl.configure_size(r.width(), r.height()),
            WindowKind::X11(xw) => xw.configure_to(r),
        }
    }

    pub fn send_close(&self) {
        match &self.kind {
            WindowKind::Xdg(tl) => tl.send_close(),
            WindowKind::X11(xw) => xw.close(),
        }
    }
}

// -- workspaces --

/// every workspace starts in the configured layout mode; a bare
/// Workspace::default() carries the compile-time mode and strands the
/// scrolling verbs on whatever it happened to be
pub fn new_workspace(state: &Rc<State>) -> Rc<Workspace> {
    let w = Workspace::default();
    w.tiling.set_mode_empty(state.config.borrow().layout.mode);
    Rc::new(w)
}

pub fn active(state: &Rc<State>) -> Rc<Workspace> {
    let mut list = state.workspaces.borrow_mut();
    if list.is_empty() {
        let w = new_workspace(state);
        list.push(w);
    }
    let idx = state.active_ws.get().min(list.len() - 1);
    list[idx].clone()
}

/// grow the workspace list until `idx` exists; demand sites are switching,
/// window moves, and workspace shares. new ones land on the focused output
/// workspace indices are user input (binds, ipc, the share picker); the
/// cap keeps one garbage index from driving the grow loop through
/// billions of allocations on the thread everything shares
pub const MAX_WORKSPACES: usize = 256;

/// grows the list to reach idx, capped; returns the index actually used
pub fn ensure_workspace(state: &Rc<State>, idx: usize) -> usize {
    let idx = idx.min(MAX_WORKSPACES - 1);
    let mut list = state.workspaces.borrow_mut();
    while list.len() <= idx {
        let w = new_workspace(state);
        w.output.set(state.focused_output.get());
        list.push(w);
    }
    idx
}

pub fn switch_workspace(state: &Rc<State>, idx: usize) {
    if state.active_ws.get() == idx && !state.workspaces.borrow().is_empty() {
        return;
    }
    let idx = ensure_workspace(state, idx);
    let prev_out = state.focused_output.get();
    state.active_ws.set(idx);
    let ws = active(state);
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(out) = d.outputs.borrow().get(ws.output.get()) {
            let prev_shown = out.ws.get();
            out.ws.set(idx);
            state.focused_output.set(ws.output.get());
            if prev_shown != idx {
                crate::output::start_ws_switch(state, out, prev_shown, idx);
            }
        }
    }
    relayout(state, &ws);
    // crossing outputs by keybind drags the cursor along -
    // unless the pointer is already sitting on the target output
    if state.focused_output.get() != prev_out {
        let already_there = {
            let (cx, cy) = cursor_pos(state);
            workspace_output_rect(state, &ws).contains(cx, cy)
        };
        if !already_there {
            warp_to_workspace(state, &ws);
        }
    }
    // the whole scene changed under a stationary cursor: pointer focus
    // re-resolves, and a constraint pinned to a hidden window breaks
    if let Some(seat) = state.seat.borrow().clone() {
        seat.repick(state);
    }
    // focus lands on whatever is under the cursor, else the first tile
    let target = {
        let (cx, cy) = cursor_pos(state);
        window_at(state, cx, cy)
            .map(|(w, ..)| w)
            .or_else(|| ws.tiling.default_focus())
            .or_else(|| ws.top_float())
    };
    focus_window(state, target.as_ref());
    // casts sourcing the workspace that just left the glass switch to
    // tick feeding; without the nudge a client mid-callback never wakes
    crate::portal::cast::glass_changed(state);
    crate::ipc::emit(state, &serde_json::json!({ "workspace": idx + 1 }));
    state.damage.trigger();
}

/// scrolling workspaces own the horizontal axis; any of them forces the
/// workspace stack vertical. dwindle sessions follow workspace-axis,
/// horizontal unless the config asks otherwise
pub fn ws_axis_vertical(state: &Rc<State>) -> bool {
    state.config.borrow().layout.ws_axis == crate::config::WsAxis::Vertical
        || state
            .workspaces
            .borrow()
            .iter()
            .any(|w| w.tiling.mode() == crate::config::LayoutMode::Scrolling)
}

/// switch a workspace's tiling mode, re-tiling its windows in order; the
/// move animations carry the reshuffle
pub fn set_layout(state: &Rc<State>, ws: &Rc<Workspace>, mode: crate::config::LayoutMode) {
    if ws.tiling.mode() == mode {
        return;
    }
    let cfg = state.config.borrow().clone();
    let wins: Vec<Rc<Window>> = match ws.tiling.mode() {
        crate::config::LayoutMode::Dwindle => {
            let mut v = Vec::new();
            ws.tiling.dwindle.for_each(|w| v.push(w.clone()));
            for w in &v {
                ws.tiling.dwindle.remove(w);
            }
            v
        }
        crate::config::LayoutMode::Scrolling => ws.tiling.strip.take_all(),
    };
    ws.tiling.set_mode_empty(mode);
    let (cx, cy) = cursor_pos(state);
    for w in &wins {
        match mode {
            crate::config::LayoutMode::Dwindle => ws.tiling.dwindle.insert(w, cx, cy),
            crate::config::LayoutMode::Scrolling => {
                ws.tiling.strip.insert_ordered(w, &cfg.layout.scrolling)
            }
        }
    }
    if mode == crate::config::LayoutMode::Scrolling {
        if let Some(first) = wins.first() {
            ws.tiling.strip.note_focus(first);
        }
    }
    relayout(state, ws);
    state.damage.trigger();
}

pub(crate) fn cursor_pos(state: &Rc<State>) -> (i32, i32) {
    match &*state.seat.borrow() {
        Some(seat) => (seat.ptr_x.get() as i32, seat.ptr_y.get() as i32),
        None => {
            let (w, h) = output_extent(state);
            (w / 2, h / 2)
        }
    }
}

pub fn focused_window(state: &Rc<State>) -> Option<Rc<Window>> {
    let seat = state.seat.borrow().clone()?;
    let focus = seat.kb_focus.borrow().clone()?;
    window_for_surface(state, &focus)
}

// move the focused window to workspace n; with follow, switch there and keep it focused
pub fn send_to_workspace(state: &Rc<State>, n: usize, follow: bool) {
    if n == state.active_ws.get() {
        return;
    }
    let Some(win) = focused_window(state) else {
        return;
    };
    let ws = active(state);
    if win.fullscreen.get() {
        set_fullscreen(state, &win, false);
        win.set_fullscreen_state(false);
    }
    if win.floating.get() {
        ws.remove_float(&win);
        win.floating.set(false);
    } else {
        ws.tiling.remove(&win);
    }
    relayout(state, &ws);
    let n = ensure_workspace(state, n);
    let target = state.workspaces.borrow()[n].clone();
    let area = tiling_area_for(state, &target);
    let scfg = state.config.borrow().layout.scrolling.clone();
    target
        .tiling
        .insert(&win, (area.x1 + area.x2) / 2, (area.y1 + area.y2) / 2, &scfg);
    // the target lays out now: the window must not keep the rect it wore
    // on the source, invisible until something else touches the workspace
    relayout(state, &target);
    if ws.output.get() != target.output.get() {
        send_surface_output(state, &win.surface(), ws.output.get(), false);
        send_surface_output(state, &win.surface(), target.output.get(), true);
    }
    if follow {
        switch_workspace(state, n);
        focus_window(state, Some(&win));
    } else {
        // the sender stays put; something on this workspace takes focus
        let (cx, cy) = cursor_pos(state);
        let next = window_at(state, cx, cy)
            .map(|(w, ..)| w)
            .or_else(|| ws.tiling.default_focus())
            .or_else(|| ws.top_float());
        focus_window(state, next.as_ref());
    }
    crate::protocol::foreign_toplevel::output_changed(state, &win);
}

// the x server died; every window it owned goes with it
pub fn remove_x11_windows(state: &Rc<State>) {
    let list = state.workspaces.borrow().clone();
    for ws in list {
        let mut gone = Vec::new();
        ws.for_each(|w| {
            if w.x11_opt().is_some() {
                gone.push(w.clone());
            }
        });
        for w in gone {
            if w.fullscreen.get() {
                w.fullscreen.set(false);
                let mut slot = ws.fullscreen.borrow_mut();
                if slot.as_ref().is_some_and(|f| Rc::ptr_eq(f, &w)) {
                    *slot = None;
                }
            }
            if w.floating.get() {
                ws.remove_float(&w);
            } else {
                ws.tiling.remove(&w);
            }
            if let Some(xw) = w.x11_opt() {
                *xw.window.borrow_mut() = None;
            }
        }
    }
    let ws = active(state);
    relayout(state, &ws);
    state.damage.trigger();
}

// step focus through the workspace in tree order; wraps around
pub fn focus_cycle(state: &Rc<State>, dir: i32) {
    let ws = active(state);
    let mut wins = Vec::new();
    ws.for_each(|w| wins.push(w.clone()));
    if wins.is_empty() {
        return;
    }
    // everything under a fullscreen window is buried; cycling either
    // stays or snaps a stranded focus back to the visible one
    let fs = ws.fullscreen.borrow().clone();
    if let Some(fs) = fs {
        if focused_window(state).is_none_or(|c| !Rc::ptr_eq(&c, &fs)) {
            focus_window(state, Some(&fs));
            state.damage.trigger();
        }
        return;
    }
    let cur = focused_window(state);
    let from = cur.as_ref().map(|c| c.rect.get()).unwrap_or_default();
    let idx = cur.and_then(|c| wins.iter().position(|w| Rc::ptr_eq(w, &c)));
    let next = match idx {
        Some(i) => (i as i32 + dir).rem_euclid(wins.len() as i32) as usize,
        None => 0,
    };
    focus_window(state, Some(&wins[next]));
    warp_pointer_into(state, from, &wins[next]);
    state.damage.trigger();
}

/// keyboard focus moved windows: carry the pointer along, holding its
/// relative position from the old rect; a pointer that was elsewhere
/// lands centered
fn warp_pointer_into(state: &Rc<State>, from: Rect, win: &Rc<Window>) {
    let Some(seat) = state.seat.borrow().clone() else { return };
    let to = win.draw_rect(state);
    if to.is_empty() {
        return;
    }
    let (px, py) = (seat.ptr_x.get(), seat.ptr_y.get());
    let inside = from.width() > 0 && from.height() > 0 && from.contains(px as i32, py as i32);
    let (fx, fy) = if inside {
        (
            (px - from.x1 as f64) / from.width() as f64,
            (py - from.y1 as f64) / from.height() as f64,
        )
    } else {
        (0.5, 0.5)
    };
    let nx = to.x1 as f64 + fx * to.width() as f64;
    let ny = to.y1 as f64 + fy * to.height() as f64;
    // place_pointer yields whole under a lock: a locked pointer is frozen
    // by contract and the drawn cursor must not teleport away from it
    seat.place_pointer(state, nx, ny);
}

// -- directional navigation --

pub fn switch_workspace_rel(state: &Rc<State>, delta: i32) {
    let count = state.workspaces.borrow().len();
    switch_workspace(state, rel_wrap(state.active_ws.get(), delta, count));
}

fn rel_wrap(cur: usize, delta: i32, count: usize) -> usize {
    (cur as i32 + delta).rem_euclid(count.max(1) as i32) as usize
}

fn span_overlap(a1: i32, a2: i32, b1: i32, b2: i32) -> i32 {
    a2.min(b2) - a1.max(b1)
}

/// nearest rect whose facing edge lies in `dir` with some perpendicular
/// overlap; ties go to the greatest overlap
fn dir_pick(from: Rect, rects: &[Rect], dir: Dir) -> Option<usize> {
    let mut best: Option<(i32, i32, usize)> = None;
    for (i, r) in rects.iter().enumerate() {
        let (dist, overlap) = match dir {
            Dir::Left => (from.x1 - r.x2, span_overlap(from.y1, from.y2, r.y1, r.y2)),
            Dir::Right => (r.x1 - from.x2, span_overlap(from.y1, from.y2, r.y1, r.y2)),
            Dir::Up => (from.y1 - r.y2, span_overlap(from.x1, from.x2, r.x1, r.x2)),
            Dir::Down => (r.y1 - from.y2, span_overlap(from.x1, from.x2, r.x1, r.x2)),
        };
        if dist < 0 || overlap <= 0 {
            continue;
        }
        if best.is_none_or(|(bd, bo, _)| dist < bd || (dist == bd && overlap > bo)) {
            best = Some((dist, overlap, i));
        }
    }
    best.map(|(.., i)| i)
}

pub fn focus_dir(state: &Rc<State>, dir: Dir) {
    let Some(cur) = focused_window(state) else {
        return;
    };
    let ws = workspace_of(state, &cur).unwrap_or_else(|| active(state));
    // a fullscreen window is the only visible thing on its workspace;
    // focus verbs never wander under it, and focus stranded on a buried
    // window snaps back out
    let fs = ws.fullscreen.borrow().clone();
    if let Some(fs) = fs {
        if !Rc::ptr_eq(&fs, &cur) {
            focus_window(state, Some(&fs));
            state.damage.trigger();
        }
        return;
    }
    if ws.tiling.mode() == crate::config::LayoutMode::Scrolling && !cur.floating.get() {
        ws.tiling.strip.note_focus(&cur);
        if let Some(next) = ws.tiling.strip.focus_dir(dir) {
            let from = cur.rect.get();
            // keep-in-view follows the newly active column
            relayout(state, &ws);
            focus_window(state, Some(&next));
            warp_pointer_into(state, from, &next);
            state.damage.trigger();
            return;
        }
        // no neighbor that way: an over-wide column pans instead, so the
        // part past the view is reachable from the same keys
        if matches!(dir, Dir::Left | Dir::Right) && ws.tiling.strip.pan(dir == Dir::Left) {
            relayout(state, &ws);
            state.damage.trigger();
        }
        return;
    }
    let mut cands = Vec::new();
    ws.for_each(|w| {
        if !Rc::ptr_eq(w, &cur) {
            cands.push(w.clone());
        }
    });
    let rects: Vec<Rect> = cands.iter().map(|w| w.rect.get()).collect();
    if let Some(i) = dir_pick(cur.rect.get(), &rects, dir) {
        focus_window(state, Some(&cands[i]));
        warp_pointer_into(state, cur.rect.get(), &cands[i]);
        state.damage.trigger();
    }
}

pub fn swap_dir(state: &Rc<State>, dir: Dir) {
    let Some(cur) = focused_window(state) else {
        return;
    };
    // floats and fullscreen have no tree slot to trade
    if cur.floating.get() || cur.fullscreen.get() {
        return;
    }
    let ws = workspace_of(state, &cur).unwrap_or_else(|| active(state));
    if ws.tiling.mode() == crate::config::LayoutMode::Scrolling {
        ws.tiling.strip.note_focus(&cur);
        if ws.tiling.strip.swap_dir(&cur, dir) {
            relayout(state, &ws);
            state.damage.trigger();
        }
        return;
    }
    let mut cands = Vec::new();
    ws.tiling.for_each(|w| {
        if !Rc::ptr_eq(w, &cur) {
            cands.push(w.clone());
        }
    });
    let rects: Vec<Rect> = cands.iter().map(|w| w.rect.get()).collect();
    if let Some(i) = dir_pick(cur.rect.get(), &rects, dir) {
        if dwindle::swap_windows(&cur, &cands[i]) {
            relayout(state, &ws);
            state.damage.trigger();
        }
    }
}

pub(crate) fn focus_window(state: &Rc<State>, win: Option<&Rc<Window>>) {
    // an exclusive layer surface owns the keyboard; windows wait
    if crate::shell::layer::kb_lock(state).is_some() {
        return;
    }
    if let Some(w) = win {
        if !w.floating.get() {
            if let Some(ws) = workspace_of(state, w) {
                // focus handed from outside the strip's own verbs
                // (activation, cycling, a closing window's successor) can
                // land on an offscreen column; the view follows focus
                if ws.tiling.note_focus_win(w) {
                    relayout(state, &ws);
                    state.damage.trigger();
                }
            }
        }
    }
    let seat = state.seat.borrow().clone();
    if let Some(seat) = seat {
        // picking a window is selecting outside any popup grab chain
        crate::shell::xdg::dismiss_popup_grabs(state, &seat);
        crate::input::focus::set_keyboard_focus(state, &seat, win.map(|w| w.surface()));
    }
}

/// maps a focused surface back to its window; fullscreen leaves stay in the tree
pub fn window_for_surface(state: &Rc<State>, s: &Rc<WlSurface>) -> Option<Rc<Window>> {
    let ws = active(state);
    let mut found = None;
    ws.for_each(|w| {
        if found.is_none() && Rc::ptr_eq(&w.surface(), s) {
            found = Some(w.clone());
        }
    });
    found
}

/// like window_for_surface but searches every workspace, not just the
/// active one. a hidden-workspace cast routes its source's commits by
/// surface, and those windows are by definition off the active workspace
pub fn window_for_surface_any(state: &Rc<State>, s: &Rc<WlSurface>) -> Option<Rc<Window>> {
    let mut found = None;
    for ws in state.workspaces.borrow().iter() {
        ws.for_each(|w| {
            if found.is_none() && Rc::ptr_eq(&w.surface(), s) {
                found = Some(w.clone());
            }
        });
        if found.is_some() {
            break;
        }
    }
    found
}

// -- map / unmap --

/// re-resolve the dynamic rule effects for every mapped window; a reload
/// must land on running apps, a privacy rule especially. open-time
/// effects (floating, workspace, size) stay as mapped
pub fn reapply_rules(state: &Rc<State>) {
    let cfg = state.config.borrow().clone();
    let apply = |win: &Rc<Window>| {
        let fx = crate::config::rule_effects(
            &cfg,
            &win.app_id(),
            &win.title(),
            win.x11_opt().is_some(),
            win.fullscreen.get(),
        );
        win.rule_immediate.set(fx.immediate);
        win.rule_opacity
            .set(fx.opacity.map(|o| o.clamp(0.0, 1.0) as f32));
        win.rule_rounding.set(fx.rounding);
        win.rule_shadow.set(fx.shadow);
        win.rule_dim.set(fx.dim);
        win.rule_blur.set(fx.blur.unwrap_or(false));
        win.rule_no_capture.set(fx.no_capture);
    };
    for ws in state.workspaces.borrow().iter() {
        ws.for_each(&apply);
        if let Some(fs) = ws.fullscreen.borrow().as_ref() {
            apply(fs);
        }
    }
}

pub fn map_window(state: &Rc<State>, win: &Rc<Window>) {
    let cfg = state.config.borrow().clone();
    let fx = crate::config::rule_effects(
        &cfg,
        &win.app_id(),
        &win.title(),
        win.x11_opt().is_some(),
        win.wants_fullscreen(),
    );
    win.rule_immediate.set(fx.immediate);
    win.rule_opacity
        .set(fx.opacity.map(|o| o.clamp(0.0, 1.0) as f32));
    win.rule_rounding.set(fx.rounding);
    win.rule_shadow.set(fx.shadow);
    win.rule_dim.set(fx.dim);
    win.rule_blur.set(fx.blur.unwrap_or(false));
    win.rule_no_capture.set(fx.no_capture);
    // a rule can pin the window to a workspace (already 0-based) without switching to it
    let ws = fx
        .workspace
        .and_then(|n| state.workspaces.borrow().get(n).cloned())
        .unwrap_or_else(|| active(state));
    let visible = {
        let a = active(state);
        Rc::ptr_eq(&ws, &a)
    };
    send_surface_output(state, &win.surface(), ws.output.get(), true);
    // untile any fullscreen first; splitting behind it helps nobody
    let fs = ws.fullscreen.borrow().clone();
    if let Some(fs) = fs {
        set_fullscreen(state, &fs, false);
        fs.set_fullscreen_state(false);
    }
    let (cx, cy) = cursor_pos(state);
    ws.tiling.insert(win, cx, cy, &cfg.layout.scrolling);
    relayout(state, &ws);
    if fx.floating == Some(true) && !win.floating.get() {
        float_into(state, &ws, win, fx.size, fx.center);
    }
    if win.wants_fullscreen() && fx.floating != Some(true) {
        set_fullscreen(state, win, true);
    }
    if fx.no_anim {
        win.anims_snap(state);
    } else if let Some(motion) = cfg.animations.motion(crate::config::AnimKind::WindowOpen) {
        let style = match fx.animation.clone().unwrap_or_else(|| cfg.animations.window_open.style.clone()) {
            crate::config::Style::Default => crate::config::Style::Popin { perc: 0.8 },
            s => s,
        };
        state.anim_clock.touch();
        win.anims.borrow_mut().open = Some((
            crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 0.0, 1.0, 0.0),
            style,
        ));
    }
    // a rule-targeted background workspace never steals focus
    if visible {
        focus_window(state, Some(win));
    }
    crate::protocol::foreign_toplevel::window_mapped(state, win);
    crate::ipc::emit(
        state,
        &serde_json::json!({ "window-opened": {
            "title": win.title(),
            "app-id": win.app_id(),
        }}),
    );
    state.damage.trigger();
}

// rule-driven float placement: out of the tiling, sized and positioned
fn float_into(
    state: &Rc<State>,
    ws: &Rc<Workspace>,
    win: &Rc<Window>,
    size: Option<(i32, i32)>,
    center: bool,
) {
    ws.tiling.remove(win);
    win.floating.set(true);
    // sized and centered on the workspace's output, never the union
    let r = workspace_output_rect(state, ws);
    let (w, h) = size.unwrap_or((r.width() / 2, r.height() / 2));
    let (w, h) = (w.max(1), h.max(1));
    let (x, y) = if center || size.is_none() {
        (r.x1 + (r.width() - w) / 2, r.y1 + (r.height() - h) / 2)
    } else {
        (r.x1 + r.width() / 4, r.y1 + r.height() / 4)
    };
    win.set_rect_animated(state, Rect::new_sized_saturating(x, y, w, h));
    ws.floats.borrow_mut().push(win.clone());
    win.configure_rect();
    relayout(state, ws);
}

/// the workspace actually holding a window; "active" can be stale (the user or
/// a pointer crossing moved on), and removing from the wrong list leaves a
/// click-eating zombie
pub(crate) fn workspace_of(state: &Rc<State>, win: &Rc<Window>) -> Option<Rc<Workspace>> {
    state
        .workspaces
        .borrow()
        .iter()
        .find(|ws| ws.contains(win))
        .cloned()
}

pub fn unmap_window(state: &Rc<State>, win: &Rc<Window>) {
    let ws = workspace_of(state, win).unwrap_or_else(|| active(state));
    // remembered past the clear below: the close-anim skip and the rules
    // judge the window as it stood, not as the teardown left it
    let was_fullscreen = win.fullscreen.get();
    if win.fullscreen.get() {
        win.fullscreen.set(false);
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.as_ref().is_some_and(|w| Rc::ptr_eq(w, win)) {
            *slot = None;
        }
    }
    let old = win.rect.get();
    // the close animation seizes the window's last composed batch before
    // the tree forgets it
    {
        let cfg = state.config.borrow().clone();
        let fx = crate::config::rule_effects(
            &cfg,
            &win.app_id(),
            &win.title(),
            win.x11_opt().is_some(),
            was_fullscreen,
        );
        if !fx.no_anim && !was_fullscreen {
            if let Some(motion) = cfg.animations.motion(crate::config::AnimKind::WindowClose) {
                let out = state
                    .display
                    .borrow()
                    .as_ref()
                    .and_then(|d| d.outputs.borrow().get(ws.output.get()).cloned());
                if let Some(out) = out {
                    state.anim_clock.touch();
                    let style = match fx
                        .animation
                        .clone()
                        .unwrap_or_else(|| cfg.animations.window_close.style.clone())
                    {
                        crate::config::Style::Default => crate::config::Style::Popin { perc: 0.8 },
                        s => s,
                    };
                    let anim = crate::config::build_anim(
                        &state.anim_clock,
                        motion,
                        &cfg.animations,
                        1.0,
                        0.0,
                        0.0,
                    );
                    let rect = win.visual_rect(state);
                    if let Some(cw) = crate::output::capture_closing(&out, win, rect, anim, style) {
                        crate::output::push_closing(&out, &ws, cw);
                    }
                }
            }
        }
    }
    win.retire_gpu(state);
    if win.floating.get() {
        ws.remove_float(win);
    } else {
        ws.tiling.remove(win);
        relayout(state, &ws);
    }
    // hand focus to whoever now owns the freed spot
    let focused = state
        .seat
        .borrow()
        .as_ref()
        .and_then(|s| s.kb_focus.borrow().clone());
    let lost_focus = focused.is_some_and(|f| Rc::ptr_eq(&f.get_root(), &win.surface()));
    if lost_focus {
        let (mx, my) = ((old.x1 + old.x2) / 2, (old.y1 + old.y2) / 2);
        let next = ws
            .tiling
            .window_at(mx, my)
            .or_else(|| ws.tiling.default_focus())
            .or_else(|| ws.top_float());
        focus_window(state, next.as_ref());
    }
    crate::protocol::foreign_toplevel::window_unmapped(state, &win);
    crate::ipc::emit(
        state,
        &serde_json::json!({ "window-closed": {
            "title": win.title(),
        }}),
    );
    state.damage.trigger();
}

/// a fullscreen window fills the output of the workspace holding it
fn fullscreen_rect(state: &State, win: &Window) -> Rect {
    for ws in state.workspaces.borrow().iter() {
        let holds = ws
            .fullscreen
            .borrow()
            .as_ref()
            .is_some_and(|w| std::ptr::eq(&**w, win));
        if holds {
            return workspace_output_rect(state, ws);
        }
    }
    let (w, h) = output_extent(state);
    Rect::new_sized_saturating(0, 0, w, h)
}

pub fn set_fullscreen(state: &Rc<State>, win: &Rc<Window>, on: bool) {
    // the request may come from a client whose window sits on a hidden
    // workspace; the slot belongs to the window's own, never the active
    let ws = workspace_of(state, win).unwrap_or_else(|| active(state));
    if on {
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.is_some() {
            return;
        }
        *slot = Some(win.clone());
        win.fullscreen.set(true);
        let r = workspace_output_rect(state, &ws);
        match &win.kind {
            WindowKind::Xdg(tl) => tl.configure_size(r.width(), r.height()),
            WindowKind::X11(xw) => {
                xw.configure_to(r);
            }
        }
    } else {
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.as_ref().is_some_and(|w| Rc::ptr_eq(w, win)) {
            *slot = None;
        }
        win.fullscreen.set(false);
        win.configure_rect();
    }
    // the window's painted origin just jumped; a stationary cursor over it
    // needs its surface-local coordinates rebased
    if let Some(seat) = state.seat.borrow().clone() {
        seat.repick(state);
    }
    crate::ipc::emit(state, &serde_json::json!({ "fullscreen": on }));
    crate::protocol::foreign_toplevel::state_changed(state, win);
    state.damage.trigger();
}

// -- layout --

// the tiling area: whatever the layer-shell arranger left over, else the
// whole output
pub fn tiling_area(state: &Rc<State>) -> Rect {
    tiling_area_for(state, &active(state))
}

pub fn tiling_area_for(state: &Rc<State>, ws: &Workspace) -> Rect {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(out) = d.outputs.borrow().get(ws.output.get()) {
            let usable = out.usable.get();
            let full = out.rect();
            return if usable.is_empty() { full } else { usable.intersect(full) };
        }
    }
    let (sw, sh) = output_extent(state);
    let full = Rect::new_sized_saturating(0, 0, sw.max(1), sh.max(1));
    let usable = state.usable.get();
    if usable.is_empty() { full } else { usable.intersect(full) }
}

// outer gap on screen-flush edges, inner gap on shared edges, then the border
// insets all four sides; never below 1px
fn apply_gaps(r: Rect, area: Rect, cfg: &crate::config::Config) -> Rect {
    let left = if r.x1 <= area.x1 { cfg.layout.gaps_out } else { cfg.layout.gaps_in };
    let top = if r.y1 <= area.y1 { cfg.layout.gaps_out } else { cfg.layout.gaps_in };
    let right = if r.x2 >= area.x2 { cfg.layout.gaps_out } else { cfg.layout.gaps_in };
    let bottom = if r.y2 >= area.y2 { cfg.layout.gaps_out } else { cfg.layout.gaps_in };
    let x1 = r.x1 + left + cfg.layout.border.width;
    let y1 = r.y1 + top + cfg.layout.border.width;
    let x2 = (r.x2 - right - cfg.layout.border.width).max(x1 + 1);
    let y2 = (r.y2 - bottom - cfg.layout.border.width).max(y1 + 1);
    Rect { x1, y1, x2, y2 }
}

pub fn relayout(state: &Rc<State>, ws: &Workspace) {
    let (sw, sh) = output_extent(state);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let area = tiling_area_for(state, ws);
    let cfg = state.config.borrow().clone();
    match ws.tiling.mode() {
        crate::config::LayoutMode::Dwindle => {
            ws.tiling.dwindle.recalculate(area);
            ws.tiling.dwindle.for_each(|win| {
                let raw = win
                    .node
                    .borrow()
                    .upgrade()
                    .map(|n| n.rect.get())
                    .unwrap_or_default();
                win.set_rect_animated(state, apply_gaps(raw, area, &cfg));
                if !win.fullscreen.get() {
                    win.configure_rect();
                }
            });
        }
        crate::config::LayoutMode::Scrolling => {
            let old_view = ws.tiling.strip.view_px();
            let rects = ws.tiling.strip.layout(area, &cfg.layout.scrolling);
            // the scroll itself draws as one strip translate; feeding it
            // into per-window move anims would animate the delta twice.
            // a view step of +V moves targets by -V, so the scroll share
            // inside (old.x1 - new.x1) is +V = new_view - old_view
            let view_dx = ws.tiling.strip.view_px() - old_view;
            for (win, raw) in rects {
                win.set_rect_animated_bias(state, apply_gaps(raw, area, &cfg), view_dx);
                if !win.fullscreen.get() {
                    win.configure_rect();
                }
            }
            ws.tiling.strip.animate_view(state, old_view);
        }
    }
}

// -- hit testing --

/// deepest surface under the point; z order fullscreen, floats top-down, tiled
pub fn window_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
    let ws = workspace_at(state, x, y);
    let fs = ws.fullscreen.borrow().clone();
    // popups stack above every body, so the part overhanging the parent
    // hits the popup and not the window it hangs over; float popups over
    // fullscreen's over tiled, mirroring the draw order
    let float_vis = fs.is_none() || state.config.borrow().layout.float_above_fullscreen;
    if float_vis {
        for win in ws.floats.borrow().iter().rev() {
            if let Some(hit) = win_popups_hit(state, win, x, y) {
                return Some(hit);
            }
        }
    }
    if let Some(f) = &fs {
        if let Some(hit) = win_popups_hit(state, f, x, y) {
            return Some(hit);
        }
    } else {
        // later columns draw later; the last hit is the topmost
        let mut hit = None;
        ws.tiling.for_each(|w| {
            hit = win_popups_hit(state, w, x, y).or(hit.take());
        });
        if hit.is_some() {
            return hit;
        }
    }
    let check_floats = |list: &Workspace| -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
        for win in list.floats.borrow().iter().rev() {
            if let Some(hit) = window_hit(state, win, x, y) {
                return Some(hit);
            }
        }
        None
    };
    if let Some(fs) = &fs {
        if state.config.borrow().layout.float_above_fullscreen {
            if let Some(hit) = check_floats(&ws) {
                return Some(hit);
            }
        }
        if let Some(hit) = window_hit(state, fs, x, y) {
            return Some(hit);
        }
        // fullscreen covers the output; nothing under it is reachable
        return None;
    }
    if let Some(hit) = check_floats(&ws) {
        return Some(hit);
    }
    let win = ws.tiling.window_at(x, y)?;
    window_hit(state, &win, x, y)
}

/// a window's popup subtree alone, anchored like the draw pass
fn win_popups_hit(
    state: &Rc<State>,
    win: &Rc<Window>,
    x: i32,
    y: i32,
) -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
    if !win.surface().mapped.get() {
        return None;
    }
    let tl = win.xdg_opt()?;
    let rect = win.draw_rect(state);
    let (s, sx, sy) = popups_hit(&tl.xdg, rect.x1, rect.y1, x, y)?;
    Some((win.clone(), s, sx, sy))
}

fn window_hit(
    state: &Rc<State>,
    win: &Rc<Window>,
    x: i32,
    y: i32,
) -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
    // an unmapped surface draws nothing and must catch nothing
    if !win.surface().mapped.get() {
        return None;
    }
    let rect = win.draw_rect(state);
    if let Some(tl) = win.xdg_opt() {
        if let Some(hit) = popups_hit(&tl.xdg, rect.x1, rect.y1, x, y) {
            return Some((win.clone(), hit.0, hit.1, hit.2));
        }
    }
    let geo = win.geometry();
    let (lx, ly) = (x - rect.x1 + geo.x1, y - rect.y1 + geo.y1);
    let (s, sx, sy) = win.surface().find_surface_at(lx, ly)?;
    Some((win.clone(), s, sx, sy))
}

/// popups stack above parent, newest sibling topmost; positions relative to parent geometry
fn popup_hit(
    p: &Rc<crate::shell::xdg::XdgPopup>,
    ox: i32,
    oy: i32,
    x: i32,
    y: i32,
) -> Option<(Rc<WlSurface>, i32, i32)> {
    if !p.xdg.surface.mapped.get() {
        return None;
    }
    let (rx, ry) = p.rel.get();
    let (px, py) = (ox + rx, oy + ry);
    if let Some(h) = popups_hit(&p.xdg, px, py, x, y) {
        return Some(h);
    }
    let geo = p.xdg.geometry();
    let (lx, ly) = (x - px + geo.x1, y - py + geo.y1);
    p.xdg.surface.find_surface_at(lx, ly)
}

fn popups_hit(
    xdg: &Rc<crate::shell::xdg::XdgSurface>,
    ox: i32,
    oy: i32,
    x: i32,
    y: i32,
) -> Option<(Rc<WlSurface>, i32, i32)> {
    let mut hit = None;
    xdg.for_each_popup_rev(|p| {
        if hit.is_none() {
            hit = popup_hit(p, ox, oy, x, y);
        }
    });
    hit
}

// -- the full-scene hit test --

// layer-parented popups sit above every layer, then overlay, top, the
// windows, bottom, background. fullscreen hides top and everything below
// the windows.
/// where a surface's local (0,0) sits in global space right now: the
/// inverse of surface_at, for surface-local protocol coordinates that
/// resolve at application time (a lock's cursor position hint). the
/// surface may have moved since such a coordinate was produced
pub fn surface_origin(state: &Rc<State>, surface: &Rc<WlSurface>) -> Option<(i32, i32)> {
    let root = surface.get_root();
    if let Some(win) = window_for_surface_any(state, &root) {
        if !win.surface().mapped.get() {
            return None;
        }
        let rect = win.draw_rect(state);
        let geo = win.geometry();
        return offset_within(&root, surface, (rect.x1 - geo.x1, rect.y1 - geo.y1));
    }
    let layers = state.layers.borrow().clone();
    for ls in layers.iter() {
        if Rc::ptr_eq(&ls.surface, &root) && ls.mapped() {
            let r = ls.rect.get();
            return offset_within(&root, surface, (r.x1, r.y1));
        }
    }
    None
}

/// walk subsurfaces from root to target, accumulating child offsets
fn offset_within(
    root: &Rc<WlSurface>,
    target: &Rc<WlSurface>,
    base: (i32, i32),
) -> Option<(i32, i32)> {
    if Rc::ptr_eq(root, target) {
        return Some(base);
    }
    let children = root.children.borrow();
    let ch = children.as_ref()?;
    for e in ch.below.iter().chain(ch.above.iter()) {
        if e.pending.get() {
            continue;
        }
        let (px, py) = e.sub.position.get();
        if let Some(o) = offset_within(&e.sub.surface, target, (base.0 + px, base.1 + py)) {
            return Some(o);
        }
    }
    None
}

pub fn surface_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    use crate::shell::layer;
    // a locked session has exactly one hittable thing per output
    if crate::protocol::session_lock::locked(state) {
        return crate::protocol::session_lock::surface_at(state, x, y);
    }
    let fs_active = workspace_at(state, x, y).fullscreen.borrow().is_some();
    if let Some(hit) = layer_popups_hit(state, x, y, fs_active) {
        return Some(hit);
    }
    for l in [layer::OVERLAY, layer::TOP] {
        if l == layer::TOP && fs_active {
            continue;
        }
        if let Some(hit) = layer_hit(state, l, x, y) {
            return Some(hit);
        }
    }
    if let Some((_, s, sx, sy)) = window_at(state, x, y) {
        return Some((s, sx, sy));
    }
    if fs_active {
        return None;
    }
    for l in [layer::BOTTOM, layer::BACKGROUND] {
        if let Some(hit) = layer_hit(state, l, x, y) {
            return Some(hit);
        }
    }
    None
}

// newest mapped surface within a layer sits on top
fn layer_hit(state: &Rc<State>, layer: u32, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    let layers = state.layers.borrow().clone();
    for ls in layers.iter().rev() {
        if ls.current.get().layer != layer || !ls.mapped() {
            continue;
        }
        let r = ls.rect.get();
        if let Some(h) = ls.surface.find_surface_at(x - r.x1, y - r.y1) {
            return Some(h);
        }
    }
    None
}

// layer-parented popups form one band above every layer; a popup of a
// layer hidden by fullscreen hides with its parent
fn layer_popups_hit(
    state: &Rc<State>,
    x: i32,
    y: i32,
    fs_active: bool,
) -> Option<(Rc<WlSurface>, i32, i32)> {
    let layers = state.layers.borrow().clone();
    for ls in layers.iter().rev() {
        if !ls.mapped() {
            continue;
        }
        if fs_active && ls.current.get().layer != crate::shell::layer::OVERLAY {
            continue;
        }
        let r = ls.rect.get();
        let mut hit = None;
        ls.for_each_popup_rev(|p| {
            if hit.is_none() {
                hit = popup_hit(p, r.x1, r.y1, x, y);
            }
        });
        if hit.is_some() {
            return hit;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_new_workspace_carries_the_configured_mode() {
        let (state, _client) = crate::client::test_utils::test_client();
        let mut c = crate::config::Config::default();
        c.layout.mode = crate::config::LayoutMode::Scrolling;
        *state.config.borrow_mut() = Rc::new(c);
        let w = new_workspace(&state);
        assert_eq!(w.tiling.mode(), crate::config::LayoutMode::Scrolling);
    }

    #[test]
    fn workspace_indices_are_capped() {
        let (state, _client) = crate::client::test_utils::test_client();
        // a garbage index lands on the cap instead of allocating forever
        switch_workspace(&state, usize::MAX);
        assert_eq!(state.active_ws.get(), MAX_WORKSPACES - 1);
        assert_eq!(state.workspaces.borrow().len(), MAX_WORKSPACES);
    }

    #[test]
    fn move_anim_offsets_visual_rect() {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 90);
        let (_s, _xdg, tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 91, 92, 93);
        let win = Rc::new(Window::new(&state, WindowKind::Xdg(tl)));
        win.set_rect_animated(&state, Rect::new_sized_saturating(0, 0, 100, 100));
        // first placement never animates - there is no previous rect
        assert!(win.anims.borrow().move_.is_none());
        win.set_rect_animated(&state, Rect::new_sized_saturating(200, 0, 100, 100));
        assert!(win.anims.borrow().move_.is_some());
        // mid-flight the visual sits between the old and new x (the start
        // stamp is real monotonic time - anims begin in event context)
        let t0 = state.anim_clock.now();
        state.anim_clock.freeze(t0 + 30_000_000);
        let vr = win.visual_rect(&state);
        assert!(vr.x1 > 0 && vr.x1 < 200, "got x1={}", vr.x1);
        // far future: settled on the target, and the anim prunes away
        state.anim_clock.freeze(t0 + 10_000_000_000);
        assert_eq!(win.visual_rect(&state).x1, 200);
        assert!(!win.anims_live(state.anim_clock.now()));
        assert!(win.anims.borrow().move_.is_none());
    }

    #[test]
    fn send_to_workspace_lays_the_target_out() {
        let (state, client) = crate::client::test_utils::test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat);
        let base = crate::shell::xdg::tests::mk_base(&client, 90);
        // two tiled windows split the source workspace
        let (s1, x1, _t1) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 91, 92, 93);
        crate::shell::xdg::tests::map(&state, &client, &s1, &x1, 30);
        let (s2, x2, _t2) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 94, 95, 96);
        crate::shell::xdg::tests::map(&state, &client, &s2, &x2, 31);
        let win = window_for_surface(&state, &s2).unwrap();
        let half = win.rect.get();
        assert!(half.width() < 800, "the source split leaves half-width rects");
        // sent without following, the hidden target lays out immediately
        send_to_workspace(&state, 1, false);
        let area = tiling_area_for(&state, &state.workspaces.borrow()[1]);
        assert_eq!(win.rect.get(), area, "the moved window wears the target layout");
    }

    #[test]
    fn fullscreen_lands_on_the_windows_own_workspace() {
        let (state, client) = crate::client::test_utils::test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat);
        let base = crate::shell::xdg::tests::mk_base(&client, 90);
        let (s, x, _tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 91, 92, 93);
        crate::shell::xdg::tests::map(&state, &client, &s, &x, 30);
        let win = window_for_surface(&state, &s).unwrap();
        // the window moves to a hidden workspace; the request follows it
        send_to_workspace(&state, 1, false);
        assert_eq!(state.active_ws.get(), 0);
        set_fullscreen(&state, &win, true);
        let list = state.workspaces.borrow().clone();
        assert!(list[0].fullscreen.borrow().is_none(), "the active slot stays empty");
        assert!(
            list[1].fullscreen.borrow().as_ref().is_some_and(|w| Rc::ptr_eq(w, &win)),
            "the window's own workspace holds the slot"
        );
        set_fullscreen(&state, &win, false);
        assert!(list[1].fullscreen.borrow().is_none(), "unset clears the same slot");
        assert!(!win.fullscreen.get());
    }

    #[test]
    fn outside_focus_scrolls_the_strip_to_the_window() {
        let (state, client) = crate::client::test_utils::test_client();
        state.output_size.set((800, 600));
        let base = crate::shell::xdg::tests::mk_base(&client, 500);
        let mut surfs = Vec::new();
        for i in 0..4u32 {
            let (s, x, _t) = crate::shell::xdg::tests::mk_toplevel(
                &client,
                &base,
                501 + i * 3,
                502 + i * 3,
                503 + i * 3,
            );
            crate::shell::xdg::tests::map(&state, &client, &s, &x, 30 + i);
            surfs.push(s);
        }
        let ws = active(&state);
        set_layout(&state, &ws, crate::config::LayoutMode::Scrolling);
        relayout(&state, &ws);
        let head = window_for_surface(&state, &surfs[0]).unwrap();
        let tail = window_for_surface(&state, &surfs[3]).unwrap();
        // a protocol-side focus (activation) on the far column scrolls
        // the view there, no strip verb involved
        focus_window(&state, Some(&tail));
        let at_tail = ws.tiling.strip.view_px();
        assert!(at_tail > 0.0, "the strip scrolled to the tail column");
        assert!(ws.tiling.strip.view_anim.borrow().is_some(), "the scroll animates");
        // and back: the head column left-aligns the view again
        focus_window(&state, Some(&head));
        assert!(ws.tiling.strip.view_px().abs() < 0.5, "the view follows focus back");
        // a stationary focus never restarts the scroll
        *ws.tiling.strip.view_anim.borrow_mut() = None;
        focus_window(&state, Some(&head));
        assert!(ws.tiling.strip.view_anim.borrow().is_none(), "no column change, no scroll");
    }

    #[test]
    fn layout_conversion_roundtrip_preserves_windows() {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 400);
        let ws = active(&state);
        let mut wins = Vec::new();
        for i in 0..3u32 {
            let (_s, _x, tl) = crate::shell::xdg::tests::mk_toplevel(
                &client,
                &base,
                401 + i * 3,
                402 + i * 3,
                403 + i * 3,
            );
            let w = Rc::new(Window::new(&state, WindowKind::Xdg(tl)));
            ws.tiling.insert(&w, 0, 0, &state.config.borrow().layout.scrolling);
            wins.push(w);
        }
        assert_eq!(ws.tiling.mode(), crate::config::LayoutMode::Dwindle);
        assert!(!ws_axis_vertical(&state));
        set_layout(&state, &ws, crate::config::LayoutMode::Scrolling);
        assert_eq!(ws.tiling.mode(), crate::config::LayoutMode::Scrolling);
        assert!(ws_axis_vertical(&state), "one scrolling workspace flips the axis");
        let mut n = 0;
        ws.tiling.for_each(|_| n += 1);
        assert_eq!(n, 3, "every window survives the conversion");
        set_layout(&state, &ws, crate::config::LayoutMode::Dwindle);
        let mut n = 0;
        ws.tiling.for_each(|_| n += 1);
        assert_eq!(n, 3);
        assert!(!ws_axis_vertical(&state));
    }

    #[test]
    fn workspace_axis_config_forces_vertical_on_dwindle() {
        let (state, _client) = crate::client::test_utils::test_client();
        assert!(!ws_axis_vertical(&state));
        let mut cfg = (**state.config.borrow()).clone();
        cfg.layout.ws_axis = crate::config::WsAxis::Vertical;
        *state.config.borrow_mut() = Rc::new(cfg);
        assert!(ws_axis_vertical(&state), "dwindle honors the configured axis");
    }

    #[test]
    fn dim_eases_between_targets() {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 70);
        let (_s, _xdg, tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 71, 72, 73);
        let win = Rc::new(Window::new(&state, WindowKind::Xdg(tl)));
        // first paint settles instantly
        assert_eq!(win.dim_now(&state, 0.0), 0.0);
        // focus loss eases toward the configured strength
        win.dim_now(&state, 0.2);
        let t0 = state.anim_clock.now();
        state.anim_clock.freeze(t0 + 50_000_000);
        let mid = win.dim_now(&state, 0.2);
        assert!(mid > 0.0 && mid < 0.2, "got {mid}");
        state.anim_clock.freeze(t0 + 10_000_000_000);
        assert!((win.dim_now(&state, 0.2) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn border_color_eases_between_targets() {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 80);
        let (_s, _xdg, tl) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 81, 82, 83);
        let win = Rc::new(Window::new(&state, WindowKind::Xdg(tl)));
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 1.0];
        // first paint settles instantly
        assert_eq!(win.border_color_now(&state, red), red);
        // a new target starts easing: shortly after, neither endpoint
        win.border_color_now(&state, blue);
        let t0 = state.anim_clock.now();
        state.anim_clock.freeze(t0 + 50_000_000);
        let mid = win.border_color_now(&state, blue);
        assert_ne!(mid, red);
        assert_ne!(mid, blue);
        // far future: settled on the new target
        state.anim_clock.freeze(t0 + 10_000_000_000);
        assert_eq!(win.border_color_now(&state, blue), blue);
    }

    #[test]
    fn relative_workspace_wraps_both_ways() {
        assert_eq!(rel_wrap(0, 1, 3), 1);
        assert_eq!(rel_wrap(2, 1, 3), 0, "past the end wraps to the first");
        assert_eq!(rel_wrap(0, -1, 3), 2, "before the first wraps to the last");
        assert_eq!(rel_wrap(1, -4, 3), 0);
        assert_eq!(rel_wrap(1, 7, 3), 2);
        // a single workspace absorbs every jump
        assert_eq!(rel_wrap(0, 1, 1), 0);
        assert_eq!(rel_wrap(0, -5, 0), 0, "empty list never divides by zero");
    }

    #[test]
    fn dir_pick_takes_the_facing_neighbor() {
        // 2x2 grid: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right
        let r = |x1, y1, x2, y2| Rect { x1, y1, x2, y2 };
        let grid = [
            r(0, 0, 400, 300),
            r(400, 0, 800, 300),
            r(0, 300, 400, 600),
            r(400, 300, 800, 600),
        ];
        let from = grid[0];
        let others = [grid[1], grid[2], grid[3]];
        assert_eq!(dir_pick(from, &others, Dir::Right), Some(0));
        assert_eq!(dir_pick(from, &others, Dir::Down), Some(1));
        // the diagonal never wins: no perpendicular overlap leftward or upward
        assert_eq!(dir_pick(from, &others, Dir::Left), None);
        assert_eq!(dir_pick(from, &others, Dir::Up), None);
        // from the bottom-right corner both axes resolve
        let others = [grid[0], grid[1], grid[2]];
        assert_eq!(dir_pick(grid[3], &others, Dir::Left), Some(2));
        assert_eq!(dir_pick(grid[3], &others, Dir::Up), Some(1));
    }

    #[test]
    fn dir_pick_prefers_near_then_overlap() {
        let r = |x1, y1, x2, y2| Rect { x1, y1, x2, y2 };
        let from = r(400, 0, 800, 600);
        // a nearer column beats a farther one even with less overlap
        let cands = [r(0, 0, 200, 600), r(200, 0, 400, 100)];
        assert_eq!(dir_pick(from, &cands, Dir::Left), Some(1));
        // equal distance: the taller shared edge wins
        let cands = [r(0, 0, 400, 100), r(0, 100, 400, 600)];
        assert_eq!(dir_pick(from, &cands, Dir::Left), Some(1));
        // behind or overlapping rects are never neighbors
        let cands = [r(500, 0, 900, 600)];
        assert_eq!(dir_pick(from, &cands, Dir::Left), None);
    }
}
