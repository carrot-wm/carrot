// dwindle tree + workspaces + the floating stack.
// nodes have stable identity - no vec indices. z-order is
// fullscreen > floats > tiled (float_above_fullscreen swaps the first two).
// new windows split whatever is under the cursor, not the focused one.

pub mod dwindle;
pub mod float;
pub mod workspace;

use crate::config::Dir;
use crate::rect::Rect;
use crate::shell::xdg::XdgToplevel;
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use workspace::Workspace;

/// gaps/borders; config keys later
// gaps, borders, colors and binds all live in state.config now

pub fn output_extent(state: &State) -> (i32, i32) {
    let (w, h) = state.output_size.get();
    (w as i32, h as i32)
}

pub enum WindowKind {
    Xdg(Rc<XdgToplevel>),
    X11(Rc<crate::xwayland::XWindow>),
}

pub struct Window {
    pub kind: WindowKind,
    /// stable identity for the window's lifetime; uids never get reused
    pub ident: u64,
    /// assigned box, gaps/border applied
    pub rect: Cell<Rect>,
    pub node: RefCell<Weak<dwindle::Node>>,
    pub floating: Cell<bool>,
    pub fullscreen: Cell<bool>,
    /// a rule ORed tearing on for this window
    pub rule_immediate: Cell<bool>,
    /// a rule set per-window opacity; None means fully opaque
    pub rule_opacity: Cell<Option<f32>>,
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
            let (w, h) = output_extent(state);
            Rect::new_sized_saturating(0, 0, w, h)
        } else {
            self.rect.get()
        }
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

pub fn active(state: &Rc<State>) -> Rc<Workspace> {
    let mut list = state.workspaces.borrow_mut();
    if list.is_empty() {
        list.push(Rc::new(Workspace::default()));
    }
    let idx = state.active_ws.get().min(list.len() - 1);
    list[idx].clone()
}

pub fn switch_workspace(state: &Rc<State>, idx: usize) {
    if state.active_ws.get() == idx && !state.workspaces.borrow().is_empty() {
        return;
    }
    {
        let mut list = state.workspaces.borrow_mut();
        while list.len() <= idx {
            list.push(Rc::new(Workspace::default()));
        }
    }
    state.active_ws.set(idx);
    let ws = active(state);
    relayout(state, &ws);
    // focus lands on whatever is under the cursor, else the first tile
    let target = {
        let (cx, cy) = cursor_pos(state);
        window_at(state, cx, cy)
            .map(|(w, ..)| w)
            .or_else(|| ws.tiling.first())
            .or_else(|| ws.top_float())
    };
    focus_window(state, target.as_ref());
    crate::ipc::emit(state, &serde_json::json!({ "workspace": idx + 1 }));
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

// move the focused window to workspace n and follow it there; the moved
// window keeps the keyboard
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
    {
        let mut list = state.workspaces.borrow_mut();
        while list.len() <= n {
            list.push(Rc::new(Workspace::default()));
        }
    }
    let target = state.workspaces.borrow()[n].clone();
    let area = tiling_area(state);
    target
        .tiling
        .insert(&win, (area.x1 + area.x2) / 2, (area.y1 + area.y2) / 2);
    if follow {
        switch_workspace(state, n);
        focus_window(state, Some(&win));
    } else {
        // the sender stays put; something on this workspace takes focus
        let (cx, cy) = cursor_pos(state);
        let next = window_at(state, cx, cy)
            .map(|(w, ..)| w)
            .or_else(|| ws.tiling.first())
            .or_else(|| ws.top_float());
        focus_window(state, next.as_ref());
    }
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
    let cur = focused_window(state);
    let idx = cur.and_then(|c| wins.iter().position(|w| Rc::ptr_eq(w, &c)));
    let next = match idx {
        Some(i) => (i as i32 + dir).rem_euclid(wins.len() as i32) as usize,
        None => 0,
    };
    focus_window(state, Some(&wins[next]));
    state.damage.trigger();
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
    let mut cands = Vec::new();
    ws.for_each(|w| {
        if !Rc::ptr_eq(w, &cur) {
            cands.push(w.clone());
        }
    });
    let rects: Vec<Rect> = cands.iter().map(|w| w.rect.get()).collect();
    if let Some(i) = dir_pick(cur.rect.get(), &rects, dir) {
        focus_window(state, Some(&cands[i]));
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

// -- map / unmap --

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
    // a rule can pin the window to a workspace (already 0-based) without switching to it
    let ws = fx
        .workspace
        .and_then(|n| state.workspaces.borrow().get(n).cloned())
        .unwrap_or_else(|| active(state));
    let visible = Rc::ptr_eq(&ws, &active(state));
    // untile any fullscreen first; splitting behind it helps nobody
    let fs = ws.fullscreen.borrow().clone();
    if let Some(fs) = fs {
        set_fullscreen(state, &fs, false);
        fs.set_fullscreen_state(false);
    }
    let (cx, cy) = cursor_pos(state);
    ws.tiling.insert(win, cx, cy);
    relayout(state, &ws);
    if fx.floating == Some(true) && !win.floating.get() {
        float_into(state, &ws, win, fx.size, fx.center);
    }
    if win.wants_fullscreen() && fx.floating != Some(true) {
        set_fullscreen(state, win, true);
    }
    // a rule-targeted background workspace never steals focus
    if visible {
        focus_window(state, Some(win));
    }
    crate::ipc::emit(
        state,
        &serde_json::json!({ "window-opened": {
            "title": win.title(),
            "app-id": win.app_id(),
        }}),
    );
    crate::protocol::foreign_toplevel::window_mapped(state, win);
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
    let (sw, sh) = output_extent(state);
    let (w, h) = size.unwrap_or((sw / 2, sh / 2));
    let (w, h) = (w.max(1), h.max(1));
    let (x, y) = if center || size.is_none() {
        ((sw - w) / 2, (sh - h) / 2)
    } else {
        (sw / 4, sh / 4)
    };
    win.rect.set(Rect::new_sized_saturating(x, y, w, h));
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
    if win.fullscreen.get() {
        win.fullscreen.set(false);
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.as_ref().is_some_and(|w| Rc::ptr_eq(w, win)) {
            *slot = None;
        }
    }
    let old = win.rect.get();
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
            .or_else(|| ws.tiling.first())
            .or_else(|| ws.top_float());
        focus_window(state, next.as_ref());
    }
    crate::ipc::emit(
        state,
        &serde_json::json!({ "window-closed": {
            "title": win.title(),
        }}),
    );
    crate::protocol::foreign_toplevel::window_unmapped(state, win);
    state.damage.trigger();
}

pub fn set_fullscreen(state: &Rc<State>, win: &Rc<Window>, on: bool) {
    let ws = active(state);
    if on {
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.is_some() {
            return;
        }
        *slot = Some(win.clone());
        win.fullscreen.set(true);
        let (w, h) = output_extent(state);
        match &win.kind {
            WindowKind::Xdg(tl) => tl.configure_size(w, h),
            WindowKind::X11(xw) => {
                xw.configure_to(Rect::new_sized_saturating(0, 0, w, h));
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
    crate::ipc::emit(state, &serde_json::json!({ "fullscreen": on }));
    crate::protocol::foreign_toplevel::state_changed(state, win);
    state.damage.trigger();
}

// -- layout --

/// screen-flush edges get the outer gap, inner edges the inner gap, then the
/// border insets all four sides; nothing shrinks below 1px
// the tiling area: whatever the layer-shell arranger left over, else the
// whole output
pub fn tiling_area(state: &Rc<State>) -> Rect {
    let (sw, sh) = output_extent(state);
    let full = Rect::new_sized_saturating(0, 0, sw.max(1), sh.max(1));
    let usable = state.usable.get();
    if usable.is_empty() { full } else { usable.intersect(full) }
}

fn apply_gaps(r: Rect, area: Rect, cfg: &crate::config::Config) -> Rect {
    let left = if r.x1 <= area.x1 { cfg.gaps_out } else { cfg.gaps_in };
    let top = if r.y1 <= area.y1 { cfg.gaps_out } else { cfg.gaps_in };
    let right = if r.x2 >= area.x2 { cfg.gaps_out } else { cfg.gaps_in };
    let bottom = if r.y2 >= area.y2 { cfg.gaps_out } else { cfg.gaps_in };
    let x1 = r.x1 + left + cfg.border;
    let y1 = r.y1 + top + cfg.border;
    let x2 = (r.x2 - right - cfg.border).max(x1 + 1);
    let y2 = (r.y2 - bottom - cfg.border).max(y1 + 1);
    Rect { x1, y1, x2, y2 }
}

pub fn relayout(state: &Rc<State>, ws: &Workspace) {
    let (sw, sh) = output_extent(state);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let area = tiling_area(state);
    let cfg = state.config.borrow().clone();
    ws.tiling.recalculate(area);
    ws.tiling.for_each(|win| {
        let raw = win
            .node
            .borrow()
            .upgrade()
            .map(|n| n.rect.get())
            .unwrap_or_default();
        win.rect.set(apply_gaps(raw, area, &cfg));
        if !win.fullscreen.get() {
            win.configure_rect();
        }
    });
}

// -- hit testing --

/// deepest surface under the point; z order fullscreen, floats top-down, tiled
pub fn window_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
    let ws = active(state);
    let fs = ws.fullscreen.borrow().clone();
    let check_floats = |list: &Workspace| -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
        for win in list.floats.borrow().iter().rev() {
            if let Some(hit) = window_hit(state, win, x, y) {
                return Some(hit);
            }
        }
        None
    };
    if let Some(fs) = &fs {
        if state.config.borrow().float_above_fullscreen {
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

fn window_hit(
    state: &Rc<State>,
    win: &Rc<Window>,
    x: i32,
    y: i32,
) -> Option<(Rc<Window>, Rc<WlSurface>, i32, i32)> {
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

/// popups stack above parent, topmost last; positions relative to parent geometry
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
    xdg.for_each_popup(|p| {
        if hit.is_none() {
            hit = popup_hit(p, ox, oy, x, y);
        }
    });
    hit
}

// -- the full-scene hit test --

// layer surfaces join the z order: overlay, top, the windows, bottom,
// background. fullscreen hides top and everything below the windows.
pub fn surface_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    use crate::shell::layer;
    let fs_active = active(state).fullscreen.borrow().is_some();
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
        let mut hit = None;
        ls.for_each_popup(|p| {
            if hit.is_none() {
                hit = popup_hit(p, r.x1, r.y1, x, y);
            }
        });
        if hit.is_some() {
            return hit;
        }
        if let Some(h) = ls.surface.find_surface_at(x - r.x1, y - r.y1) {
            return Some(h);
        }
    }
    None
}
