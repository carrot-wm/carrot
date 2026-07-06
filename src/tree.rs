// dwindle tree + workspaces + the floating stack.
// nodes have stable identity - no vec indices. z-order is
// fullscreen > floats > tiled (float_above_fullscreen swaps the first two).
// new windows split whatever is under the cursor, not the focused one.

pub mod dwindle;
pub mod float;
pub mod workspace;

use crate::rect::Rect;
use crate::shell::xdg::XdgToplevel;
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use workspace::Workspace;

/// gaps/borders; config keys later
pub const GAPS_IN: i32 = 6;
pub const GAPS_OUT: i32 = 12;
pub const BORDER: i32 = 2;
pub const FLOAT_ABOVE_FULLSCREEN: bool = false;

pub fn output_extent(state: &State) -> (i32, i32) {
    let (w, h) = state.output_size.get();
    (w as i32, h as i32)
}

pub enum WindowKind {
    Xdg(Rc<XdgToplevel>),
}

pub struct Window {
    pub kind: WindowKind,
    /// assigned box, gaps/border applied
    pub rect: Cell<Rect>,
    pub node: RefCell<Weak<dwindle::Node>>,
    pub floating: Cell<bool>,
    pub fullscreen: Cell<bool>,
}

impl Window {
    pub fn new(kind: WindowKind) -> Window {
        Window {
            kind,
            rect: Cell::new(Rect::default()),
            node: RefCell::new(Weak::new()),
            floating: Cell::new(false),
            fullscreen: Cell::new(false),
        }
    }

    pub fn xdg(&self) -> &Rc<XdgToplevel> {
        match &self.kind {
            WindowKind::Xdg(tl) => tl,
        }
    }

    pub fn surface(&self) -> Rc<WlSurface> {
        self.xdg().xdg.surface.clone()
    }

    pub fn geometry(&self) -> Rect {
        self.xdg().xdg.geometry()
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
        self.xdg().configure_size(r.width(), r.height());
    }

    pub fn send_close(&self) {
        self.xdg().send_close();
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

fn focus_window(state: &Rc<State>, win: Option<&Rc<Window>>) {
    // an exclusive layer surface owns the keyboard; windows wait
    if crate::shell::layer::kb_lock(state).is_some() {
        return;
    }
    let seat = state.seat.borrow().clone();
    if let Some(seat) = seat {
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
    let ws = active(state);
    // untile any fullscreen first; splitting behind it helps nobody
    let fs = ws.fullscreen.borrow().clone();
    if let Some(fs) = fs {
        set_fullscreen(state, &fs, false);
        fs.xdg().set_fullscreen_state(false);
    }
    let (cx, cy) = cursor_pos(state);
    ws.tiling.insert(win, cx, cy);
    relayout(state, &ws);
    if win.xdg().wants_fullscreen() {
        set_fullscreen(state, win, true);
    }
    focus_window(state, Some(win));
    state.damage.trigger();
}

pub fn unmap_window(state: &Rc<State>, win: &Rc<Window>) {
    let ws = active(state);
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
        win.xdg().configure_size(w, h);
    } else {
        let mut slot = ws.fullscreen.borrow_mut();
        if slot.as_ref().is_some_and(|w| Rc::ptr_eq(w, win)) {
            *slot = None;
        }
        win.fullscreen.set(false);
        win.configure_rect();
    }
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

fn apply_gaps(r: Rect, area: Rect) -> Rect {
    let left = if r.x1 <= area.x1 { GAPS_OUT } else { GAPS_IN };
    let top = if r.y1 <= area.y1 { GAPS_OUT } else { GAPS_IN };
    let right = if r.x2 >= area.x2 { GAPS_OUT } else { GAPS_IN };
    let bottom = if r.y2 >= area.y2 { GAPS_OUT } else { GAPS_IN };
    let x1 = r.x1 + left + BORDER;
    let y1 = r.y1 + top + BORDER;
    let x2 = (r.x2 - right - BORDER).max(x1 + 1);
    let y2 = (r.y2 - bottom - BORDER).max(y1 + 1);
    Rect { x1, y1, x2, y2 }
}

pub fn relayout(state: &Rc<State>, ws: &Workspace) {
    let (sw, sh) = output_extent(state);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let area = tiling_area(state);
    ws.tiling.recalculate(area);
    ws.tiling.for_each(|win| {
        let raw = win
            .node
            .borrow()
            .upgrade()
            .map(|n| n.rect.get())
            .unwrap_or_default();
        win.rect.set(apply_gaps(raw, area));
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
        if FLOAT_ABOVE_FULLSCREEN {
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
    if let Some(hit) = popups_hit(&win.xdg().xdg, rect.x1, rect.y1, x, y) {
        return Some((win.clone(), hit.0, hit.1, hit.2));
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
