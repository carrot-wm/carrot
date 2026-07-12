// the scrolling layout: an endless horizontal strip of columns, each a
// vertical stack of tiles; a single view offset scrolls the strip.

use super::Window;
use crate::config::{CenterFocus, ColWidthCfg, Dir, ScrollCfg};
use crate::rect::Rect;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ColWidth {
    Prop(f64),
    Fixed(i32),
}

pub struct Column {
    pub tiles: Vec<Rc<Window>>,
    pub active_tile: usize,
    pub width: ColWidth,
    pub preset_idx: Option<usize>,
    pub full_width: bool,
    pub weights: Vec<f64>,
}

#[derive(Default)]
pub struct Strip {
    cols: RefCell<Vec<Column>>,
    active: Cell<usize>,
    /// previously active column; on-overflow centering keys off it
    prev_active: Cell<usize>,
    /// target view offset in strip px; 0 = column 0 at the area's left edge
    view: Cell<f64>,
    pub view_anim: RefCell<Option<crate::anim::Anim>>,
    /// set when a fresh column opens; closing it restores (column, view)
    restore: Cell<Option<(usize, f64)>>,
    last_area: Cell<Rect>,
}

fn default_width(cfg: &ScrollCfg) -> (ColWidth, Option<usize>) {
    match cfg.default_width {
        ColWidthCfg::Prop(p) => {
            let preset = cfg.preset_widths.iter().position(|w| (w - p).abs() < 1e-9);
            (ColWidth::Prop(p), preset)
        }
        ColWidthCfg::FixedPx(px) => (ColWidth::Fixed(px), None),
    }
}

impl Strip {
    pub fn is_empty(&self) -> bool {
        self.cols.borrow().is_empty()
    }

    pub fn col_count(&self) -> usize {
        self.cols.borrow().len()
    }

    pub fn view_px(&self) -> f64 {
        self.view.get()
    }

    pub fn for_each(&self, mut f: impl FnMut(&Rc<Window>)) {
        for c in self.cols.borrow().iter() {
            for t in &c.tiles {
                f(t);
            }
        }
    }

    /// the active column's active tile, else the first tile anywhere
    pub fn first(&self) -> Option<Rc<Window>> {
        let cols = self.cols.borrow();
        if let Some(c) = cols.get(self.active.get()) {
            return c.tiles.get(c.active_tile).or_else(|| c.tiles.first()).cloned();
        }
        cols.first().and_then(|c| c.tiles.first().cloned())
    }

    pub fn window_at(&self, x: i32, y: i32) -> Option<Rc<Window>> {
        let cols = self.cols.borrow();
        for c in cols.iter() {
            for t in &c.tiles {
                if t.rect.get().contains(x, y) {
                    return Some(t.clone());
                }
            }
        }
        None
    }

    fn locate(&self, win: &Window) -> Option<(usize, usize)> {
        let cols = self.cols.borrow();
        for (ci, c) in cols.iter().enumerate() {
            for (ti, t) in c.tiles.iter().enumerate() {
                if std::ptr::eq(&**t, win) {
                    return Some((ci, ti));
                }
            }
        }
        None
    }

    /// a new window opens in its own column right of the active one
    pub fn insert(&self, win: &Rc<Window>, cfg: &ScrollCfg) {
        let mut cols = self.cols.borrow_mut();
        let (width, preset_idx) = default_width(cfg);
        let col = Column {
            tiles: vec![win.clone()],
            active_tile: 0,
            width,
            preset_idx,
            full_width: false,
            weights: vec![1.0],
        };
        if cols.is_empty() {
            cols.push(col);
            self.active.set(0);
            self.prev_active.set(0);
            self.restore.set(None);
            return;
        }
        let at = (self.active.get() + 1).min(cols.len());
        cols.insert(at, col);
        self.restore.set(Some((self.active.get(), self.view.get())));
        self.prev_active.set(self.active.get());
        self.active.set(at);
    }

    /// conversion path: append as its own column, no focus/view bookkeeping
    pub fn insert_ordered(&self, win: &Rc<Window>, cfg: &ScrollCfg) {
        let mut cols = self.cols.borrow_mut();
        let (width, preset_idx) = default_width(cfg);
        cols.push(Column {
            tiles: vec![win.clone()],
            active_tile: 0,
            width,
            preset_idx,
            full_width: false,
            weights: vec![1.0],
        });
    }

    pub fn remove(&self, win: &Window) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let single = cols[ci].tiles.len() == 1;
        if !single {
            let c = &mut cols[ci];
            c.tiles.remove(ti);
            c.weights.remove(ti);
            if c.active_tile >= c.tiles.len() {
                c.active_tile = c.tiles.len() - 1;
            }
            if c.tiles.len() == 1 {
                c.weights[0] = 1.0;
            }
            return true;
        }
        cols.remove(ci);
        let active = self.active.get();
        if ci < active {
            self.active.set(active - 1);
        } else if ci == active {
            // closing the fresh column goes back where we were
            match self.restore.take() {
                Some((prev, view)) if prev < cols.len() => {
                    self.active.set(prev);
                    self.view.set(view);
                }
                _ => self.active.set(active.min(cols.len().saturating_sub(1))),
            }
        }
        if self.prev_active.get() >= cols.len() {
            self.prev_active.set(0);
        }
        true
    }

    pub fn take_all(&self) -> Vec<Rc<Window>> {
        let mut out = Vec::new();
        for c in self.cols.borrow_mut().drain(..) {
            out.extend(c.tiles);
        }
        self.active.set(0);
        self.prev_active.set(0);
        self.view.set(0.0);
        self.restore.set(None);
        *self.view_anim.borrow_mut() = None;
        out
    }

    // -- geometry --

    fn col_w(c: &Column, area_w: i32) -> i32 {
        if c.full_width {
            return area_w;
        }
        match c.width {
            ColWidth::Prop(p) => ((p * area_w as f64).round() as i32).clamp(1, area_w),
            ColWidth::Fixed(px) => px.clamp(1, area_w),
        }
    }

    fn extents(&self, area_w: i32) -> Vec<(i32, i32)> {
        let cols = self.cols.borrow();
        let mut xs = Vec::with_capacity(cols.len());
        let mut x = 0i32;
        for c in cols.iter() {
            let w = Self::col_w(c, area_w);
            xs.push((x, w));
            x += w;
        }
        xs
    }

    /// raw edge-to-edge rects at the target view; updates the view per
    /// keep-in-view / centering and remembers the area for conversions
    pub fn layout(&self, area: Rect, cfg: &ScrollCfg) -> Vec<(Rc<Window>, Rect)> {
        self.last_area.set(area);
        let cols = self.cols.borrow();
        if cols.is_empty() {
            return Vec::new();
        }
        let aw = area.width();
        drop(cols);
        let xs = self.extents(aw);
        let cols = self.cols.borrow();
        let active = self.active.get().min(xs.len() - 1);
        let (ax, acw) = xs[active];
        self.view.set(self.keep_in_view(&xs, ax as f64, acw as f64, aw as f64, cfg.center_focus));
        let view = self.view.get();
        let mut out = Vec::new();
        for (c, (cx, cw)) in cols.iter().zip(xs.iter()) {
            let x1 = area.x1 + (*cx as f64 - view).round() as i32;
            let total: f64 = c.weights.iter().sum::<f64>().max(1e-9);
            let mut y = area.y1 as f64;
            for (i, t) in c.tiles.iter().enumerate() {
                let h = area.height() as f64 * c.weights[i] / total;
                let y1 = y.round() as i32;
                let y2 = if i + 1 == c.tiles.len() {
                    area.y2
                } else {
                    (y + h).round() as i32
                };
                out.push((t.clone(), Rect { x1, y1, x2: x1 + cw, y2 }));
                y += h;
            }
        }
        out
    }

    // the view only scrolls when the active column would clip, and it
    // scrolls the minimum amount; wider-than-view columns left-align
    fn keep_in_view(&self, xs: &[(i32, i32)], col_x: f64, col_w: f64, area_w: f64, mode: CenterFocus) -> f64 {
        let cur = self.view.get();
        if col_w >= area_w {
            return col_x;
        }
        if mode == CenterFocus::Always {
            return col_x - (area_w - col_w) / 2.0;
        }
        let (lo, hi) = (col_x + col_w - area_w, col_x);
        if cur >= lo && cur <= hi {
            return cur;
        }
        if mode == CenterFocus::OnOverflow {
            // fit together with the column we came from when possible
            let (px, pw) = xs[self.prev_active.get().min(xs.len() - 1)];
            let union_lo = (px as f64).min(col_x);
            let union_hi = (px as f64 + pw as f64).max(col_x + col_w);
            if union_hi - union_lo > area_w {
                return col_x - (area_w - col_w) / 2.0;
            }
        }
        cur.clamp(lo, hi)
    }

    pub fn center_active(&self, area: Rect) {
        let xs = self.extents(area.width());
        if xs.is_empty() {
            return;
        }
        let (ax, aw_col) = xs[self.active.get().min(xs.len() - 1)];
        self.view
            .set(ax as f64 - (area.width() as f64 - aw_col as f64) / 2.0);
    }

    // -- focus --

    /// sync active column/tile to wherever focus actually landed
    pub fn note_focus(&self, win: &Window) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        if ci != self.active.get() {
            self.prev_active.set(self.active.get());
            self.restore.set(None);
            self.active.set(ci);
        }
        self.cols.borrow_mut()[ci].active_tile = ti;
        true
    }

    pub fn focus_dir(&self, dir: Dir) -> Option<Rc<Window>> {
        let mut cols = self.cols.borrow_mut();
        if cols.is_empty() {
            return None;
        }
        let active = self.active.get().min(cols.len() - 1);
        match dir {
            Dir::Left | Dir::Right => {
                let next = if dir == Dir::Left {
                    active.checked_sub(1)?
                } else if active + 1 < cols.len() {
                    active + 1
                } else {
                    return None;
                };
                self.prev_active.set(active);
                self.restore.set(None);
                self.active.set(next);
                let c = &cols[next];
                c.tiles.get(c.active_tile).or_else(|| c.tiles.first()).cloned()
            }
            Dir::Up | Dir::Down => {
                let c = &mut cols[active];
                let next = if dir == Dir::Up {
                    c.active_tile.checked_sub(1)?
                } else if c.active_tile + 1 < c.tiles.len() {
                    c.active_tile + 1
                } else {
                    return None;
                };
                c.active_tile = next;
                c.tiles.get(next).cloned()
            }
        }
    }

    pub fn swap_dir(&self, win: &Window, dir: Dir) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        match dir {
            Dir::Left | Dir::Right => {
                let other = if dir == Dir::Left {
                    match ci.checked_sub(1) {
                        Some(o) => o,
                        None => return false,
                    }
                } else if ci + 1 < cols.len() {
                    ci + 1
                } else {
                    return false;
                };
                cols.swap(ci, other);
                if self.active.get() == ci {
                    self.active.set(other);
                } else if self.active.get() == other {
                    self.active.set(ci);
                }
                true
            }
            Dir::Up | Dir::Down => {
                let c = &mut cols[ci];
                let other = if dir == Dir::Up {
                    match ti.checked_sub(1) {
                        Some(o) => o,
                        None => return false,
                    }
                } else if ti + 1 < c.tiles.len() {
                    ti + 1
                } else {
                    return false;
                };
                c.tiles.swap(ti, other);
                c.weights.swap(ti, other);
                if c.active_tile == ti {
                    c.active_tile = other;
                } else if c.active_tile == other {
                    c.active_tile = ti;
                }
                true
            }
        }
    }

    // -- column verbs --

    /// a lone tile joins the neighbor column; a stacked tile breaks out
    /// into its own column on that side
    pub fn consume_or_expel(&self, win: &Window, left: bool) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        if cols[ci].tiles.len() == 1 {
            // consume into the adjacent column
            let target = if left {
                match ci.checked_sub(1) {
                    Some(t) => t,
                    None => return false,
                }
            } else if ci + 1 < cols.len() {
                ci + 1
            } else {
                return false;
            };
            let col = cols.remove(ci);
            let target = if target > ci { target - 1 } else { target };
            let t = &mut cols[target];
            t.tiles.extend(col.tiles);
            t.weights.push(1.0);
            t.active_tile = t.tiles.len() - 1;
            self.active.set(target);
            self.restore.set(None);
            true
        } else {
            // expel into a fresh column beside this one
            let c = &mut cols[ci];
            let tile = c.tiles.remove(ti);
            c.weights.remove(ti);
            if c.active_tile >= c.tiles.len() {
                c.active_tile = c.tiles.len() - 1;
            }
            if c.tiles.len() == 1 {
                c.weights[0] = 1.0;
            }
            let width = c.width;
            let preset_idx = c.preset_idx;
            let at = if left { ci } else { ci + 1 };
            cols.insert(
                at,
                Column {
                    tiles: vec![tile],
                    active_tile: 0,
                    width,
                    preset_idx,
                    full_width: false,
                    weights: vec![1.0],
                },
            );
            self.active.set(at);
            self.restore.set(None);
            true
        }
    }

    pub fn cycle_width(&self, win: &Window, cfg: &ScrollCfg, back: bool) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        if cfg.preset_widths.is_empty() {
            return false;
        }
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        let n = cfg.preset_widths.len();
        let next = match c.preset_idx {
            Some(i) => {
                if back {
                    (i + n - 1) % n
                } else {
                    (i + 1) % n
                }
            }
            None => {
                // snap onto the ladder relative to the current width
                let area_w = self.last_area.get().width().max(1);
                let cur = match c.width {
                    ColWidth::Prop(p) => p,
                    ColWidth::Fixed(px) => px as f64 / area_w as f64,
                };
                let eps = 1.0 / area_w as f64; // fractional-scaling allowance
                if back {
                    cfg.preset_widths
                        .iter()
                        .rposition(|w| *w < cur - eps)
                        .unwrap_or(n - 1)
                } else {
                    cfg.preset_widths
                        .iter()
                        .position(|w| *w > cur + eps)
                        .unwrap_or(0)
                }
            }
        };
        c.width = ColWidth::Prop(cfg.preset_widths[next]);
        c.preset_idx = Some(next);
        c.full_width = false;
        true
    }

    pub fn toggle_full_width(&self, win: &Window) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        c.full_width = !c.full_width;
        true
    }

    /// signed proportion delta on the window's column
    pub fn adjust_width(&self, win: &Window, delta: f64) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let area_w = self.last_area.get().width().max(1);
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        let cur = match c.width {
            ColWidth::Prop(p) => p,
            ColWidth::Fixed(px) => px as f64 / area_w as f64,
        };
        c.width = ColWidth::Prop((cur + delta).clamp(0.05, 1.0));
        c.preset_idx = None;
        c.full_width = false;
        true
    }

    /// interactive drag: horizontal edges pin the column width in px,
    /// vertical edges shift weight between the tile and its edge neighbor
    pub fn resize_by_edges(&self, win: &Window, edges: u32, dx: f64, dy: f64) -> bool {
        use super::dwindle::{EDGE_BOTTOM as BOTTOM, EDGE_LEFT as LEFT, EDGE_RIGHT as RIGHT, EDGE_TOP as TOP};
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let mut hit = false;
        let area = self.last_area.get();
        let mut cols = self.cols.borrow_mut();
        if edges & (LEFT | RIGHT) != 0 && dx != 0.0 {
            let c = &mut cols[ci];
            let cur = Self::col_w(c, area.width().max(1));
            let grow = if edges & RIGHT != 0 { dx } else { -dx };
            c.width = ColWidth::Fixed(((cur as f64 + grow).round() as i32).max(50));
            c.preset_idx = None;
            c.full_width = false;
            hit = true;
        }
        if edges & (TOP | BOTTOM) != 0 && dy != 0.0 {
            let c = &mut cols[ci];
            let other = if edges & BOTTOM != 0 { ti + 1 } else { ti.wrapping_sub(1) };
            if other < c.tiles.len() {
                let total: f64 = c.weights.iter().sum::<f64>().max(1e-9);
                let shift = dy / area.height().max(1) as f64 * total;
                let shift = if edges & BOTTOM != 0 { shift } else { -shift };
                let (a, b) = (c.weights[ti] + shift, c.weights[other] - shift);
                if a > 0.05 && b > 0.05 {
                    c.weights[ti] = a;
                    c.weights[other] = b;
                    hit = true;
                }
            }
        }
        hit
    }

    // -- view animation --

    /// called after layout() moved the target; keeps the glass continuous
    pub fn animate_view(&self, state: &crate::state::State, old_view: f64) {
        let new_view = self.view.get();
        if (new_view - old_view).abs() < 0.5 {
            return;
        }
        let cfg = state.config.borrow().clone();
        let Some(motion) = cfg.animations.motion(crate::config::AnimKind::ViewMovement) else {
            *self.view_anim.borrow_mut() = None;
            return;
        };
        state.anim_clock.touch();
        let now = state.anim_clock.now();
        let mut slot = self.view_anim.borrow_mut();
        let (from, vel) = match &*slot {
            Some(a) if !a.is_done(now) => (a.value(now), a.velocity(now)),
            _ => (old_view, 0.0),
        };
        *slot = Some(crate::config::build_anim(
            &state.anim_clock,
            motion,
            &cfg.animations,
            from,
            new_view,
            vel,
        ));
    }

    /// px the drawn strip lags the laid-out target this frame
    pub fn draw_offset_px(&self, now: u64) -> f64 {
        match &*self.view_anim.borrow() {
            Some(a) if !a.is_done(now) => self.view.get() - a.value(now),
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CenterFocus, ScrollCfg};

    fn setup(n: usize) -> (Rc<crate::state::State>, Vec<Rc<crate::tree::Window>>) {
        let (state, client) = crate::client::test_utils::test_client();
        let base = crate::shell::xdg::tests::mk_base(&client, 300);
        let wins = (0..n as u32)
            .map(|i| {
                let (_s, _x, tl) = crate::shell::xdg::tests::mk_toplevel(
                    &client,
                    &base,
                    301 + i * 3,
                    302 + i * 3,
                    303 + i * 3,
                );
                Rc::new(crate::tree::Window::new(&state, crate::tree::WindowKind::Xdg(tl)))
            })
            .collect();
        (state, wins)
    }

    fn area() -> Rect {
        Rect { x1: 0, y1: 0, x2: 1000, y2: 600 }
    }

    fn cfg() -> ScrollCfg {
        ScrollCfg::default()
    }

    #[test]
    fn insert_opens_right_of_active_and_layout_tiles() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        let rects = s.layout(area(), &cfg());
        assert_eq!(rects.len(), 2);
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        let r1 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[1])).unwrap().1;
        assert_eq!(r0.width(), 500);
        assert_eq!(r1.x1, r0.x2, "columns are edge to edge");
        assert_eq!(s.view_px(), 0.0);
        // a third column overflows: keep-in-view scrolls the minimum
        s.insert(&w[2], &cfg());
        let rects = s.layout(area(), &cfg());
        let r2 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[2])).unwrap().1;
        assert_eq!(r2.x2, 1000, "active column snaps to the near edge");
        assert_eq!(s.view_px(), 500.0);
    }

    #[test]
    fn close_fresh_column_restores_view_and_focus() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        for win in &w {
            s.insert(win, &cfg());
        }
        s.layout(area(), &cfg());
        let v_before = s.view_px();
        assert!(s.remove(&w[2]));
        s.layout(area(), &cfg());
        assert_eq!(s.view_px(), 0.0);
        assert!(Rc::ptr_eq(&s.first().unwrap(), &w[1]));
        assert_ne!(v_before, 0.0);
    }

    #[test]
    fn consume_and_expel_roundtrip() {
        let (_st, w) = setup(2);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        assert!(s.consume_or_expel(&w[1], true));
        assert_eq!(s.col_count(), 1);
        let rects = s.layout(area(), &cfg());
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        let r1 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[1])).unwrap().1;
        assert_eq!(r0.x1, r1.x1, "stacked in one column");
        assert_eq!(r0.height(), r1.height(), "equal weights");
        assert!(s.consume_or_expel(&w[1], false));
        assert_eq!(s.col_count(), 2);
    }

    #[test]
    fn width_cycling_and_full_width() {
        let (_st, w) = setup(1);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        assert!(s.cycle_width(&w[0], &cfg(), false));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 667);
        assert!(s.cycle_width(&w[0], &cfg(), false));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 333);
        assert!(s.toggle_full_width(&w[0]));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 1000);
        assert!(s.toggle_full_width(&w[0]));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 333);
    }

    #[test]
    fn center_modes() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        let mut c = cfg();
        c.center_focus = CenterFocus::Always;
        for win in &w {
            s.insert(win, &c);
        }
        let r2 = s
            .layout(area(), &c)
            .iter()
            .find(|(win, _)| Rc::ptr_eq(win, &w[2]))
            .unwrap()
            .1;
        assert_eq!((r2.x1, r2.x2), (250, 750));
    }

    #[test]
    fn focus_and_swap_follow_columns() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        assert!(s.consume_or_expel(&w[1], true));
        s.insert(&w[2], &cfg());
        let f = s.focus_dir(Dir::Left).unwrap();
        assert!(Rc::ptr_eq(&f, &w[1]), "remembered active tile");
        let f = s.focus_dir(Dir::Up).unwrap();
        assert!(Rc::ptr_eq(&f, &w[0]));
        assert!(s.focus_dir(Dir::Up).is_none(), "saturates");
        assert!(s.swap_dir(&w[0], Dir::Right));
        let rects = s.layout(area(), &cfg());
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        let r2 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[2])).unwrap().1;
        assert!(r2.x1 < r0.x1, "columns exchanged");
    }
}
