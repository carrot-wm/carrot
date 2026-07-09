// wlr-layer-shell: anchors, exclusive zones, layers, keyboard interactivity.
//
// all state is double buffered and re-validated on every commit - the
// defaults are already invalid, so a bare commit errors per spec. unmap
// resets everything back to those defaults and restarts the configure
// cycle. exclusive zones accumulate in arrangement order (overlay down to
// background, mapping order within a layer), so two bars on one edge
// stack instead of overlapping; what's left over is the tiling area.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use crate::state::State;
use crate::surface::{PendingState, SurfaceExt, SurfaceRole, WlSurface};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

// layers
pub const BACKGROUND: u32 = 0;
pub const BOTTOM: u32 = 1;
pub const TOP: u32 = 2;
pub const OVERLAY: u32 = 3;
// anchor bits
const A_TOP: u32 = 1;
const A_BOTTOM: u32 = 2;
const A_LEFT: u32 = 4;
const A_RIGHT: u32 = 8;
// keyboard interactivity
pub const KI_NONE: u32 = 0;
pub const KI_EXCLUSIVE: u32 = 1;
pub const KI_ON_DEMAND: u32 = 2;
// zwlr_layer_shell_v1 errors
const ROLE: u32 = 0;
const INVALID_LAYER: u32 = 1;
// zwlr_layer_surface_v1 errors
const INVALID_SURFACE_STATE: u32 = 0;
const INVALID_SIZE: u32 = 1;
const INVALID_ANCHOR: u32 = 2;
const INVALID_KI: u32 = 3;
const INVALID_EXCLUSIVE_EDGE: u32 = 4;

// -- the global --

pub struct LayerShellGlobal;

impl Global for LayerShellGlobal {
    fn interface(&self) -> &'static str {
        zwlr_layer_shell_v1::NAME
    }

    fn version(&self) -> u32 {
        5
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(LayerShell {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct LayerShell {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwlr_layer_shell_v1::Handler for LayerShell {
    fn get_layer_surface(
        &self,
        req: zwlr_layer_shell_v1::get_layer_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        if req.layer > OVERLAY {
            c.protocol_error(self.id, INVALID_LAYER, "invalid layer");
            return Ok(());
        }
        if surface.has_live_role() {
            c.protocol_error(self.id, ROLE, "the surface already has a role object");
            return Ok(());
        }
        if let Err(old) = surface.set_role(SurfaceRole::LayerSurface) {
            c.protocol_error(
                self.id,
                ROLE,
                &format!("the surface already has the {} role", old.name()),
            );
            return Ok(());
        }
        if surface.buffer.borrow().is_some() {
            c.protocol_error(self.id, ROLE, "the surface already has a committed buffer");
            return Ok(());
        }
        // a named output pins the layer there; null means the focused one
        let output = if req.output.0 == 0 {
            c.state.focused_output.get()
        } else {
            let name = c.objects.output(req.output).map(|o| o.name.clone());
            let slot = c.state.display.borrow().as_ref().and_then(|d| {
                d.outputs
                    .borrow()
                    .iter()
                    .position(|o| Some(&o.conn.name) == name.as_ref())
            });
            slot.unwrap_or_else(|| c.state.focused_output.get())
        };
        let ls = Rc::new_cyclic(|me| LayerSurface {
            output: Cell::new(output),
            id: req.id,
            client: c.clone(),
            version: self.version,
            me: me.clone(),
            surface: surface.clone(),
            pending: Cell::new(LayerState::new(req.layer)),
            current: Cell::new(LayerState::new(req.layer)),
            next_serial: Cell::new(1),
            last_sent: Cell::new(0),
            acked: Cell::new(0),
            ack_floor: Cell::new(0),
            scheduled: Cell::new(false),
            configured: Cell::new(false),
            last_cfg: Cell::new(None),
            rect: Cell::new(Rect::default()),
            linked: Cell::new(false),
            closed: Cell::new(false),
            popups: RefCell::new(Vec::new()),
        });
        c.add_client_obj(ls.clone())?;
        *surface.ext.borrow_mut() = Rc::new(LayerExt { ls });
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_layer_shell_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for LayerShell {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_layer_shell_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_layer_shell_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the layer surface --

#[derive(Copy, Clone, PartialEq)]
pub struct LayerState {
    pub size: (u32, u32),
    pub anchor: u32,
    pub exclusive: i32,
    /// top, right, bottom, left
    pub margin: (i32, i32, i32, i32),
    pub ki: u32,
    pub layer: u32,
    pub exclusive_edge: u32,
}

impl LayerState {
    fn new(layer: u32) -> LayerState {
        LayerState {
            size: (0, 0),
            anchor: 0,
            exclusive: 0,
            margin: (0, 0, 0, 0),
            ki: KI_NONE,
            layer,
            exclusive_edge: 0,
        }
    }

    // the whole-combo checks the spec wants re-run on every commit
    fn validate(&self) -> Result<(), (u32, &'static str)> {
        let lr = A_LEFT | A_RIGHT;
        let tb = A_TOP | A_BOTTOM;
        if self.size.0 == 0 && self.anchor & lr != lr {
            return Err((INVALID_SIZE, "width 0 needs both left and right anchors"));
        }
        if self.size.1 == 0 && self.anchor & tb != tb {
            return Err((INVALID_SIZE, "height 0 needs both top and bottom anchors"));
        }
        if self.exclusive_edge != 0 {
            if self.exclusive_edge.count_ones() != 1 || self.exclusive_edge > A_RIGHT {
                return Err((INVALID_EXCLUSIVE_EDGE, "exclusive edge must be one edge"));
            }
            if self.anchor & self.exclusive_edge == 0 {
                return Err((INVALID_EXCLUSIVE_EDGE, "exclusive edge must be anchored"));
            }
        }
        Ok(())
    }

    // the edge a positive exclusive zone claims; None means the zone is
    // ignored for arrangement (spec: not an error)
    fn exclusive_edge(&self) -> Option<u32> {
        if self.exclusive <= 0 {
            return None;
        }
        if self.exclusive_edge != 0 {
            return Some(self.exclusive_edge);
        }
        let a = self.anchor;
        let lr = a & (A_LEFT | A_RIGHT);
        let tb = a & (A_TOP | A_BOTTOM);
        match (lr, tb) {
            // one edge, alone or with both perpendiculars
            (A_LEFT, 0) | (A_LEFT, 3) => Some(A_LEFT),
            (A_RIGHT, 0) | (A_RIGHT, 3) => Some(A_RIGHT),
            (0, A_TOP) | (12, A_TOP) => Some(A_TOP),
            (0, A_BOTTOM) | (12, A_BOTTOM) => Some(A_BOTTOM),
            _ => None,
        }
    }
}

pub struct LayerSurface {
    /// output slot this layer lives on, fixed at creation
    pub output: Cell<usize>,
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    me: Weak<LayerSurface>,
    pub surface: Rc<WlSurface>,
    pending: Cell<LayerState>,
    pub current: Cell<LayerState>,
    next_serial: Cell<u32>,
    last_sent: Cell<u32>,
    acked: Cell<u32>,
    ack_floor: Cell<u32>,
    scheduled: Cell<bool>,
    configured: Cell<bool>,
    /// last configured size; identical re-configures are configure-loop bait
    last_cfg: Cell<Option<(u32, u32)>>,
    /// placement from the arranger
    pub rect: Cell<Rect>,
    linked: Cell<bool>,
    closed: Cell<bool>,
    popups: RefCell<Vec<Rc<super::xdg::XdgPopup>>>,
}

impl LayerSurface {
    fn rc(&self) -> Rc<LayerSurface> {
        self.me.upgrade().expect("layer surface outlived its own rc")
    }

    fn edit(&self, f: impl FnOnce(&mut LayerState)) {
        let mut s = self.pending.get();
        f(&mut s);
        self.pending.set(s);
    }

    pub fn mapped(&self) -> bool {
        self.linked.get() && self.surface.mapped.get()
    }

    fn schedule_configure(&self) {
        if !self.scheduled.replace(true) {
            let state = &self.client.state;
            state.configures.borrow_mut().push(self.rc());
            state.configure_event.trigger();
        }
    }

    // configure carries the size the arranger would give the surface now
    fn send_configure_now(&self) {
        let (w, h) = compute_size(&self.client.state, self, self.current.get());
        if self.last_cfg.get() == Some((w, h)) {
            return;
        }
        self.last_cfg.set(Some((w, h)));
        let serial = self.next_serial.get();
        self.next_serial.set(serial.wrapping_add(1).max(1));
        self.last_sent.set(serial);
        self.client
            .event(|o| zwlr_layer_surface_v1::configure::send(o, self.id, serial, w, h));
    }

    pub fn send_closed(&self) {
        if !self.closed.replace(true) {
            self.client.event(|o| zwlr_layer_surface_v1::closed::send(o, self.id));
        }
    }

    pub fn for_each_popup(&self, mut f: impl FnMut(&Rc<super::xdg::XdgPopup>)) {
        for p in self.popups.borrow().iter() {
            f(p);
        }
    }

    pub fn unlink_popup(&self, id: ObjectId) {
        self.popups.borrow_mut().retain(|p| p.id != id);
    }

    // spec: unmapping resets everything to post-create defaults and the
    // whole configure cycle runs again on remap
    fn reset_after_unmap(&self) {
        let layer = self.current.get().layer;
        self.pending.set(LayerState::new(layer));
        self.current.set(LayerState::new(layer));
        self.configured.set(false);
        self.ack_floor.set(self.last_sent.get());
        self.last_cfg.set(None);
        for p in self.popups.borrow().iter() {
            p.send_done();
        }
    }

    fn unlink(&self, state: &Rc<State>) {
        if self.linked.replace(false) {
            state.layers.borrow_mut().retain(|l| l.id != self.id || l.client.id != self.client.id);
            arrange(state);
            apply_kb_lock(state);
            // a closed surface already left its (gone) output by name; the
            // stored slot points at whatever holds that index now
            if !self.surface.destroyed.get() && !self.closed.get() {
                crate::tree::send_surface_output(state, &self.surface, self.output.get(), false);
            }
            state.damage.trigger();
        }
    }
}

impl zwlr_layer_surface_v1::Handler for LayerSurface {
    fn set_size(
        &self,
        req: zwlr_layer_surface_v1::set_size::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.edit(|s| s.size = (req.width.min(65535), req.height.min(65535)));
        Ok(())
    }

    fn set_anchor(
        &self,
        req: zwlr_layer_surface_v1::set_anchor::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.anchor > 15 {
            self.client
                .protocol_error(self.id, INVALID_ANCHOR, "invalid anchor bits");
            return Ok(());
        }
        self.edit(|s| s.anchor = req.anchor);
        Ok(())
    }

    fn set_exclusive_zone(
        &self,
        req: zwlr_layer_surface_v1::set_exclusive_zone::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.edit(|s| s.exclusive = req.zone.max(-1).min(65535));
        Ok(())
    }

    fn set_margin(
        &self,
        req: zwlr_layer_surface_v1::set_margin::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.edit(|s| s.margin = (req.top, req.right, req.bottom, req.left));
        Ok(())
    }

    fn set_keyboard_interactivity(
        &self,
        req: zwlr_layer_surface_v1::set_keyboard_interactivity::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // v1-3 clients speak bool; anything nonzero meant exclusive
        let ki = if self.version < 4 {
            if req.keyboard_interactivity == 0 { KI_NONE } else { KI_EXCLUSIVE }
        } else {
            if req.keyboard_interactivity > KI_ON_DEMAND {
                self.client
                    .protocol_error(self.id, INVALID_KI, "invalid keyboard interactivity");
                return Ok(());
            }
            req.keyboard_interactivity
        };
        self.edit(|s| s.ki = ki);
        Ok(())
    }

    fn get_popup(
        &self,
        req: zwlr_layer_surface_v1::get_popup::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(popup) = self.client.objects.popup(req.popup) else {
            self.client.invalid_object(req.popup);
            return Ok(());
        };
        popup.set_layer_parent(&self.rc());
        self.popups.borrow_mut().push(popup);
        Ok(())
    }

    fn ack_configure(
        &self,
        req: zwlr_layer_surface_v1::ack_configure::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.serial == 0 || req.serial > self.last_sent.get() {
            self.client
                .protocol_error(self.id, INVALID_SURFACE_STATE, "ack of a serial that was never sent");
            return Ok(());
        }
        if req.serial <= self.acked.get() {
            self.client
                .protocol_error(self.id, INVALID_SURFACE_STATE, "ack serials must increase");
            return Ok(());
        }
        self.acked.set(req.serial);
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_layer_surface_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.unlink(&self.client.state);
        for p in self.popups.borrow().iter() {
            p.send_done();
        }
        self.popups.borrow_mut().clear();
        *self.surface.ext.borrow_mut() = Rc::new(crate::surface::NoneExt);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_layer(
        &self,
        req: zwlr_layer_surface_v1::set_layer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.layer > OVERLAY {
            self.client.protocol_error(self.id, INVALID_LAYER, "invalid layer");
            return Ok(());
        }
        self.edit(|s| s.layer = req.layer);
        Ok(())
    }

    fn set_exclusive_edge(
        &self,
        req: zwlr_layer_surface_v1::set_exclusive_edge::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.edge > A_RIGHT || req.edge.count_ones() > 1 {
            self.client
                .protocol_error(self.id, INVALID_EXCLUSIVE_EDGE, "invalid exclusive edge");
            return Ok(());
        }
        self.edit(|s| s.exclusive_edge = req.edge);
        Ok(())
    }
}

impl Object for LayerSurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_layer_surface_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_layer_surface_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.linked.set(false);
        self.popups.borrow_mut().clear();
    }
}

impl crate::shell::Configurable for LayerSurface {
    fn flush_configure(&self) {
        self.scheduled.set(false);
        if !self.surface.destroyed.get() && !self.closed.get() {
            self.send_configure_now();
        }
    }
}

// -- the wl_surface role hook --

pub struct LayerExt {
    pub ls: Rc<LayerSurface>,
}

impl SurfaceExt for LayerExt {
    fn layer_surface(&self) -> Option<Rc<LayerSurface>> {
        Some(self.ls.clone())
    }

    fn commit_requested(self: Rc<Self>, pending: Box<PendingState>) -> Option<Box<PendingState>> {
        if self.ls.closed.get() {
            // a closed surface is dead air until destroyed
            return None;
        }
        let attaching = matches!(&pending.buffer, Some(Some(_)));
        if attaching && self.ls.acked.get() <= self.ls.ack_floor.get() {
            self.ls.client.protocol_error(
                self.ls.id,
                INVALID_SURFACE_STATE,
                "buffer attached before the initial configure was acked",
            );
            return None;
        }
        Some(pending)
    }

    fn before_apply(&self) {
        let ls = &self.ls;
        let next = ls.pending.get();
        if let Err((code, msg)) = next.validate() {
            ls.client.protocol_error(ls.id, code, msg);
            return;
        }
        ls.current.set(next);
    }

    fn after_apply(&self) {
        let ls = self.ls.clone();
        let state = ls.client.state.clone();
        if ls.closed.get() {
            return;
        }
        if !ls.configured.get() {
            // the initial commit is answered with a configure and maps
            // nothing; the buffer gate in commit_requested enforces order
            ls.configured.set(true);
            ls.schedule_configure();
            return;
        }
        let mapped = ls.surface.mapped.get();
        if mapped && !ls.linked.get() {
            ls.linked.set(true);
            state.layers.borrow_mut().push(ls.clone());
            arrange(&state);
            apply_kb_lock(&state);
            // the enter event tells the client which output shows it (scale, geometry)
            crate::tree::send_surface_output(&state, &ls.surface, ls.output.get(), true);
            state.damage.trigger();
        } else if !mapped && ls.linked.get() {
            ls.unlink(&state);
            ls.reset_after_unmap();
        } else if ls.linked.get() {
            // a state change on a mapped surface can move everything
            arrange(&state);
            apply_kb_lock(&state);
        }
    }
}

// -- arrangement --

// where a surface with this state gets placed inside `avail`
fn place(s: LayerState, avail: Rect) -> Rect {
    let (mt, mr, mb, ml) = s.margin;
    let mut a = avail;
    if s.anchor & A_LEFT != 0 {
        a.x1 += ml;
    }
    if s.anchor & A_RIGHT != 0 {
        a.x2 -= mr;
    }
    if s.anchor & A_TOP != 0 {
        a.y1 += mt;
    }
    if s.anchor & A_BOTTOM != 0 {
        a.y2 -= mb;
    }
    let w = if s.size.0 > 0 { s.size.0 as i32 } else { a.width() };
    let h = if s.size.1 > 0 { s.size.1 as i32 } else { a.height() };
    // negative margins may push the box past the output; drawing clips
    let w = w.max(1);
    let h = h.max(1);
    let lr = A_LEFT | A_RIGHT;
    let x = match s.anchor & lr {
        x if x == A_LEFT => a.x1,
        x if x == A_RIGHT => a.x2 - w,
        // both or neither: centered
        _ => a.x1 + (a.width() - w) / 2,
    };
    let tb = A_TOP | A_BOTTOM;
    let y = match s.anchor & tb {
        y if y == A_TOP => a.y1,
        y if y == A_BOTTOM => a.y2 - h,
        _ => a.y1 + (a.height() - h) / 2,
    };
    Rect { x1: x, y1: y, x2: x + w, y2: y + h }
}

// exclusive zones accumulate: overlay down to background, mapping order
// within a layer; positives claim their edge and shrink what follows
pub fn arrange(state: &Rc<State>) {
    let layers = state.layers.borrow().clone();
    let slots: usize = state
        .display
        .borrow()
        .as_ref()
        .map(|d| d.outputs.borrow().len())
        .unwrap_or(1)
        .max(1);
    let mut changed = false;
    for slot in 0..slots {
        let out = slot_rect(state, slot);
        let mut usable = out;
        let on_slot = |l: &&Rc<LayerSurface>| l.output.get() == slot;
        for layer in [OVERLAY, TOP, BOTTOM, BACKGROUND] {
            for ls in layers
                .iter()
                .filter(on_slot)
                .filter(|l| l.current.get().layer == layer)
            {
                let s = ls.current.get();
                let Some(edge) = s.exclusive_edge() else { continue };
                ls.rect.set(place(s, usable));
                let zone = s.exclusive;
                let (mt, mr, mb, ml) = s.margin;
                match edge {
                    A_TOP => usable.y1 += zone + mt,
                    A_BOTTOM => usable.y2 -= zone + mb,
                    A_LEFT => usable.x1 += zone + ml,
                    _ => usable.x2 -= zone + mr,
                }
            }
        }
        if usable.width() < 1 || usable.height() < 1 {
            usable = out;
        }
        for ls in layers.iter().filter(on_slot) {
            let s = ls.current.get();
            if s.exclusive_edge().is_some() {
                continue;
            }
            // zone >= 0 respects the claimed edges (a positive zone without
            // a single anchored edge counts as zero); -1 ignores them
            let base = if s.exclusive >= 0 { usable } else { out };
            ls.rect.set(place(s, base));
        }
        if slot == 0 {
            changed |= state.usable.replace(usable) != usable;
        }
        if let Some(d) = state.display.borrow().as_ref() {
            if let Some(o) = d.outputs.borrow().get(slot) {
                changed |= o.usable.replace(usable) != usable;
            }
        }
    }
    for ls in layers.iter() {
        ls.schedule_configure();
    }
    if changed {
        for ws in crate::tree::visible_workspaces(state) {
            crate::tree::relayout(state, &ws);
        }
        state.damage.trigger();
    }
    // surfaces moved under a stationary cursor
    if let Some(seat) = state.seat.borrow().clone() {
        seat.repick(state);
    }
}

/// the global rect of an output slot; the union extent when headless
pub(crate) fn slot_rect(state: &Rc<State>, slot: usize) -> Rect {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(o) = d.outputs.borrow().get(slot) {
            return o.rect();
        }
    }
    let (ow, oh) = crate::tree::output_extent(state);
    Rect::new_sized_saturating(0, 0, ow.max(1), oh.max(1))
}

/// what the arranger left over on a slot; state.usable mirrors slot 0 for
/// the headless test path
fn usable_of(state: &Rc<State>, slot: usize) -> Rect {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(o) = d.outputs.borrow().get(slot) {
            return o.usable.get();
        }
    }
    state.usable.get()
}

// the size a configure should carry right now
fn compute_size(state: &Rc<State>, ls: &LayerSurface, s: LayerState) -> (u32, u32) {
    // an arranged surface's configure echoes the placement it already has
    if ls.linked.get() {
        let r = ls.rect.get();
        return (r.width() as u32, r.height() as u32);
    }
    let out = slot_rect(state, ls.output.get());
    let base = if s.exclusive >= 0 {
        let u = usable_of(state, ls.output.get());
        if u.is_empty() { out } else { u }
    } else {
        out
    };
    let r = place(s, base);
    (r.width() as u32, r.height() as u32)
}

// -- keyboard interactivity --

pub fn from_surface(_state: &Rc<State>, s: &Rc<WlSurface>) -> Option<Rc<LayerSurface>> {
    let ext = s.ext.borrow().clone();
    ext.layer_surface()
}

// the surface an exclusive top/overlay layer pins the seat to
pub fn kb_lock(state: &Rc<State>) -> Option<Rc<LayerSurface>> {
    let layers = state.layers.borrow();
    let mut lock: Option<Rc<LayerSurface>> = None;
    for ls in layers.iter() {
        let s = ls.current.get();
        if s.ki != KI_EXCLUSIVE || s.layer < TOP || !ls.mapped() {
            continue;
        }
        let better = match &lock {
            None => true,
            // overlay beats top; ties go to the latest mapped
            Some(cur) => s.layer >= cur.current.get().layer,
        };
        if better {
            lock = Some(ls.clone());
        }
    }
    lock
}

// route the keyboard to the lock holder, or release it when the lock died
pub fn apply_kb_lock(state: &Rc<State>) {
    let Some(seat) = state.seat.borrow().clone() else {
        return;
    };
    match kb_lock(state) {
        Some(ls) => {
            crate::input::focus::set_keyboard_focus(state, &seat, Some(ls.surface.clone()));
        }
        None => {
            let focused = seat.kb_focus.borrow().clone();
            let on_layer = focused
                .is_some_and(|s| s.role.get() == SurfaceRole::LayerSurface && !s.mapped.get());
            if on_layer {
                let ws = crate::tree::active(state);
                let (cx, cy) = crate::tree::cursor_pos(state);
                let next = crate::tree::window_at(state, cx, cy)
                    .map(|(w, ..)| w)
                    .or_else(|| ws.tiling.first())
                    .or_else(|| ws.top_float());
                crate::input::focus::set_keyboard_focus(
                    state,
                    &seat,
                    next.map(|w| w.surface()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::wl_surface;
    use crate::protocol::shm::test_buffer;
    use wl_surface::Handler as _;
    use zwlr_layer_shell_v1::Handler as _;
    use zwlr_layer_surface_v1::Handler as _;

    const ERR: ObjectId = ObjectId(1);

    fn mk_layer(
        client: &Rc<Client>,
        sid: u32,
        lid: u32,
        layer: u32,
    ) -> (Rc<WlSurface>, Rc<LayerSurface>) {
        let s = WlSurface::new(ObjectId(sid), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        let shell = Rc::new(LayerShell {
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
        let ls = from_surface_ext(&s);
        (s, ls)
    }

    fn from_surface_ext(s: &Rc<WlSurface>) -> Rc<LayerSurface> {
        let ext = s.ext.borrow().clone();
        ext.layer_surface().expect("surface has no layer ext")
    }

    fn commit(s: &Rc<WlSurface>) {
        s.commit(wl_surface::commit::Request {}).unwrap();
    }

    // anchor + size + zone, then the whole configure/ack/map cycle
    fn map_bar(
        state: &Rc<State>,
        client: &Rc<Client>,
        s: &Rc<WlSurface>,
        ls: &Rc<LayerSurface>,
        height: u32,
        zone: i32,
        buf: u32,
    ) {
        ls.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 4 | 8 })
            .unwrap();
        ls.set_size(zwlr_layer_surface_v1::set_size::Request { width: 0, height })
            .unwrap();
        ls.set_exclusive_zone(zwlr_layer_surface_v1::set_exclusive_zone::Request { zone })
            .unwrap();
        commit(s);
        crate::shell::xdg::flush_configures(state);
        ls.ack_configure(zwlr_layer_surface_v1::ack_configure::Request {
            serial: ls.last_sent.get(),
        })
        .unwrap();
        let b = test_buffer(client, ObjectId(buf), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        commit(s);
    }

    #[test]
    fn bare_commit_is_invalid() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, _ls) = mk_layer(&client, 10, 20, TOP);
        // defaults: size 0x0 with no anchors
        commit(&s);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn same_edge_bars_stack_and_shrink_the_tiling_area() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s1, l1) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s1, &l1, 30, 30, 40);
        assert!(l1.mapped());
        assert_eq!(l1.rect.get(), Rect { x1: 0, y1: 0, x2: 800, y2: 30 });
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 30, x2: 800, y2: 600 });

        let (s2, l2) = mk_layer(&client, 11, 21, TOP);
        map_bar(&state, &client, &s2, &l2, 24, 24, 41);
        // the second bar arranges inside what the first left over
        assert_eq!(l2.rect.get(), Rect { x1: 0, y1: 30, x2: 800, y2: 54 });
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 54, x2: 800, y2: 600 });
        assert_eq!(crate::tree::tiling_area(&state).y1, 54);

        // unmapping the first gives the space back and resets its state
        s1.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s1);
        assert!(!l1.mapped());
        assert_eq!(l1.current.get().anchor, 0);
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 24, x2: 800, y2: 600 });
    }

    #[test]
    fn mapping_a_bar_under_the_cursor_repicks_pointer_focus() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        // park the pointer where the bar is about to appear (inside the
        // 64x64 test buffer's extent)
        seat.warp(&state, 30.0, 10.0);
        assert!(seat.pointer_focus().is_none());
        let (s1, l1) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s1, &l1, 30, 30, 40);
        // no motion happened; the map alone must claim pointer focus
        assert!(seat
            .pointer_focus()
            .is_some_and(|f| Rc::ptr_eq(&f, &s1)));
        // and unmapping hands it back
        s1.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s1);
        assert!(seat.pointer_focus().is_none());
    }

    #[test]
    fn identical_configures_are_deduped() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        // the map already re-scheduled via arrange; nothing changed, so
        // only the initial configure went out
        crate::shell::xdg::flush_configures(&state);
        commit(&s);
        crate::shell::xdg::flush_configures(&state);
        assert_eq!(count_events(&client.queued_out_bytes(), ls.id, 0), 1);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
    }

    #[test]
    fn exclusive_top_layer_pins_the_keyboard() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let (s, ls) = mk_layer(&client, 10, 20, OVERLAY);
        ls.set_keyboard_interactivity(zwlr_layer_surface_v1::set_keyboard_interactivity::Request {
            keyboard_interactivity: KI_EXCLUSIVE,
        })
        .unwrap();
        map_bar(&state, &client, &s, &ls, 40, 0, 40);
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)));
        // unmap releases the lock
        s.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        assert!(kb_lock(&state).is_none());
        assert!(seat.kb_focus.borrow().is_none());
    }
}
