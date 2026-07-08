// workspaces own their tree and float stack; switching goes through set_focus.

use super::{Window, dwindle};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
pub struct Workspace {
    pub tiling: dwindle::Tree,
    /// z order: last is topmost
    pub floats: RefCell<Vec<Rc<Window>>>,
    pub fullscreen: RefCell<Option<Rc<Window>>>,
}

impl Workspace {
    pub fn is_empty(&self) -> bool {
        self.tiling.is_empty()
            && self.floats.borrow().is_empty()
            && self.fullscreen.borrow().is_none()
    }

    /// every window on this workspace
    pub fn for_each(&self, mut f: impl FnMut(&Rc<Window>)) {
        self.tiling.for_each(&mut f);
        for w in self.floats.borrow().iter() {
            f(w);
        }
    }

    pub fn contains(&self, win: &Rc<Window>) -> bool {
        if self.floats.borrow().iter().any(|w| Rc::ptr_eq(w, win)) {
            return true;
        }
        if self.fullscreen.borrow().as_ref().is_some_and(|w| Rc::ptr_eq(w, win)) {
            return true;
        }
        let mut hit = false;
        self.tiling.for_each(|w| hit |= Rc::ptr_eq(w, win));
        hit
    }

    pub fn top_float(&self) -> Option<Rc<Window>> {
        self.floats.borrow().last().cloned()
    }

    pub fn remove_float(&self, win: &Rc<Window>) {
        self.floats.borrow_mut().retain(|w| !Rc::ptr_eq(w, win));
    }

    pub fn raise_float(&self, win: &Rc<Window>) {
        let mut floats = self.floats.borrow_mut();
        if let Some(i) = floats.iter().position(|w| Rc::ptr_eq(w, win)) {
            let w = floats.remove(i);
            floats.push(w);
        }
    }
}
