// one workspace-facing surface over both tiling containers; the mode
// decides which one is live, the other stays empty.

use super::{Window, dwindle, scrolling};
use crate::config::{LayoutMode, ScrollCfg};
use std::cell::Cell;
use std::rc::Rc;

#[derive(Default)]
pub struct Layout {
    mode: Cell<LayoutMode>,
    pub dwindle: dwindle::Tree,
    pub strip: scrolling::Strip,
}

impl Layout {
    pub fn mode(&self) -> LayoutMode {
        self.mode.get()
    }

    /// creation-time and conversion only; callers move the windows
    pub fn set_mode_empty(&self, m: LayoutMode) {
        self.mode.set(m);
    }

    pub fn is_empty(&self) -> bool {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.is_empty(),
            LayoutMode::Scrolling => self.strip.is_empty(),
        }
    }

    pub fn insert(&self, win: &Rc<Window>, cx: i32, cy: i32, cfg: &ScrollCfg) {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.insert(win, cx, cy),
            LayoutMode::Scrolling => self.strip.insert(win, cfg),
        }
    }

    pub fn remove(&self, win: &Window) {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.remove(win),
            LayoutMode::Scrolling => {
                self.strip.remove(win);
            }
        }
    }

    pub fn for_each(&self, f: impl FnMut(&Rc<Window>)) {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.for_each(f),
            LayoutMode::Scrolling => self.strip.for_each(f),
        }
    }

    pub fn first(&self) -> Option<Rc<Window>> {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.first(),
            LayoutMode::Scrolling => self.strip.first(),
        }
    }

    pub fn window_at(&self, x: i32, y: i32) -> Option<Rc<Window>> {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.window_at(x, y),
            LayoutMode::Scrolling => self.strip.window_at(x, y),
        }
    }

    /// keep the strip's active column in step with real focus; true when
    /// the active column changed
    pub fn note_focus_win(&self, win: &Window) -> bool {
        self.mode.get() == LayoutMode::Scrolling && self.strip.note_focus(win)
    }

    /// where focus lands when nothing better is known: the strip
    /// remembers its active column, dwindle starts at the first leaf
    pub fn default_focus(&self) -> Option<Rc<Window>> {
        match self.mode.get() {
            LayoutMode::Dwindle => self.dwindle.first(),
            LayoutMode::Scrolling => self.strip.active_window(),
        }
    }
}
