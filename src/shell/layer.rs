// wlr-layer-shell: anchors, exclusive zones, layers, keyboard interactivity.
//
// all state is double buffered and re-validated on every commit - the
// defaults are already invalid, so a bare commit errors per spec. unmap
// resets everything back to those defaults and restarts the configure
// cycle. exclusive zones accumulate in arrangement order (overlay down to
// background, mapping order within a layer), so two bars on one edge
// stack instead of overlapping; what's left over is the tiling area.

use crate::client::{Client, ClientError, ClientId, Object};
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
const ALREADY_CONSTRUCTED: u32 = 2;
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
        let has_buffer = surface.buffer.borrow().is_some()
            || matches!(&surface.pending.borrow().buffer, Some(Some(_)));
        if has_buffer {
            c.protocol_error(
                self.id,
                ALREADY_CONSTRUCTED,
                "the surface already has a buffer attached or committed",
            );
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
            namespace: req.namespace.clone(),
            pending: Cell::new(LayerState::new(req.layer)),
            current: Cell::new(LayerState::new(req.layer)),
            created_layer: req.layer,
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
            anim: RefCell::new(None),
            last_batch: RefCell::new((Vec::new(), Vec::new(), 0..0)),
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
    /// the get_layer_surface namespace; rules match against it
    pub namespace: String,
    pending: Cell<LayerState>,
    pub current: Cell<LayerState>,
    /// the get_layer_surface layer argument; unmap resets back to it
    created_layer: u32,
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
    /// the open animation; unmap capture reuses the closing-window path
    pub anim: RefCell<Option<(crate::anim::Anim, crate::config::Style)>>,
    /// ops + texture keys from the last compose, for the close capture
    pub last_batch: RefCell<(
        Vec<crate::render::renderer::RenderOp>,
        Vec<(crate::client::ClientId, u64)>,
        std::ops::Range<usize>,
    )>,
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

    /// the edge this surface hangs off, for slide animations; anchored to
    /// both opposing edges (or neither) means no clear home edge
    pub fn slide_dir(&self) -> Option<crate::config::Dir> {
        use crate::config::Dir;
        let a = self.current.get().anchor;
        let (top, bottom) = (a & A_TOP != 0, a & A_BOTTOM != 0);
        let (left, right) = (a & A_LEFT != 0, a & A_RIGHT != 0);
        match (top, bottom, left, right) {
            (true, false, ..) => Some(Dir::Up),
            (false, true, ..) => Some(Dir::Down),
            (.., true, false) => Some(Dir::Left),
            (.., false, true) => Some(Dir::Right),
            _ => None,
        }
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

    pub fn for_each_popup_rev(&self, mut f: impl FnMut(&Rc<super::xdg::XdgPopup>)) {
        for p in self.popups.borrow().iter().rev() {
            f(p);
        }
    }

    pub fn unlink_popup(&self, id: ObjectId) {
        self.popups.borrow_mut().retain(|p| p.id != id);
    }

    // spec: unmapping resets everything to post-create defaults and the
    // whole configure cycle runs again on remap
    fn reset_after_unmap(&self) {
        let layer = self.created_layer;
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

    /// the output the surface is pinned to is gone: it will no longer be
    /// shown - leave the output, send closed, ignore it until destroy
    pub fn close_for_output_loss(&self, state: &Rc<State>, dead: Option<&str>) {
        if self.linked.get() && !self.surface.destroyed.get() {
            if let Some(name) = dead {
                crate::tree::send_surface_output_named(&self.surface, name, false);
            }
        }
        self.send_closed();
        for p in self.popups.borrow().iter() {
            p.send_done();
        }
        self.unlink(state);
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
        // spec: the popup must have been created with a NULL parent
        if popup.has_parent() {
            self.client.implementation_error("the popup already has a parent");
            return Ok(());
        }
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
        // backdrop content feeds the blur cache; its commits invalidate it
        if ls.current.get().layer <= BOTTOM {
            if let Some(d) = state.display.borrow().as_ref() {
                if let Some(out) = d.outputs.borrow().get(ls.output.get()) {
                    out.blur_dirty.set(true);
                }
            }
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
            // on_demand top/overlay grabs the keyboard the moment it maps
            // (launcher pattern); exclusive locks still win
            if ls.current.get().ki == KI_ON_DEMAND
                && ls.current.get().layer >= TOP
                && kb_lock(&state).is_none()
            {
                if let Some(seat) = state.seat.borrow().clone() {
                    if seat.popup_grab.borrow().is_empty() {
                        crate::input::focus::set_keyboard_focus(
                            &state,
                            &seat,
                            Some(ls.surface.clone()),
                        );
                    }
                }
            }
            // the enter event tells the client which output shows it (scale, geometry)
            crate::tree::send_surface_output(&state, &ls.surface, ls.output.get(), true);
            start_layer_open(&state, &ls);
            state.damage.trigger();
        } else if !mapped && ls.linked.get() {
            capture_layer_close(&state, &ls);
            ls.unlink(&state);
            ls.reset_after_unmap();
        } else if ls.linked.get() {
            // a state change on a mapped surface can move everything
            arrange(&state);
            apply_kb_lock(&state);
        }
    }
}

fn start_layer_open(state: &Rc<State>, ls: &Rc<LayerSurface>) {
    let cfg = state.config.borrow().clone();
    let Some(motion) = cfg.animations.motion(crate::config::AnimKind::LayerOpen) else {
        *ls.anim.borrow_mut() = None;
        return;
    };
    let style = match cfg.animations.layer_open.style.clone() {
        crate::config::Style::Default => crate::config::Style::Slide { dir: None },
        s => s,
    };
    state.anim_clock.touch();
    *ls.anim.borrow_mut() = Some((
        crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 0.0, 1.0, 0.0),
        style,
    ));
}

fn capture_layer_close(state: &Rc<State>, ls: &Rc<LayerSurface>) {
    let cfg = state.config.borrow().clone();
    let Some(motion) = cfg.animations.motion(crate::config::AnimKind::LayerClose) else {
        return;
    };
    let out = state
        .display
        .borrow()
        .as_ref()
        .and_then(|d| d.outputs.borrow().get(ls.output.get()).cloned());
    let Some(out) = out else { return };
    let style = match cfg.animations.layer_close.style.clone() {
        crate::config::Style::Default => crate::config::Style::Slide { dir: ls.slide_dir() },
        crate::config::Style::Slide { dir: None } => {
            crate::config::Style::Slide { dir: ls.slide_dir() }
        }
        s => s,
    };
    state.anim_clock.touch();
    let anim =
        crate::config::build_anim(&state.anim_clock, motion, &cfg.animations, 1.0, 0.0, 0.0);
    let batch = std::mem::take(&mut *ls.last_batch.borrow_mut());
    if let Some(cw) = crate::output::seize_batch(&out, batch, ls.rect.get(), anim, style) {
        crate::output::push_closing_layer(&out, cw);
        state.damage.trigger();
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

/// a dead client never sends destroy: sweep its layer surfaces out and
/// give any exclusive-zone space back to the tiling area
pub fn drop_client(state: &Rc<State>, id: ClientId) {
    let removed = {
        let mut layers = state.layers.borrow_mut();
        let before = layers.len();
        layers.retain(|l| l.client.id != id);
        before != layers.len()
    };
    if removed {
        arrange(state);
        apply_kb_lock(state);
        state.damage.trigger();
    }
}

// route the keyboard to the lock holder, or release it when the lock died
pub fn apply_kb_lock(state: &Rc<State>) {
    let Some(seat) = state.seat.borrow().clone() else {
        return;
    };
    // an active popup grab owns the keyboard; the lock re-asserts when
    // the chain empties (popup_closed calls back in)
    if !seat.popup_grab.borrow().is_empty() {
        return;
    }
    match kb_lock(state) {
        Some(ls) => {
            crate::input::focus::set_keyboard_focus(state, &seat, Some(ls.surface.clone()));
        }
        None => {
            let focused = seat.kb_focus.borrow().clone();
            // release when the holder unmapped, was closed, or dropped
            // interactivity to none; a mapped on_demand holder keeps the
            // keyboard
            let on_layer = focused.is_some_and(|s| {
                if s.role.get() != SurfaceRole::LayerSurface {
                    return false;
                }
                if !s.mapped.get() {
                    return true;
                }
                s.ext
                    .borrow()
                    .layer_surface()
                    .is_some_and(|l| l.closed.get() || l.current.get().ki == KI_NONE)
            });
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

    // the configure/ack/attach/commit cycle once the state is staged
    fn map_cycle(
        state: &Rc<State>,
        client: &Rc<Client>,
        s: &Rc<WlSurface>,
        ls: &Rc<LayerSurface>,
        buf: u32,
    ) {
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
        map_cycle(state, client, s, ls, buf);
    }

    // (cited object, code) from the first wl_display.error on the wire
    fn first_error(bytes: &[u8]) -> Option<(u32, u32)> {
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let obj = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
            let w2 = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            let len = ((w2 >> 16) as usize).max(8);
            if obj == ERR.0 && w2 & 0xffff == 0 && off + 16 <= bytes.len() {
                let cited = u32::from_ne_bytes(bytes[off + 8..off + 12].try_into().unwrap());
                let code = u32::from_ne_bytes(bytes[off + 12..off + 16].try_into().unwrap());
                return Some((cited, code));
            }
            off += len;
        }
        None
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
    fn a_dead_client_hands_its_exclusive_zone_back() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s1, l1) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s1, &l1, 30, 30, 40);
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 30, x2: 800, y2: 600 });
        // the client is killed: no destroy requests, just teardown
        client.objects.destroy();
        drop_client(&state, client.id);
        assert!(state.layers.borrow().is_empty());
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 0, x2: 800, y2: 600 });
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
    fn on_demand_focuses_on_map_and_releases_on_downgrade() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        ls.set_keyboard_interactivity(
            zwlr_layer_surface_v1::set_keyboard_interactivity::Request {
                keyboard_interactivity: KI_ON_DEMAND,
            },
        )
        .unwrap();
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        // the launcher pattern: typable the moment it maps
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)));
        // dropping interactivity to none hands the keyboard back
        ls.set_keyboard_interactivity(
            zwlr_layer_surface_v1::set_keyboard_interactivity::Request {
                keyboard_interactivity: KI_NONE,
            },
        )
        .unwrap();
        commit(&s);
        assert!(!seat.kb_focus.borrow().as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)));
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

    #[test]
    fn output_loss_closes_the_bar_and_returns_its_zone() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 30, x2: 800, y2: 600 });
        ls.close_for_output_loss(&state, None);
        ls.close_for_output_loss(&state, None);
        // closed once despite the double call
        assert_eq!(count_events(&client.queued_out_bytes(), ls.id, 1), 1);
        assert!(!ls.mapped());
        assert!(state.layers.borrow().is_empty());
        assert_eq!(state.usable.get(), Rect { x1: 0, y1: 0, x2: 800, y2: 600 });
    }

    #[test]
    fn a_closed_surface_ignores_commits_until_destroyed() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        ls.close_for_output_loss(&state, None);
        let configures = count_events(&client.queued_out_bytes(), ls.id, 0);
        // further changes are ignored, never errored on
        let b = test_buffer(&client, ObjectId(41), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        crate::shell::xdg::flush_configures(&state);
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
        assert!(!ls.mapped());
        assert!(state.layers.borrow().is_empty());
        assert_eq!(count_events(&client.queued_out_bytes(), ls.id, 0), configures);
        // the client answers closed with destroy
        ls.destroy(zwlr_layer_surface_v1::destroy::Request {}).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 0);
    }

    #[test]
    fn output_loss_releases_an_exclusive_keyboard_lock() {
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
        ls.close_for_output_loss(&state, None);
        assert!(kb_lock(&state).is_none());
        assert!(!seat.kb_focus.borrow().as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)));
    }

    #[test]
    fn output_loss_repicks_pointer_focus_from_under_the_dead_bar() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        seat.warp(&state, 30.0, 10.0);
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        assert!(seat.pointer_focus().is_some_and(|f| Rc::ptr_eq(&f, &s)));
        ls.close_for_output_loss(&state, None);
        assert!(seat.pointer_focus().is_none());
    }

    #[test]
    fn perpendicular_exclusive_zones_agree_with_placement() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        // a left dock claims 40px of the edge
        let (s1, dock) = mk_layer(&client, 10, 20, TOP);
        dock.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 2 | 4 })
            .unwrap();
        dock.set_size(zwlr_layer_surface_v1::set_size::Request { width: 40, height: 0 })
            .unwrap();
        dock.set_exclusive_zone(zwlr_layer_surface_v1::set_exclusive_zone::Request { zone: 40 })
            .unwrap();
        map_cycle(&state, &client, &s1, &dock, 40);
        assert_eq!(dock.rect.get(), Rect { x1: 0, y1: 0, x2: 40, y2: 600 });
        // a top bar arranges inside what the dock left over, and its
        // configure carries that same width
        let (s2, bar) = mk_layer(&client, 11, 21, TOP);
        map_bar(&state, &client, &s2, &bar, 30, 30, 41);
        assert_eq!(bar.rect.get(), Rect { x1: 40, y1: 0, x2: 800, y2: 30 });
        assert_eq!(bar.last_cfg.get(), Some((760, 30)));
        assert_eq!(state.usable.get(), Rect { x1: 40, y1: 30, x2: 800, y2: 600 });
    }

    #[test]
    fn positive_zone_without_a_single_edge_is_treated_as_zero() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s1, dock) = mk_layer(&client, 10, 20, TOP);
        dock.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 2 | 4 })
            .unwrap();
        dock.set_size(zwlr_layer_surface_v1::set_size::Request { width: 40, height: 0 })
            .unwrap();
        dock.set_exclusive_zone(zwlr_layer_surface_v1::set_exclusive_zone::Request { zone: 40 })
            .unwrap();
        map_cycle(&state, &client, &s1, &dock, 40);
        // corner-anchored with a positive zone: placed inside the usable
        // area and reserving nothing
        let (s2, corner) = mk_layer(&client, 11, 21, TOP);
        corner
            .set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 4 })
            .unwrap();
        corner
            .set_size(zwlr_layer_surface_v1::set_size::Request { width: 100, height: 100 })
            .unwrap();
        corner
            .set_exclusive_zone(zwlr_layer_surface_v1::set_exclusive_zone::Request { zone: 10 })
            .unwrap();
        map_cycle(&state, &client, &s2, &corner, 41);
        assert_eq!(corner.rect.get(), Rect { x1: 40, y1: 0, x2: 140, y2: 100 });
        assert_eq!(state.usable.get(), Rect { x1: 40, y1: 0, x2: 800, y2: 600 });
    }

    #[test]
    fn unmap_restores_the_creation_layer() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        map_bar(&state, &client, &s, &ls, 30, 30, 40);
        ls.set_layer(zwlr_layer_surface_v1::set_layer::Request { layer: BACKGROUND })
            .unwrap();
        commit(&s);
        assert_eq!(ls.current.get().layer, BACKGROUND);
        s.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        assert!(!ls.mapped());
        assert_eq!(ls.current.get().layer, TOP);
    }

    #[test]
    fn a_committed_buffer_gets_already_constructed() {
        let (_state, client) = test_client();
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        let b = test_buffer(&client, ObjectId(40), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        commit(&s);
        let shell = Rc::new(LayerShell {
            id: ObjectId(90),
            client: client.clone(),
            version: 5,
        });
        shell
            .get_layer_surface(zwlr_layer_shell_v1::get_layer_surface::Request {
                id: ObjectId(20),
                surface: ObjectId(10),
                output: ObjectId::NONE,
                layer: TOP,
                namespace: "test".to_string(),
            })
            .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((90, ALREADY_CONSTRUCTED))
        );
    }

    #[test]
    fn a_pending_attach_gets_already_constructed() {
        let (_state, client) = test_client();
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        // attached but never committed still counts
        let b = test_buffer(&client, ObjectId(40), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 })
            .unwrap();
        let shell = Rc::new(LayerShell {
            id: ObjectId(90),
            client: client.clone(),
            version: 5,
        });
        shell
            .get_layer_surface(zwlr_layer_shell_v1::get_layer_surface::Request {
                id: ObjectId(20),
                surface: ObjectId(10),
                output: ObjectId::NONE,
                layer: TOP,
                namespace: "test".to_string(),
            })
            .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((90, ALREADY_CONSTRUCTED))
        );
    }

    #[test]
    fn negative_margins_overhang_the_output() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let (s, ls) = mk_layer(&client, 10, 20, TOP);
        ls.set_anchor(zwlr_layer_surface_v1::set_anchor::Request { anchor: 1 | 4 | 8 })
            .unwrap();
        ls.set_size(zwlr_layer_surface_v1::set_size::Request { width: 0, height: 30 })
            .unwrap();
        ls.set_margin(zwlr_layer_surface_v1::set_margin::Request {
            top: 0,
            right: -50,
            bottom: 0,
            left: -50,
        })
        .unwrap();
        map_cycle(&state, &client, &s, &ls, 40);
        // the layout keeps the overhang; drawing clips it to the output
        assert_eq!(ls.rect.get(), Rect { x1: -50, y1: 0, x2: 850, y2: 30 });
        assert_eq!(ls.last_cfg.get(), Some((900, 30)));
    }
}
