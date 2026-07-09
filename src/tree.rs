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
    let r = ws
        .fullscreen
        .borrow()
        .as_ref()
        .map(|w| w.rect.get())
        .or_else(|| ws.tiling.first().map(|w| w.rect.get()))
        .or_else(|| ws.top_float().map(|w| w.rect.get()))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| workspace_output_rect(state, ws));
    let (cx, cy) = ((r.x1 + r.x2) / 2, (r.y1 + r.y2) / 2);
    if let Some(seat) = state.seat.borrow().clone() {
        seat.warp(state, cx as f64, cy as f64);
    }
    if let Some(d) = state.display.borrow().as_ref() {
        d.move_cursor(state, cx, cy);
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
    /// assigned box, gaps/border applied
    pub rect: Cell<Rect>,
    pub node: RefCell<Weak<dwindle::Node>>,
    pub floating: Cell<bool>,
    pub fullscreen: Cell<bool>,
    /// window-rule `immediate`: tearing without the client's async hint
    pub rule_immediate: Cell<bool>,
    /// window-rule `opacity`, multiplied into every sampled quad
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
            fullscreen_rect(state, self)
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
            let w = Workspace::default();
            w.output.set(state.focused_output.get());
            list.push(Rc::new(w));
        }
    }
    let prev_out = state.focused_output.get();
    state.active_ws.set(idx);
    let ws = active(state);
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(out) = d.outputs.borrow().get(ws.output.get()) {
            out.ws.set(idx);
            state.focused_output.set(ws.output.get());
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
    {
        let mut list = state.workspaces.borrow_mut();
        while list.len() <= n {
            let w = Workspace::default();
            w.output.set(state.focused_output.get());
            list.push(Rc::new(w));
        }
    }
    let target = state.workspaces.borrow()[n].clone();
    let area = tiling_area_for(state, &target);
    target
        .tiling
        .insert(&win, (area.x1 + area.x2) / 2, (area.y1 + area.y2) / 2);
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
    let ws = active(state);
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
    let area = tiling_area_for(state, ws);
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
    let ws = workspace_at(state, x, y);
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
pub fn surface_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    use crate::shell::layer;
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
