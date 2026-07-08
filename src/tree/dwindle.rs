// the dwindle tree - binary splits; recalculate resets the root box to full
// screen, closing a window never rebuilds the structure. children strong,
// parents weak, workspace owns the root - no cycles. a leaf's window holds a
// weak pointer back, so removal never scans. orientation is re-derived from
// box aspect on every recalculate.

use super::Window;
use crate::rect::Rect;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub struct Node {
    parent: RefCell<Weak<Node>>,
    kind: RefCell<Kind>,
    pub rect: Cell<Rect>,
    split_top: Cell<bool>,
    /// first child's share of the split span; 0.5 is even
    ratio: Cell<f64>,
}

const RATIO_MIN: f64 = 0.1;
const RATIO_MAX: f64 = 0.9;

// -- resize edges, the xdg_toplevel bitfield --
pub const EDGE_TOP: u32 = 1;
pub const EDGE_BOTTOM: u32 = 2;
pub const EDGE_LEFT: u32 = 4;
pub const EDGE_RIGHT: u32 = 8;

enum Kind {
    Leaf(Rc<Window>),
    Branch([Rc<Node>; 2]),
}

impl Node {
    fn leaf(win: &Rc<Window>) -> Rc<Node> {
        Rc::new(Node {
            parent: RefCell::new(Weak::new()),
            kind: RefCell::new(Kind::Leaf(win.clone())),
            rect: Cell::new(Rect::default()),
            split_top: Cell::new(false),
            ratio: Cell::new(0.5),
        })
    }

    fn window(&self) -> Option<Rc<Window>> {
        match &*self.kind.borrow() {
            Kind::Leaf(w) => Some(w.clone()),
            Kind::Branch(_) => None,
        }
    }

    fn layout(&self) {
        let children = match &*self.kind.borrow() {
            Kind::Leaf(_) => return,
            Kind::Branch(c) => c.clone(),
        };
        let r = self.rect.get();
        self.split_top.set(r.height() > r.width());
        let ratio = self.ratio.get().clamp(RATIO_MIN, RATIO_MAX);
        if self.split_top.get() {
            let first = (r.height() as f64 * ratio) as i32;
            children[0]
                .rect
                .set(Rect { x1: r.x1, y1: r.y1, x2: r.x2, y2: r.y1 + first });
            children[1]
                .rect
                .set(Rect { x1: r.x1, y1: r.y1 + first, x2: r.x2, y2: r.y2 });
        } else {
            let first = (r.width() as f64 * ratio) as i32;
            children[0]
                .rect
                .set(Rect { x1: r.x1, y1: r.y1, x2: r.x1 + first, y2: r.y2 });
            children[1]
                .rect
                .set(Rect { x1: r.x1 + first, y1: r.y1, x2: r.x2, y2: r.y2 });
        }
        children[0].layout();
        children[1].layout();
    }

    fn for_each(&self, f: &mut impl FnMut(&Rc<Window>)) {
        match &*self.kind.borrow() {
            Kind::Leaf(w) => f(w),
            Kind::Branch(c) => {
                c[0].for_each(f);
                c[1].for_each(f);
            }
        }
    }
}

#[derive(Default)]
pub struct Tree {
    root: RefCell<Option<Rc<Node>>>,
}

impl Tree {
    pub fn is_empty(&self) -> bool {
        self.root.borrow().is_none()
    }

    /// descend by point-in-box; half-open contiguous boxes, so exactly one leaf
    fn leaf_node_at(&self, x: i32, y: i32) -> Option<Rc<Node>> {
        let mut cur = self.root.borrow().clone()?;
        loop {
            let next = match &*cur.kind.borrow() {
                Kind::Leaf(_) => return Some(cur.clone()),
                Kind::Branch(c) => {
                    if c[0].rect.get().contains(x, y) {
                        c[0].clone()
                    } else {
                        c[1].clone()
                    }
                }
            };
            cur = next;
        }
    }

    pub fn insert(&self, win: &Rc<Window>, cx: i32, cy: i32) {
        let leaf = Node::leaf(win);
        *win.node.borrow_mut() = Rc::downgrade(&leaf);
        let Some(target) = self.leaf_node_at(cx, cy) else {
            *self.root.borrow_mut() = Some(leaf);
            return;
        };
        let t_rect = target.rect.get();
        let side_by_side = t_rect.width() >= t_rect.height();
        // new window goes on the cursor's side of the target's midpoint
        let new_first = if side_by_side {
            cx < (t_rect.x1 + t_rect.x2) / 2
        } else {
            cy < (t_rect.y1 + t_rect.y2) / 2
        };
        let children = if new_first {
            [leaf.clone(), target.clone()]
        } else {
            [target.clone(), leaf.clone()]
        };
        let branch = Rc::new(Node {
            parent: RefCell::new(target.parent.borrow().clone()),
            kind: RefCell::new(Kind::Branch(children)),
            rect: Cell::new(t_rect),
            split_top: Cell::new(!side_by_side),
            ratio: Cell::new(0.5),
        });
        self.replace_child(&target, &branch);
        *target.parent.borrow_mut() = Rc::downgrade(&branch);
        *leaf.parent.borrow_mut() = Rc::downgrade(&branch);
    }

    /// sibling promotion: survivor inherits the parent's box and grandparent slot
    pub fn remove(&self, win: &Window) {
        let leaf = std::mem::take(&mut *win.node.borrow_mut());
        let Some(leaf) = leaf.upgrade() else {
            return;
        };
        let Some(parent) = leaf.parent.borrow().upgrade() else {
            *self.root.borrow_mut() = None;
            return;
        };
        let sibling = match &*parent.kind.borrow() {
            Kind::Branch(c) => {
                if Rc::ptr_eq(&c[0], &leaf) {
                    c[1].clone()
                } else {
                    c[0].clone()
                }
            }
            Kind::Leaf(_) => unreachable!("a leaf cannot be a parent"),
        };
        sibling.rect.set(parent.rect.get());
        *sibling.parent.borrow_mut() = parent.parent.borrow().clone();
        self.replace_child(&parent, &sibling);
    }

    /// swap old for new in the grandparent's slot, or at the root
    fn replace_child(&self, old: &Rc<Node>, new: &Rc<Node>) {
        match old.parent.borrow().upgrade() {
            None => *self.root.borrow_mut() = Some(new.clone()),
            Some(gp) => {
                if let Kind::Branch(c) = &mut *gp.kind.borrow_mut() {
                    for slot in c {
                        if Rc::ptr_eq(slot, old) {
                            *slot = new.clone();
                            return;
                        }
                    }
                }
            }
        }
    }

    pub fn recalculate(&self, area: Rect) {
        if let Some(root) = &*self.root.borrow() {
            root.rect.set(area);
            root.layout();
        }
    }

    pub fn window_at(&self, x: i32, y: i32) -> Option<Rc<Window>> {
        let leaf = self.leaf_node_at(x, y)?;
        if !leaf.rect.get().contains(x, y) {
            return None;
        }
        leaf.window()
    }

    pub fn for_each(&self, mut f: impl FnMut(&Rc<Window>)) {
        if let Some(root) = &*self.root.borrow() {
            root.for_each(&mut f);
        }
    }

    pub fn first(&self) -> Option<Rc<Window>> {
        let mut cur = self.root.borrow().clone()?;
        loop {
            let next = match &*cur.kind.borrow() {
                Kind::Leaf(w) => return Some(w.clone()),
                Kind::Branch(c) => c[0].clone(),
            };
            cur = next;
        }
    }
}

// -- leaf swap --

/// exchange two windows' leaf slots; the nodes (and so the split
/// structure) stay put, only the occupants trade places
pub fn swap_windows(a: &Rc<Window>, b: &Rc<Window>) -> bool {
    let (Some(na), Some(nb)) = (a.node.borrow().upgrade(), b.node.borrow().upgrade()) else {
        return false;
    };
    if Rc::ptr_eq(&na, &nb) {
        return false;
    }
    *na.kind.borrow_mut() = Kind::Leaf(b.clone());
    *nb.kind.borrow_mut() = Kind::Leaf(a.clone());
    *a.node.borrow_mut() = Rc::downgrade(&nb);
    *b.node.borrow_mut() = Rc::downgrade(&na);
    true
}

// -- split ratio control --

/// one boundary step: pointer delta over the split span, clamped
fn ratio_step(ratio: f64, delta_px: f64, span: f64) -> f64 {
    (ratio + delta_px / span.max(1.0)).clamp(RATIO_MIN, RATIO_MAX)
}

/// nearest ancestor split whose interior boundary is the dragged edge:
/// a right/bottom drag moves the boundary after the window's subtree,
/// a left/top drag the one before it. the axis must match the edge
fn boundary_split(win: &Window, edges: u32, vertical: bool) -> Option<Rc<Node>> {
    let (low, high) = if vertical {
        (EDGE_TOP, EDGE_BOTTOM)
    } else {
        (EDGE_LEFT, EDGE_RIGHT)
    };
    let mut cur = win.node.borrow().upgrade()?;
    loop {
        let parent = cur.parent.borrow().upgrade()?;
        let first = match &*parent.kind.borrow() {
            Kind::Branch(c) => Rc::ptr_eq(&c[0], &cur),
            Kind::Leaf(_) => unreachable!("a leaf cannot be a parent"),
        };
        if parent.split_top.get() == vertical
            && ((edges & high != 0 && first) || (edges & low != 0 && !first))
        {
            return Some(parent);
        }
        cur = parent;
    }
}

/// drag a tiled window's edge: the matching split boundary follows the pointer
pub fn resize_by_edges(win: &Window, edges: u32, dx: f64, dy: f64) -> bool {
    let mut changed = false;
    for (vertical, delta) in [(false, dx), (true, dy)] {
        if delta == 0.0 {
            continue;
        }
        if let Some(split) = boundary_split(win, edges, vertical) {
            let r = split.rect.get();
            let span = if vertical { r.height() } else { r.width() } as f64;
            let old = split.ratio.get();
            let new = ratio_step(old, delta, span);
            split.ratio.set(new);
            changed |= new != old;
        }
    }
    changed
}

/// keybind nudge: a positive delta grows the window's share of its
/// immediate parent split
pub fn adjust_parent_ratio(win: &Window, delta: f64) -> bool {
    let Some(leaf) = win.node.borrow().upgrade() else {
        return false;
    };
    let Some(parent) = leaf.parent.borrow().upgrade() else {
        return false;
    };
    let first = match &*parent.kind.borrow() {
        Kind::Branch(c) => Rc::ptr_eq(&c[0], &leaf),
        Kind::Leaf(_) => unreachable!("a leaf cannot be a parent"),
    };
    let old = parent.ratio.get();
    let new = (old + if first { delta } else { -delta }).clamp(RATIO_MIN, RATIO_MAX);
    parent.ratio.set(new);
    new != old
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_steps_by_delta_over_span() {
        assert_eq!(ratio_step(0.5, 100.0, 1000.0), 0.6);
        assert_eq!(ratio_step(0.5, -100.0, 1000.0), 0.4);
        // a zero span cannot divide by zero
        assert_eq!(ratio_step(0.5, 1.0, 0.0), 0.9);
    }

    #[test]
    fn ratio_clamps_to_its_band() {
        assert_eq!(ratio_step(0.85, 500.0, 1000.0), RATIO_MAX);
        assert_eq!(ratio_step(0.15, -500.0, 1000.0), RATIO_MIN);
        assert_eq!(ratio_step(RATIO_MAX, 1.0, 10.0), RATIO_MAX);
    }
}
