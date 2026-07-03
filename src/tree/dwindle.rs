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
    /// first child gets dim/2 * ratio; 1.0 is an even split
    ratio: Cell<f64>,
}

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
            ratio: Cell::new(1.0),
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
        let ratio = self.ratio.get().clamp(0.1, 1.9);
        if self.split_top.get() {
            let first = (r.height() as f64 / 2.0 * ratio) as i32;
            children[0]
                .rect
                .set(Rect { x1: r.x1, y1: r.y1, x2: r.x2, y2: r.y1 + first });
            children[1]
                .rect
                .set(Rect { x1: r.x1, y1: r.y1 + first, x2: r.x2, y2: r.y2 });
        } else {
            let first = (r.width() as f64 / 2.0 * ratio) as i32;
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
            ratio: Cell::new(1.0),
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
