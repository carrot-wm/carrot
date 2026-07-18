// the surface tree. subsurfaces stack above/below their parent; input regions
// decide hit-testing to the deepest surface under the point.
// mapping is a pure function of the committed buffer: first buffer maps, null unmaps.
// coords are i32 logical throughout; scale is n/120 fixed point, converted to pixels in the renderer.

mod commit;
mod role;
mod subsurface;

pub use commit::PendingState;
pub use role::{NoneExt, SurfaceExt, SurfaceRole};
pub use subsurface::{WlSubcompositorGlobal, WlSubsurface};

use crate::client::{Client, ClientError, Object};
use crate::protocol::display::WlCallback;
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wl_compositor, wl_region, wl_surface};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::{Rect, RegionBuilder};
use crate::util::{Stack, Time};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// wl_surface protocol error codes
pub const INVALID_SCALE: u32 = 0;
pub const INVALID_TRANSFORM: u32 = 1;
#[allow(dead_code)]
pub const INVALID_SIZE: u32 = 2;
pub const INVALID_OFFSET: u32 = 3;
pub const DEFUNCT_ROLE_OBJECT: u32 = 4;

// -- buffer transforms --

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Transform {
    #[default]
    Normal,
    R90,
    R180,
    R270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    fn from_wl(v: i32) -> Option<Transform> {
        Some(match v {
            0 => Transform::Normal,
            1 => Transform::R90,
            2 => Transform::R180,
            3 => Transform::R270,
            4 => Transform::Flipped,
            5 => Transform::Flipped90,
            6 => Transform::Flipped180,
            7 => Transform::Flipped270,
            _ => return None,
        })
    }

    pub fn swaps_dimensions(self) -> bool {
        matches!(
            self,
            Transform::R90 | Transform::R270 | Transform::Flipped90 | Transform::Flipped270
        )
    }
}

// -- stacking --

pub struct StackEntry {
    pub pending: Cell<bool>,
    pub sub: Rc<WlSubsurface>,
}

/// z order: below[..], parent's own buffer, above[..]. index order is render
/// order; hit tests walk it backwards.
#[derive(Default)]
pub struct ParentData {
    pub subs: HashMap<ObjectId, Rc<WlSubsurface>>,
    pub below: Vec<Rc<StackEntry>>,
    pub above: Vec<Rc<StackEntry>>,
}

// -- the surface --

pub struct WlSurface {
    pub id: ObjectId,
    /// never-reused identity; texture caches key on this, not the wire id
    pub uid: u64,
    pub client: Rc<Client>,
    pub version: u32,
    pub(crate) role: Cell<SurfaceRole>,
    pub(crate) ext: RefCell<Rc<dyn SurfaceExt>>,
    pub(crate) pending: RefCell<Box<PendingState>>,
    /// consumed pending boxes come back here instead of the allocator
    pub(crate) pending_free: Stack<Box<PendingState>>,
    pub(crate) buffer: RefCell<Option<crate::protocol::shm::AttachedBuffer>>,
    pub(crate) input_region: RefCell<Option<Rc<crate::rect::Region>>>,
    pub(crate) opaque_region: RefCell<Option<Rc<crate::rect::Region>>>,
    pub(crate) scale: Cell<i32>,
    pub(crate) transform: Cell<Transform>,
    pub(crate) buf_x: Cell<i32>,
    pub(crate) buf_y: Cell<i32>,
    /// logical size after transform swap and scale ceil-div
    pub(crate) size: Cell<(i32, i32)>,
    /// own rect union child extents, in local coords
    pub(crate) extents: Cell<Rect>,
    pub(crate) frame_callbacks: RefCell<Vec<FrameCallback>>,
    /// presentation feedbacks of the committed content, waiting for a flip
    pub(crate) latched_feedbacks: RefCell<Vec<crate::protocol::presentation::Feedback>>,
    pub(crate) children: RefCell<Option<Box<ParentData>>>,
    pub(crate) mapped: Cell<bool>,
    /// some compose drew this surface since the last present; frame
    /// callbacks fire only then, so occluded clients stop rendering
    pub(crate) shown: Cell<bool>,
    pub(crate) destroyed: Cell<bool>,
    /// bumps when a commit attaches or damages; shm re-uploads key off it
    pub(crate) content_gen: Cell<u64>,
    /// commit-time copy of the shm pixels (tight w*4 rows); the client
    /// buffer releases immediately, so this is what compositing reads
    pub(crate) shm_shadow: RefCell<Option<Vec<u8>>>,
    /// prefers async presentation (tearing) when it's the fullscreen surface
    pub(crate) tearing: Cell<bool>,
    /// a wp_tearing_control_v1 exists; the protocol allows exactly one
    pub(crate) tearing_control: Cell<bool>,
}

impl WlSurface {
    pub fn new(id: ObjectId, client: &Rc<Client>, version: u32) -> Rc<WlSurface> {
        Rc::new(WlSurface {
            id,
            uid: client.state.next_uid(),
            client: client.clone(),
            version,
            role: Cell::new(SurfaceRole::None),
            ext: RefCell::new(Rc::new(NoneExt)),
            pending: RefCell::new(Box::default()),
            pending_free: Stack::default(),
            buffer: RefCell::new(None),
            input_region: RefCell::new(None),
            opaque_region: RefCell::new(None),
            scale: Cell::new(1),
            transform: Cell::new(Transform::Normal),
            buf_x: Cell::new(0),
            buf_y: Cell::new(0),
            size: Cell::new((0, 0)),
            extents: Cell::new(Rect::default()),
            frame_callbacks: RefCell::new(Vec::new()),
            latched_feedbacks: RefCell::new(Vec::new()),
            tearing: Cell::new(false),
            tearing_control: Cell::new(false),
            children: RefCell::new(None),
            mapped: Cell::new(false),
            shown: Cell::new(false),
            destroyed: Cell::new(false),
            content_gen: Cell::new(0),
            shm_shadow: RefCell::new(None),
        })
    }

    pub fn is_subsurface(&self) -> bool {
        self.role.get() == SurfaceRole::Subsurface
    }

    /// None->X and X->X ok (role objects recreatable); anything else violates protocol
    pub(crate) fn set_role(&self, role: SurfaceRole) -> Result<(), SurfaceRole> {
        let old = self.role.get();
        if old != SurfaceRole::None && old != role {
            return Err(old);
        }
        self.role.set(role);
        Ok(())
    }

    pub(crate) fn has_live_role(&self) -> bool {
        self.ext.borrow().on_surface_destroy().is_err()
    }

    pub fn get_root(self: &Rc<Self>) -> Rc<WlSurface> {
        let mut cur = self.clone();
        loop {
            let parent = cur.ext.borrow().parent();
            match parent {
                Some(p) => cur = p,
                None => return cur,
            }
        }
    }

    pub(crate) fn depth(self: &Rc<Self>) -> u32 {
        let mut cur = self.clone();
        let mut depth = 0;
        loop {
            let parent = cur.ext.borrow().parent();
            match parent {
                Some(p) => {
                    cur = p;
                    depth += 1;
                }
                None => return depth,
            }
        }
    }

    /// deepest subsurface nesting below this surface; 0 for a leaf
    pub(crate) fn height(&self) -> u32 {
        let mut h = 0;
        if let Some(ch) = &*self.children.borrow() {
            for sub in ch.subs.values() {
                h = h.max(1 + sub.surface.height());
            }
        }
        h
    }

    pub fn accepts_input_at(&self, x: i32, y: i32) -> bool {
        let (w, h) = self.size.get();
        if x < 0 || y < 0 || x >= w || y >= h {
            return false;
        }
        match &*self.input_region.borrow() {
            Some(r) => r.contains(x, y),
            // no input region: whole surface accepts
            None => true,
        }
    }

    /// deepest surface under the point, topmost first: above (rev), self, below (rev)
    pub fn find_surface_at(self: &Rc<Self>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
        let children = self.children.borrow();
        if let Some(ch) = &*children {
            for e in ch.above.iter().rev() {
                if let Some(hit) = test_child(e, x, y) {
                    return Some(hit);
                }
            }
            if self.accepts_input_at(x, y) {
                return Some((self.clone(), x, y));
            }
            for e in ch.below.iter().rev() {
                if let Some(hit) = test_child(e, x, y) {
                    return Some(hit);
                }
            }
            None
        } else {
            drop(children);
            self.accepts_input_at(x, y).then(|| (self.clone(), x, y))
        }
    }

    pub(crate) fn update_extents(&self) {
        let old = self.extents.get();
        let (w, h) = self.size.get();
        let mut ext = Rect::new_sized_saturating(0, 0, w, h);
        if let Some(ch) = &*self.children.borrow() {
            for e in ch.below.iter().chain(ch.above.iter()) {
                if e.pending.get() {
                    continue;
                }
                let cext = e.sub.surface.extents.get();
                if !cext.is_empty() {
                    let (px, py) = e.sub.position.get();
                    ext = ext.union(cext.move_(px, py));
                }
            }
        }
        if ext != old {
            self.extents.set(ext);
            let parent = self.ext.borrow().parent();
            if let Some(p) = parent {
                p.update_extents();
            }
        }
    }

    pub(crate) fn remove_stack_entry(&self, entry: &Rc<StackEntry>) {
        if let Some(ch) = &mut *self.children.borrow_mut() {
            ch.above.retain(|e| !Rc::ptr_eq(e, entry));
            ch.below.retain(|e| !Rc::ptr_eq(e, entry));
        }
    }

    pub(crate) fn children_mut(&self) -> std::cell::RefMut<'_, Box<ParentData>> {
        let mut ch = self.children.borrow_mut();
        if ch.is_none() {
            *ch = Some(Box::default());
        }
        std::cell::RefMut::map(ch, |c| c.as_mut().unwrap())
    }

    pub(crate) fn fire_frame_callbacks(&self, ms: u32) {
        let cbs = std::mem::take(&mut *self.frame_callbacks.borrow_mut());
        for cb in cbs {
            cb.fire(ms);
        }
    }
}

fn test_child(e: &Rc<StackEntry>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    if e.pending.get() {
        return None;
    }
    let (px, py) = e.sub.position.get();
    if !e.sub.surface.extents.get().move_(px, py).contains(x, y) {
        return None;
    }
    e.sub.surface.find_surface_at(x - px, y - py)
}

fn now_ms() -> u32 {
    (Time::now().nsec() / 1_000_000) as u32
}

/// a wl_callback from wl_surface.frame. fire sends done and releases the object;
/// dropping one un-fired fires it via Drop so the id can never leak. discard() is
/// for teardown, where the client is past caring.
pub(crate) struct FrameCallback {
    client: Rc<Client>,
    id: ObjectId,
    fired: Cell<bool>,
}

impl FrameCallback {
    fn new(client: &Rc<Client>, id: ObjectId) -> FrameCallback {
        FrameCallback {
            client: client.clone(),
            id,
            fired: Cell::new(false),
        }
    }

    pub(crate) fn fire(&self, ms: u32) {
        if self.fired.replace(true) {
            return;
        }
        self.client
            .event(|o| crate::protocol::interfaces::wl_callback::done::send(o, self.id, ms));
        let _ = self.client.remove_obj(self.id);
    }

    pub(crate) fn discard(&self) {
        self.fired.set(true);
    }
}

impl Drop for FrameCallback {
    fn drop(&mut self) {
        self.fire(now_ms());
    }
}

impl wl_surface::Handler for WlSurface {
    fn destroy(&self, _req: wl_surface::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        let ext = self.ext.borrow().clone();
        if ext.on_surface_destroy().is_err() {
            self.client.protocol_error(
                self.id,
                DEFUNCT_ROLE_OBJECT,
                "the surface still has a live role object",
            );
            return Ok(());
        }
        self.destroyed.set(true);
        // orphan children: role sticks, tree link dies
        let children = self.children.borrow_mut().take();
        if let Some(ch) = children {
            for (_, sub) in ch.subs {
                sub.orphan();
            }
        }
        let old = self.buffer.borrow_mut().take();
        drop(old);
        self.fire_frame_callbacks(now_ms());
        // uncommitted callbacks fire via Drop
        self.pending.borrow_mut().frame_callbacks.clear();
        self.client.objects.forget_surface(self.id);
        self.client.remove_obj(self.id)?;
        // the id is free for reuse once the client sees delete_id, but the
        // wm side (xwayland especially) unlinks its window later on its own
        // socket; the surface must stop being hittable and holding foci NOW
        // or the next enter names a recycled id and kills the connection
        if self.mapped.replace(false) {
            let state = &self.client.state;
            state.damage.trigger();
            let seat = state.seat.borrow().clone();
            if let Some(seat) = seat {
                let focused = seat
                    .kb_focus
                    .borrow()
                    .as_ref()
                    .is_some_and(|f| std::ptr::eq(f.as_ref(), self));
                if focused {
                    crate::input::focus::set_keyboard_focus(state, &seat, None);
                }
                let role = self.role.get();
                if role != SurfaceRole::Cursor && role != SurfaceRole::DndIcon {
                    seat.repick(state);
                }
            }
        }
        Ok(())
    }

    fn attach(&self, req: wl_surface::attach::Request) -> Result<(), Box<dyn std::error::Error>> {
        if self.version >= 5 && (req.x != 0 || req.y != 0) {
            self.client.protocol_error(
                self.id,
                INVALID_OFFSET,
                "attach offsets must be zero since version 5",
            );
            return Ok(());
        }
        let buffer = if req.buffer == ObjectId::NONE {
            None
        } else {
            let Some(buf) = self.client.objects.buffer(req.buffer) else {
                self.client.invalid_object(req.buffer);
                return Ok(());
            };
            Some(crate::protocol::shm::AttachedBuffer {
                buf,
                send_release: Cell::new(false),
            })
        };
        let mut p = self.pending.borrow_mut();
        if self.version < 5 {
            p.offset = (req.x, req.y);
        }
        p.buffer = Some(buffer);
        Ok(())
    }

    fn damage(&self, req: wl_surface::damage::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.push_damage(req.x, req.y, req.width, req.height, false)
    }

    fn frame(&self, req: wl_surface::frame::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client
            .add_client_obj(Rc::new(WlCallback { id: req.callback }))?;
        self.pending
            .borrow_mut()
            .frame_callbacks
            .push(FrameCallback::new(&self.client, req.callback));
        Ok(())
    }

    fn set_opaque_region(
        &self,
        req: wl_surface::set_opaque_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(region) = self.snapshot_region(req.region) {
            self.pending.borrow_mut().opaque_region = Some(region);
        }
        Ok(())
    }

    fn set_input_region(
        &self,
        req: wl_surface::set_input_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(region) = self.snapshot_region(req.region) {
            self.pending.borrow_mut().input_region = Some(region);
        }
        Ok(())
    }

    fn commit(&self, _req: wl_surface::commit::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.commit_impl();
        Ok(())
    }

    fn set_buffer_transform(
        &self,
        req: wl_surface::set_buffer_transform::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(t) = Transform::from_wl(req.transform) else {
            self.client.protocol_error(
                self.id,
                INVALID_TRANSFORM,
                &format!("{} is not a valid buffer transform", req.transform),
            );
            return Ok(());
        };
        self.pending.borrow_mut().transform = Some(t);
        Ok(())
    }

    fn set_buffer_scale(
        &self,
        req: wl_surface::set_buffer_scale::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.scale < 1 {
            self.client.protocol_error(
                self.id,
                INVALID_SCALE,
                &format!("{} is not a valid buffer scale", req.scale),
            );
            return Ok(());
        }
        self.pending.borrow_mut().scale = Some(req.scale);
        Ok(())
    }

    fn damage_buffer(
        &self,
        req: wl_surface::damage_buffer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.push_damage(req.x, req.y, req.width, req.height, true)
    }

    fn offset(&self, req: wl_surface::offset::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.pending.borrow_mut().offset = (req.x, req.y);
        Ok(())
    }
}

impl WlSurface {
    fn push_damage(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        buffer: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if w < 0 || h < 0 {
            return Err("damage with negative dimensions".into());
        }
        if w == 0 || h == 0 {
            return Ok(());
        }
        let rect = Rect::new_sized_saturating(x, y, w, h);
        let mut p = self.pending.borrow_mut();
        if p.damage_full {
            return Ok(());
        }
        if buffer {
            p.buffer_damage.push(rect);
        } else {
            p.surface_damage.push(rect);
        }
        p.check_damage_cap();
        Ok(())
    }

    /// outer None = bogus id, error already on the wire
    fn snapshot_region(&self, id: ObjectId) -> Option<Option<Rc<crate::rect::Region>>> {
        if id == ObjectId::NONE {
            return Some(None);
        }
        match self.client.objects.region(id) {
            Some(region) => Some(Some(region.snapshot())),
            None => {
                self.client.invalid_object(id);
                None
            }
        }
    }
}

impl Object for WlSurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_surface::NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_surface::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.children.borrow_mut().take();
        *self.ext.borrow_mut() = Rc::new(NoneExt);
        self.buffer.borrow_mut().take();
        for cb in self.frame_callbacks.borrow_mut().drain(..) {
            cb.discard();
        }
        for cb in self.pending.borrow_mut().frame_callbacks.drain(..) {
            cb.discard();
        }
    }
}

// -- wl_compositor --

pub struct WlCompositorGlobal;

impl Global for WlCompositorGlobal {
    fn interface(&self) -> &'static str {
        wl_compositor::NAME
    }

    fn version(&self) -> u32 {
        6
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(WlCompositor {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct WlCompositor {
    id: ObjectId,
    client: Rc<Client>,
    version: u32,
}

impl wl_compositor::Handler for WlCompositor {
    fn create_surface(
        &self,
        req: wl_compositor::create_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let surface = WlSurface::new(req.id, &self.client, self.version);
        self.client.add_client_obj(surface.clone())?;
        self.client.objects.track_surface(surface);
        Ok(())
    }

    fn create_region(
        &self,
        req: wl_compositor::create_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let region = Rc::new(WlRegion {
            id: req.id,
            client: self.client.clone(),
            builder: RefCell::new(RegionBuilder::default()),
        });
        self.client.add_client_obj(region.clone())?;
        self.client.objects.track_region(region);
        Ok(())
    }
}

impl Object for WlCompositor {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_compositor::NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_compositor::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_region --

pub struct WlRegion {
    pub id: ObjectId,
    client: Rc<Client>,
    builder: RefCell<RegionBuilder>,
}

impl WlRegion {
    /// immutable snapshot: later add/subtract don't affect it
    pub fn snapshot(&self) -> Rc<crate::rect::Region> {
        self.builder.borrow_mut().get()
    }
}

impl wl_region::Handler for WlRegion {
    fn destroy(&self, _req: wl_region::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.objects.forget_region(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn add(&self, req: wl_region::add::Request) -> Result<(), Box<dyn std::error::Error>> {
        if req.width < 0 || req.height < 0 {
            return Err("region rect with negative dimensions".into());
        }
        self.builder
            .borrow_mut()
            .add(Rect::new_sized_saturating(req.x, req.y, req.width, req.height));
        Ok(())
    }

    fn subtract(&self, req: wl_region::subtract::Request) -> Result<(), Box<dyn std::error::Error>> {
        if req.width < 0 || req.height < 0 {
            return Err("region rect with negative dimensions".into());
        }
        self.builder
            .borrow_mut()
            .sub(Rect::new_sized_saturating(req.x, req.y, req.width, req.height));
        Ok(())
    }
}

impl Object for WlRegion {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_region::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_region::dispatch(&*self, 1, opcode, r)
    }
}

#[cfg(test)]
mod tests {
    use super::subsurface::WlSubcompositor;
    use super::*;
    use crate::client::Client;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::{wl_subcompositor, wl_subsurface};
    use crate::protocol::shm::test_buffer;
    use crate::state::State;
    use wl_surface::Handler as _;

    fn setup() -> (Rc<State>, Rc<Client>, Rc<WlSurface>) {
        let (state, client) = test_client();
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        (state, client, s)
    }

    fn attach_commit(s: &Rc<WlSurface>, buffer: ObjectId) {
        s.attach(wl_surface::attach::Request {
            buffer,
            x: 0,
            y: 0,
        })
        .unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
    }

    fn make_child(
        client: &Rc<Client>,
        parent: &Rc<WlSurface>,
        surface_id: u32,
        sub_id: u32,
    ) -> (Rc<WlSurface>, Rc<WlSubsurface>) {
        use wl_subcompositor::Handler as _;
        let child = WlSurface::new(ObjectId(surface_id), client, 6);
        client.add_client_obj(child.clone()).unwrap();
        client.objects.track_surface(child.clone());
        let sc = WlSubcompositor {
            id: ObjectId(90),
            client: client.clone(),
        };
        sc.get_subsurface(wl_subcompositor::get_subsurface::Request {
            id: ObjectId(sub_id),
            surface: child.id,
            parent: parent.id,
        })
        .unwrap();
        let sub = parent
            .children
            .borrow()
            .as_ref()
            .unwrap()
            .subs
            .get(&child.id)
            .cloned()
            .unwrap();
        (child, sub)
    }

    #[test]
    fn content_gen_moves_on_attach_or_damage_only() {
        let (_st, client, s) = setup();
        let buf = test_buffer(&client, ObjectId(20), 8, 8);
        let g0 = s.content_gen.get();
        attach_commit(&s, buf.id);
        let g1 = s.content_gen.get();
        assert_ne!(g0, g1, "attach bumps");
        // a bare commit leaves the pixels alone
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert_eq!(s.content_gen.get(), g1, "empty commit holds");
        // damage without a fresh attach still bumps
        s.damage(wl_surface::damage::Request {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        })
        .unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert_ne!(s.content_gen.get(), g1, "damage bumps");
    }

    #[test]
    fn mapping_follows_the_buffer() {
        let (_st, client, s) = setup();
        let buf = test_buffer(&client, ObjectId(20), 8, 8);
        assert!(!s.mapped.get());
        attach_commit(&s, buf.id);
        assert!(s.mapped.get());
        assert_eq!(s.size.get(), (8, 8));
        // removing the buffer unmaps
        attach_commit(&s, ObjectId::NONE);
        assert!(!s.mapped.get());
        assert_eq!(s.size.get(), (0, 0));
        assert_eq!(s.buf_x.get(), 0);
    }

    #[test]
    fn destroy_unmaps_immediately() {
        // the wm side unlinks its window later on its own socket; a
        // destroyed surface must already be unhittable by then
        let (_st, client, s) = setup();
        let buf = test_buffer(&client, ObjectId(20), 8, 8);
        attach_commit(&s, buf.id);
        assert!(s.mapped.get());
        s.destroy(wl_surface::destroy::Request {}).unwrap();
        assert!(s.destroyed.get());
        assert!(!s.mapped.get());
    }

    #[test]
    fn release_contract() {
        // shm releases at commit: pixels shadow out, the client buffer is
        // free immediately and clients can single-buffer
        let (_st, client, s) = setup();
        let b1 = test_buffer(&client, ObjectId(20), 8, 8);
        let b2 = test_buffer(&client, ObjectId(21), 8, 8);
        attach_commit(&s, b1.id);
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, b1.id, 0), 1, "released at commit");
        assert!(s.shm_shadow.borrow().is_some(), "pixels shadowed");
        // the replacement releases at its own commit; b1 owes nothing more
        attach_commit(&s, b2.id);
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, b1.id, 0), 1);
        assert_eq!(count_events(&bytes, b2.id, 0), 1);
        // a pending attach replaced before commit is never released
        let b3 = test_buffer(&client, ObjectId(22), 8, 8);
        s.attach(wl_surface::attach::Request { buffer: b3.id, x: 0, y: 0 }).unwrap();
        s.attach(wl_surface::attach::Request { buffer: b2.id, x: 0, y: 0 }).unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, b3.id, 0), 0);
        // every committed attach hands its content over and gets a release
        assert_eq!(count_events(&bytes, b2.id, 0), 2);
        // damage on the still-attached buffer re-captures and re-releases
        s.damage(wl_surface::damage::Request { x: 0, y: 0, width: 4, height: 4 })
            .unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, b2.id, 0), 3);
        // null attach drops the shadow; nothing further to release
        attach_commit(&s, ObjectId::NONE);
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, b2.id, 0), 3);
        assert!(s.shm_shadow.borrow().is_none());
    }

    #[test]
    fn sync_child_commits_wait_for_the_parent() {
        let (_st, client, s) = setup();
        let pbuf = test_buffer(&client, ObjectId(20), 100, 100);
        attach_commit(&s, pbuf.id);
        let (child, sub) = make_child(&client, &s, 11, 30);
        assert!(sub.sync());
        let cbuf = test_buffer(&client, ObjectId(21), 50, 50);
        // the child's commit stashes; nothing applies yet
        attach_commit(&child, cbuf.id);
        assert!(!child.mapped.get());
        assert_eq!(child.size.get(), (0, 0));
        // the parent's commit applies cached state and the pending stack entry
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert!(child.mapped.get());
        assert_eq!(child.size.get(), (50, 50));
        let entry = sub.entry.borrow().clone().unwrap();
        assert!(!entry.pending.get());
    }

    #[test]
    fn desync_applies_immediately() {
        use wl_subsurface::Handler as _;
        let (_st, client, s) = setup();
        let pbuf = test_buffer(&client, ObjectId(20), 100, 100);
        attach_commit(&s, pbuf.id);
        let (child, sub) = make_child(&client, &s, 11, 30);
        s.commit(wl_surface::commit::Request {}).unwrap();
        sub.set_desync(wl_subsurface::set_desync::Request {}).unwrap();
        assert!(!sub.sync());
        let cbuf = test_buffer(&client, ObjectId(21), 50, 50);
        attach_commit(&child, cbuf.id);
        assert!(child.mapped.get());
    }

    #[test]
    fn hit_test_honors_stacking_and_input_regions() {
        use wl_subsurface::Handler as _;
        let (_st, client, s) = setup();
        let pbuf = test_buffer(&client, ObjectId(20), 100, 100);
        attach_commit(&s, pbuf.id);
        let (child, sub) = make_child(&client, &s, 11, 30);
        sub.set_position(wl_subsurface::set_position::Request { x: 25, y: 25 })
            .unwrap();
        let cbuf = test_buffer(&client, ObjectId(21), 50, 50);
        attach_commit(&child, cbuf.id);
        s.commit(wl_surface::commit::Request {}).unwrap();
        // the child is topmost where it overlaps
        let (hit, hx, hy) = s.find_surface_at(30, 30).unwrap();
        assert!(Rc::ptr_eq(&hit, &child));
        assert_eq!((hx, hy), (5, 5));
        // outside the child, the parent takes it
        let (hit, ..) = s.find_surface_at(10, 10).unwrap();
        assert!(Rc::ptr_eq(&hit, &s));
        // punch a hole in the child's input region and fall through
        let hole = {
            let mut b = crate::rect::RegionBuilder::default();
            b.add(Rect::new_sized(0, 0, 50, 50).unwrap());
            b.sub(Rect::new_sized(0, 0, 10, 10).unwrap());
            std::cell::RefCell::new(b)
        };
        child.pending.borrow_mut().input_region = Some(Some(hole.borrow_mut().get()));
        attach_commit(&child, cbuf.id);
        s.commit(wl_surface::commit::Request {}).unwrap();
        let (hit, ..) = s.find_surface_at(30, 30).unwrap();
        assert!(Rc::ptr_eq(&hit, &s), "hole should fall through to the parent");
        let (hit, ..) = s.find_surface_at(40, 40).unwrap();
        assert!(Rc::ptr_eq(&hit, &child));
    }

    #[test]
    fn place_below_reorders_at_parent_commit() {
        use wl_subsurface::Handler as _;
        let (_st, client, s) = setup();
        let pbuf = test_buffer(&client, ObjectId(20), 100, 100);
        attach_commit(&s, pbuf.id);
        let (child, sub) = make_child(&client, &s, 11, 30);
        let cbuf = test_buffer(&client, ObjectId(21), 100, 100);
        attach_commit(&child, cbuf.id);
        s.commit(wl_surface::commit::Request {}).unwrap();
        // child fully covers the parent and sits above
        let (hit, ..) = s.find_surface_at(50, 50).unwrap();
        assert!(Rc::ptr_eq(&hit, &child));
        // request below-the-parent; nothing changes until the commit
        sub.place_below(wl_subsurface::place_below::Request { sibling: s.id })
            .unwrap();
        let (hit, ..) = s.find_surface_at(50, 50).unwrap();
        assert!(Rc::ptr_eq(&hit, &child));
        s.commit(wl_surface::commit::Request {}).unwrap();
        let (hit, ..) = s.find_surface_at(50, 50).unwrap();
        assert!(Rc::ptr_eq(&hit, &s));
    }

    #[test]
    fn regrafted_trees_respect_the_depth_bound() {
        use wl_subcompositor::Handler as _;
        let (_st, client, s) = setup();
        let sc = WlSubcompositor {
            id: ObjectId(90),
            client: client.clone(),
        };
        // grow one level per graft: a fresh root adopts the old tree - the
        // parent's own depth is always 0, only the grafted height climbs
        let mut root = s;
        for i in 0..super::subsurface::MAX_DEPTH {
            let p = WlSurface::new(ObjectId(1000 + i * 2), &client, 6);
            client.add_client_obj(p.clone()).unwrap();
            client.objects.track_surface(p.clone());
            sc.get_subsurface(wl_subcompositor::get_subsurface::Request {
                id: ObjectId(1001 + i * 2),
                surface: root.id,
                parent: p.id,
            })
            .unwrap();
            root = p;
        }
        let errs = count_events(&client.queued_out_bytes(), crate::protocol::WL_DISPLAY_ID, 0);
        assert_eq!(errs, 0, "grafting up to the bound is fine");
        let p = WlSurface::new(ObjectId(3000), &client, 6);
        client.add_client_obj(p.clone()).unwrap();
        client.objects.track_surface(p.clone());
        sc.get_subsurface(wl_subcompositor::get_subsurface::Request {
            id: ObjectId(3001),
            surface: root.id,
            parent: p.id,
        })
        .unwrap();
        let errs = count_events(&client.queued_out_bytes(), crate::protocol::WL_DISPLAY_ID, 0);
        assert_eq!(errs, 1, "one more level is refused");
        assert!(p.children.borrow().is_none(), "the rejected graft did not link");
    }

    #[test]
    fn role_conflicts_and_destroy_order() {
        use wl_subcompositor::Handler as _;
        let (_st, client, s) = setup();
        let (child, _sub) = make_child(&client, &s, 11, 30);
        // a second role object for the same surface is refused
        let sc = WlSubcompositor {
            id: ObjectId(91),
            client: client.clone(),
        };
        let before = count_events(&client.queued_out_bytes(), crate::protocol::WL_DISPLAY_ID, 0);
        sc.get_subsurface(wl_subcompositor::get_subsurface::Request {
            id: ObjectId(31),
            surface: child.id,
            parent: s.id,
        })
        .unwrap();
        // destroying a surface under a live role object is refused
        child.destroy(wl_surface::destroy::Request {}).unwrap();
        let after = count_events(&client.queued_out_bytes(), crate::protocol::WL_DISPLAY_ID, 0);
        assert_eq!(after - before, 2);
        assert!(!child.destroyed.get());
    }
}
