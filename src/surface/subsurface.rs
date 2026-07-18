// subsurfaces - the tree's stacking primitive. a new child starts topmost
// but pending; order and position change only on parent commit. sync
// children (the default) stash whole commits into the parent.

use super::commit::PendingSub;
use super::role::{SurfaceExt, SurfaceRole};
use super::{StackEntry, WlSurface};
use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wl_subcompositor, wl_subsurface};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub const BAD_SURFACE: u32 = 0;
pub const BAD_PARENT: u32 = 1;
/// wl_subsurface's own error space
pub const SUB_BAD_SURFACE: u32 = 0;

pub(super) const MAX_DEPTH: u32 = 100;

fn next_uid() -> u64 {
    thread_local! {
        static NEXT: Cell<u64> = const { Cell::new(1) };
    }
    NEXT.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    })
}

// -- wl_subcompositor --

pub struct WlSubcompositorGlobal;

impl Global for WlSubcompositorGlobal {
    fn interface(&self) -> &'static str {
        wl_subcompositor::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, _version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(WlSubcompositor {
            id,
            client: client.clone(),
        }))
    }
}

pub struct WlSubcompositor {
    pub(crate) id: ObjectId,
    pub(crate) client: Rc<Client>,
}

impl wl_subcompositor::Handler for WlSubcompositor {
    fn destroy(
        &self,
        _req: wl_subcompositor::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_subsurface(
        &self,
        req: wl_subcompositor::get_subsurface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        let Some(parent) = c.objects.surface(req.parent) else {
            c.invalid_object(req.parent);
            return Ok(());
        };
        if Rc::ptr_eq(&surface, &parent) {
            c.protocol_error(self.id, BAD_PARENT, "a surface cannot be its own subsurface");
            return Ok(());
        }
        if surface.has_live_role() {
            c.protocol_error(self.id, BAD_SURFACE, "the surface already has a role object");
            return Ok(());
        }
        if let Err(old) = surface.set_role(SurfaceRole::Subsurface) {
            c.protocol_error(
                self.id,
                BAD_SURFACE,
                &format!("the surface already has the {} role", old.name()),
            );
            return Ok(());
        }
        if Rc::ptr_eq(&parent.get_root(), &surface) {
            c.protocol_error(self.id, BAD_PARENT, "circular subsurface relationship");
            return Ok(());
        }
        // the surface may root a tree of its own; bound the grafted total
        if parent.depth() + 1 + surface.height() > MAX_DEPTH {
            c.protocol_error(self.id, BAD_PARENT, "subsurfaces nested too deeply");
            return Ok(());
        }
        let parent_sync = parent.ext.borrow().effective_sync();
        let sub = Rc::new_cyclic(|me| WlSubsurface {
            id: req.id,
            client: c.clone(),
            me: me.clone(),
            uid: next_uid(),
            surface: surface.clone(),
            parent: parent.clone(),
            position: Cell::new((0, 0)),
            sync_requested: Cell::new(true),
            sync_ancestor: Cell::new(parent_sync),
            entry: RefCell::new(None),
            latest: RefCell::new(None),
            orphaned: Cell::new(false),
        });
        c.add_client_obj(sub.clone())?;
        *surface.ext.borrow_mut() = Rc::new(SubExt { sub: sub.clone() });
        let entry = Rc::new(StackEntry {
            pending: Cell::new(true),
            sub: sub.clone(),
        });
        {
            let mut ch = parent.children_mut();
            ch.subs.insert(surface.id, sub.clone());
            ch.above.push(entry.clone());
        }
        *sub.latest.borrow_mut() = Some(entry.clone());
        sub.stash(|slot| slot.entry = Some(entry));
        Ok(())
    }
}

impl Object for WlSubcompositor {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_subcompositor::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_subcompositor::dispatch(&*self, 1, opcode, r)
    }
}

// -- wl_subsurface --

pub struct WlSubsurface {
    pub id: ObjectId,
    pub client: Rc<Client>,
    me: std::rc::Weak<WlSubsurface>,
    /// pending-map key; wire ids get reused, this never does
    pub uid: u64,
    pub surface: Rc<WlSurface>,
    pub parent: Rc<WlSurface>,
    pub position: Cell<(i32, i32)>,
    sync_requested: Cell<bool>,
    sync_ancestor: Cell<bool>,
    /// applied stack slot; replaced at parent commit
    pub entry: RefCell<Option<Rc<StackEntry>>>,
    /// most recent slot, maybe pending - anchor for chained reorders in one commit
    pub latest: RefCell<Option<Rc<StackEntry>>>,
    orphaned: Cell<bool>,
}

impl WlSubsurface {
    pub fn sync(&self) -> bool {
        self.sync_requested.get() || self.sync_ancestor.get()
    }

    fn rc(&self) -> Rc<WlSubsurface> {
        self.me.upgrade().expect("subsurface outlived its own rc")
    }

    fn stash(&self, f: impl FnOnce(&mut PendingSub)) {
        let mut p = self.parent.pending.borrow_mut();
        let slot = p
            .subsurfaces
            .entry(self.uid)
            .or_insert_with(|| PendingSub::new(self.rc()));
        f(slot);
    }

    /// the parent died; the role sticks but the tree link is gone
    pub(super) fn orphan(&self) {
        self.orphaned.set(true);
        *self.surface.ext.borrow_mut() = Rc::new(super::NoneExt);
        self.entry.borrow_mut().take();
        self.latest.borrow_mut().take();
        self.surface.mapped.set(false);
    }

    fn remove_own_entries(&self) {
        if let Some(e) = self.entry.borrow_mut().take() {
            self.parent.remove_stack_entry(&e);
        }
        if let Some(e) = self.latest.borrow_mut().take() {
            self.parent.remove_stack_entry(&e);
        }
    }

    fn propagate_sync(&self) {
        let effective = self.sync();
        if let Some(ch) = &*self.surface.children.borrow() {
            for sub in ch.subs.values() {
                let was = sub.sync();
                sub.sync_ancestor.set(effective);
                if was && !sub.sync() {
                    sub.flush_cached();
                }
                sub.propagate_sync();
            }
        }
    }

    /// leaving sync mode applies whatever was stashed
    fn flush_cached(&self) {
        let state = {
            let mut p = self.parent.pending.borrow_mut();
            p.subsurfaces.get_mut(&self.uid).and_then(|s| s.state.take())
        };
        if let Some(st) = state {
            self.surface.apply_state(st);
        }
    }
}

impl wl_subsurface::Handler for WlSubsurface {
    fn destroy(&self, _req: wl_subsurface::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if !self.orphaned.get() {
            // a stashed sync commit still belongs to the surface
            self.flush_cached();
            self.parent.pending.borrow_mut().subsurfaces.remove(&self.uid);
            self.remove_own_entries();
            if let Some(ch) = &mut *self.parent.children.borrow_mut() {
                ch.subs.remove(&self.surface.id);
            }
            *self.surface.ext.borrow_mut() = Rc::new(super::NoneExt);
            self.surface.mapped.set(false);
            self.orphaned.set(true);
            self.parent.update_extents();
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_position(
        &self,
        req: wl_subsurface::set_position::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.orphaned.get() {
            return Ok(());
        }
        self.stash(|slot| slot.position = Some((req.x, req.y)));
        Ok(())
    }

    fn place_above(
        &self,
        req: wl_subsurface::place_above::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.place(req.sibling, true)
    }

    fn place_below(
        &self,
        req: wl_subsurface::place_below::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.place(req.sibling, false)
    }

    fn set_sync(&self, _req: wl_subsurface::set_sync::Request) -> Result<(), Box<dyn std::error::Error>> {
        if !self.orphaned.get() {
            self.sync_requested.set(true);
            self.propagate_sync();
        }
        Ok(())
    }

    fn set_desync(
        &self,
        _req: wl_subsurface::set_desync::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.orphaned.get() {
            let was = self.sync();
            self.sync_requested.set(false);
            if was && !self.sync() {
                self.flush_cached();
            }
            self.propagate_sync();
        }
        Ok(())
    }
}

impl WlSubsurface {
    fn place(&self, sibling_id: ObjectId, above: bool) -> Result<(), Box<dyn std::error::Error>> {
        if self.orphaned.get() {
            return Ok(());
        }
        let c = &self.client;
        let Some(sibling) = c.objects.surface(sibling_id) else {
            c.invalid_object(sibling_id);
            return Ok(());
        };
        let entry = Rc::new(StackEntry {
            pending: Cell::new(true),
            sub: self.rc(),
        });
        if Rc::ptr_eq(&sibling, &self.parent) {
            let mut ch = self.parent.children_mut();
            if above {
                // directly above the parent's own buffer
                ch.above.insert(0, entry.clone());
            } else {
                ch.below.push(entry.clone());
            }
        } else {
            if Rc::ptr_eq(&sibling, &self.surface) {
                c.protocol_error(self.id, SUB_BAD_SURFACE, "cannot place a subsurface against itself");
                return Ok(());
            }
            let sib = self.parent.children.borrow().as_ref().and_then(|ch| ch.subs.get(&sibling.id).cloned());
            let Some(sib) = sib else {
                c.protocol_error(
                    self.id,
                    SUB_BAD_SURFACE,
                    "the anchor is neither a sibling nor the parent",
                );
                return Ok(());
            };
            let anchor = sib.latest.borrow().clone().or_else(|| sib.entry.borrow().clone());
            let Some(anchor) = anchor else {
                return Ok(());
            };
            let mut ch = self.parent.children_mut();
            if let Some(i) = ch.above.iter().position(|e| Rc::ptr_eq(e, &anchor)) {
                let at = if above { i + 1 } else { i };
                ch.above.insert(at, entry.clone());
            } else if let Some(i) = ch.below.iter().position(|e| Rc::ptr_eq(e, &anchor)) {
                let at = if above { i + 1 } else { i };
                ch.below.insert(at, entry.clone());
            }
        }
        *self.latest.borrow_mut() = Some(entry.clone());
        self.stash(move |slot| {
            // replacing an unapplied reorder drops its stale entry
            if let Some(old) = slot.entry.replace(entry) {
                if old.pending.get() {
                    old.sub.parent.remove_stack_entry(&old);
                }
            }
        });
        Ok(())
    }
}

impl Object for WlSubsurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_subsurface::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_subsurface::dispatch(&*self, 1, opcode, r)
    }

    fn break_loops(&self) {
        self.entry.borrow_mut().take();
        self.latest.borrow_mut().take();
    }
}

// -- the role hook --

pub struct SubExt {
    pub sub: Rc<WlSubsurface>,
}

impl SurfaceExt for SubExt {
    fn commit_requested(
        self: Rc<Self>,
        pending: Box<super::PendingState>,
    ) -> Option<Box<super::PendingState>> {
        if self.sub.sync() {
            let sub = &self.sub;
            let mut p = sub.parent.pending.borrow_mut();
            let slot = p
                .subsurfaces
                .entry(sub.uid)
                .or_insert_with(|| PendingSub::new(sub.clone()));
            match &mut slot.state {
                Some(cur) => cur.merge(pending),
                None => slot.state = Some(pending),
            }
            None
        } else {
            Some(pending)
        }
    }

    fn after_apply(&self) {
        // mapped iff it has a buffer and the parent is mapped
        let own = self.sub.surface.mapped.get();
        self.sub
            .surface
            .mapped
            .set(own && self.sub.parent.mapped.get());
    }

    fn parent(&self) -> Option<Rc<WlSurface>> {
        Some(self.sub.parent.clone())
    }

    fn effective_sync(&self) -> bool {
        self.sub.sync()
    }
}
