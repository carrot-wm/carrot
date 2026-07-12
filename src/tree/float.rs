// the floating stack - z ordered, between tiled and fullscreen unless float_above_fullscreen flips it.

use super::{Window, active};
use crate::rect::Rect;
use crate::state::State;
use std::rc::Rc;

/// explicit toggle only; no auto-float heuristics
pub fn toggle_floating(state: &Rc<State>, win: &Rc<Window>) {
    let ws = active(state);
    if win.fullscreen.get() {
        return;
    }
    if !win.floating.get() {
        ws.tiling.remove(win);
        win.floating.set(true);
        let (sw, sh) = super::output_extent(state);
        let (w, h) = (sw / 2, sh / 2);
        win.set_rect_animated(
            state,
            Rect::new_sized_saturating(sw / 4, sh / 4, w.max(1), h.max(1)),
        );
        ws.floats.borrow_mut().push(win.clone());
        win.configure_rect();
    } else {
        ws.remove_float(win);
        win.floating.set(false);
        let (cx, cy) = super::cursor_pos(state);
        let scfg = state.config.borrow().layout.scrolling.clone();
        ws.tiling.insert(win, cx, cy, &scfg);
    }
    super::relayout(state, &ws);
    state.damage.trigger();
}
