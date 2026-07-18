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
            if ti < c.active_tile {
                c.active_tile -= 1;
            }
            if c.active_tile >= c.tiles.len() {
                c.active_tile = c.tiles.len() - 1;
            }
            if c.tiles.len() == 1 {
                c.weights[0] = 1.0;
            }
            return true;
        }
        let area_w = self.last_area.get().width().max(1);
        let removed_x: i32 = cols[..ci].iter().map(|c| Self::col_w(c, area_w)).sum();
        let removed_w = Self::col_w(&cols[ci], area_w);
        cols.remove(ci);
        let active = self.active.get();
        if ci < active {
            self.active.set(active - 1);
            // keep the pending restore aimed at the column it meant; the
            // stored view only shifts if it had scrolled past the dead one
            match self.restore.get() {
                Some((prev, view)) if ci < prev => {
                    let view = if view > removed_x as f64 {
                        (view - removed_w as f64).max(removed_x as f64)
                    } else {
                        view
                    };
                    self.restore.set(Some((prev - 1, view)));
                }
                Some((prev, _)) if ci == prev => self.restore.set(None),
                _ => {}
            }
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
        let pa = self.prev_active.get();
        if ci < pa {
            self.prev_active.set(pa - 1);
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
        // wider than the view is fine: the strip scrolls across the column
        // and keep-in-view pins to its leading edge
        match c.width {
            ColWidth::Prop(p) => ((p * area_w as f64).round() as i32).max(1),
            ColWidth::Fixed(px) => px.max(1),
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
        self.view.set(self.keep_in_view(&xs, ax as f64, acw as f64, aw as f64, cfg));
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
    fn keep_in_view(&self, xs: &[(i32, i32)], col_x: f64, col_w: f64, area_w: f64, cfg: &ScrollCfg) -> f64 {
        let mode = cfg.center_focus;
        let cur = self.view.get();
        if col_w >= area_w {
            // over-wide: any pan across the column survives relayout;
            // arriving from elsewhere clamps to the nearer edge
            return cur.clamp(col_x, col_x + col_w - area_w);
        }
        if mode == CenterFocus::Always || (xs.len() == 1 && cfg.center_single) {
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

    /// sync active column/tile to wherever focus actually landed; true
    /// when the active column moved and the view may need to follow
    pub fn note_focus(&self, win: &Window) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let moved = ci != self.active.get();
        if moved {
            self.prev_active.set(self.active.get());
            self.restore.set(None);
            self.active.set(ci);
        }
        self.cols.borrow_mut()[ci].active_tile = ti;
        moved
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

    /// exchange two tiles wherever they sit; slots keep their weights
    pub fn swap_tiles(&self, a: &Window, b: &Window) -> bool {
        let (Some((ca, ta)), Some((cb, tb))) = (self.locate(a), self.locate(b)) else {
            return false;
        };
        if ca == cb {
            if ta == tb {
                return false;
            }
            self.cols.borrow_mut()[ca].tiles.swap(ta, tb);
            return true;
        }
        let mut cols = self.cols.borrow_mut();
        let wa = cols[ca].tiles[ta].clone();
        let wb = cols[cb].tiles[tb].clone();
        cols[ca].tiles[ta] = wb;
        cols[cb].tiles[tb] = wa;
        true
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
                // trade the tile against the neighbor column's active one;
                // slots keep their width and weight, the windows cross.
                // move-column is the verb that carries a column wholesale
                let oti = cols[other].active_tile.min(cols[other].tiles.len() - 1);
                let a = cols[ci].tiles[ti].clone();
                cols[ci].tiles[ti] = cols[other].tiles[oti].clone();
                cols[other].tiles[oti] = a;
                cols[other].active_tile = oti;
                if self.active.get() == ci {
                    self.prev_active.set(ci);
                    self.restore.set(None);
                    self.active.set(other);
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

    /// the window's whole column leapfrogs one slot along the strip
    pub fn move_column(&self, win: &Window, left: bool) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let other = if left {
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
        self.restore.set(None);
        true
    }

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
            if ti < c.active_tile {
                c.active_tile -= 1;
            }
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

    /// slide the view across an over-wide active column; keep-in-view
    /// clamps rather than pins, so the pan holds until focus moves on
    pub fn pan(&self, back: bool) -> bool {
        let area_w = self.last_area.get().width();
        if area_w <= 0 {
            return false;
        }
        let xs = self.extents(area_w);
        if xs.is_empty() {
            return false;
        }
        let (cx, cw) = xs[self.active.get().min(xs.len() - 1)];
        if cw <= area_w {
            return false;
        }
        let (lo, hi) = (cx as f64, (cx + cw - area_w) as f64);
        let step = area_w as f64 / 2.0 * if back { -1.0 } else { 1.0 };
        let new = (self.view.get() + step).clamp(lo, hi);
        if (new - self.view.get()).abs() < 1.0 {
            return false;
        }
        self.view.set(new);
        true
    }

    /// jump focus to the strip's first or last column
    pub fn focus_edge(&self, last: bool) -> Option<Rc<Window>> {
        let cols = self.cols.borrow();
        if cols.is_empty() {
            return None;
        }
        let target = if last { cols.len() - 1 } else { 0 };
        let active = self.active.get().min(cols.len() - 1);
        if target == active {
            return None;
        }
        self.prev_active.set(active);
        self.restore.set(None);
        self.active.set(target);
        let c = &cols[target];
        c.tiles.get(c.active_tile).or_else(|| c.tiles.first()).cloned()
    }

    /// carry the window's whole column to the strip's first or last slot
    pub fn move_column_to_edge(&self, win: &Window, last: bool) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let target = if last { cols.len() - 1 } else { 0 };
        if ci == target {
            return false;
        }
        let col = cols.remove(ci);
        cols.insert(target, col);
        self.active.set(target);
        self.restore.set(None);
        true
    }

    /// pull the first window of the column to the right into the bottom
    /// of this one
    pub fn consume_into(&self, win: &Window) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        if ci + 1 >= cols.len() {
            return false;
        }
        let (tile, emptied) = {
            let src = &mut cols[ci + 1];
            let t = src.tiles.remove(0);
            src.weights.remove(0);
            src.active_tile = src.active_tile.saturating_sub(1);
            if src.tiles.len() == 1 {
                src.weights[0] = 1.0;
            }
            (t, src.tiles.is_empty())
        };
        if emptied {
            cols.remove(ci + 1);
        }
        let c = &mut cols[ci];
        c.tiles.push(tile);
        c.weights.push(1.0);
        self.active.set(ci);
        self.restore.set(None);
        true
    }

    /// push this column's bottom window into a fresh column on its right;
    /// focus follows only when the bottom window was the focused one
    pub fn expel_from(&self, win: &Window) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        if c.tiles.len() < 2 {
            return false;
        }
        let was_bottom = ti == c.tiles.len() - 1;
        let tile = c.tiles.pop().unwrap();
        c.weights.pop();
        if c.active_tile >= c.tiles.len() {
            c.active_tile = c.tiles.len() - 1;
        }
        if c.tiles.len() == 1 {
            c.weights[0] = 1.0;
        }
        let (width, preset_idx) = (c.width, c.preset_idx);
        cols.insert(
            ci + 1,
            Column {
                tiles: vec![tile],
                active_tile: 0,
                width,
                preset_idx,
                full_width: false,
                weights: vec![1.0],
            },
        );
        self.active.set(if was_bottom { ci + 1 } else { ci });
        self.restore.set(None);
        true
    }

    /// grow the active column into the view space the other fully visible
    /// columns leave unused; a lone column just fills the view
    pub fn expand_width(&self, win: &Window) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let area_w = self.last_area.get().width();
        if area_w <= 0 {
            return false;
        }
        let xs = self.extents(area_w);
        let view = self.view.get();
        let mut visible = 0i64;
        let mut ours = 0i32;
        for (i, (x, w)) in xs.iter().enumerate() {
            if *x as f64 >= view - 0.5 && (*x + *w) as f64 <= view + area_w as f64 + 0.5 {
                visible += *w as i64;
                if i == ci {
                    ours = *w;
                }
            }
        }
        if ours == 0 {
            return false;
        }
        let leftover = area_w as i64 - visible;
        if leftover <= 0 {
            return false;
        }
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        c.width = ColWidth::Fixed(ours + leftover as i32);
        c.preset_idx = None;
        c.full_width = false;
        true
    }

    /// the strip's notion of the focused window, for refocus after ops
    /// that shuffle columns
    pub fn active_window(&self) -> Option<Rc<Window>> {
        let cols = self.cols.borrow();
        let c = cols.get(self.active.get().min(cols.len().checked_sub(1)?))?;
        c.tiles.get(c.active_tile).or_else(|| c.tiles.first()).cloned()
    }

    /// walk a tile's height share along the preset ladder: it takes that
    /// fraction of the column and the rest keep their relative shares
    pub fn cycle_tile_height(&self, win: &Window, cfg: &ScrollCfg, back: bool) -> bool {
        let Some((ci, ti)) = self.locate(win) else {
            return false;
        };
        if cfg.preset_heights.is_empty() {
            return false;
        }
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        if c.tiles.len() < 2 {
            return false;
        }
        let total: f64 = c.weights.iter().sum::<f64>().max(1e-9);
        let cur = c.weights[ti] / total;
        let eps = 0.01;
        let n = cfg.preset_heights.len();
        let next = if back {
            cfg.preset_heights.iter().rposition(|h| *h < cur - eps).unwrap_or(n - 1)
        } else {
            cfg.preset_heights.iter().position(|h| *h > cur + eps).unwrap_or(0)
        };
        let p = cfg.preset_heights[next].clamp(0.05, 0.95);
        let others = total - c.weights[ti];
        c.weights[ti] = p / (1.0 - p) * others.max(1e-9);
        true
    }

    /// every tile in the window's column back to an equal share
    pub fn reset_tile_heights(&self, win: &Window) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            return false;
        };
        let mut cols = self.cols.borrow_mut();
        let c = &mut cols[ci];
        if c.tiles.len() < 2 {
            return false;
        }
        for w in c.weights.iter_mut() {
            *w = 1.0;
        }
        true
    }

    pub fn cycle_width(&self, win: &Window, cfg: &ScrollCfg, back: bool) -> bool {
        let Some((ci, _)) = self.locate(win) else {
            crate::trace!("cycle-width: #{} not in this strip", win.ident);
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
            // the target sat still, but a live glide may head somewhere
            // the strip no longer goes (mid-anim close restore, pan,
            // centering write straight to view); pull it back here
            let now = state.anim_clock.now();
            let stale = matches!(
                &*self.view_anim.borrow(),
                Some(a) if !a.is_done(now) && (a.to() - new_view).abs() >= 0.5
            );
            if !stale {
                return;
            }
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
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 1000);
        assert!(s.cycle_width(&w[0], &cfg(), false));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 333);
        assert!(s.toggle_full_width(&w[0]));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 1000);
        assert!(s.toggle_full_width(&w[0]));
        assert_eq!(s.layout(area(), &cfg())[0].1.width(), 333);
    }

    #[test]
    fn a_column_wider_than_the_view_scrolls_instead_of_clamping() {
        let (_st, w) = setup(1);
        let s = Strip::default();
        let mut c = cfg();
        c.preset_widths = vec![1.5];
        s.insert(&w[0], &c);
        assert!(s.cycle_width(&w[0], &c, false));
        let r = s.layout(area(), &c)[0].1;
        // half a view wider than the output, pinned to its leading edge
        assert_eq!(r.width(), 1500);
        assert_eq!(r.x1, area().x1);
    }

    #[test]
    fn panning_reveals_the_far_side_of_an_over_wide_column_and_sticks() {
        let (_st, w) = setup(1);
        let s = Strip::default();
        let mut c = cfg();
        c.preset_widths = vec![2.0];
        s.insert(&w[0], &c);
        assert!(s.cycle_width(&w[0], &c, false));
        s.layout(area(), &c);
        // half-view steps, clamped at the column's trailing edge
        assert!(s.pan(false));
        assert_eq!(s.layout(area(), &c)[0].1.x1, area().x1 - 500);
        assert!(s.pan(false));
        assert_eq!(s.layout(area(), &c)[0].1.x1, area().x1 - 1000);
        assert!(!s.pan(false), "already at the trailing edge");
        assert!(s.pan(true));
        // a fitting column never pans
        let mut fit = c.clone();
        fit.preset_widths = vec![0.5];
        assert!(s.cycle_width(&w[0], &fit, false));
        s.layout(area(), &fit);
        assert!(!s.pan(false));
    }

    #[test]
    fn edge_jumps_and_column_carries() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        for win in &w {
            s.insert(win, &cfg());
        }
        let first = s.focus_edge(false).unwrap();
        assert!(Rc::ptr_eq(&first, &w[0]));
        assert!(s.focus_edge(false).is_none(), "already first");
        let last = s.focus_edge(true).unwrap();
        assert!(Rc::ptr_eq(&last, &w[2]));
        // carry the first column to the end of the strip
        assert!(s.move_column_to_edge(&w[0], true));
        let order: Vec<_> = s.layout(area(), &cfg()).iter().map(|(win, _)| win.clone()).collect();
        assert!(Rc::ptr_eq(&order[2], &w[0]));
    }

    #[test]
    fn consume_pulls_right_and_expel_pushes_right() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        for win in &w {
            s.insert(win, &cfg());
        }
        s.note_focus(&w[0]);
        // w1's column drains into w0's
        assert!(s.consume_into(&w[0]));
        assert_eq!(s.col_count(), 2);
        assert!(s.consume_into(&w[0]));
        assert_eq!(s.col_count(), 1);
        assert!(!s.consume_into(&w[0]), "nothing right of the last column");
        // bottom tile leaves into its own column; focus stays on w0
        assert!(s.expel_from(&w[0]));
        assert_eq!(s.col_count(), 2);
        assert!(Rc::ptr_eq(&s.active_window().unwrap(), &w[0]));
    }

    #[test]
    fn expand_takes_the_leftover_and_heights_cycle() {
        let (_st, w) = setup(2);
        let s = Strip::default();
        let c = cfg();
        for win in &w {
            s.insert(win, &c);
        }
        s.layout(area(), &c);
        // two half-width columns leave nothing; shrink one and expand it
        let mut narrow = c.clone();
        narrow.preset_widths = vec![0.25];
        assert!(s.cycle_width(&w[1], &narrow, false));
        s.layout(area(), &c);
        assert!(s.expand_width(&w[1]));
        let xs: i32 = s.layout(area(), &c).iter().map(|(_, r)| r.width()).sum();
        assert_eq!(xs, 1000, "columns exactly fill the view");
        // stack both into one column and walk a tile's height share
        assert!(s.consume_into(&w[0]));
        let mut hc = c.clone();
        hc.preset_heights = vec![0.75];
        assert!(s.cycle_tile_height(&w[0], &hc, false));
        let r = s
            .layout(area(), &hc)
            .iter()
            .find(|(win, _)| Rc::ptr_eq(win, &w[0]))
            .unwrap()
            .1;
        assert_eq!(r.height(), 450, "three quarters of a 600 tall area");
        assert!(s.reset_tile_heights(&w[0]));
        let r = s
            .layout(area(), &hc)
            .iter()
            .find(|(win, _)| Rc::ptr_eq(win, &w[0]))
            .unwrap()
            .1;
        assert_eq!(r.height(), 300);
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
    fn swap_tiles_exchanges_arbitrary_pairs() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        assert!(s.consume_or_expel(&w[1], true)); // column 0: [w0, w1]
        s.insert(&w[2], &cfg()); // column 1: [w2]
        assert!(s.swap_tiles(&w[0], &w[2]), "across columns");
        let rects = s.layout(area(), &cfg());
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        let r2 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[2])).unwrap().1;
        assert!(r0.x1 > r2.x1, "w0 moved into the right column");
        assert!(s.swap_tiles(&w[2], &w[1]), "within a column");
        assert!(!s.swap_tiles(&w[0], &w[0]));
    }

    #[test]
    fn move_column_leapfrogs_the_strip() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        for win in &w {
            s.insert(win, &cfg());
        }
        assert!(s.move_column(&w[2], true)); // [w0, w2, w1]
        assert!(s.move_column(&w[2], true)); // [w2, w0, w1]
        assert!(!s.move_column(&w[2], true), "saturates at the edge");
        let rects = s.layout(area(), &cfg());
        let r2 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[2])).unwrap().1;
        assert_eq!(r2.x1, 0, "walked to the strip's head");
        assert!(Rc::ptr_eq(&s.first().unwrap(), &w[2]), "focus rode along");
    }

    #[test]
    fn swap_lr_trades_tiles_not_columns() {
        let (_st, w) = setup(3);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        assert!(s.consume_or_expel(&w[1], true)); // column 0: [w0, w1]
        s.insert(&w[2], &cfg()); // column 1: [w2]
        s.note_focus(&w[1]);
        assert!(s.swap_dir(&w[1], Dir::Right));
        let rects = s.layout(area(), &cfg());
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        let r1 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[1])).unwrap().1;
        let r2 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[2])).unwrap().1;
        assert_eq!(r0.x1, r2.x1, "the neighbor took the stacked slot");
        assert!(r1.x1 > r0.x1, "the mover stands alone on the right");
        assert!(Rc::ptr_eq(&s.first().unwrap(), &w[1]), "active follows the mover");
    }

    #[test]
    fn background_close_above_keeps_the_remembered_tile() {
        let (_st, w) = setup(4);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        s.insert(&w[2], &cfg());
        s.note_focus(&w[0]);
        assert!(s.consume_into(&w[0]));
        assert!(s.consume_into(&w[0])); // one column: [w0, w1, w2]
        s.insert(&w[3], &cfg());
        s.note_focus(&w[1]); // remember the middle tile
        s.focus_dir(Dir::Right);
        assert!(s.remove(&w[0])); // the tile above it closes unfocused
        let f = s.focus_dir(Dir::Left).unwrap();
        assert!(Rc::ptr_eq(&f, &w[1]), "focus returns to the remembered tile");
    }

    #[test]
    fn expel_and_consume_reset_the_lone_survivors_weight() {
        let (_st, w) = setup(2);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        assert!(s.consume_or_expel(&w[1], true)); // one column: [w0, w1]
        let mut hc = cfg();
        hc.preset_heights = vec![0.75];
        assert!(s.cycle_tile_height(&w[0], &hc, false));
        assert!(s.expel_from(&w[0])); // w1 leaves, w0 alone
        assert!(s.consume_into(&w[0])); // w1 comes back
        let rects = s.layout(area(), &hc);
        let r0 = rects.iter().find(|(win, _)| Rc::ptr_eq(win, &w[0])).unwrap().1;
        assert_eq!(r0.height(), 300, "the survivor forgot its old share");
    }

    #[test]
    fn closing_the_fresh_column_survives_an_unrelated_close() {
        let (_st, w) = setup(4);
        let s = Strip::default();
        s.insert(&w[0], &cfg());
        s.insert(&w[1], &cfg());
        s.insert(&w[2], &cfg());
        s.note_focus(&w[1]);
        s.layout(area(), &cfg());
        s.insert(&w[3], &cfg()); // fresh column, restore armed at w1
        s.layout(area(), &cfg());
        assert!(s.remove(&w[0])); // an unrelated column dies on its own
        assert!(s.remove(&w[3])); // closing the fresh one goes back
        assert!(Rc::ptr_eq(&s.first().unwrap(), &w[1]), "restore lands on the remembered column");
        s.layout(area(), &cfg());
        assert_eq!(s.view_px(), 0.0, "the restored view dropped the dead column's width");
    }

    #[test]
    fn a_dead_targets_view_glide_gets_retargeted() {
        let (st, w) = setup(3);
        let s = Strip::default();
        for win in &w {
            s.insert(win, &cfg());
        }
        let old = s.view_px();
        s.layout(area(), &cfg());
        s.animate_view(&st, old); // glide toward the scrolled view
        assert!(s.view_anim.borrow().is_some());
        assert!(s.remove(&w[2])); // its target column closes mid-glide
        let old = s.view_px();
        s.layout(area(), &cfg());
        s.animate_view(&st, old);
        let now = st.anim_clock.now();
        if let Some(a) = &*s.view_anim.borrow() {
            if !a.is_done(now) {
                assert!(
                    (a.to() - s.view_px()).abs() < 0.5,
                    "a live glide must head at the live view"
                );
            }
        }
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
