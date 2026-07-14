// xdg-shell: wm_base, surface, toplevel, popup, positioner.
//
// configures are per-surface: a monotonic serial and a scheduled flag draining
// one state-level queue; the flag is the whole debounce mechanism. acks are
// validated (never-issued or non-increasing serials are protocol errors),
// double buffered, and latch at commit.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use crate::state::State;
use crate::surface::{PendingState, SurfaceExt, SurfaceRole, WlSurface};
use crate::tree::{Window, WindowKind};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

// xdg_wm_base errors
pub const ROLE: u32 = 0;
pub const DEFUNCT_SURFACES: u32 = 1;
pub const INVALID_POSITIONER: u32 = 5;
// xdg_surface errors
pub const ALREADY_CONSTRUCTED: u32 = 2;
pub const UNCONFIGURED_BUFFER: u32 = 3;
pub const INVALID_SERIAL: u32 = 4;
pub const INVALID_SIZE: u32 = 5;
pub const DEFUNCT_ROLE_OBJECT: u32 = 6;
// xdg_toplevel errors
pub const TL_INVALID_SIZE: u32 = 2;

// xdg_toplevel state bits, 1 << (state - 1)
const MAXIMIZED: u32 = 1 << 0;
const FULLSCREEN: u32 = 1 << 1;
const ACTIVATED: u32 = 1 << 3;
const TILED_ALL: u32 = 0b1111 << 4;

// wm_capabilities values
const CAP_FULLSCREEN: u32 = 3;

// -- xdg_wm_base --

pub struct XdgWmBaseGlobal;

impl Global for XdgWmBaseGlobal {
    fn interface(&self) -> &'static str {
        xdg_wm_base::NAME
    }

    fn version(&self) -> u32 {
        6
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new_cyclic(|me| XdgWmBase {
            id,
            client: client.clone(),
            version,
            me: me.clone(),
            surfaces: RefCell::new(HashMap::new()),
        }))
    }
}

pub struct XdgWmBase {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    me: Weak<XdgWmBase>,
    surfaces: RefCell<HashMap<ObjectId, Rc<XdgSurface>>>,
}

impl xdg_wm_base::Handler for XdgWmBase {
    fn destroy(
        &self,
        _req: xdg_wm_base::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.surfaces.borrow().is_empty() {
            self.client
                .protocol_error(self.id, DEFUNCT_SURFACES, "xdg_surfaces still exist");
            return Ok(());
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn create_positioner(
        &self,
        req: xdg_wm_base::create_positioner::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = Rc::new(XdgPositioner {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
            v: Cell::new(Positioned::default()),
        });
        self.client.add_client_obj(p.clone())?;
        // client-scoped: a positioner is usable by popups from any
        // xdg_wm_base bind, not just the one that created it
        self.client.objects.track_positioner(p);
        Ok(())
    }

    fn get_xdg_surface(
        &self,
        req: xdg_wm_base::get_xdg_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        if surface.has_live_role() {
            c.protocol_error(self.id, ROLE, "the surface already has a role object");
            return Ok(());
        }
        if surface.buffer.borrow().is_some() {
            c.protocol_error(self.id, ROLE, "the surface already has a committed buffer");
            return Ok(());
        }
        let base = self.me.upgrade().expect("wm_base outlived its own rc");
        let xdg = Rc::new_cyclic(|weak| XdgSurface {
            id: req.id,
            client: c.clone(),
            version: self.version,
            me: weak.clone(),
            base,
            surface: surface.clone(),
            ext: RefCell::new(XdgExt::None),
            popups: RefCell::new(Vec::new()),
            next_serial: Cell::new(1),
            last_sent: Cell::new(0),
            acked: Cell::new(0),
            committed_ack: Cell::new(0),
            ack_floor: Cell::new(0),
            scheduled: Cell::new(false),
            configured: Cell::new(false),
            pending_geom: Cell::new(None),
            geom: Cell::new(None),
        });
        c.add_client_obj(xdg.clone())?;
        *surface.ext.borrow_mut() = Rc::new(XdgSurfaceExt { xdg: xdg.clone() });
        self.surfaces.borrow_mut().insert(req.id, xdg);
        Ok(())
    }

    fn pong(&self, _req: xdg_wm_base::pong::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

impl Object for XdgWmBase {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_wm_base::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_wm_base::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.surfaces.borrow_mut().clear();
    }
}

// -- xdg_positioner --

/// pure value object; snapshotted at get_popup time
#[derive(Copy, Clone, Default)]
struct Positioned {
    size: (i32, i32),
    anchor_rect: Rect,
    anchor: u32,
    gravity: u32,
    offset: (i32, i32),
    ca: u32,
}

// edge values: 1 top, 2 bottom, 3 left, 4 right, then the corners
fn mirror_x(v: u32) -> u32 {
    match v {
        3 => 4,
        4 => 3,
        5 => 7,
        7 => 5,
        6 => 8,
        8 => 6,
        other => other,
    }
}

fn mirror_y(v: u32) -> u32 {
    match v {
        1 => 2,
        2 => 1,
        5 => 6,
        6 => 5,
        7 => 8,
        8 => 7,
        other => other,
    }
}

impl Positioned {
    /// anchor point on the rect, then extend away from it per gravity
    fn place(&self) -> (i32, i32) {
        let r = self.anchor_rect;
        let ax = match self.anchor {
            3 | 5 | 6 => r.x1,
            4 | 7 | 8 => r.x2,
            _ => (r.x1 + r.x2) / 2,
        };
        let ay = match self.anchor {
            1 | 5 | 7 => r.y1,
            2 | 6 | 8 => r.y2,
            _ => (r.y1 + r.y2) / 2,
        };
        let (w, h) = self.size;
        let x = match self.gravity {
            3 | 5 | 6 => ax - w,
            4 | 7 | 8 => ax,
            _ => ax - w / 2,
        };
        let y = match self.gravity {
            1 | 5 | 7 => ay - h,
            2 | 6 | 8 => ay,
            _ => ay - h / 2,
        };
        (x + self.offset.0, y + self.offset.1)
    }

    /// constraint solving, spec order per axis: flip, slide, resize.
    /// parent_org is the absolute origin of the parent's geometry; the
    /// result is parent-relative again
    fn solve(&self, parent_org: (i32, i32), bounds: Rect) -> ((i32, i32), (i32, i32)) {
        const SLIDE_X: u32 = 1;
        const SLIDE_Y: u32 = 2;
        const FLIP_X: u32 = 4;
        const FLIP_Y: u32 = 8;
        const RESIZE_X: u32 = 16;
        const RESIZE_Y: u32 = 32;
        let (mut w, mut h) = self.size;
        let abs = |p: &Positioned| {
            let (x, y) = p.place();
            (parent_org.0 + x, parent_org.1 + y)
        };
        let (mut x, mut y) = abs(self);
        // a flip only sticks when it actually unconstrains the axis
        if self.ca & FLIP_X != 0 && (x < bounds.x1 || x + w > bounds.x2) {
            let mut f = *self;
            f.anchor = mirror_x(f.anchor);
            f.gravity = mirror_x(f.gravity);
            let (fx, _) = abs(&f);
            if fx >= bounds.x1 && fx + w <= bounds.x2 {
                x = fx;
            }
        }
        if self.ca & FLIP_Y != 0 && (y < bounds.y1 || y + h > bounds.y2) {
            let mut f = *self;
            f.anchor = mirror_y(f.anchor);
            f.gravity = mirror_y(f.gravity);
            let (_, fy) = abs(&f);
            if fy >= bounds.y1 && fy + h <= bounds.y2 {
                y = fy;
            }
        }
        if self.ca & SLIDE_X != 0 {
            x = x.min(bounds.x2 - w).max(bounds.x1);
        }
        if self.ca & SLIDE_Y != 0 {
            y = y.min(bounds.y2 - h).max(bounds.y1);
        }
        if self.ca & RESIZE_X != 0 {
            if x < bounds.x1 {
                w -= bounds.x1 - x;
                x = bounds.x1;
            }
            w = w.min(bounds.x2 - x).max(1);
        }
        if self.ca & RESIZE_Y != 0 {
            if y < bounds.y1 {
                h -= bounds.y1 - y;
                y = bounds.y1;
            }
            h = h.min(bounds.y2 - y).max(1);
        }
        ((x - parent_org.0, y - parent_org.1), (w, h))
    }
}

pub struct XdgPositioner {
    pub id: ObjectId,
    pub client: Rc<Client>,
    version: u32,
    v: Cell<Positioned>,
}

impl XdgPositioner {
    fn edit(&self, f: impl FnOnce(&mut Positioned)) {
        let mut v = self.v.get();
        f(&mut v);
        self.v.set(v);
    }
}

impl xdg_positioner::Handler for XdgPositioner {
    fn destroy(
        &self,
        _req: xdg_positioner::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.objects.forget_positioner(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_size(
        &self,
        req: xdg_positioner::set_size::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.width <= 0 || req.height <= 0 {
            self.client
                .protocol_error(self.id, 0, "positioner size must be positive");
            return Ok(());
        }
        self.edit(|v| v.size = (req.width, req.height));
        Ok(())
    }

    fn set_anchor_rect(
        &self,
        req: xdg_positioner::set_anchor_rect::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.width < 0 || req.height < 0 {
            self.client
                .protocol_error(self.id, 0, "anchor rect size must be non-negative");
            return Ok(());
        }
        let r = Rect::new_sized_saturating(req.x, req.y, req.width, req.height);
        self.edit(|v| v.anchor_rect = r);
        Ok(())
    }

    fn set_anchor(
        &self,
        req: xdg_positioner::set_anchor::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.anchor > 8 {
            self.client.protocol_error(self.id, 0, "invalid anchor");
            return Ok(());
        }
        self.edit(|v| v.anchor = req.anchor);
        Ok(())
    }

    fn set_gravity(
        &self,
        req: xdg_positioner::set_gravity::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.gravity > 8 {
            self.client.protocol_error(self.id, 0, "invalid gravity");
            return Ok(());
        }
        self.edit(|v| v.gravity = req.gravity);
        Ok(())
    }

    fn set_constraint_adjustment(
        &self,
        req: xdg_positioner::set_constraint_adjustment::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.constraint_adjustment > 63 {
            self.client
                .protocol_error(self.id, 0, "invalid constraint adjustment");
            return Ok(());
        }
        self.edit(|v| v.ca = req.constraint_adjustment);
        Ok(())
    }

    fn set_offset(
        &self,
        req: xdg_positioner::set_offset::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.edit(|v| v.offset = (req.x, req.y));
        Ok(())
    }

    fn set_reactive(
        &self,
        _req: xdg_positioner::set_reactive::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_parent_size(
        &self,
        _req: xdg_positioner::set_parent_size::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_parent_configure(
        &self,
        _req: xdg_positioner::set_parent_configure::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

impl Object for XdgPositioner {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_positioner::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_positioner::dispatch(&*self, self.version, opcode, r)
    }
}

// -- xdg_surface --

enum XdgExt {
    None,
    Toplevel(Rc<XdgToplevel>),
    Popup(Rc<XdgPopup>),
}

pub struct XdgSurface {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    me: Weak<XdgSurface>,
    base: Rc<XdgWmBase>,
    pub surface: Rc<WlSurface>,
    ext: RefCell<XdgExt>,
    popups: RefCell<Vec<Rc<XdgPopup>>>,
    next_serial: Cell<u32>,
    last_sent: Cell<u32>,
    /// highest ack received; latches into committed_ack at commit
    acked: Cell<u32>,
    committed_ack: Cell<u32>,
    /// acks at or below this belong to a previous map cycle
    ack_floor: Cell<u32>,
    scheduled: Cell<bool>,
    configured: Cell<bool>,
    pending_geom: Cell<Option<Rect>>,
    geom: Cell<Option<Rect>>,
}

impl XdgSurface {
    fn rc(&self) -> Rc<XdgSurface> {
        self.me.upgrade().expect("xdg surface outlived its own rc")
    }

    /// effective geometry: the set rect, else the surface extents
    pub fn geometry(&self) -> Rect {
        match self.geom.get() {
            Some(g) => g,
            None => self.surface.extents.get(),
        }
    }

    pub fn toplevel(&self) -> Option<Rc<XdgToplevel>> {
        match &*self.ext.borrow() {
            XdgExt::Toplevel(tl) => Some(tl.clone()),
            _ => None,
        }
    }

    pub fn popup(&self) -> Option<Rc<XdgPopup>> {
        match &*self.ext.borrow() {
            XdgExt::Popup(p) => Some(p.clone()),
            _ => None,
        }
    }

    // absolute origin of this xdg surface's geometry on screen
    fn abs_origin(&self) -> Option<(i32, i32)> {
        match &*self.ext.borrow() {
            XdgExt::Toplevel(tl) => {
                let win = tl.window.borrow().clone()?;
                let r = win.draw_rect(&self.client.state);
                Some((r.x1, r.y1))
            }
            XdgExt::Popup(p) => {
                let (px, py) = p.parent_origin()?;
                let (rx, ry) = p.rel.get();
                Some((px + rx, py + ry))
            }
            XdgExt::None => None,
        }
    }

    pub fn schedule_configure(&self) {
        if !self.scheduled.replace(true) {
            let state = &self.client.state;
            state.configures.borrow_mut().push(self.rc());
            state.configure_event.trigger();
        }
    }

    fn send_configure_now(&self) {
        let serial = self.next_serial.get();
        self.next_serial.set(serial.wrapping_add(1).max(1));
        self.last_sent.set(serial);
        match &*self.ext.borrow() {
            XdgExt::Toplevel(tl) => {
                let (w, h) = tl.desired.get();
                let states = tl.states_bytes();
                if tl.last_logged.replace((w, h)) != (w, h) {
                    crate::trace!("configure {}x{} -> {}", w, h, tl.title.borrow());
                }
                self.client.event(|o| {
                    xdg_toplevel::configure::send(o, tl.id, w, h, &states);
                    xdg_surface::configure::send(o, self.id, serial);
                });
            }
            XdgExt::Popup(p) => {
                let (x, y) = p.rel.get();
                let (w, h) = p.size.get();
                self.client.event(|o| {
                    xdg_popup::configure::send(o, p.id, x, y, w, h);
                    xdg_surface::configure::send(o, self.id, serial);
                });
            }
            XdgExt::None => {}
        }
    }

    fn unlink_popup(&self, popup: &XdgPopup) {
        self.popups.borrow_mut().retain(|p| p.id != popup.id);
    }

    pub fn for_each_popup(&self, mut f: impl FnMut(&Rc<XdgPopup>)) {
        for p in self.popups.borrow().iter() {
            f(p);
        }
    }

    /// siblings in reverse: a new popup stacks above all previously created ones
    pub fn for_each_popup_rev(&self, mut f: impl FnMut(&Rc<XdgPopup>)) {
        for p in self.popups.borrow().iter().rev() {
            f(p);
        }
    }
}

impl xdg_surface::Handler for XdgSurface {
    fn destroy(
        &self,
        _req: xdg_surface::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !matches!(&*self.ext.borrow(), XdgExt::None) {
            self.client
                .protocol_error(self.id, DEFUNCT_ROLE_OBJECT, "the role object still exists");
            return Ok(());
        }
        *self.surface.ext.borrow_mut() = Rc::new(crate::surface::NoneExt);
        self.base.surfaces.borrow_mut().remove(&self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_toplevel(
        &self,
        req: xdg_surface::get_toplevel::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if !matches!(&*self.ext.borrow(), XdgExt::None) {
            c.protocol_error(self.id, ALREADY_CONSTRUCTED, "a role object already exists");
            return Ok(());
        }
        if let Err(old) = self.surface.set_role(SurfaceRole::Toplevel) {
            c.protocol_error(
                self.id,
                ROLE,
                &format!("the surface already has the {} role", old.name()),
            );
            return Ok(());
        }
        let base = if self.version >= 2 { TILED_ALL } else { MAXIMIZED };
        let tl = Rc::new(XdgToplevel {
            id: req.id,
            client: c.clone(),
            version: self.version,
            xdg: self.rc(),
            window: RefCell::new(None),
            title: RefCell::new(String::new()),
            app_id: RefCell::new(String::new()),
            pending_min: Cell::new((0, 0)),
            pending_max: Cell::new((0, 0)),
            min_size: Cell::new((0, 0)),
            max_size: Cell::new((0, 0)),
            states: Cell::new(base),
            desired: Cell::new((0, 0)),
            last_logged: Cell::new((0, 0)),
        });
        c.add_client_obj(tl.clone())?;
        c.objects.track_toplevel(tl.clone());
        if self.version >= 5 {
            c.event(|o| {
                xdg_toplevel::wm_capabilities::send(o, tl.id, &CAP_FULLSCREEN.to_ne_bytes())
            });
        }
        *self.ext.borrow_mut() = XdgExt::Toplevel(tl);
        Ok(())
    }

    fn get_popup(
        &self,
        req: xdg_surface::get_popup::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if !matches!(&*self.ext.borrow(), XdgExt::None) {
            c.protocol_error(self.id, ALREADY_CONSTRUCTED, "a role object already exists");
            return Ok(());
        }
        if let Err(old) = self.surface.set_role(SurfaceRole::Popup) {
            c.protocol_error(
                self.id,
                ROLE,
                &format!("the surface already has the {} role", old.name()),
            );
            return Ok(());
        }
        let parent = if req.parent == ObjectId::NONE {
            None
        } else {
            let p = self.base.surfaces.borrow().get(&req.parent).cloned();
            match p {
                Some(p) => Some(p),
                None => {
                    c.invalid_object(req.parent);
                    return Ok(());
                }
            }
        };
        let Some(positioner) = c.objects.positioner(req.positioner) else {
            c.invalid_object(req.positioner);
            return Ok(());
        };
        let pos = positioner.v.get();
        if pos.size.0 == 0 || pos.size.1 == 0 {
            c.protocol_error(self.id, INVALID_POSITIONER, "positioner is incomplete");
            return Ok(());
        }
        let popup = Rc::new(XdgPopup {
            id: req.id,
            client: c.clone(),
            version: self.version,
            xdg: self.rc(),
            parent: RefCell::new(parent.clone().map(PopupParent::Xdg)),
            positioned: Cell::new(pos),
            rel: Cell::new(pos.place()),
            size: Cell::new(pos.size),
            done: Cell::new(false),
        });
        c.add_client_obj(popup.clone())?;
        c.objects.track_popup(popup.clone());
        if let Some(p) = &parent {
            p.popups.borrow_mut().push(popup.clone());
        }
        *self.ext.borrow_mut() = XdgExt::Popup(popup);
        Ok(())
    }

    fn set_window_geometry(
        &self,
        req: xdg_surface::set_window_geometry::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // ignore a 0x0 geometry rather than erroring; clients send it spuriously
        if req.width == 0 && req.height == 0 {
            return Ok(());
        }
        if req.width <= 0 || req.height <= 0 {
            self.client
                .protocol_error(self.id, INVALID_SIZE, "window geometry must be positive");
            return Ok(());
        }
        self.pending_geom
            .set(Some(Rect::new_sized_saturating(req.x, req.y, req.width, req.height)));
        Ok(())
    }

    fn ack_configure(
        &self,
        req: xdg_surface::ack_configure::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.serial == 0 || req.serial > self.last_sent.get() {
            self.client
                .protocol_error(self.id, INVALID_SERIAL, "ack of a serial that was never sent");
            return Ok(());
        }
        if req.serial <= self.acked.get() {
            self.client
                .protocol_error(self.id, INVALID_SERIAL, "ack serials must increase");
            return Ok(());
        }
        self.acked.set(req.serial);
        Ok(())
    }
}

impl Object for XdgSurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_surface::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_surface::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        *self.ext.borrow_mut() = XdgExt::None;
        self.popups.borrow_mut().clear();
    }
}

// -- the wl_surface role hook --

pub struct XdgSurfaceExt {
    pub xdg: Rc<XdgSurface>,
}

impl SurfaceExt for XdgSurfaceExt {
    fn commit_requested(self: Rc<Self>, pending: Box<PendingState>) -> Option<Box<PendingState>> {
        // buffer legal only after this map cycle's initial configure was acked; pre-unmap acks don't count
        let attaching = matches!(&pending.buffer, Some(Some(_)));
        if attaching && self.xdg.acked.get() <= self.xdg.ack_floor.get() {
            self.xdg.client.protocol_error(
                self.xdg.id,
                UNCONFIGURED_BUFFER,
                "buffer attached before the initial configure was acked",
            );
            return None;
        }
        Some(pending)
    }

    fn before_apply(&self) {
        let x = &self.xdg;
        x.committed_ack.set(x.committed_ack.get().max(x.acked.get()));
        if let Some(g) = x.pending_geom.take() {
            x.geom.set(Some(g));
        }
        if let XdgExt::Toplevel(tl) = &*x.ext.borrow() {
            tl.latch_limits();
        }
    }

    fn after_apply(&self) {
        let x = &self.xdg;
        let ext = x.ext.borrow();
        match &*ext {
            XdgExt::Toplevel(tl) => {
                let tl = tl.clone();
                drop(ext);
                if !x.configured.get() {
                    // first commit on an unconfigured toplevel: full configure, map nothing
                    x.configured.set(true);
                    x.schedule_configure();
                    return;
                }
                let mapped = x.surface.mapped.get();
                let in_tree = tl.window.borrow().is_some();
                if mapped && !in_tree {
                    let win = Rc::new(Window::new(&x.client.state, WindowKind::Xdg(tl.clone())));
                    *tl.window.borrow_mut() = Some(win.clone());
                    crate::tree::map_window(&x.client.state, &win);
                } else if !mapped && in_tree {
                    let win = tl.window.borrow_mut().take().unwrap();
                    crate::tree::unmap_window(&x.client.state, &win);
                    tl.reset_after_unmap();
                }
            }
            XdgExt::Popup(p) => {
                let p = p.clone();
                drop(ext);
                if !x.configured.get() {
                    // the parent is on screen by now, so this is where the
                    // positioner constraints can actually be solved
                    p.solve_position();
                    x.configured.set(true);
                    x.schedule_configure();
                    return;
                }
                if !x.surface.mapped.get() {
                    // an unmapped grabbing popup can't hold the keyboard
                    popup_closed(&x.client.state, &p);
                }
            }
            XdgExt::None => {}
        }
    }

    fn set_active(&self, active: bool) {
        if let XdgExt::Toplevel(tl) = &*self.xdg.ext.borrow() {
            tl.set_activated(active);
        }
    }
}

// -- xdg_toplevel --

pub struct XdgToplevel {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub xdg: Rc<XdgSurface>,
    pub window: RefCell<Option<Rc<Window>>>,
    pub title: RefCell<String>,
    pub app_id: RefCell<String>,
    pending_min: Cell<(i32, i32)>,
    pending_max: Cell<(i32, i32)>,
    pub min_size: Cell<(i32, i32)>,
    pub max_size: Cell<(i32, i32)>,
    states: Cell<u32>,
    pub desired: Cell<(i32, i32)>,
    /// last size printed to the log; dedupes the configure trace
    last_logged: Cell<(i32, i32)>,
}

impl XdgToplevel {
    fn states_bytes(&self) -> Vec<u8> {
        let bits = self.states.get();
        let mut out = Vec::with_capacity(6 * 4);
        for s in 1..=9u32 {
            if bits & (1 << (s - 1)) == 0 {
                continue;
            }
            // suspended is v6+; the constructor version-gated the rest
            if s == 9 && self.version < 6 {
                continue;
            }
            out.extend(s.to_ne_bytes());
        }
        out
    }

    fn latch_limits(&self) {
        let min = self.pending_min.get();
        let max = self.pending_max.get();
        if min.0 > 0 && max.0 > 0 && min.0 > max.0 || min.1 > 0 && max.1 > 0 && min.1 > max.1 {
            self.client
                .protocol_error(self.id, TL_INVALID_SIZE, "min size exceeds max size");
            return;
        }
        self.min_size.set(min);
        self.max_size.set(max);
    }

    pub fn set_activated(&self, active: bool) {
        let old = self.states.get();
        let new = if active { old | ACTIVATED } else { old & !ACTIVATED };
        if new != old {
            self.states.set(new);
            self.xdg.schedule_configure();
        }
    }

    pub fn set_fullscreen_state(&self, on: bool) {
        let old = self.states.get();
        let new = if on { old | FULLSCREEN } else { old & !FULLSCREEN };
        self.states.set(new);
        self.xdg.schedule_configure();
    }

    /// a pre-map set_fullscreen leaves only this bit; map picks it up
    pub fn wants_fullscreen(&self) -> bool {
        self.states.get() & FULLSCREEN != 0
    }

    pub fn configure_size(&self, w: i32, h: i32) {
        // state-only changes schedule directly; an unchanged size is a
        // no-op, not a configure broadcast
        if self.desired.replace((w, h)) == (w, h) {
            return;
        }
        self.xdg.schedule_configure();
    }

    pub fn send_close(&self) {
        self.client.event(|o| xdg_toplevel::close::send(o, self.id));
    }

    /// unmap drops dynamic state; the next buffer reruns the initial-configure cycle
    fn reset_after_unmap(&self) {
        let base = if self.version >= 2 { TILED_ALL } else { MAXIMIZED };
        self.states.set(base);
        self.desired.set((0, 0));
        self.xdg.configured.set(false);
        self.xdg.ack_floor.set(self.xdg.last_sent.get());
        self.xdg.geom.set(None);
        self.xdg.pending_geom.set(None);
    }

    fn detach_from_tree(&self) {
        if let Some(win) = self.window.borrow_mut().take() {
            crate::tree::unmap_window(&self.client.state, &win);
        }
    }
}

impl xdg_toplevel::Handler for XdgToplevel {
    fn destroy(
        &self,
        _req: xdg_toplevel::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.detach_from_tree();
        self.reset_after_unmap();
        *self.xdg.ext.borrow_mut() = XdgExt::None;
        self.client.objects.forget_toplevel(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_parent(
        &self,
        _req: xdg_toplevel::set_parent::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // stored once float placement wants it
        Ok(())
    }

    fn set_title(
        &self,
        req: xdg_toplevel::set_title::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *self.title.borrow_mut() = req.title.to_string();
        if let Some(win) = self.window.borrow().clone() {
            crate::protocol::foreign_toplevel::title_changed(&self.client.state, &win);
        }
        Ok(())
    }

    fn set_app_id(
        &self,
        req: xdg_toplevel::set_app_id::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *self.app_id.borrow_mut() = req.app_id.to_string();
        if let Some(win) = self.window.borrow().clone() {
            crate::protocol::foreign_toplevel::app_id_changed(&self.client.state, &win);
        }
        Ok(())
    }

    fn show_window_menu(
        &self,
        _req: xdg_toplevel::show_window_menu::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn r#move(&self, req: xdg_toplevel::r#move::Request) -> Result<(), Box<dyn std::error::Error>> {
        let Some(seat) = self.client.state.seat.borrow().clone() else {
            return Ok(());
        };
        let Some(win) = self.window.borrow().clone() else {
            return Ok(());
        };
        if seat.move_resize_grab_valid(&self.xdg.surface, req.serial) {
            seat.start_move_grab(&self.client.state, win);
        }
        Ok(())
    }

    fn resize(&self, req: xdg_toplevel::resize::Request) -> Result<(), Box<dyn std::error::Error>> {
        if req.edges > 10 || req.edges == 3 || req.edges == 7 {
            self.client.protocol_error(self.id, 0, "invalid resize edge");
            return Ok(());
        }
        if req.edges == 0 {
            return Ok(());
        }
        let Some(seat) = self.client.state.seat.borrow().clone() else {
            return Ok(());
        };
        let Some(win) = self.window.borrow().clone() else {
            return Ok(());
        };
        if seat.move_resize_grab_valid(&self.xdg.surface, req.serial) {
            seat.start_resize_grab(&self.client.state, win, req.edges);
        }
        Ok(())
    }

    fn set_max_size(
        &self,
        req: xdg_toplevel::set_max_size::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.width < 0 || req.height < 0 {
            self.client
                .protocol_error(self.id, TL_INVALID_SIZE, "max size must be non-negative");
            return Ok(());
        }
        self.pending_max.set((req.width, req.height));
        Ok(())
    }

    fn set_min_size(
        &self,
        req: xdg_toplevel::set_min_size::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.width < 0 || req.height < 0 {
            self.client
                .protocol_error(self.id, TL_INVALID_SIZE, "min size must be non-negative");
            return Ok(());
        }
        self.pending_min.set((req.width, req.height));
        Ok(())
    }

    fn set_maximized(
        &self,
        _req: xdg_toplevel::set_maximized::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // no maximize concept, but the spec wants a configure answer
        self.xdg.schedule_configure();
        Ok(())
    }

    fn unset_maximized(
        &self,
        _req: xdg_toplevel::unset_maximized::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.xdg.schedule_configure();
        Ok(())
    }

    fn set_fullscreen(
        &self,
        _req: xdg_toplevel::set_fullscreen::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.set_fullscreen_state(true);
        if let Some(win) = &*self.window.borrow() {
            crate::tree::set_fullscreen(&self.client.state, win, true);
        }
        Ok(())
    }

    fn unset_fullscreen(
        &self,
        _req: xdg_toplevel::unset_fullscreen::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.set_fullscreen_state(false);
        if let Some(win) = &*self.window.borrow() {
            crate::tree::set_fullscreen(&self.client.state, win, false);
        }
        Ok(())
    }

    fn set_minimized(
        &self,
        _req: xdg_toplevel::set_minimized::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

impl Object for XdgToplevel {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_toplevel::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_toplevel::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.detach_from_tree();
        *self.window.borrow_mut() = None;
    }
}

// -- xdg_popup --

// a popup's parent is another xdg surface, or - via layer get_popup - a
// layer surface that adopts a popup created with a null parent
#[derive(Clone)]
pub enum PopupParent {
    Xdg(Rc<XdgSurface>),
    Layer(Rc<crate::shell::layer::LayerSurface>),
}

impl PopupParent {
    fn abs_origin(&self) -> Option<(i32, i32)> {
        match self {
            PopupParent::Xdg(x) => x.abs_origin(),
            PopupParent::Layer(l) => {
                let r = l.rect.get();
                Some((r.x1, r.y1))
            }
        }
    }

    /// what the positioner constrains against: the parent's output stands
    /// in for the compositor work area; the union extent headless
    fn constraint_bounds(&self, state: &Rc<State>) -> Rect {
        match self {
            PopupParent::Layer(l) => crate::shell::layer::slot_rect(state, l.output.get()),
            PopupParent::Xdg(x) => {
                if let Some(tl) = x.toplevel() {
                    let win = tl.window.borrow().clone();
                    if let Some(win) = win {
                        if let Some(ws) = crate::tree::workspace_of(state, &win) {
                            return crate::tree::workspace_output_rect(state, &ws);
                        }
                    }
                } else if let Some(p) = x.popup() {
                    let parent = p.parent.borrow().clone();
                    if let Some(parent) = parent {
                        return parent.constraint_bounds(state);
                    }
                }
                let (w, h) = crate::tree::output_extent(state);
                Rect::new_sized_saturating(0, 0, w.max(1), h.max(1))
            }
        }
    }
}

/// renders above its parent at the positioner's chosen spot
pub struct XdgPopup {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub xdg: Rc<XdgSurface>,
    parent: RefCell<Option<PopupParent>>,
    positioned: Cell<Positioned>,
    /// relative to the parent's window geometry origin
    pub rel: Cell<(i32, i32)>,
    pub size: Cell<(i32, i32)>,
    done: Cell<bool>,
}

impl XdgPopup {
    pub fn send_done(&self) {
        if !self.done.replace(true) {
            self.client.event(|o| xdg_popup::popup_done::send(o, self.id));
        }
    }

    fn me(&self) -> Option<Rc<XdgPopup>> {
        match &*self.xdg.ext.borrow() {
            XdgExt::Popup(p) => Some(p.clone()),
            _ => None,
        }
    }

    pub fn set_layer_parent(&self, ls: &Rc<crate::shell::layer::LayerSurface>) {
        *self.parent.borrow_mut() = Some(PopupParent::Layer(ls.clone()));
    }

    pub fn has_parent(&self) -> bool {
        self.parent.borrow().is_some()
    }

    /// absolute origin of the parent's geometry, walking nested popups
    /// down to the toplevel's window rect or the layer surface's slot
    fn parent_origin(&self) -> Option<(i32, i32)> {
        let parent = self.parent.borrow().clone()?;
        parent.abs_origin()
    }

    fn solve_position(&self) {
        let parent = self.parent.borrow().clone();
        let Some(parent) = parent else {
            return;
        };
        let Some(org) = parent.abs_origin() else {
            return;
        };
        let bounds = parent.constraint_bounds(&self.client.state);
        let (rel, size) = self.positioned.get().solve(org, bounds);
        self.rel.set(rel);
        self.size.set(size);
    }
}

impl xdg_popup::Handler for XdgPopup {
    fn destroy(&self, _req: xdg_popup::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(me) = self.me() {
            popup_closed(&self.client.state, &me);
        }
        match self.parent.borrow_mut().take() {
            Some(PopupParent::Xdg(p)) => p.unlink_popup(self),
            Some(PopupParent::Layer(l)) => l.unlink_popup(self.id),
            None => {}
        }
        *self.xdg.ext.borrow_mut() = XdgExt::None;
        self.xdg.configured.set(false);
        self.client.objects.forget_popup(self.id);
        self.client.state.damage.trigger();
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn grab(&self, _req: xdg_popup::grab::Request) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        let Some(seat) = state.seat.borrow().clone() else {
            return Ok(());
        };
        let Some(me) = self.me() else {
            return Ok(());
        };
        {
            let mut stack = seat.popup_grab.borrow_mut();
            // only the topmost grabbing popup may parent another grab
            let ok = match stack.last() {
                None => true,
                Some(top) => matches!(
                    &*self.parent.borrow(),
                    Some(PopupParent::Xdg(p)) if Rc::ptr_eq(p, &top.xdg)
                ),
            };
            if !ok {
                drop(stack);
                self.client
                    .protocol_error(self.id, 0, "grab on a popup that is not the topmost");
                return Ok(());
            }
            if stack.is_empty() {
                *seat.grab_prev_focus.borrow_mut() = seat.kb_focus.borrow().clone();
            }
            stack.push(me);
        }
        crate::input::focus::set_keyboard_focus(state, &seat, Some(self.xdg.surface.clone()));
        Ok(())
    }

    fn reposition(
        &self,
        req: xdg_popup::reposition::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(positioner) = self.client.objects.positioner(req.positioner) else {
            self.client.invalid_object(req.positioner);
            return Ok(());
        };
        self.positioned.set(positioner.v.get());
        self.solve_position();
        self.client
            .event(|o| xdg_popup::repositioned::send(o, self.id, req.token));
        self.xdg.schedule_configure();
        Ok(())
    }
}

// a popup left the screen (destroy or unmap): drop it from the grab
// chain and put the keyboard back where it belongs
pub fn popup_closed(state: &Rc<State>, popup: &Rc<XdgPopup>) {
    let Some(seat) = state.seat.borrow().clone() else {
        return;
    };
    let next = {
        let mut stack = seat.popup_grab.borrow_mut();
        let before = stack.len();
        stack.retain(|p| !Rc::ptr_eq(p, popup));
        if stack.len() == before {
            return;
        }
        stack.last().cloned()
    };
    let target = match next {
        Some(p) => Some(p.xdg.surface.clone()),
        None => seat.grab_prev_focus.borrow_mut().take(),
    };
    let target = target.filter(|s| !s.destroyed.get());
    crate::input::focus::set_keyboard_focus(state, &seat, target);
    // an exclusive layer lock that yielded to the grab re-asserts
    if seat.popup_grab.borrow().is_empty() {
        crate::shell::layer::apply_kb_lock(state);
    }
    // the pointer may have been parked on the popup that just left
    seat.repick(state);
}

// click outside the grab chain: every grabbing popup gets popup_done,
// topmost first, and the keyboard returns to the pre-grab owner
pub fn dismiss_popup_grabs(state: &Rc<State>, seat: &Rc<crate::input::seat::SeatGlobal>) {
    let stack: Vec<_> = seat.popup_grab.borrow_mut().drain(..).collect();
    if stack.is_empty() {
        return;
    }
    for p in stack.iter().rev() {
        p.send_done();
    }
    let prev = seat.grab_prev_focus.borrow_mut().take();
    let prev = prev.filter(|s| !s.destroyed.get());
    crate::input::focus::set_keyboard_focus(state, seat, prev);
    state.damage.trigger();
}

impl Object for XdgPopup {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_popup::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_popup::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        *self.parent.borrow_mut() = None;
    }
}

// -- xdg-decoration --

// the answer is always server_side: carrot draws the borders, clients
// keep their pixels to themselves
const DECO_CLIENT_SIDE: u32 = 1;
const DECO_SERVER_SIDE: u32 = 2;

pub struct XdgDecorationManagerGlobal;

impl Global for XdgDecorationManagerGlobal {
    fn interface(&self) -> &'static str {
        zxdg_decoration_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(XdgDecorationManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct XdgDecorationManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zxdg_decoration_manager_v1::Handler for XdgDecorationManager {
    fn destroy(
        &self,
        _req: zxdg_decoration_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_toplevel_decoration(
        &self,
        req: zxdg_decoration_manager_v1::get_toplevel_decoration::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(toplevel) = self.client.objects.toplevel(req.toplevel) else {
            self.client.invalid_object(req.toplevel);
            return Ok(());
        };
        let deco = Rc::new(XdgDecoration {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
            toplevel,
            requested: std::cell::Cell::new(None),
        });
        self.client.add_client_obj(deco.clone())?;
        deco.announce();
        Ok(())
    }
}

impl Object for XdgDecorationManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zxdg_decoration_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zxdg_decoration_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct XdgDecoration {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    toplevel: Rc<XdgToplevel>,
    /// the client's set_mode wish; None means it left the choice to us
    requested: std::cell::Cell<Option<u32>>,
}

impl XdgDecoration {
    // decoration configure first, then the xdg_surface configure that
    // makes it take effect
    fn announce(&self) {
        // prefer-no-csd overrides the client's wish; otherwise it decides
        let mode = if self.client.state.config.borrow().prefer_no_csd {
            DECO_SERVER_SIDE
        } else {
            self.requested.get().unwrap_or(DECO_SERVER_SIDE)
        };
        self.client
            .event(|o| zxdg_toplevel_decoration_v1::configure::send(o, self.id, mode));
        self.toplevel.xdg.schedule_configure();
    }
}

impl zxdg_toplevel_decoration_v1::Handler for XdgDecoration {
    fn destroy(
        &self,
        _req: zxdg_toplevel_decoration_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_mode(
        &self,
        req: zxdg_toplevel_decoration_v1::set_mode::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.requested
            .set((req.mode == DECO_CLIENT_SIDE).then_some(DECO_CLIENT_SIDE));
        self.announce();
        Ok(())
    }

    fn unset_mode(
        &self,
        _req: zxdg_toplevel_decoration_v1::unset_mode::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.requested.set(None);
        self.announce();
        Ok(())
    }
}

impl Object for XdgDecoration {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zxdg_toplevel_decoration_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zxdg_toplevel_decoration_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the flush task --

impl crate::shell::Configurable for XdgSurface {
    fn flush_configure(&self) {
        self.scheduled.set(false);
        if !self.surface.destroyed.get() {
            self.send_configure_now();
        }
    }
}

pub fn flush_configures(state: &Rc<State>) {
    loop {
        let batch: Vec<_> = state.configures.borrow_mut().drain(..).collect();
        if batch.is_empty() {
            return;
        }
        for s in batch {
            s.flush_configure();
        }
    }
}

pub async fn configure_loop(state: Rc<State>) {
    loop {
        state.configure_event.triggered().await;
        flush_configures(&state);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::wl_surface;
    use crate::protocol::shm::test_buffer;
    use wl_surface::Handler as _;
    use xdg_surface::Handler as _;
    use xdg_toplevel::Handler as _;
    use xdg_wm_base::Handler as _;

    const ERR: ObjectId = ObjectId(1);

    pub(crate) fn mk_base(client: &Rc<Client>, id: u32) -> Rc<XdgWmBase> {
        let base = Rc::new_cyclic(|me| XdgWmBase {
            id: ObjectId(id),
            client: client.clone(),
            version: 6,
            me: me.clone(),
            surfaces: RefCell::new(HashMap::new()),
        });
        client.add_client_obj(base.clone()).unwrap();
        base
    }

    pub(crate) fn mk_toplevel(
        client: &Rc<Client>,
        base: &Rc<XdgWmBase>,
        sid: u32,
        xid: u32,
        tid: u32,
    ) -> (Rc<WlSurface>, Rc<XdgSurface>, Rc<XdgToplevel>) {
        let s = WlSurface::new(ObjectId(sid), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        base.get_xdg_surface(xdg_wm_base::get_xdg_surface::Request {
            id: ObjectId(xid),
            surface: ObjectId(sid),
        })
        .unwrap();
        let xdg = base.surfaces.borrow().get(&ObjectId(xid)).cloned().unwrap();
        xdg.get_toplevel(xdg_surface::get_toplevel::Request { id: ObjectId(tid) })
            .unwrap();
        let tl = xdg.toplevel().unwrap();
        (s, xdg, tl)
    }

    fn setup() -> (Rc<State>, Rc<Client>, Rc<WlSurface>, Rc<XdgSurface>, Rc<XdgToplevel>) {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base = mk_base(&client, 30);
        let (s, xdg, tl) = mk_toplevel(&client, &base, 10, 40, 50);
        (state, client, s, xdg, tl)
    }

    fn commit(s: &Rc<WlSurface>) {
        s.commit(wl_surface::commit::Request {}).unwrap();
    }

    fn attach_commit(client: &Rc<Client>, s: &Rc<WlSurface>, buf: u32) {
        let b = test_buffer(client, ObjectId(buf), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        commit(s);
    }

    /// first commit -> initial configure, ack, buffer -> mapped
    pub(crate) fn map(state: &Rc<State>, client: &Rc<Client>, s: &Rc<WlSurface>, xdg: &Rc<XdgSurface>, buf: u32) {
        map_sized(state, client, s, xdg, buf, 64, 64);
    }

    pub(crate) fn map_sized(
        state: &Rc<State>,
        client: &Rc<Client>,
        s: &Rc<WlSurface>,
        xdg: &Rc<XdgSurface>,
        buf: u32,
        w: i32,
        h: i32,
    ) {
        commit(s);
        flush_configures(state);
        xdg.ack_configure(xdg_surface::ack_configure::Request {
            serial: xdg.last_sent.get(),
        })
        .unwrap();
        let b = test_buffer(client, ObjectId(buf), w, h);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        commit(s);
    }

    #[test]
    fn first_commit_configures_and_maps_nothing() {
        let (state, client, s, xdg, tl) = setup();
        commit(&s);
        assert!(xdg.configured.get());
        flush_configures(&state);
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, tl.id, 0), 1);
        assert_eq!(count_events(&bytes, xdg.id, 0), 1);
        assert!(tl.window.borrow().is_none());
        assert!(crate::tree::active(&state).tiling.is_empty());
    }

    #[test]
    fn premature_buffer_is_an_error() {
        let (_state, client, s, _xdg, tl) = setup();
        attach_commit(&client, &s, 20);
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ERR, 0), 1);
        assert!(tl.window.borrow().is_none());
    }

    #[test]
    fn acks_must_exist_and_increase() {
        let (state, client, s, xdg, _tl) = setup();
        commit(&s);
        flush_configures(&state);
        // never sent
        xdg.ack_configure(xdg_surface::ack_configure::Request { serial: 99 })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
        // valid, then a duplicate
        xdg.ack_configure(xdg_surface::ack_configure::Request { serial: 1 })
            .unwrap();
        xdg.ack_configure(xdg_surface::ack_configure::Request { serial: 1 })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 2);
    }

    #[test]
    fn maps_into_the_tree_with_gaps() {
        let (state, client, s, xdg, tl) = setup();
        map(&state, &client, &s, &xdg, 20);
        assert!(tl.window.borrow().is_some());
        let win = tl.window.borrow().clone().unwrap();
        let g = {
            let c = state.config.borrow();
            c.layout.gaps_out + c.layout.border.width
        };
        assert_eq!(win.rect.get(), Rect { x1: g, y1: g, x2: 800 - g, y2: 600 - g });
        // the relayout configure carries the tile size
        flush_configures(&state);
        assert_eq!(tl.desired.get(), (800 - 2 * g, 600 - 2 * g));
    }

    #[test]
    fn hidden_workspace_window_is_still_found_by_surface() {
        // the freeze bug: a cast routes a hidden source's commits by
        // surface, but window_for_surface only searched the active
        // workspace, so the lookup lost the window the instant you swapped
        // off it and the stream stopped updating.
        let (state, client, s, xdg, tl) = setup();
        map(&state, &client, &s, &xdg, 20);
        let win = tl.window.borrow().clone().unwrap();
        assert!(crate::tree::window_for_surface(&state, &s).is_some());
        assert!(crate::tree::window_for_surface_any(&state, &s).is_some());
        // push a second workspace and make it active: the window is hidden
        state
            .workspaces
            .borrow_mut()
            .push(std::rc::Rc::new(crate::tree::workspace::Workspace::default()));
        state.active_ws.set(1);
        // active-only lookup loses it - this is what starved the cast
        assert!(
            crate::tree::window_for_surface(&state, &s).is_none(),
            "active-only lookup should not see the hidden window"
        );
        // the all-workspace lookup keeps finding it - the fix
        let found = crate::tree::window_for_surface_any(&state, &s);
        assert!(found.is_some(), "hidden-workspace window must still resolve");
        assert!(Rc::ptr_eq(&found.unwrap(), &win));
    }

    #[test]
    fn second_window_splits_the_first() {
        let (state, client, s1, x1, t1) = setup();
        map(&state, &client, &s1, &x1, 20);
        let base = mk_base(&client, 31);
        let (s2, x2, t2) = mk_toplevel(&client, &base, 11, 41, 51);
        map(&state, &client, &s2, &x2, 21);
        let (w1, w2) = (
            t1.window.borrow().clone().unwrap(),
            t2.window.borrow().clone().unwrap(),
        );
        let (r1, r2) = (w1.rect.get(), w2.rect.get());
        assert!(!r1.intersects(r2), "{r1:?} overlaps {r2:?}");
        // side-by-side split of an 800x600 root: both tiles at most half
        // wide (exactly half under the gapless neutral defaults)
        assert_eq!(r1.y1, r2.y1);
        assert!(r1.width() <= 800 / 2 && r2.width() <= 800 / 2);
    }

    #[test]
    fn swap_trades_leaf_slots_and_rects() {
        let (state, client, s1, x1, t1) = setup();
        map(&state, &client, &s1, &x1, 20);
        let base = mk_base(&client, 31);
        let (s2, x2, t2) = mk_toplevel(&client, &base, 11, 41, 51);
        map(&state, &client, &s2, &x2, 21);
        let (w1, w2) = (
            t1.window.borrow().clone().unwrap(),
            t2.window.borrow().clone().unwrap(),
        );
        let (r1, r2) = (w1.rect.get(), w2.rect.get());
        assert_ne!(r1, r2);
        let ws = crate::tree::active(&state);
        assert!(crate::tree::dwindle::swap_windows(&w1, &w2));
        crate::tree::relayout(&state, &ws);
        assert_eq!(w1.rect.get(), r2, "windows traded tiles");
        assert_eq!(w2.rect.get(), r1);
        // swapping back restores the layout; self-swap is refused
        assert!(crate::tree::dwindle::swap_windows(&w1, &w2));
        crate::tree::relayout(&state, &ws);
        assert_eq!(w1.rect.get(), r1);
        assert!(!crate::tree::dwindle::swap_windows(&w1, &w1));
        // removal still works through the swapped backpointers
        ws.tiling.remove(&w2);
        crate::tree::relayout(&state, &ws);
        assert_eq!(w1.rect.get().width(), r1.width() + r2.width());
    }

    #[test]
    fn unmap_resets_the_configure_cycle() {
        let (state, client, s, xdg, tl) = setup();
        map(&state, &client, &s, &xdg, 20);
        s.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        assert!(tl.window.borrow().is_none());
        assert!(crate::tree::active(&state).tiling.is_empty());
        assert!(!xdg.configured.get());
        // the next bufferless commit starts a fresh initial configure
        commit(&s);
        flush_configures(&state);
        assert!(xdg.configured.get());
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
    }

    #[test]
    fn stale_acks_dont_legalize_a_remap_buffer() {
        let (state, client, s, xdg, _tl) = setup();
        map(&state, &client, &s, &xdg, 20);
        s.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        // the old cycle's ack is on record, but the new cycle needs its own
        attach_commit(&client, &s, 21);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn close_promotes_the_sibling() {
        let (state, client, s1, x1, t1) = setup();
        map(&state, &client, &s1, &x1, 20);
        let base = mk_base(&client, 31);
        let (s2, x2, t2) = mk_toplevel(&client, &base, 11, 41, 51);
        map(&state, &client, &s2, &x2, 21);
        let w1_rect = t1.window.borrow().clone().unwrap().rect.get();
        // closing the second window gives the first its space back
        s2.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s2);
        assert!(t2.window.borrow().is_none());
        let win = t1.window.borrow().clone().unwrap();
        let g = {
            let c = state.config.borrow();
            c.layout.gaps_out + c.layout.border.width
        };
        assert_eq!(win.rect.get(), Rect { x1: g, y1: g, x2: 800 - g, y2: 600 - g });
        assert!(win.rect.get().width() > w1_rect.width());
    }

    #[test]
    fn fullscreen_fills_the_output_and_returns() {
        let (state, client, s, xdg, tl) = setup();
        map(&state, &client, &s, &xdg, 20);
        let win = tl.window.borrow().clone().unwrap();
        let tiled = win.rect.get();
        tl.set_fullscreen(xdg_toplevel::set_fullscreen::Request {
            output: ObjectId::NONE,
        })
        .unwrap();
        assert!(win.fullscreen.get());
        assert_eq!(win.draw_rect(&state), Rect { x1: 0, y1: 0, x2: 800, y2: 600 });
        assert_eq!(tl.desired.get(), (800, 600));
        tl.unset_fullscreen(xdg_toplevel::unset_fullscreen::Request {})
            .unwrap();
        assert!(!win.fullscreen.get());
        assert!(crate::tree::active(&state).fullscreen.borrow().is_none());
        assert_eq!(win.draw_rect(&state), tiled);
    }

    #[test]
    fn min_over_max_is_an_error() {
        let (_state, client, s, _xdg, tl) = setup();
        tl.set_min_size(xdg_toplevel::set_min_size::Request { width: 500, height: 0 })
            .unwrap();
        tl.set_max_size(xdg_toplevel::set_max_size::Request { width: 100, height: 0 })
            .unwrap();
        commit(&s);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn constraints_flip_then_slide() {
        // anchored to the parent's right edge, extending right - overflows
        // an 800-wide screen when the parent sits at x=700
        let p = Positioned {
            size: (100, 50),
            anchor_rect: Rect { x1: 0, y1: 0, x2: 10, y2: 10 },
            anchor: 4,
            gravity: 4,
            offset: (0, 0),
            ca: 4, // flip_x
        };
        let bounds = Rect { x1: 0, y1: 0, x2: 800, y2: 600 };
        let ((rx, _), (w, _)) = p.solve((700, 0), bounds);
        // flipped to the left edge, extending left
        assert_eq!((rx, w), (-100, 100));
        // same overflow with slide_x instead: clamped to the screen edge
        let p = Positioned { ca: 1, ..p };
        let ((rx, _), _) = p.solve((700, 0), bounds);
        assert_eq!(rx + 700, 700);
    }

    #[test]
    fn constraints_respect_offset_bounds() {
        // anchored to the parent's left edge, extending left - crosses the
        // left edge of a box that does not start at the global origin
        let p = Positioned {
            size: (100, 50),
            anchor_rect: Rect { x1: 0, y1: 0, x2: 10, y2: 10 },
            anchor: 3,
            gravity: 3,
            offset: (0, 0),
            ca: 4, // flip_x
        };
        let bounds = Rect { x1: 800, y1: 0, x2: 1600, y2: 600 };
        // parent at x=850: unflipped the popup starts at 750, left of the box
        let ((rx, _), (w, _)) = p.solve((850, 0), bounds);
        // flipped to the anchor rect's right edge, extending right
        assert_eq!((rx, w), (10, 100));
        // same overflow with slide_x: clamped to the box's left edge, not 0
        let p = Positioned { ca: 1, ..p };
        let ((rx, _), _) = p.solve((850, 0), bounds);
        assert_eq!(rx + 850, 800);
    }

    #[test]
    fn popup_grab_holds_and_returns_the_keyboard() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let base = mk_base(&client, 30);
        let (s1, x1, _t1) = mk_toplevel(&client, &base, 10, 40, 50);
        map(&state, &client, &s1, &x1, 20);
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|s| Rc::ptr_eq(s, &s1)));

        // a popup parented to the toplevel
        let ps = WlSurface::new(ObjectId(11), &client, 6);
        client.add_client_obj(ps.clone()).unwrap();
        client.objects.track_surface(ps.clone());
        base.get_xdg_surface(xdg_wm_base::get_xdg_surface::Request {
            id: ObjectId(41),
            surface: ObjectId(11),
        })
        .unwrap();
        let px = base.surfaces.borrow().get(&ObjectId(41)).cloned().unwrap();
        base.create_positioner(xdg_wm_base::create_positioner::Request { id: ObjectId(45) })
            .unwrap();
        {
            let pos = client.objects.positioner(ObjectId(45)).unwrap();
            use xdg_positioner::Handler as _;
            pos.set_size(xdg_positioner::set_size::Request { width: 50, height: 30 })
                .unwrap();
            pos.set_anchor_rect(xdg_positioner::set_anchor_rect::Request {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            })
            .unwrap();
        }
        px.get_popup(xdg_surface::get_popup::Request {
            id: ObjectId(51),
            parent: ObjectId(40),
            positioner: ObjectId(45),
        })
        .unwrap();
        let popup = px.popup().unwrap();
        map(&state, &client, &ps, &px, 21);

        use xdg_popup::Handler as _;
        popup
            .grab(xdg_popup::grab::Request { seat: ObjectId(9), serial: 1 })
            .unwrap();
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|s| Rc::ptr_eq(s, &ps)));

        dismiss_popup_grabs(&state, &seat);
        // popup_done went out and the toplevel got the keyboard back
        assert_eq!(count_events(&client.queued_out_bytes(), popup.id, 1), 1);
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|s| Rc::ptr_eq(s, &s1)));
        assert!(seat.popup_grab.borrow().is_empty());
    }

    /// popup at parent-relative (10,10): anchor rect (0,0,10,10), anchor
    /// and gravity both bottom-right
    fn mk_popup(
        client: &Rc<Client>,
        base: &Rc<XdgWmBase>,
        sid: u32,
        xid: u32,
        pid: u32,
        popid: u32,
        parent: ObjectId,
    ) -> (Rc<WlSurface>, Rc<XdgSurface>, Rc<XdgPopup>) {
        let s = WlSurface::new(ObjectId(sid), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        base.get_xdg_surface(xdg_wm_base::get_xdg_surface::Request {
            id: ObjectId(xid),
            surface: ObjectId(sid),
        })
        .unwrap();
        let xdg = base.surfaces.borrow().get(&ObjectId(xid)).cloned().unwrap();
        base.create_positioner(xdg_wm_base::create_positioner::Request { id: ObjectId(pid) })
            .unwrap();
        {
            let pos = client.objects.positioner(ObjectId(pid)).unwrap();
            use xdg_positioner::Handler as _;
            pos.set_size(xdg_positioner::set_size::Request { width: 50, height: 30 })
                .unwrap();
            pos.set_anchor_rect(xdg_positioner::set_anchor_rect::Request {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            })
            .unwrap();
            pos.set_anchor(xdg_positioner::set_anchor::Request { anchor: 8 }).unwrap();
            pos.set_gravity(xdg_positioner::set_gravity::Request { gravity: 8 }).unwrap();
        }
        xdg.get_popup(xdg_surface::get_popup::Request {
            id: ObjectId(popid),
            parent,
            positioner: ObjectId(pid),
        })
        .unwrap();
        let popup = xdg.popup().unwrap();
        (s, xdg, popup)
    }

    #[test]
    fn overlapping_sibling_popups_hit_newest_first() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base = mk_base(&client, 30);
        let (s1, x1, t1) = mk_toplevel(&client, &base, 10, 40, 50);
        map(&state, &client, &s1, &x1, 20);
        let (ps1, px1, p1) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId(40));
        map(&state, &client, &ps1, &px1, 21);
        let (ps2, px2, p2) = mk_popup(&client, &base, 12, 42, 46, 52, ObjectId(40));
        map(&state, &client, &ps2, &px2, 22);
        assert_eq!(p1.rel.get(), p2.rel.get(), "siblings fully overlap");

        let win = t1.window.borrow().clone().unwrap();
        let r = win.draw_rect(&state);
        let (rx, ry) = p2.rel.get();
        let (px, py) = (r.x1 + rx + 5, r.y1 + ry + 5);
        let (hit, _, _) = crate::tree::surface_at(&state, px, py).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps2), "the newest sibling popup is on top");

        // unmapping the top sibling uncovers the older one
        ps2.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&ps2);
        let (hit, _, _) = crate::tree::surface_at(&state, px, py).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps1));
    }

    #[test]
    fn nested_popup_siblings_hit_newest_first() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base = mk_base(&client, 30);
        let (s1, x1, t1) = mk_toplevel(&client, &base, 10, 40, 50);
        map(&state, &client, &s1, &x1, 20);
        let (psa, pxa, pa) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId(40));
        map(&state, &client, &psa, &pxa, 21);
        // two overlapping children of the first popup
        let (c1s, c1x, _) = mk_popup(&client, &base, 12, 42, 46, 52, ObjectId(41));
        map(&state, &client, &c1s, &c1x, 22);
        let (c2s, c2x, c2) = mk_popup(&client, &base, 13, 43, 47, 53, ObjectId(41));
        map(&state, &client, &c2s, &c2x, 23);

        let win = t1.window.borrow().clone().unwrap();
        let r = win.draw_rect(&state);
        let (ax, ay) = pa.rel.get();
        let (cx, cy) = c2.rel.get();
        let (hit, _, _) =
            crate::tree::surface_at(&state, r.x1 + ax + cx + 5, r.y1 + ay + cy + 5).unwrap();
        assert!(Rc::ptr_eq(&hit, &c2s), "newest-first applies at every sibling level");
    }

    #[test]
    fn layer_surface_popups_hit_newest_first() {
        use crate::protocol::interfaces::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
        use zwlr_layer_shell_v1::Handler as _;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        // a bar across the top of the output
        let bar = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(bar.clone()).unwrap();
        client.objects.track_surface(bar.clone());
        let shell = Rc::new(crate::shell::layer::LayerShell {
            id: ObjectId(90),
            client: client.clone(),
            version: 5,
        });
        shell
            .get_layer_surface(zwlr_layer_shell_v1::get_layer_surface::Request {
                id: ObjectId(20),
                surface: ObjectId(10),
                output: ObjectId::NONE,
                layer: crate::shell::layer::TOP,
                namespace: "test".to_string(),
            })
            .unwrap();
        let ext = bar.ext.borrow().clone();
        let ls = ext.layer_surface().unwrap();
        ls.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 4 | 8 })
            .unwrap();
        ls.set_size(zwlr_layer_surface_v1::set_size::Request { width: 0, height: 32 })
            .unwrap();
        commit(&bar);
        flush_configures(&state);
        // the first configure on a fresh layer surface carries serial 1
        ls.ack_configure(zwlr_layer_surface_v1::ack_configure::Request { serial: 1 })
            .unwrap();
        attach_commit(&client, &bar, 33);
        assert!(ls.mapped());

        // two overlapping popups adopted by the layer surface in creation order
        let base = mk_base(&client, 30);
        let (ps1, px1, _) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(51) })
            .unwrap();
        map(&state, &client, &ps1, &px1, 34);
        let (ps2, px2, p2) = mk_popup(&client, &base, 12, 42, 46, 52, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(52) })
            .unwrap();
        map(&state, &client, &ps2, &px2, 35);

        let r = ls.rect.get();
        let (rx, ry) = p2.rel.get();
        let (hit, _, _) = crate::tree::surface_at(&state, r.x1 + rx + 5, r.y1 + ry + 5).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps2), "the newest layer-surface popup is on top");
    }

    /// a mapped bar across the top of the output on the given shell layer
    fn mk_bar(
        state: &Rc<State>,
        client: &Rc<Client>,
        sid: u32,
        lid: u32,
        layer: u32,
        buf: u32,
    ) -> (Rc<WlSurface>, Rc<crate::shell::layer::LayerSurface>) {
        use crate::protocol::interfaces::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
        use zwlr_layer_shell_v1::Handler as _;
        use zwlr_layer_surface_v1::Handler as _;
        let s = WlSurface::new(ObjectId(sid), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        let shell = Rc::new(crate::shell::layer::LayerShell {
            id: ObjectId(90),
            client: client.clone(),
            version: 5,
        });
        shell
            .get_layer_surface(zwlr_layer_shell_v1::get_layer_surface::Request {
                id: ObjectId(lid),
                surface: ObjectId(sid),
                output: ObjectId::NONE,
                layer,
                namespace: "test".to_string(),
            })
            .unwrap();
        let ls = crate::shell::layer::from_surface(state, &s).unwrap();
        ls.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 4 | 8 })
            .unwrap();
        ls.set_size(zwlr_layer_surface_v1::set_size::Request { width: 0, height: 32 })
            .unwrap();
        commit(&s);
        flush_configures(state);
        // the first configure on a fresh layer surface carries serial 1
        ls.ack_configure(zwlr_layer_surface_v1::ack_configure::Request { serial: 1 })
            .unwrap();
        attach_commit(client, &s, buf);
        (s, ls)
    }

    #[test]
    fn layer_popup_wins_input_over_a_higher_layer() {
        use crate::protocol::interfaces::zwlr_layer_surface_v1;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (_bar, ls) = mk_bar(&state, &client, 10, 20, crate::shell::layer::TOP, 33);
        let base = mk_base(&client, 30);
        let (ps, px, p) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(51) })
            .unwrap();
        map(&state, &client, &ps, &px, 34);
        // an overlay surface covering the same corner
        let (ov, _lo) = mk_bar(&state, &client, 12, 21, crate::shell::layer::OVERLAY, 35);
        let r = ls.rect.get();
        let (rx, ry) = p.rel.get();
        let (hit, _, _) = crate::tree::surface_at(&state, r.x1 + rx + 5, r.y1 + ry + 5).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps), "a layer popup outranks a higher layer");
        // outside the popup the overlay still wins
        let (hit, _, _) = crate::tree::surface_at(&state, r.x1 + 2, r.y1 + 2).unwrap();
        assert!(Rc::ptr_eq(&hit, &ov));
    }

    #[test]
    fn get_popup_rejects_an_already_parented_popup() {
        use crate::protocol::interfaces::zwlr_layer_surface_v1;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (_bar, ls) = mk_bar(&state, &client, 10, 20, crate::shell::layer::TOP, 33);
        let base = mk_base(&client, 30);
        let (s1, x1, _t1) = mk_toplevel(&client, &base, 11, 40, 50);
        map(&state, &client, &s1, &x1, 34);
        // a popup already parented to a toplevel cannot be adopted
        let (_ps, _px, _p) = mk_popup(&client, &base, 12, 41, 45, 51, ObjectId(40));
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(51) })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
        let mut n = 0;
        ls.for_each_popup(|_| n += 1);
        assert_eq!(n, 0, "the rejected popup was not adopted");
        // a second adoption of the same popup fails too
        let (_ps2, _px2, _p2) = mk_popup(&client, &base, 13, 42, 46, 52, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(52) })
            .unwrap();
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(52) })
            .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 2);
        let mut n = 0;
        ls.for_each_popup(|_| n += 1);
        assert_eq!(n, 1, "the popup was adopted exactly once");
    }

    #[test]
    fn fullscreen_hides_popups_of_hidden_layers() {
        use crate::protocol::interfaces::zwlr_layer_surface_v1;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (_bar, ls) = mk_bar(&state, &client, 10, 20, crate::shell::layer::TOP, 33);
        let base = mk_base(&client, 30);
        let (ps, px, p) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(51) })
            .unwrap();
        map(&state, &client, &ps, &px, 34);
        let r = ls.rect.get();
        let (rx, ry) = p.rel.get();
        let (x, y) = (r.x1 + rx + 5, r.y1 + ry + 5);
        let (hit, _, _) = crate::tree::surface_at(&state, x, y).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps));
        // fullscreen hides the top bar and its popup with it
        let (ws, wx, wt) = mk_toplevel(&client, &base, 12, 42, 52);
        map(&state, &client, &ws, &wx, 35);
        let win = wt.window.borrow().clone().unwrap();
        crate::tree::set_fullscreen(&state, &win, true);
        let (hit, _, _) = crate::tree::surface_at(&state, x, y).unwrap();
        assert!(Rc::ptr_eq(&hit, &ws), "the popup hides with its parent layer");
    }

    #[test]
    fn overlay_popups_stay_clickable_above_fullscreen() {
        use crate::protocol::interfaces::zwlr_layer_surface_v1;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (_bar, ls) = mk_bar(&state, &client, 10, 20, crate::shell::layer::OVERLAY, 33);
        let base = mk_base(&client, 30);
        let (ps, px, p) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(51) })
            .unwrap();
        map(&state, &client, &ps, &px, 34);
        let (ws, wx, wt) = mk_toplevel(&client, &base, 12, 42, 52);
        map(&state, &client, &ws, &wx, 35);
        let win = wt.window.borrow().clone().unwrap();
        crate::tree::set_fullscreen(&state, &win, true);
        let r = ls.rect.get();
        let (rx, ry) = p.rel.get();
        let (hit, _, _) = crate::tree::surface_at(&state, r.x1 + rx + 5, r.y1 + ry + 5).unwrap();
        assert!(Rc::ptr_eq(&hit, &ps), "an overlay popup keeps input above fullscreen");
    }

    #[test]
    fn popup_bounds_follow_the_parents_output() {
        use crate::protocol::interfaces::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
        use zwlr_layer_shell_v1::Handler as _;
        use zwlr_layer_surface_v1::Handler as _;
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base = mk_base(&client, 30);
        let (s1, x1, _t1) = mk_toplevel(&client, &base, 10, 40, 50);
        map(&state, &client, &s1, &x1, 60);
        let union = Rect { x1: 0, y1: 0, x2: 800, y2: 600 };

        // a toplevel parent resolves through its window's workspace
        let (ps1, px1, p1) = mk_popup(&client, &base, 11, 41, 45, 51, ObjectId(40));
        map(&state, &client, &ps1, &px1, 61);
        let parent = p1.parent.borrow().clone().unwrap();
        assert_eq!(parent.constraint_bounds(&state), union);

        // a layer parent resolves through its output slot
        let bar = WlSurface::new(ObjectId(12), &client, 6);
        client.add_client_obj(bar.clone()).unwrap();
        client.objects.track_surface(bar.clone());
        let shell = Rc::new(crate::shell::layer::LayerShell {
            id: ObjectId(90),
            client: client.clone(),
            version: 5,
        });
        shell
            .get_layer_surface(zwlr_layer_shell_v1::get_layer_surface::Request {
                id: ObjectId(21),
                surface: ObjectId(12),
                output: ObjectId::NONE,
                layer: crate::shell::layer::TOP,
                namespace: "test".to_string(),
            })
            .unwrap();
        let ls = crate::shell::layer::from_surface(&state, &bar).unwrap();
        let (ps2, px2, p2) = mk_popup(&client, &base, 13, 42, 46, 52, ObjectId::NONE);
        ls.get_popup(zwlr_layer_surface_v1::get_popup::Request { popup: ObjectId(52) })
            .unwrap();
        map(&state, &client, &ps2, &px2, 62);
        let parent = p2.parent.borrow().clone().unwrap();
        assert_eq!(parent.constraint_bounds(&state), union);

        // a nested popup walks its parent chain down to the toplevel
        let (ps3, px3, p3) = mk_popup(&client, &base, 14, 43, 47, 53, ObjectId(41));
        map(&state, &client, &ps3, &px3, 63);
        let parent = p3.parent.borrow().clone().unwrap();
        assert_eq!(parent.constraint_bounds(&state), union);
    }

    #[test]
    fn positioners_work_across_wm_base_binds() {
        // a client binds xdg_wm_base twice and repositions popups from
        // one bind with positioners created on the other
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base_a = mk_base(&client, 30);
        let (_s1, _x1, _t1) = mk_toplevel(&client, &base_a, 10, 40, 50);
        let ps = WlSurface::new(ObjectId(11), &client, 6);
        client.add_client_obj(ps.clone()).unwrap();
        client.objects.track_surface(ps.clone());
        base_a
            .get_xdg_surface(xdg_wm_base::get_xdg_surface::Request {
                id: ObjectId(41),
                surface: ObjectId(11),
            })
            .unwrap();
        let px = base_a.surfaces.borrow().get(&ObjectId(41)).cloned().unwrap();
        // the positioner comes from a SECOND bind
        let base_b = mk_base(&client, 31);
        base_b
            .create_positioner(xdg_wm_base::create_positioner::Request { id: ObjectId(45) })
            .unwrap();
        {
            let pos = client.objects.positioner(ObjectId(45)).unwrap();
            use xdg_positioner::Handler as _;
            pos.set_size(xdg_positioner::set_size::Request { width: 50, height: 30 })
                .unwrap();
            pos.set_anchor_rect(xdg_positioner::set_anchor_rect::Request {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            })
            .unwrap();
        }
        px.get_popup(xdg_surface::get_popup::Request {
            id: ObjectId(51),
            parent: ObjectId(40),
            positioner: ObjectId(45),
        })
        .unwrap();
        let popup = px.popup().unwrap();
        use xdg_popup::Handler as _;
        popup
            .reposition(xdg_popup::reposition::Request {
                positioner: ObjectId(45),
                token: 7,
            })
            .unwrap();
        // repositioned went out; no display error killed the client
        assert_eq!(count_events(&client.queued_out_bytes(), popup.id, 2), 1);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
        // destroy forgets the client-scoped entry
        {
            let pos = client.objects.positioner(ObjectId(45)).unwrap();
            use xdg_positioner::Handler as _;
            pos.destroy(xdg_positioner::destroy::Request {}).unwrap();
        }
        assert!(client.objects.positioner(ObjectId(45)).is_none());
    }

    #[test]
    fn positioner_places_by_anchor_and_gravity() {
        // bottom edge midpoint, extending down
        let p = Positioned {
            size: (50, 30),
            anchor_rect: Rect { x1: 0, y1: 0, x2: 100, y2: 20 },
            anchor: 2,
            gravity: 2,
            offset: (3, 4),
            ca: 0,
        };
        assert_eq!(p.place(), (25 + 3, 20 + 4));
        // top-left corner, extending up-left
        let p = Positioned { anchor: 5, gravity: 5, offset: (0, 0), ..p };
        assert_eq!(p.place(), (-50, -30));
    }

    // -- interactive move/resize grabs --

    /// two tiled windows side by side on 800x600, seat pointer over the second
    fn grab_setup() -> (
        Rc<State>,
        Rc<Client>,
        Rc<crate::input::seat::SeatGlobal>,
        Rc<XdgToplevel>,
        Rc<XdgToplevel>,
    ) {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let base = mk_base(&client, 30);
        let (s1, x1, tl1) = mk_toplevel(&client, &base, 10, 40, 50);
        map(&state, &client, &s1, &x1, 20);
        let (s2, x2, tl2) = mk_toplevel(&client, &base, 11, 41, 51);
        map(&state, &client, &s2, &x2, 21);
        // hit testing reads the surface extent; grow past the test buffer
        s1.size.set((800, 600));
        s2.size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        seat.pointer_motion(&state, 1_000, 600.0, 300.0, 600.0, 300.0);
        (state, client, seat, tl1, tl2)
    }

    /// a button press followed by the serial it was delivered with
    fn press(state: &Rc<State>, seat: &Rc<crate::input::seat::SeatGlobal>) -> u32 {
        seat.pointer_button(state, 2_000, 0x110, true);
        (state.next_serial(None) - 1) as u32
    }

    #[test]
    fn a_tiled_resize_drag_steps_the_split_ratio() {
        let (state, _client, seat, tl1, tl2) = grab_setup();
        let win1 = tl1.window.borrow().clone().unwrap();
        assert_eq!(win1.rect.get().x2, 400, "even split to start");
        let serial = press(&state, &seat);
        tl2.resize(xdg_toplevel::resize::Request {
            seat: ObjectId(0),
            serial,
            edges: 4, // left
        })
        .unwrap();
        // dragging the second tile's left edge 80px left grows it
        seat.pointer_motion(&state, 3_000, -80.0, 0.0, -80.0, 0.0);
        assert_eq!(win1.rect.get().x2, 320);
        let win2 = tl2.window.borrow().clone().unwrap();
        assert_eq!(win2.rect.get().x1, 320);
        // release ends the session; further motion is plain motion
        seat.pointer_button(&state, 4_000, 0x110, false);
        seat.pointer_motion(&state, 5_000, -80.0, 0.0, -80.0, 0.0);
        assert_eq!(win1.rect.get().x2, 320);
    }

    #[test]
    fn a_resize_drag_needs_a_matching_split_axis() {
        let (state, _client, seat, tl1, tl2) = grab_setup();
        let win1 = tl1.window.borrow().clone().unwrap();
        let serial = press(&state, &seat);
        tl2.resize(xdg_toplevel::resize::Request {
            seat: ObjectId(0),
            serial,
            edges: 1, // top, but the only split is side by side
        })
        .unwrap();
        seat.pointer_motion(&state, 3_000, 0.0, -50.0, 0.0, -50.0);
        assert_eq!(win1.rect.get(), Rect { x1: 0, y1: 0, x2: 400, y2: 600 });
    }

    #[test]
    fn a_move_grab_drags_a_floating_window_within_the_output() {
        let (state, _client, seat, _tl1, tl2) = grab_setup();
        let win2 = tl2.window.borrow().clone().unwrap();
        crate::tree::float::toggle_floating(&state, &win2);
        assert_eq!(win2.rect.get(), Rect { x1: 200, y1: 150, x2: 600, y2: 450 });
        seat.pointer_motion(&state, 1_500, -200.0, 0.0, -200.0, 0.0);
        let serial = press(&state, &seat);
        tl2.r#move(xdg_toplevel::r#move::Request { seat: ObjectId(0), serial })
            .unwrap();
        seat.pointer_motion(&state, 3_000, 50.0, -30.0, 50.0, -30.0);
        assert_eq!(win2.rect.get(), Rect { x1: 250, y1: 120, x2: 650, y2: 420 });
        // a huge delta pins the box to the output edge
        seat.pointer_motion(&state, 4_000, 5000.0, 5000.0, 5000.0, 5000.0);
        assert_eq!(win2.rect.get(), Rect { x1: 400, y1: 300, x2: 800, y2: 600 });
        seat.pointer_button(&state, 5_000, 0x110, false);
    }

    #[test]
    fn dragging_a_tile_swaps_it_under_the_pointer() {
        let (state, _client, seat, tl1, tl2) = grab_setup();
        let win1 = tl1.window.borrow().clone().unwrap();
        let win2 = tl2.window.borrow().clone().unwrap();
        assert_eq!(win1.rect.get().x2, 400);
        let serial = press(&state, &seat);
        tl2.r#move(xdg_toplevel::r#move::Request { seat: ObjectId(0), serial })
            .unwrap();
        // crossing onto the left tile trades the slots
        seat.pointer_motion(&state, 3_000, -300.0, 0.0, -300.0, 0.0);
        assert_eq!(win1.rect.get().x1, 400, "first window took the right slot");
        assert_eq!(win2.rect.get().x1, 0, "dragged window landed left");
        // sitting inside its own new slot swaps nothing further
        seat.pointer_motion(&state, 4_000, -50.0, 0.0, -50.0, 0.0);
        assert_eq!(win2.rect.get().x1, 0);
        seat.pointer_button(&state, 5_000, 0x110, false);
    }

    #[test]
    fn a_stale_serial_starts_no_grab() {
        let (state, _client, seat, tl1, tl2) = grab_setup();
        let win1 = tl1.window.borrow().clone().unwrap();
        let serial = press(&state, &seat);
        tl2.resize(xdg_toplevel::resize::Request {
            seat: ObjectId(0),
            serial: serial + 1,
            edges: 4,
        })
        .unwrap();
        seat.pointer_motion(&state, 3_000, -80.0, 0.0, -80.0, 0.0);
        assert_eq!(win1.rect.get().x2, 400);
    }

    #[test]
    fn split_ratio_action_grows_the_focused_window() {
        let (state, _client, seat, tl1, tl2) = grab_setup();
        let win1 = tl1.window.borrow().clone().unwrap();
        *seat.kb_focus.borrow_mut() = Some(tl2.xdg.surface.clone());
        crate::ipc::dispatch_action(&state, &crate::config::Action::AdjustSplitRatio(0.2));
        assert_eq!(win1.rect.get().x2, 240, "the second tile grew");
        // the ratio bottoms out instead of collapsing the sibling
        for _ in 0..5 {
            crate::ipc::dispatch_action(&state, &crate::config::Action::AdjustSplitRatio(0.2));
        }
        assert_eq!(win1.rect.get().x2, 80);
    }
}

// -- test scaffolding shared with the capture protocol suites --

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::protocol::interfaces::wl_surface;
    use wl_surface::Handler as _;
    use xdg_surface::Handler as _;
    use xdg_wm_base::Handler as _;

    /// a mapped toplevel driven through the real request path.
    /// ids = [base, surface, xdg, toplevel, buffer], all unused client ids.
    pub(crate) fn mapped_toplevel(
        state: &Rc<State>,
        client: &Rc<Client>,
        ids: [u32; 5],
    ) -> (Rc<WlSurface>, Rc<XdgSurface>, Rc<XdgToplevel>) {
        let [bid, sid, xid, tid, bufid] = ids;
        if state.output_size.get() == (0, 0) {
            state.output_size.set((800, 600));
        }
        let base = Rc::new_cyclic(|me| XdgWmBase {
            id: ObjectId(bid),
            client: client.clone(),
            version: 6,
            me: me.clone(),
            surfaces: RefCell::new(HashMap::new()),
        });
        client.add_client_obj(base.clone()).unwrap();
        let s = WlSurface::new(ObjectId(sid), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        base.get_xdg_surface(xdg_wm_base::get_xdg_surface::Request {
            id: ObjectId(xid),
            surface: ObjectId(sid),
        })
        .unwrap();
        let xdg = base.surfaces.borrow().get(&ObjectId(xid)).cloned().unwrap();
        xdg.get_toplevel(xdg_surface::get_toplevel::Request { id: ObjectId(tid) })
            .unwrap();
        let tl = xdg.toplevel().unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        flush_configures(state);
        xdg.ack_configure(xdg_surface::ack_configure::Request {
            serial: xdg.last_sent.get(),
        })
        .unwrap();
        let b = crate::protocol::shm::test_buffer(client, ObjectId(bufid), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        (s, xdg, tl)
    }

    /// null attach + commit: the toplevel unmaps
    pub(crate) fn unmap_toplevel(s: &Rc<WlSurface>) {
        s.attach(wl_surface::attach::Request {
            buffer: ObjectId::NONE,
            x: 0,
            y: 0,
        })
        .unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
    }
}
