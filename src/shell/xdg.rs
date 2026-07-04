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
            positioners: RefCell::new(HashMap::new()),
        }))
    }
}

pub struct XdgWmBase {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    me: Weak<XdgWmBase>,
    surfaces: RefCell<HashMap<ObjectId, Rc<XdgSurface>>>,
    positioners: RefCell<HashMap<ObjectId, Rc<XdgPositioner>>>,
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
        self.positioners.borrow_mut().clear();
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
        self.positioners.borrow_mut().insert(req.id, p);
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
        self.positioners.borrow_mut().clear();
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
        _req: xdg_positioner::set_constraint_adjustment::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // stored nowhere yet: constraint solving lands with real popups
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
        });
        c.add_client_obj(tl.clone())?;
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
        let positioner = self.base.positioners.borrow().get(&req.positioner).cloned();
        let Some(positioner) = positioner else {
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
            parent: RefCell::new(parent.clone()),
            rel: Cell::new(pos.place()),
            size: Cell::new(pos.size),
            done: Cell::new(false),
        });
        c.add_client_obj(popup.clone())?;
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
        // 0x0 is silently dropped: chromium sends it (crbug.com/1329214)
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
                    let win = Rc::new(Window::new(WindowKind::Xdg(tl.clone())));
                    *tl.window.borrow_mut() = Some(win.clone());
                    crate::tree::map_window(&x.client.state, &win);
                } else if !mapped && in_tree {
                    let win = tl.window.borrow_mut().take().unwrap();
                    crate::tree::unmap_window(&x.client.state, &win);
                    tl.reset_after_unmap();
                }
            }
            XdgExt::Popup(_) => {
                drop(ext);
                if !x.configured.get() {
                    x.configured.set(true);
                    x.schedule_configure();
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
        self.desired.set((w, h));
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
        Ok(())
    }

    fn set_app_id(
        &self,
        req: xdg_toplevel::set_app_id::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *self.app_id.borrow_mut() = req.app_id.to_string();
        Ok(())
    }

    fn show_window_menu(
        &self,
        _req: xdg_toplevel::show_window_menu::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn r#move(&self, _req: xdg_toplevel::r#move::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn resize(&self, req: xdg_toplevel::resize::Request) -> Result<(), Box<dyn std::error::Error>> {
        if req.edges > 10 || req.edges == 3 || req.edges == 7 {
            self.client.protocol_error(self.id, 0, "invalid resize edge");
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

/// popups render above their parent at the positioner's spot; grabs and constraint solving TODO.
pub struct XdgPopup {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub xdg: Rc<XdgSurface>,
    parent: RefCell<Option<Rc<XdgSurface>>>,
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
}

impl xdg_popup::Handler for XdgPopup {
    fn destroy(&self, _req: xdg_popup::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = self.parent.borrow_mut().take() {
            parent.unlink_popup(self);
        }
        *self.xdg.ext.borrow_mut() = XdgExt::None;
        self.xdg.configured.set(false);
        self.client.state.damage.trigger();
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn grab(&self, _req: xdg_popup::grab::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn reposition(
        &self,
        req: xdg_popup::reposition::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let positioner = self.xdg.base.positioners.borrow().get(&req.positioner).cloned();
        let Some(positioner) = positioner else {
            self.client.invalid_object(req.positioner);
            return Ok(());
        };
        let pos = positioner.v.get();
        self.rel.set(pos.place());
        if pos.size != (0, 0) {
            self.size.set(pos.size);
        }
        self.client
            .event(|o| xdg_popup::repositioned::send(o, self.id, req.token));
        self.xdg.schedule_configure();
        Ok(())
    }
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

// -- the flush task --

pub fn flush_configures(state: &Rc<State>) {
    loop {
        let batch: Vec<_> = state.configures.borrow_mut().drain(..).collect();
        if batch.is_empty() {
            return;
        }
        for s in batch {
            s.scheduled.set(false);
            if s.surface.destroyed.get() {
                continue;
            }
            s.send_configure_now();
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
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::wl_surface;
    use crate::protocol::shm::test_buffer;
    use wl_surface::Handler as _;
    use xdg_surface::Handler as _;
    use xdg_toplevel::Handler as _;
    use xdg_wm_base::Handler as _;

    const ERR: ObjectId = ObjectId(1);

    fn mk_base(client: &Rc<Client>, id: u32) -> Rc<XdgWmBase> {
        let base = Rc::new_cyclic(|me| XdgWmBase {
            id: ObjectId(id),
            client: client.clone(),
            version: 6,
            me: me.clone(),
            surfaces: RefCell::new(HashMap::new()),
            positioners: RefCell::new(HashMap::new()),
        });
        client.add_client_obj(base.clone()).unwrap();
        base
    }

    fn mk_toplevel(
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
    fn map(state: &Rc<State>, client: &Rc<Client>, s: &Rc<WlSurface>, xdg: &Rc<XdgSurface>, buf: u32) {
        commit(s);
        flush_configures(state);
        xdg.ack_configure(xdg_surface::ack_configure::Request {
            serial: xdg.last_sent.get(),
        })
        .unwrap();
        attach_commit(client, s, buf);
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
        let g = crate::tree::GAPS_OUT + crate::tree::BORDER;
        assert_eq!(win.rect.get(), Rect { x1: g, y1: g, x2: 800 - g, y2: 600 - g });
        // the relayout configure carries the tile size
        flush_configures(&state);
        assert_eq!(tl.desired.get(), (800 - 2 * g, 600 - 2 * g));
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
        // side-by-side split of an 800x600 root: both tiles half wide
        assert_eq!(r1.y1, r2.y1);
        assert!(r1.width() < 800 / 2 && r2.width() < 800 / 2);
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
        let g = crate::tree::GAPS_OUT + crate::tree::BORDER;
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
    fn positioner_places_by_anchor_and_gravity() {
        // bottom edge midpoint, extending down
        let p = Positioned {
            size: (50, 30),
            anchor_rect: Rect { x1: 0, y1: 0, x2: 100, y2: 20 },
            anchor: 2,
            gravity: 2,
            offset: (3, 4),
        };
        assert_eq!(p.place(), (25 + 3, 20 + 4));
        // top-left corner, extending up-left
        let p = Positioned { anchor: 5, gravity: 5, offset: (0, 0), ..p };
        assert_eq!(p.place(), (-50, -30));
    }
}
