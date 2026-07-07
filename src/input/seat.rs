// the seat. events gate on the binding's version, not the client's -
// a client may bind wl_seat many times at different versions. one xkb
// state per seat; keys process before any focus/bind check.

use super::keymap::{KbState, Keymap, Mods};
use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::interfaces::{wl_keyboard, wl_pointer, wl_seat, zwp_relative_pointer_v1};
use crate::protocol::pointer_constraints::{Constraint, Kind};
use crate::protocol::relative_pointer::RelativePointer;
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, Fixed, ObjectId};
use crate::protocol::globals::Global;
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const CAP_POINTER: u32 = 1;
const CAP_KEYBOARD: u32 = 2;
const MISSING_CAPABILITY: u32 = 0;

const XKB_V1: u32 = 1;
/// server-side repeat starts at wl_keyboard v10
const REPEATED_SINCE: u32 = 10;

pub struct SeatGlobal {
    pub keymap: Rc<Keymap>,
    pub kb_state: RefCell<KbState>,
    bindings: RefCell<HashMap<ClientId, Vec<Rc<WlSeat>>>>,
    pub kb_focus: RefCell<Option<Rc<WlSurface>>>,
    /// held keys in press order - the enter array
    pub pressed: RefCell<Vec<u32>>,
    pub mods: Cell<Mods>,
    /// server-side repeat, v10+ keyboards only. the version counter
    /// invalidates any timer already in flight
    repeat_key: Cell<Option<u32>>,
    repeat_version: crate::util::NumCell<u64>,
    pub repeat_armed: crate::util::AsyncEvent,
    /// pointer position and the surface under it, pinned while a button is
    /// held (implicit grab); ptr_origin is that surface's global origin
    pub ptr_x: Cell<f64>,
    pub ptr_y: Cell<f64>,
    ptr_focus: RefCell<Option<Rc<WlSurface>>>,
    ptr_origin: Cell<(i32, i32)>,
    ptr_buttons: RefCell<Vec<u32>>,
    /// serial of the most recent button press; drag/move grabs validate on it
    last_press_serial: Cell<u32>,
    // clipboard state rides on the seat: devices, sources, selection
    pub data: crate::protocol::data_device::DataDevices,
    pub primary: crate::protocol::primary_selection::PrimaryDevices,
    // the popup grab chain, bottom first; keyboard focus to restore on
    // full dismissal
    pub popup_grab: RefCell<Vec<Rc<crate::shell::xdg::XdgPopup>>>,
    pub grab_prev_focus: RefCell<Option<Rc<WlSurface>>>,
    pub relative: RefCell<HashMap<ClientId, Vec<Rc<RelativePointer>>>>,
    pub constraints: RefCell<Vec<Rc<Constraint>>>,
}

impl SeatGlobal {
    pub fn new() -> Result<Rc<SeatGlobal>, String> {
        let keymap = Keymap::new_default()?;
        let kb_state = RefCell::new(keymap.create_state());
        Ok(Rc::new(SeatGlobal {
            keymap,
            kb_state,
            bindings: RefCell::new(HashMap::new()),
            kb_focus: RefCell::new(None),
            pressed: RefCell::new(Vec::new()),
            mods: Cell::new(Mods::default()),
            repeat_key: Cell::new(None),
            repeat_version: crate::util::NumCell::new(0),
            repeat_armed: crate::util::AsyncEvent::default(),
            ptr_x: Cell::new(0.0),
            ptr_y: Cell::new(0.0),
            ptr_focus: RefCell::new(None),
            ptr_origin: Cell::new((0, 0)),
            ptr_buttons: RefCell::new(Vec::new()),
            last_press_serial: Cell::new(0),
            data: Default::default(),
            primary: Default::default(),
            popup_grab: RefCell::new(Vec::new()),
            grab_prev_focus: RefCell::new(None),
            relative: RefCell::new(HashMap::new()),
            constraints: RefCell::new(Vec::new()),
        }))
    }

    pub fn cancel_repeat(&self) {
        self.repeat_key.set(None);
        self.repeat_version.fetch_add(1);
    }

    fn arm_repeat(&self, key: u32) {
        self.repeat_key.set(Some(key));
        self.repeat_version.fetch_add(1);
        self.repeat_armed.trigger();
    }

    /// one persistent future per seat
    pub async fn repeat_loop(self: Rc<Self>, state: Rc<State>) {
        use crate::util::Time;
        loop {
            self.repeat_armed.triggered().await;
            let mut first = true;
            loop {
                let version = self.repeat_version.get();
                let Some(key) = self.repeat_key.get() else {
                    break;
                };
                let (rate, delay) = {
                    let c = state.config.borrow();
                    (c.repeat_rate.max(1) as u64, c.repeat_delay.max(1) as u64)
                };
                let wait_ns = if first {
                    delay * 1_000_000
                } else {
                    1_000_000_000 / rate
                };
                first = false;
                let deadline = Time::from_nsec(Time::now().nsec() + wait_ns);
                if state.ring.timeout(deadline).await.is_err() {
                    return;
                }
                // superseded or cancelled while we slept
                if self.repeat_version.get() != version || self.repeat_key.get() != Some(key) {
                    break;
                }
                self.repeat_fire(&state, key);
            }
        }
    }

    /// v10+ got rate=0 and rely on us for Repeated; v4-9 repeat client-side
    fn repeat_fire(&self, state: &Rc<State>, key: u32) {
        const REPEATED: u32 = 2;
        let focus = self.kb_focus.borrow().clone();
        let Some(surface) = focus.filter(|s| !s.destroyed.get()) else {
            self.cancel_repeat();
            return;
        };
        let client = &surface.client;
        let serial = state.next_serial(Some(client)) as u32;
        let ms = (crate::util::Time::now().nsec() / 1_000_000) as u32;
        self.for_each_keyboard(client.id, REPEATED_SINCE, |kb| {
            kb.client
                .event(|o| wl_keyboard::key::send(o, kb.id, serial, ms, key, REPEATED));
        });
    }

    pub fn drop_client(&self, id: ClientId) {
        self.bindings.borrow_mut().remove(&id);
        self.data.drop_client(id);
        self.primary.drop_client(id);
        self.popup_grab
            .borrow_mut()
            .retain(|p| p.client.id != id);
        let focused = self
            .kb_focus
            .borrow()
            .as_ref()
            .map(|s| s.client.id == id)
            .unwrap_or(false);
        if focused {
            self.kb_focus.borrow_mut().take();
        }
    }

    /// every keyboard of one client whose binding is new enough
    pub fn for_each_keyboard(
        &self,
        client: ClientId,
        min_version: u32,
        mut f: impl FnMut(&Rc<WlKeyboard>),
    ) {
        if let Some(seats) = self.bindings.borrow().get(&client) {
            for seat in seats {
                if seat.version >= min_version {
                    for kb in seat.keyboards.borrow().iter() {
                        f(kb);
                    }
                }
            }
        }
    }

    pub fn keys_bytes(&self) -> Vec<u8> {
        let pressed = self.pressed.borrow();
        let mut bytes = Vec::with_capacity(pressed.len() * 4);
        for k in pressed.iter() {
            bytes.extend_from_slice(&k.to_le_bytes());
        }
        bytes
    }
}

impl Global for SeatGlobal {
    fn interface(&self) -> &'static str {
        wl_seat::NAME
    }

    fn version(&self) -> u32 {
        9
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let state = client.state.clone();
        let seat_global = state
            .seat
            .borrow()
            .clone()
            .expect("seat global bound while seat exists");
        let seat = Rc::new(WlSeat {
            id,
            client: client.clone(),
            version,
            global: seat_global.clone(),
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        });
        client.add_client_obj(seat.clone())?;
        client.event(|o| {
            wl_seat::capabilities::send(o, id, CAP_POINTER | CAP_KEYBOARD);
        });
        if version >= wl_seat::name::SINCE {
            client.event(|o| wl_seat::name::send(o, id, "seat0"));
        }
        seat_global
            .bindings
            .borrow_mut()
            .entry(client.id)
            .or_default()
            .push(seat);
        Ok(())
    }
}

pub struct WlSeat {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    global: Rc<SeatGlobal>,
    keyboards: RefCell<Vec<Rc<WlKeyboard>>>,
    pointers: RefCell<Vec<Rc<WlPointer>>>,
}

impl wl_seat::Handler for WlSeat {
    fn get_pointer(
        &self,
        req: wl_seat::get_pointer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ptr = Rc::new(WlPointer {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
        });
        self.client.add_client_obj(ptr.clone())?;
        self.pointers.borrow_mut().push(ptr);
        Ok(())
    }

    fn get_keyboard(
        &self,
        req: wl_seat::get_keyboard::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let kb = Rc::new(WlKeyboard {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
        });
        self.client.add_client_obj(kb.clone())?;
        kb.send_keymap(&self.global.keymap);
        if self.version >= wl_keyboard::repeat_info::SINCE {
            // zero rate tells v10+ the server sends Repeated; don't repeat locally
            let (rate, delay) = if self.version >= REPEATED_SINCE {
                (0, 0)
            } else {
                let c = self.client.state.config.borrow();
                (c.repeat_rate, c.repeat_delay)
            };
            self.client
                .event(|o| wl_keyboard::repeat_info::send(o, kb.id, rate, delay));
        }
        // late enter: focus may already be ours
        let focus = self.global.kb_focus.borrow().clone();
        if let Some(surface) = focus {
            if surface.client.id == self.client.id {
                let serial = self.client.state.next_serial(Some(&self.client)) as u32;
                let keys = self.global.keys_bytes();
                let mods = self.global.mods.get();
                self.client.event(|o| {
                    wl_keyboard::enter::send(o, kb.id, serial, surface.id, &keys);
                });
                kb.send_modifiers(serial, mods);
            }
        }
        self.keyboards.borrow_mut().push(kb);
        Ok(())
    }

    fn get_touch(
        &self,
        req: wl_seat::get_touch::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = req;
        self.client.protocol_error(
            self.id,
            MISSING_CAPABILITY,
            "this seat has no touch capability",
        );
        Ok(())
    }

    fn release(&self, _req: wl_seat::release::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seats) = self.global.bindings.borrow_mut().get_mut(&self.client.id) {
            seats.retain(|s| s.id != self.id);
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlSeat {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_seat::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_seat::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.global.drop_client(self.client.id);
        self.keyboards.borrow_mut().clear();
        self.pointers.borrow_mut().clear();
    }
}

// -- wl_keyboard --

pub struct WlKeyboard {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl WlKeyboard {
    fn send_keymap(&self, map: &Keymap) {
        // same sealed fd for all versions; sealing blocks writes, no copy path yet
        let fd = map.fd.clone();
        let size = map.size;
        self.client
            .event(|o| wl_keyboard::keymap::send(o, self.id, XKB_V1, fd, size));
    }

    pub fn send_modifiers(&self, serial: u32, mods: Mods) {
        self.client.event(|o| {
            wl_keyboard::modifiers::send(
                o,
                self.id,
                serial,
                mods.depressed,
                mods.latched,
                mods.locked,
                mods.group,
            )
        });
    }
}

impl wl_keyboard::Handler for WlKeyboard {
    fn release(
        &self,
        _req: wl_keyboard::release::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlKeyboard {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_keyboard::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_keyboard::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_pointer (events land with the pointer routing pass) --

pub struct WlPointer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wl_pointer::Handler for WlPointer {
    fn set_cursor(
        &self,
        _req: wl_pointer::set_cursor::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // cursor snapshotting arrives with pointer routing
        Ok(())
    }

    fn release(
        &self,
        _req: wl_pointer::release::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlPointer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_pointer::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_pointer::dispatch(&*self, self.version, opcode, r)
    }
}

// -- key delivery --

impl SeatGlobal {
    /// one key edge: xkb, then binds, then the client
    pub fn key(&self, state: &Rc<State>, time_usec: u64, key: u32, pressed: bool) -> KeyAction {
        let changed = self
            .kb_state
            .borrow_mut()
            .process(&self.keymap, key, pressed);
        if let Some(mods) = changed {
            self.mods.set(mods);
        }
        {
            let mut held = self.pressed.borrow_mut();
            if pressed {
                if !held.contains(&key) {
                    held.push(key);
                }
            } else {
                held.retain(|&k| k != key);
            }
        }

        // binds exact-match the depressed set masked to shift|ctrl|alt|super
        if pressed {
            const MASK: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 6);
            const CTRL_ALT: u32 = (1 << 2) | (1 << 3);
            let held_mods = self.mods.get().depressed & MASK;
            if held_mods == CTRL_ALT {
                let vt = match key {
                    59..=68 => Some(key - 58),
                    87 => Some(11),
                    88 => Some(12),
                    _ => None,
                };
                if let Some(vt) = vt {
                    self.cancel_repeat(); // bound keys never repeat
                    return KeyAction::SwitchVt(vt);
                }
            }
            // configured binds, exact-set match; vt switching stays
            // hardcoded above so a broken config can't strand the seat
            let cfg = state.config.borrow().clone();
            use crate::config::BindKind;
            for b in cfg.binds.iter() {
                if held_mods == b.mods && key == b.key {
                    // release and mouse binds wait for their own dispatch paths
                    if matches!(b.kind, BindKind::Release | BindKind::Mouse) {
                        continue;
                    }
                    self.cancel_repeat();
                    return KeyAction::Act(b.action.clone());
                }
            }
        }

        // never deliver to a destroyed surface
        let focus = self.kb_focus.borrow().clone();
        let focus = match focus {
            Some(s) if !s.destroyed.get() => Some(s),
            Some(_) => {
                self.kb_focus.borrow_mut().take();
                None
            }
            None => None,
        };
        if let Some(surface) = focus {
            let client = &surface.client;
            let serial = state.next_serial(Some(client)) as u32;
            let ms = (time_usec / 1000) as u32;
            let mods = changed;
            self.for_each_keyboard(client.id, 1, |kb| {
                kb.client.event(|o| {
                    wl_keyboard::key::send(o, kb.id, serial, ms, key, pressed as u32)
                });
                if let Some(m) = mods {
                    kb.send_modifiers(serial, m);
                }
            });
            let group = self.mods.get().group;
            if pressed && self.keymap.repeats(key, group) {
                self.arm_repeat(key);
            } else if !pressed && self.repeat_key.get() == Some(key) {
                self.cancel_repeat();
            }
        }
        KeyAction::Handled
    }

    pub fn for_each_pointer(
        &self,
        client: ClientId,
        min_version: u32,
        mut f: impl FnMut(&Rc<WlPointer>),
    ) {
        if let Some(seats) = self.bindings.borrow().get(&client) {
            for seat in seats {
                if seat.version >= min_version {
                    for p in seat.pointers.borrow().iter() {
                        f(p);
                    }
                }
            }
        }
    }

    fn ptr_frame(&self, client: ClientId) {
        self.for_each_pointer(client, wl_pointer::frame::SINCE, |p| {
            p.client.event(|o| wl_pointer::frame::send(o, p.id));
        });
    }

    /// deepest mapped surface under the global point, in z order
    fn surface_at(&self, state: &Rc<State>, x: f64, y: f64) -> Option<(Rc<WlSurface>, i32, i32)> {
        crate::tree::surface_at(state, x as i32, y as i32)
    }

    // -- relative pointers and constraints --

    pub fn add_relative_pointer(&self, client: ClientId, rp: Rc<RelativePointer>) {
        self.relative.borrow_mut().entry(client).or_default().push(rp);
    }

    pub fn remove_relative_pointer(&self, client: ClientId, id: ObjectId) {
        if let Some(list) = self.relative.borrow_mut().get_mut(&client) {
            list.retain(|r| r.id != id);
        }
    }

    pub fn send_relative(
        &self,
        client: ClientId,
        time_usec: u64,
        dx: f64,
        dy: f64,
        udx: f64,
        udy: f64,
    ) {
        let rel = self.relative.borrow();
        let Some(list) = rel.get(&client) else { return };
        let hi = (time_usec >> 32) as u32;
        let lo = time_usec as u32;
        let (fdx, fdy) = (Fixed::from_f64(dx), Fixed::from_f64(dy));
        let (fux, fuy) = (Fixed::from_f64(udx), Fixed::from_f64(udy));
        for r in list.iter() {
            r.client.event(|o| {
                zwp_relative_pointer_v1::relative_motion::send(o, r.id, hi, lo, fdx, fdy, fux, fuy)
            });
        }
    }

    pub fn constraint_for(&self, surface: &Rc<WlSurface>) -> Option<Rc<Constraint>> {
        self.constraints
            .borrow()
            .iter()
            .find(|c| Rc::ptr_eq(&c.surface, surface))
            .cloned()
    }

    pub fn add_constraint(self: &Rc<Self>, state: &Rc<State>, con: Rc<Constraint>) {
        self.constraints.borrow_mut().push(con);
        self.refresh_constraint(state);
    }

    pub fn remove_constraint(self: &Rc<Self>, state: &Rc<State>, con: &Rc<Constraint>) {
        self.deactivate_constraint(state, con);
        self.constraints.borrow_mut().retain(|c| !Rc::ptr_eq(c, con));
    }

    /// focus decides which constraint is live; call after every focus move
    pub fn refresh_constraint(self: &Rc<Self>, state: &Rc<State>) {
        let focus = self.ptr_focus.borrow().clone();
        let cons: Vec<_> = self.constraints.borrow().clone();
        for con in cons.iter() {
            let usable = !con.surface.destroyed.get() && con.surface.mapped.get();
            let is_focus =
                usable && focus.as_ref().is_some_and(|f| Rc::ptr_eq(f, &con.surface));
            if con.active.get() && !is_focus {
                self.deactivate_constraint(state, con);
            } else if !con.active.get() && !con.dead.get() && is_focus {
                con.origin.set(self.ptr_origin.get());
                con.send_active(true);
            }
        }
        self.constraints.borrow_mut().retain(|c| !c.dead.get());
    }

    fn deactivate_constraint(&self, state: &Rc<State>, con: &Rc<Constraint>) {
        if !con.active.get() {
            return;
        }
        con.send_active(false);
        // release the lock at the client's position hint, where it last drew
        if con.kind == Kind::Lock {
            if let Some((hx, hy)) = con.hint.take() {
                let (ox, oy) = con.origin.get();
                let (w, h) = state.output_size.get();
                let x = (ox as f64 + hx).clamp(0.0, (w.max(1) - 1) as f64);
                let y = (oy as f64 + hy).clamp(0.0, (h.max(1) - 1) as f64);
                self.ptr_x.set(x);
                self.ptr_y.set(y);
                if let Some(d) = state.display.borrow().as_ref() {
                    d.move_cursor(x as i32, y as i32);
                }
            }
        }
    }

    fn active_lock(&self) -> Option<Rc<Constraint>> {
        self.constraints
            .borrow()
            .iter()
            .find(|c| c.active.get() && c.kind == Kind::Lock)
            .cloned()
    }

    fn active_confine(&self) -> Option<Rc<Constraint>> {
        self.constraints
            .borrow()
            .iter()
            .find(|c| c.active.get() && c.kind == Kind::Confine)
            .cloned()
    }

    /// clamp into the constrained surface, minus any client region
    fn confine_clamp(&self, con: &Constraint, x: f64, y: f64) -> (f64, f64) {
        let (ox, oy) = self.ptr_origin.get();
        let (sw, sh) = con.surface.size.get();
        let mut r = crate::rect::Rect::new_sized_saturating(ox, oy, sw, sh);
        if let Some(reg) = con.region.borrow().as_ref() {
            let e = reg.extents();
            let e = crate::rect::Rect::new_sized_saturating(
                ox + e.x1,
                oy + e.y1,
                e.width(),
                e.height(),
            );
            r = r.intersect(e);
        }
        if r.is_empty() {
            return (x, y);
        }
        (
            x.clamp(r.x1 as f64, (r.x2 - 1).max(r.x1) as f64),
            y.clamp(r.y1 as f64, (r.y2 - 1).max(r.y1) as f64),
        )
    }

    pub fn pointer_motion(
        self: &Rc<Self>,
        state: &Rc<State>,
        time_usec: u64,
        dx: f64,
        dy: f64,
        udx: f64,
        udy: f64,
    ) {
        // an active lock freezes the cursor: raw deltas keep flowing, the
        // absolute stream and focus stay exactly where they are
        if let Some(con) = self.active_lock() {
            if con.surface.destroyed.get() || !con.surface.mapped.get() {
                self.refresh_constraint(state);
            } else {
                self.send_relative(con.client.id, time_usec, dx, dy, udx, udy);
                self.ptr_frame(con.client.id);
                return;
            }
        }
        let (w, h) = state.output_size.get();
        let mut x = (self.ptr_x.get() + dx).clamp(0.0, (w.max(1) - 1) as f64);
        let mut y = (self.ptr_y.get() + dy).clamp(0.0, (h.max(1) - 1) as f64);
        if let Some(con) = self.active_confine() {
            if con.surface.destroyed.get() || !con.surface.mapped.get() {
                self.refresh_constraint(state);
            } else {
                (x, y) = self.confine_clamp(&con, x, y);
            }
        }
        self.ptr_x.set(x);
        self.ptr_y.set(y);

        // an active drag owns the pointer: the wl_pointer stream stays quiet
        // and dnd enter/leave/motion track the surface underneath
        if let Some(drag) = self.data.drag() {
            self.drag_motion(state, &drag, time_usec, x, y);
            return;
        }

        let grabbed = !self.ptr_buttons.borrow().is_empty();
        if !grabbed {
            let hit = self.surface_at(state, x, y);
            let cur = self.ptr_focus.borrow().clone();
            let same = match (&cur, &hit) {
                (Some(a), Some((b, _, _))) => Rc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                if let Some(old) = cur {
                    if !old.destroyed.get() {
                        let serial = state.next_serial(Some(&old.client)) as u32;
                        self.for_each_pointer(old.client.id, 1, |p| {
                            p.client
                                .event(|o| wl_pointer::leave::send(o, p.id, serial, old.id));
                        });
                        self.ptr_frame(old.client.id);
                    }
                }
                if let Some((new, lx, ly)) = &hit {
                    let serial = state.next_serial(Some(&new.client)) as u32;
                    let (fx, fy) = (Fixed::from_int(*lx), Fixed::from_int(*ly));
                    self.for_each_pointer(new.client.id, 1, |p| {
                        p.client.event(|o| {
                            wl_pointer::enter::send(o, p.id, serial, new.id, fx, fy)
                        });
                    });
                    self.ptr_frame(new.client.id);
                    self.ptr_origin.set((x as i32 - lx, y as i32 - ly));
                    // focus follows mouse, onto the window root never a
                    // subsurface; hovering a popup must not steal focus from its toplevel,
                    // and neither may hovering anything while a grab holds the keyboard
                    // layer surfaces only take the keyboard by click or
                    // exclusivity, never by hover
                    let root = new.get_root();
                    let role = root.role.get();
                    if role != crate::surface::SurfaceRole::Popup
                        && role != crate::surface::SurfaceRole::LayerSurface
                        && self.popup_grab.borrow().is_empty()
                        && crate::shell::layer::kb_lock(state).is_none()
                    {
                        super::focus::set_keyboard_focus(state, self, Some(root));
                    }
                }
                *self.ptr_focus.borrow_mut() = hit.as_ref().map(|(s, _, _)| s.clone());
            }
        }

        let focus = self.ptr_focus.borrow().clone();
        if let Some(surface) = focus.filter(|s| !s.destroyed.get()) {
            let (ox, oy) = self.ptr_origin.get();
            let (sx, sy) = (x - ox as f64, y - oy as f64);
            let ms = (time_usec / 1000) as u32;
            let (fx, fy) = (Fixed::from_f64(sx), Fixed::from_f64(sy));
            self.for_each_pointer(surface.client.id, 1, |p| {
                p.client
                    .event(|o| wl_pointer::motion::send(o, p.id, ms, fx, fy));
            });
            self.send_relative(surface.client.id, time_usec, dx, dy, udx, udy);
        }
    }

    // -- drag and drop; the session itself lives on self.data --

    /// start_drag is only honored for the client holding the implicit grab,
    /// naming the press serial, from the surface under the pointer
    pub fn drag_grab_valid(&self, origin: &Rc<WlSurface>, serial: u32) -> bool {
        if self.ptr_buttons.borrow().is_empty() || self.data.drag().is_some() {
            return false;
        }
        if serial != self.last_press_serial.get() {
            return false;
        }
        self.ptr_focus
            .borrow()
            .as_ref()
            .is_some_and(|f| Rc::ptr_eq(&f.get_root(), &origin.get_root()))
    }

    pub fn begin_drag(self: &Rc<Self>, state: &Rc<State>, drag: Rc<crate::protocol::data_device::Drag>) {
        // the grab moves to the dnd session; the origin loses wl_pointer
        // focus and gets it back when the session ends
        if let Some(old) = self.ptr_focus.borrow_mut().take() {
            if !old.destroyed.get() {
                let serial = state.next_serial(Some(&old.client)) as u32;
                self.for_each_pointer(old.client.id, 1, |p| {
                    p.client
                        .event(|o| wl_pointer::leave::send(o, p.id, serial, old.id));
                });
                self.ptr_frame(old.client.id);
            }
        }
        self.data.begin_drag_session(drag.clone());
        // whatever sits under the pointer right now gets the first enter
        let usec = crate::util::Time::now().nsec() / 1_000;
        self.drag_motion(state, &drag, usec, self.ptr_x.get(), self.ptr_y.get());
        state.damage.trigger();
    }

    fn drag_motion(
        &self,
        state: &Rc<State>,
        drag: &Rc<crate::protocol::data_device::Drag>,
        time_usec: u64,
        x: f64,
        y: f64,
    ) {
        // a source-less drag is client-internal: only the initiator's
        // surfaces are targets
        let hit = self
            .surface_at(state, x, y)
            .filter(|(s, _, _)| drag.source.is_some() || s.client.id == drag.client.id);
        let cur = drag.target();
        let same = match (&cur, &hit) {
            (Some(a), Some((b, _, _))) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.data.dnd_leave();
            if let Some((s, lx, ly)) = &hit {
                self.data.dnd_enter(state, s, *lx as f64, *ly as f64);
            }
        } else if let Some((_, lx, ly)) = &hit {
            self.data
                .dnd_motion((time_usec / 1000) as u32, *lx as f64, *ly as f64);
        }
        if drag.icon.borrow().is_some() {
            state.damage.trigger();
        }
    }

    fn end_drag(self: &Rc<Self>, state: &Rc<State>) {
        self.data.dnd_finish_session();
        state.damage.trigger();
        // the pointer re-enters whatever it is over
        let usec = crate::util::Time::now().nsec() / 1_000;
        self.pointer_motion(state, usec, 0.0, 0.0, 0.0, 0.0);
    }

    pub fn pointer_button(self: &Rc<Self>, state: &Rc<State>, time_usec: u64, button: u32, pressed: bool) {
        {
            let mut held = self.ptr_buttons.borrow_mut();
            if pressed {
                held.push(button);
            } else {
                held.retain(|&b| b != button);
            }
        }
        // buttons never reach clients during a drag; the last release ends
        // the session as a drop or a cancel
        if let Some(_drag) = self.data.drag() {
            if !pressed && self.ptr_buttons.borrow().is_empty() {
                self.end_drag(state);
            }
            return;
        }
        let focus = self.ptr_focus.borrow().clone();
        // a press outside the popup grab chain dismisses it; the click
        // then continues to whoever it landed on
        if pressed && !self.popup_grab.borrow().is_empty() {
            let in_chain = focus.as_ref().is_some_and(|s| {
                let root = s.get_root();
                self.popup_grab
                    .borrow()
                    .iter()
                    .any(|p| Rc::ptr_eq(&p.xdg.surface, &root))
            });
            if !in_chain {
                crate::shell::xdg::dismiss_popup_grabs(state, self);
            }
        }
        // on-demand keyboard interactivity: clicking a layer surface
        // hands it the keyboard, clicking a window takes it back
        if pressed && crate::shell::layer::kb_lock(state).is_none() {
            if let Some(s) = &focus {
                let root = s.get_root();
                if root.role.get() == crate::surface::SurfaceRole::LayerSurface {
                    let ls = crate::shell::layer::from_surface(state, &root);
                    if ls.is_some_and(|l| l.current.get().ki != crate::shell::layer::KI_NONE) {
                        super::focus::set_keyboard_focus(state, self, Some(root));
                    }
                }
            }
        }
        let Some(surface) = focus.filter(|s| !s.destroyed.get()) else {
            return;
        };
        let serial = state.next_serial(Some(&surface.client)) as u32;
        if pressed {
            self.last_press_serial.set(serial);
        }
        let ms = (time_usec / 1000) as u32;
        self.for_each_pointer(surface.client.id, 1, |p| {
            p.client.event(|o| {
                wl_pointer::button::send(o, p.id, serial, ms, button, pressed as u32)
            });
        });
        self.ptr_frame(surface.client.id);
    }

    pub fn pointer_axis(&self, time_usec: u64, horizontal: bool, dist: i32) {
        const SOURCE_WHEEL: u32 = 0;
        let axis = if horizontal { 1 } else { 0 };
        let focus = self.ptr_focus.borrow().clone();
        let Some(surface) = focus.filter(|s| !s.destroyed.get()) else {
            return;
        };
        let ms = (time_usec / 1000) as u32;
        // ~15 logical px per detent, the ecosystem convention
        let px = Fixed::from_f64(dist as f64 / 120.0 * 15.0);
        self.for_each_pointer(surface.client.id, 1, |p| {
            if p.version >= wl_pointer::axis_source::SINCE {
                p.client
                    .event(|o| wl_pointer::axis_source::send(o, p.id, SOURCE_WHEEL));
            }
            // value120 and discrete are mutually exclusive
            if p.version >= wl_pointer::axis_value120::SINCE {
                p.client
                    .event(|o| wl_pointer::axis_value120::send(o, p.id, axis, dist));
            } else if p.version >= wl_pointer::axis_discrete::SINCE {
                p.client
                    .event(|o| wl_pointer::axis_discrete::send(o, p.id, axis, dist / 120));
            }
            p.client
                .event(|o| wl_pointer::axis::send(o, p.id, ms, axis, px));
        });
    }

    /// SYN_REPORT edge: close the burst for v5+ clients
    pub fn pointer_frame(&self) {
        let focus = self.ptr_focus.borrow().clone();
        if let Some(surface) = focus.filter(|s| !s.destroyed.get()) {
            self.ptr_frame(surface.client.id);
        }
    }

    /// give the keyboard to the window under the cursor, else the first tile
    pub fn ensure_focus(self: &Rc<Self>, state: &Rc<State>) {
        if self.kb_focus.borrow().is_some() {
            return;
        }
        if let Some(ls) = crate::shell::layer::kb_lock(state) {
            super::focus::set_keyboard_focus(state, self, Some(ls.surface.clone()));
            return;
        }
        let ws = crate::tree::active(state);
        let target = crate::tree::window_at(state, self.ptr_x.get() as i32, self.ptr_y.get() as i32)
            .map(|(w, ..)| w)
            .or_else(|| ws.tiling.first())
            .or_else(|| ws.top_float());
        if let Some(win) = target {
            super::focus::set_keyboard_focus(state, self, Some(win.surface()));
        }
    }
}

pub enum KeyAction {
    Handled,
    SwitchVt(u32),
    Act(crate::config::Action),
}
