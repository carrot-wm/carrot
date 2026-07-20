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
    pub keymap: RefCell<Rc<Keymap>>,
    /// applied input.layout; None means the env-built boot keymap
    kb_layout: RefCell<Option<String>>,
    numlock_done: Cell<bool>,
    pub kb_state: RefCell<KbState>,
    bindings: RefCell<HashMap<ClientId, Vec<Rc<WlSeat>>>>,
    pub kb_focus: RefCell<Option<Rc<WlSurface>>>,
    /// held keys in press order - the enter array
    pub pressed: RefCell<Vec<u32>>,
    pub mods: Cell<Mods>,
    /// server-side repeat, v10+ keyboards only. the version counter
    /// invalidates any timer already in flight
    /// key and the bind-mods it was armed under; a repeat keeps its chord
    repeat_key: Cell<Option<(u32, u32)>>,
    repeat_version: crate::util::NumCell<u64>,
    pub repeat_armed: crate::util::AsyncEvent,
    /// pointer position and the surface under it, pinned while a button is
    /// held (implicit grab); ptr_origin is that surface's global origin
    pub ptr_x: Cell<f64>,
    pub ptr_y: Cell<f64>,
    ptr_focus: RefCell<Option<Rc<WlSurface>>>,
    ptr_origin: Cell<(i32, i32)>,
    /// the origin moved while a lock muzzled the corrective motion; the
    /// client's anchor is stale until one is delivered at unlock
    ptr_rebase_owed: Cell<bool>,
    ptr_buttons: RefCell<Vec<u32>>,
    /// buttons whose press a screencast pick consumed; their releases
    /// stay consumed too
    pick_swallow: RefCell<Vec<u32>>,
    /// serial of the newest button press; start_drag names it
    last_press_serial: Cell<u32>,
    /// remap translations held down: from -> to, so a release always
    /// pairs its press even if focus moved mid-hold
    remap_held: RefCell<HashMap<u32, u32>>,
    /// a matched release-kind bind waits here; any other press disarms
    armed_release: RefCell<Option<(u32, crate::config::Action)>>,
    /// the key that started a pointer grab; its release ends the grab
    grab_key: Cell<Option<u32>>,
    /// last fire per chord, for cooldown-ms binds
    bind_cooldowns: RefCell<std::collections::HashMap<(u32, u32), u64>>,
    // clipboard state rides on the seat: devices, sources, selection
    pub data: crate::protocol::data_device::DataDevices,
    pub primary: crate::protocol::primary_selection::PrimaryDevices,
    pub data_control: crate::protocol::data_control::DataControl,
    // the popup grab chain, bottom first; keyboard focus to restore on
    // full dismissal
    pub popup_grab: RefCell<Vec<Rc<crate::shell::xdg::XdgPopup>>>,
    pub grab_prev_focus: RefCell<Option<Rc<WlSurface>>>,
    /// what the pointer plane shows; back to Default on every focus change
    pub cursor: RefCell<CursorState>,
    pub cursor_hot: Cell<(i32, i32)>,
    /// interactive move/resize riding the implicit grab, like the dnd slot
    grab: RefCell<Option<PointerGrab>>,
    ptr_enter_serial: Cell<u32>,
    pub relative: RefCell<HashMap<ClientId, Vec<Rc<RelativePointer>>>>,
    /// at most one per surface; active while that surface holds pointer focus
    pub constraints: RefCell<Vec<Rc<Constraint>>>,
}

/// pointer plane contents, client-driven via wl_pointer.set_cursor
pub enum CursorState {
    Default,
    Hidden,
    Surface(Rc<WlSurface>),
}

/// an interactive move or resize; deltas apply against the start geometry
enum PointerGrab {
    Move {
        win: Rc<crate::tree::Window>,
        start: (f64, f64),
        rect: crate::rect::Rect,
    },
    Resize {
        win: Rc<crate::tree::Window>,
        edges: u32,
        start: (f64, f64),
        rect: crate::rect::Rect,
        /// tiled resizes step split ratios incrementally
        last: (f64, f64),
    },
}

impl SeatGlobal {
    pub fn new() -> Result<Rc<SeatGlobal>, String> {
        let keymap = Keymap::new_default()?;
        let kb_state = RefCell::new(keymap.create_state());
        Ok(Rc::new(SeatGlobal {
            keymap: RefCell::new(keymap),
            kb_layout: RefCell::new(None),
            numlock_done: Cell::new(false),
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
            ptr_rebase_owed: Cell::new(false),
            ptr_buttons: RefCell::new(Vec::new()),
            pick_swallow: RefCell::new(Vec::new()),
            last_press_serial: Cell::new(0),
            remap_held: RefCell::new(HashMap::new()),
            armed_release: RefCell::new(None),
            grab_key: Cell::new(None),
            bind_cooldowns: RefCell::new(std::collections::HashMap::new()),
            data: Default::default(),
            primary: Default::default(),
            data_control: Default::default(),
            popup_grab: RefCell::new(Vec::new()),
            grab_prev_focus: RefCell::new(None),
            cursor: RefCell::new(CursorState::Default),
            cursor_hot: Cell::new((0, 0)),
            grab: RefCell::new(None),
            ptr_enter_serial: Cell::new(0),
            relative: RefCell::new(HashMap::new()),
            constraints: RefCell::new(Vec::new()),
        }))
    }

    /// a session lock takes the seat: grabs and armed binds must not
    /// survive into the locked state
    pub fn prepare_for_lock(&self, state: &State) {
        *self.grab.borrow_mut() = None;
        state.grab_active.set(false);
        *self.armed_release.borrow_mut() = None;
        self.grab_key.set(None);
        self.cancel_repeat();
    }

    /// cooldown-ms gate; a bind on cooldown swallows its trigger
    fn cooldown_clear(&self, b: &crate::config::Bind) -> bool {
        let Some(cd) = b.cooldown_ms else { return true };
        let now = crate::util::Time::now().nsec();
        let mut map = self.bind_cooldowns.borrow_mut();
        match map.get(&(b.mods, b.key)) {
            Some(&last) if now < last + cd as u64 * 1_000_000 => false,
            _ => {
                map.insert((b.mods, b.key), now);
                true
            }
        }
    }

    pub fn cancel_repeat(&self) {
        self.repeat_key.set(None);
        self.repeat_version.fetch_add(1);
    }

    fn arm_repeat(&self, key: u32, mods: u32) {
        self.repeat_key.set(Some((key, mods)));
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
                let Some((key, mods)) = self.repeat_key.get() else {
                    break;
                };
                let (rate, delay) = {
                    let c = state.config.borrow();
                    (c.input.keyboard.repeat_rate.max(1) as u64, c.input.keyboard.repeat_delay.max(1) as u64)
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
                if self.repeat_version.get() != version
                    || self.repeat_key.get() != Some((key, mods))
                {
                    break;
                }
                self.repeat_fire(&state, key, mods);
            }
        }
    }

    /// v10+ got rate=0 and rely on us for Repeated; v4-9 repeat client-side
    fn repeat_fire(&self, state: &Rc<State>, key: u32, armed_mods: u32) {
        const REPEATED: u32 = 2;
        {
            const MASK: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 6);
            let held_mods = self.mods.get().depressed & MASK;
            // the chord that armed the repeat must still be down; mods
            // pressed since must not re-aim it at some other bind
            if held_mods == armed_mods {
                let cfg = state.config.borrow().clone();
                let hit = cfg
                    .binds
                    .iter()
                    .find(|b| b.repeat && b.mods == armed_mods && b.key == key);
                if let Some(b) = hit {
                    crate::ipc::dispatch_action(state, &b.action);
                    return;
                }
            }
        }
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

    pub fn drop_client(&self, state: &Rc<State>, id: ClientId) {
        self.bindings.borrow_mut().remove(&id);
        self.data.drop_client(state, id);
        self.primary.drop_client(state, id);
        self.data_control.drop_client(id);
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

    // -- keymap config --

    /// startup and reload: rebuild for input.layout, latch numlock once,
    /// push the new keymap to every bound keyboard
    pub fn apply_input_config(&self, state: &Rc<State>) {
        let cfg = state.config.borrow().clone();
        let mut rebuilt = false;
        if cfg.input.keyboard.xkb.layout != *self.kb_layout.borrow() {
            match Keymap::new(cfg.input.keyboard.xkb.layout.as_deref()) {
                Ok(map) => {
                    *self.kb_state.borrow_mut() = map.create_state();
                    *self.keymap.borrow_mut() = map;
                    *self.kb_layout.borrow_mut() = cfg.input.keyboard.xkb.layout.clone();
                    self.mods.set(Mods::default());
                    rebuilt = true;
                }
                // a bad layout keeps the working keymap; never a dead keyboard
                Err(e) => eprintln!("carrot: keymap: {e}"),
            }
        }
        if cfg.input.keyboard.numlock && (rebuilt || !self.numlock_done.get()) {
            self.latch_numlock();
        }
        self.numlock_done.set(true);
        if rebuilt {
            self.resend_keymap();
        }
    }

    /// one simulated numlock tap on the fresh xkb state locks the modifier
    fn latch_numlock(&self) {
        const KEY_NUMLOCK: u32 = 69;
        let map = self.keymap.borrow().clone();
        let mut st = self.kb_state.borrow_mut();
        st.process(&map, KEY_NUMLOCK, true);
        st.process(&map, KEY_NUMLOCK, false);
        self.mods.set(st.mods());
    }

    /// every bound keyboard gets the new fd, then modifiers to resync
    fn resend_keymap(&self) {
        let map = self.keymap.borrow().clone();
        let mods = self.mods.get();
        for seats in self.bindings.borrow().values() {
            for seat in seats {
                for kb in seat.keyboards.borrow().iter() {
                    kb.send_keymap(&map);
                    let serial = kb.client.state.next_serial(Some(&kb.client)) as u32;
                    kb.send_modifiers(serial, mods);
                }
            }
        }
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
        // late enter: the cursor may already rest on us, and motion or
        // button must never reach a pointer that was not entered first
        let focus = self.global.ptr_focus.borrow().clone();
        if let Some(surface) = focus {
            if surface.client.id == self.client.id && !surface.destroyed.get() {
                let serial = self.client.state.next_serial(Some(&self.client)) as u32;
                self.global.ptr_enter_serial.set(serial);
                let (ox, oy) = self.global.ptr_origin.get();
                let fx = Fixed::from_int(self.global.ptr_x.get() as i32 - ox);
                let fy = Fixed::from_int(self.global.ptr_y.get() as i32 - oy);
                self.client.event(|o| {
                    wl_pointer::enter::send(o, ptr.id, serial, surface.id, fx, fy);
                    if self.version >= wl_pointer::frame::SINCE {
                        wl_pointer::frame::send(o, ptr.id);
                    }
                });
            }
        }
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
        kb.send_keymap(&self.global.keymap.borrow());
        if self.version >= wl_keyboard::repeat_info::SINCE {
            // zero rate tells v10+ the server sends Repeated; don't repeat locally
            let (rate, delay) = if self.version >= REPEATED_SINCE {
                (0, 0)
            } else {
                let c = self.client.state.config.borrow();
                (c.input.keyboard.repeat_rate, c.input.keyboard.repeat_delay)
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
        self.global.drop_client(&self.client.state, self.client.id);
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
        // unlist first or the dead id keeps receiving key events
        if let Some(seat) = self.client.state.seat.borrow().clone() {
            if let Some(seats) = seat.bindings.borrow().get(&self.client.id) {
                for s in seats {
                    s.keyboards.borrow_mut().retain(|k| k.id != self.id);
                }
            }
        }
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

// -- relative pointers, constraints, cursor --

impl SeatGlobal {
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
                con.send_active(true);
            }
        }
        self.constraints.borrow_mut().retain(|c| !c.dead.get());
    }

    fn deactivate_constraint(self: &Rc<Self>, state: &Rc<State>, con: &Rc<Constraint>) {
        if !con.active.get() {
            return;
        }
        con.send_active(false);
        // release the lock at the client's position hint, where it last
        // drew. the hint is surface-local and resolves against where the
        // surface sits now - fullscreen or a retile can move the surface
        // while the lock holds, and no motion runs to rebase in between
        if con.kind == Kind::Lock {
            if let Some((hx, hy)) = con.hint.take() {
                if let Some((ox, oy)) = crate::tree::surface_origin(state, &con.surface) {
                    let (x, y) = crate::output::clamp_pointer(
                        state,
                        ox as f64 + hx,
                        oy as f64 + hy,
                    );
                    self.ptr_x.set(x);
                    self.ptr_y.set(y);
                    if let Some(d) = state.display.borrow().as_ref() {
                        d.move_cursor(state, x as i32, y as i32);
                    }
                }
            }
            // the lock is gone: whatever moved the surface while it held,
            // the cache and the client resync before any button lands
            self.rebase_origin(state);
        }
    }

    pub fn lock_active(&self) -> bool {
        self.active_lock().is_some()
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

    /// jump the pointer and re-evaluate focus as a zero-delta motion.
    /// a live lock owns the pointer; warps yield to it
    pub fn warp(self: &Rc<Self>, state: &Rc<State>, x: f64, y: f64) {
        if self.active_lock().is_some() {
            return;
        }
        self.ptr_x.set(x);
        self.ptr_y.set(y);
        let usec = crate::util::Time::now().nsec() / 1_000;
        self.pointer_motion(state, usec, 0.0, 0.0, 0.0, 0.0);
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

    fn cursor_is(&self, key: (ClientId, ObjectId)) -> bool {
        match &*self.cursor.borrow() {
            CursorState::Surface(s) => (s.client.id, s.id) == key,
            _ => false,
        }
    }

    /// push whatever the cursor state says onto the hardware plane
    pub fn apply_cursor(&self, state: &Rc<State>) {
        let d = state.display.borrow();
        let Some(d) = d.as_ref() else { return };
        match &*self.cursor.borrow() {
            CursorState::Default => d.set_cursor_default(),
            CursorState::Hidden => d.set_cursor_hidden(true),
            // a cursor surface with no buffer is hidden, per spec
            CursorState::Surface(s) => match cursor_pixels(s) {
                Some((px, w, h)) => d.set_cursor_image(&px, w, h, self.cursor_hot.get()),
                None => d.set_cursor_hidden(true),
            },
        }
        d.move_cursor(state, self.ptr_x.get() as i32, self.ptr_y.get() as i32);
    }

    fn cursor_surface_committed(&self, key: (ClientId, ObjectId)) {
        if !self.cursor_is(key) {
            return;
        }
        let state = match &*self.cursor.borrow() {
            CursorState::Surface(s) => s.client.state.clone(),
            _ => return,
        };
        self.apply_cursor(&state);
    }

    fn cursor_surface_destroyed(&self, key: (ClientId, ObjectId)) {
        if !self.cursor_is(key) {
            return;
        }
        let state = match &*self.cursor.borrow() {
            CursorState::Surface(s) => s.client.state.clone(),
            _ => return,
        };
        *self.cursor.borrow_mut() = CursorState::Default;
        self.apply_cursor(&state);
    }
}

/// role object for cursor surfaces; hotspot rides the seat, offsets shift it
struct CursorExt {
    seat: std::rc::Weak<SeatGlobal>,
    key: (ClientId, ObjectId),
}

impl crate::surface::SurfaceExt for CursorExt {
    fn commit_requested(
        self: Rc<Self>,
        pending: Box<crate::surface::PendingState>,
    ) -> Option<Box<crate::surface::PendingState>> {
        if let Some(seat) = self.seat.upgrade() {
            if seat.cursor_is(self.key) {
                let (hx, hy) = seat.cursor_hot.get();
                seat.cursor_hot.set((hx - pending.offset.0, hy - pending.offset.1));
            }
        }
        Some(pending)
    }

    fn after_apply(&self) {
        if let Some(seat) = self.seat.upgrade() {
            seat.cursor_surface_committed(self.key);
        }
    }

    fn on_surface_destroy(&self) -> Result<(), ()> {
        if let Some(seat) = self.seat.upgrade() {
            seat.cursor_surface_destroyed(self.key);
        }
        Ok(())
    }
}

/// tightly packed argb bytes out of the cursor surface's commit shadow,
/// or straight from the client buffer when no shadow exists - sealed
/// pools sample in place and keep none, and their release parks until
/// replace, so the pages are stable for as long as this cursor shows
fn cursor_pixels(s: &WlSurface) -> Option<(Vec<u8>, u32, u32)> {
    let buf = s.buffer.borrow();
    let b = &buf.as_ref()?.buf;
    let (w, h) = (b.rect.width() as u32, b.rect.height() as u32);
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let row = (w * 4) as usize;
    let need = row * h as usize;
    // dmabuf cursors would need a gpu readback; nobody ships them
    let shadow = s.shm_shadow.borrow();
    let mut px = match shadow.as_ref().filter(|p| p.len() >= need) {
        Some(src) => src[..need].to_vec(),
        None => {
            let stride = b.stride as usize;
            if stride < row {
                return None;
            }
            let access = b.shm_access()?;
            let mut px = vec![0u8; need];
            crate::output::fill_from_shm(&access, &mut px, h, row, stride);
            px
        }
    };
    // the plane blends; xrgb needs its alpha forced opaque
    if !b.format.has_alpha() {
        for c in px.chunks_exact_mut(4) {
            c[3] = 0xff;
        }
    }
    Some((px, w, h))
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
        req: wl_pointer::set_cursor::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(seat) = c.state.seat.borrow().clone() else {
            return Ok(());
        };
        // only the focused client, and only for the current enter
        let focus = seat.ptr_focus.borrow().clone();
        let focused_here = focus.is_some_and(|f| f.client.id == c.id);
        if !focused_here || req.serial != seat.ptr_enter_serial.get() {
            return Ok(());
        }
        if req.surface.0 == 0 {
            *seat.cursor.borrow_mut() = CursorState::Hidden;
            seat.apply_cursor(&c.state);
            return Ok(());
        }
        let Some(s) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        match s.role.get() {
            crate::surface::SurfaceRole::None | crate::surface::SurfaceRole::Cursor => {}
            other => {
                c.protocol_error(
                    self.id,
                    0, // wl_pointer error role
                    &format!("the surface already has the {} role", other.name()),
                );
                return Ok(());
            }
        }
        s.role.set(crate::surface::SurfaceRole::Cursor);
        *s.ext.borrow_mut() = Rc::new(CursorExt {
            seat: Rc::downgrade(&seat),
            key: (c.id, s.id),
        });
        seat.cursor_hot.set((req.hotspot_x, req.hotspot_y));
        *seat.cursor.borrow_mut() = CursorState::Surface(s);
        seat.apply_cursor(&c.state);
        Ok(())
    }

    fn release(
        &self,
        _req: wl_pointer::release::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // unlist first or the dead id keeps receiving pointer events
        if let Some(seat) = self.client.state.seat.borrow().clone() {
            if let Some(seats) = seat.bindings.borrow().get(&self.client.id) {
                for s in seats {
                    s.pointers.borrow_mut().retain(|p| p.id != self.id);
                }
            }
        }
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
    /// per-window key translation: a press consults the focused window's
    /// remap profile, a release follows whatever its press mapped to
    fn remap(&self, state: &Rc<State>, key: u32, pressed: bool) -> u32 {
        if !pressed {
            if let Some(to) = self.remap_held.borrow_mut().remove(&key) {
                return to;
            }
            return key;
        }
        let cfg = state.config.borrow().clone();
        if cfg.remaps.is_empty() {
            return key;
        }
        let focus = self.kb_focus.borrow().clone();
        let Some(win) = focus.and_then(|s| crate::tree::window_for_surface(state, &s)) else {
            return key;
        };
        let ws = crate::tree::workspace_of(state, &win)
            .and_then(|w| {
                state
                    .workspaces
                    .borrow()
                    .iter()
                    .position(|x| Rc::ptr_eq(x, &w))
            })
            .map(|i| i + 1)
            .unwrap_or(0);
        let to = crate::config::resolve_remap(
            &cfg,
            &win.app_id(),
            &win.title(),
            win.x11_opt().is_some(),
            win.surface().client.pid,
            ws,
            key,
        );
        match to {
            Some(to) => {
                self.remap_held.borrow_mut().insert(key, to);
                to
            }
            None => key,
        }
    }

    pub fn key(&self, state: &Rc<State>, time_usec: u64, key: u32, pressed: bool) -> KeyAction {
        crate::protocol::idle::note_activity(state);
        let key = self.remap(state, key, pressed);
        let map = self.keymap.borrow().clone();
        let changed = self.kb_state.borrow_mut().process(&map, key, pressed);
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

        // Escape cancels a pending screencast pick and goes no further
        if pressed && key == 1 {
            if let Some(pick) = state.cast_pick.borrow_mut().take() {
                pick.done.trigger();
                return KeyAction::Handled;
            }
        }

        // binds exact-match the depressed set masked to shift|ctrl|alt|super
        if pressed {
            // any press disarms a waiting release bind, even one that
            // matches another bind and returns early below
            self.armed_release.borrow_mut().take();
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
            // a locked session only honors binds that opted in
            let locked = crate::protocol::session_lock::locked(state);
            let mut armed = None;
            for b in cfg.binds.iter() {
                if held_mods == b.mods && key == b.key {
                    if locked && !b.allow_when_locked {
                        continue;
                    }
                    if b.on_release {
                        armed = Some((key, b.action.clone()));
                        continue;
                    }
                    if !self.cooldown_clear(b) {
                        continue;
                    }
                    // a grab lives while its key is held
                    if matches!(
                        b.action,
                        crate::config::Action::PointerMove | crate::config::Action::PointerResize
                    ) {
                        self.grab_key.set(Some(key));
                    }
                    if b.repeat {
                        // re-fires from the repeat loop while held
                        self.arm_repeat(key, held_mods);
                        return KeyAction::Act(b.action.clone());
                    }
                    self.cancel_repeat();
                    return KeyAction::Act(b.action.clone());
                }
            }
            *self.armed_release.borrow_mut() = armed;
        } else {
            // the grab's key came back up: the move/resize session ends;
            // the seat rc lives in state, the &self path can't call end_grab
            if self.grab_key.get() == Some(key) {
                self.grab_key.set(None);
                if self.grab.borrow().is_some() {
                    if let Some(seat) = state.seat.borrow().clone() {
                        seat.end_grab(state);
                    }
                }
                return KeyAction::Handled;
            }
            // the armed key came back up: fire, once
            let hit = {
                let mut slot = self.armed_release.borrow_mut();
                match slot.take() {
                    Some((k, act)) if k == key => Some(act),
                    other => {
                        *slot = other;
                        None
                    }
                }
            };
            if let Some(act) = hit {
                return KeyAction::Act(act);
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
            client.flush_eager();
            let group = self.mods.get().group;
            if pressed && map.repeats(key, group) {
                const MASK: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 6);
                self.arm_repeat(key, self.mods.get().depressed & MASK);
            } else if !pressed && self.repeat_key.get().is_some_and(|(k, _)| k == key) {
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
        let mut flush = None;
        self.for_each_pointer(client, wl_pointer::frame::SINCE, |p| {
            p.client.event(|o| wl_pointer::frame::send(o, p.id));
            flush = Some(p.client.clone());
        });
        // the frame closes an input batch: put it on the wire now instead
        // of waiting out the engine's send phase
        if let Some(c) = flush {
            c.flush_eager();
        }
    }

    /// deepest mapped surface under the global point, in z order
    fn surface_at(&self, state: &Rc<State>, x: f64, y: f64) -> Option<(Rc<WlSurface>, i32, i32)> {
        crate::tree::surface_at(state, x as i32, y as i32)
    }

    /// leave/enter dance onto whatever is under the point now
    fn pick_focus(self: &Rc<Self>, state: &Rc<State>, x: f64, y: f64) {
        let hit = self.surface_at(state, x, y);
        let cur = self.ptr_focus.borrow().clone();
        let same = match (&cur, &hit) {
            (Some(a), Some((b, _, _))) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            // the surface can change its painted origin under a stationary
            // cursor (fullscreen, retile). coordinates are surface-local, so
            // rebase and tell the client where it now stands via a motion -
            // no leave/enter, the surface is still the same one
            if let Some((s, lx, ly)) = &hit {
                let origin = (x as i32 - lx, y as i32 - ly);
                if origin != self.ptr_origin.get() && !s.destroyed.get() {
                    self.ptr_origin.set(origin);
                    self.ptr_rebase_owed.set(false);
                    let ms = (crate::util::Time::now().nsec() / 1_000_000) as u32;
                    let (fx, fy) = (Fixed::from_int(*lx), Fixed::from_int(*ly));
                    self.for_each_pointer(s.client.id, 1, |p| {
                        p.client
                            .event(|o| wl_pointer::motion::send(o, p.id, ms, fx, fy));
                    });
                    self.ptr_frame(s.client.id);
                }
            }
            return;
        }
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
            self.ptr_enter_serial.set(serial);
            let (fx, fy) = (Fixed::from_int(*lx), Fixed::from_int(*ly));
            self.for_each_pointer(new.client.id, 1, |p| {
                p.client.event(|o| {
                    wl_pointer::enter::send(o, p.id, serial, new.id, fx, fy)
                });
            });
            self.ptr_frame(new.client.id);
            self.ptr_origin.set((x as i32 - lx, y as i32 - ly));
            // the enter carried absolute coords; nothing is owed
            self.ptr_rebase_owed.set(false);
            // focus-follows-mouse targets the window root, never a subsurface,
            // and never fires while a popup grab or lock holds the keyboard.
            // popups and layer surfaces take the keyboard by click or
            // exclusivity, not by hover
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
        // default arrow until the entered client sets its own cursor
        *self.cursor.borrow_mut() = CursorState::Default;
        self.apply_cursor(state);
        self.refresh_constraint(state);
    }

    pub fn pointer_focus(&self) -> Option<Rc<WlSurface>> {
        self.ptr_focus.borrow().clone()
    }

    /// the scene changed under a stationary cursor (map/unmap/arrange):
    /// re-resolve pointer focus without waiting for the next motion
    /// re-resolve the cached painted origin of the current pointer focus
    /// from the tree. this runs even while a lock, held button, or drag
    /// pins focus identity: a fullscreen or retile under any pin used to
    /// strand ptr_origin at the old rect, and every consumer (motion
    /// delivery, confine boxes, late enters) inherited the displacement
    /// until the next free motion. a locked client hears nothing - it
    /// consumes only relative deltas and absolute motion is forbidden
    /// under a lock; otherwise the corrective motion tells the client
    /// where it now stands
    pub fn rebase_origin(self: &Rc<Self>, state: &Rc<State>) {
        let focus = self.ptr_focus.borrow().clone();
        let Some(s) = focus.filter(|s| !s.destroyed.get()) else {
            return;
        };
        let Some((ox, oy)) = crate::tree::surface_origin(state, &s) else {
            return;
        };
        let changed = (ox, oy) != self.ptr_origin.get();
        if changed {
            self.ptr_origin.set((ox, oy));
        }
        if self.active_lock().is_some() {
            // silently healed; the client's anchor resyncs at unlock
            if changed {
                self.ptr_rebase_owed.set(true);
            }
            return;
        }
        if changed || self.ptr_rebase_owed.replace(false) {
            let ms = (crate::util::Time::now().nsec() / 1_000_000) as u32;
            let fx = Fixed::from_f64(self.ptr_x.get() - ox as f64);
            let fy = Fixed::from_f64(self.ptr_y.get() - oy as f64);
            self.for_each_pointer(s.client.id, 1, |p| {
                p.client
                    .event(|o| wl_pointer::motion::send(o, p.id, ms, fx, fy));
            });
            self.ptr_frame(s.client.id);
        }
    }

    /// jump the pointer, the plane, and focus together; the one authority
    /// for programmatic pointer placement. a live lock owns the pointer,
    /// so the seat, the plane, and the client all stay where they are
    pub fn place_pointer(self: &Rc<Self>, state: &Rc<State>, x: f64, y: f64) {
        if self.active_lock().is_some() {
            return;
        }
        self.warp(state, x, y);
        if let Some(d) = state.display.borrow().as_ref() {
            d.move_cursor(state, x as i32, y as i32);
        }
    }

    pub fn repick(self: &Rc<Self>, state: &Rc<State>) {
        // the painted origin heals no matter what pins focus identity;
        // a stale cache under a pin was the recurring fullscreen
        // displacement
        self.rebase_origin(state);
        // an implicit grab pins focus; dnd re-targets on its own motion.
        // a lock pins too - but only while its window is still on the
        // visible scene, else a workspace switch leaves relative motion
        // flowing to a window nobody can see
        let lock_pins = self.active_lock().is_some_and(|con| {
            let root = con.surface.get_root();
            crate::tree::window_for_surface_any(state, &root).is_none()
                || crate::tree::window_for_surface(state, &root).is_some()
        });
        if !self.ptr_buttons.borrow().is_empty() || self.data.drag().is_some() || lock_pins {
            return;
        }
        self.pick_focus(state, self.ptr_x.get(), self.ptr_y.get());
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
        crate::protocol::idle::note_activity(state);
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
        let (mut x, mut y) =
            crate::output::clamp_pointer(state, self.ptr_x.get() + dx, self.ptr_y.get() + dy);
        if let Some(con) = self.active_confine() {
            if con.surface.destroyed.get() || !con.surface.mapped.get() {
                self.refresh_constraint(state);
            } else {
                (x, y) = self.confine_clamp(&con, x, y);
            }
        }
        self.ptr_x.set(x);
        self.ptr_y.set(y);
        crate::tree::note_pointer_output(state, x, y);

        // an active drag owns the pointer: the wl_pointer stream stays
        // quiet and dnd enter/leave/motion track the surface underneath
        if let Some(drag) = self.data.drag() {
            self.drag_motion(state, &drag, time_usec, x, y);
            return;
        }

        // so does a move/resize session: motion becomes window geometry
        if self.grab.borrow().is_some() {
            self.grab_motion(state, x, y);
            return;
        }

        let grabbed = !self.ptr_buttons.borrow().is_empty();
        if !grabbed {
            self.pick_focus(state, x, y);
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
            self.ptr_frame(surface.client.id);
        }
    }

    pub fn pointer_button(self: &Rc<Self>, state: &Rc<State>, time_usec: u64, button: u32, pressed: bool) {
        crate::protocol::idle::note_activity(state);
        {
            let mut held = self.ptr_buttons.borrow_mut();
            if pressed {
                held.push(button);
            } else {
                held.retain(|&b| b != button);
            }
        }
        // buttons never reach clients during a drag; the last release
        // ends the session as a drop or a cancel
        if let Some(_drag) = self.data.drag() {
            if !pressed && self.ptr_buttons.borrow().is_empty() {
                self.end_drag(state);
            }
            return;
        }
        // same for a move/resize session: the last release ends it
        if self.grab.borrow().is_some() {
            if !pressed && self.ptr_buttons.borrow().is_empty() {
                self.end_grab(state);
            }
            return;
        }
        // a pending screencast pick eats the next click: it is consent,
        // not input. left selects, anything else cancels
        if pressed {
            let pick = state.cast_pick.borrow_mut().take();
            if let Some(pick) = pick {
                self.pick_swallow.borrow_mut().push(button);
                if button == 0x110 {
                    *pick.result.borrow_mut() = crate::portal::cast_pick_at(
                        state,
                        pick.types,
                        self.ptr_x.get(),
                        self.ptr_y.get(),
                    );
                }
                pick.done.trigger();
                return;
            }
        } else {
            let mut sw = self.pick_swallow.borrow_mut();
            if let Some(i) = sw.iter().position(|&b| b == button) {
                sw.remove(i);
                return;
            }
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
        // mouse binds: evdev button code in the key slot, kb mods held
        if pressed {
            const MASK: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 6);
            let held_mods = self.mods.get().depressed & MASK;
            let cfg = state.config.borrow().clone();
            // a locked session only honors binds that opted in, same as keys
            let locked = crate::protocol::session_lock::locked(state);
            let hit = cfg
                .binds
                .iter()
                .filter(|b| !locked || b.allow_when_locked)
                .find(|b| b.mods == held_mods && b.key == button);
            if let Some(b) = hit {
                if !self.cooldown_clear(b) {
                    return;
                }
                crate::ipc::dispatch_action(state, &b.action);
                return;
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

    // -- drag and drop; the session itself lives on self.data --

    /// start_drag is only honored for the client holding the implicit
    /// grab, naming the press serial, from the surface under the pointer
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

    // -- interactive move/resize --

    /// move/resize is only honored for the client holding the implicit
    /// grab, naming the press serial, from the surface under the pointer
    pub fn move_resize_grab_valid(&self, origin: &Rc<WlSurface>, serial: u32) -> bool {
        if self.ptr_buttons.borrow().is_empty()
            || self.data.drag().is_some()
            || self.grab.borrow().is_some()
        {
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

    pub fn start_move_grab(&self, state: &Rc<State>, win: Rc<crate::tree::Window>) {
        // floating windows follow the pointer; tiled ones trade places
        // with whatever it crosses
        if win.fullscreen.get() {
            return;
        }
        let start = (self.ptr_x.get(), self.ptr_y.get());
        let rect = win.rect.get();
        state.grab_active.set(true);
        *self.grab.borrow_mut() = Some(PointerGrab::Move { win, start, rect });
    }

    pub fn start_resize_grab(&self, state: &Rc<State>, win: Rc<crate::tree::Window>, edges: u32) {
        if win.fullscreen.get() {
            return;
        }
        let start = (self.ptr_x.get(), self.ptr_y.get());
        let rect = win.rect.get();
        state.grab_active.set(true);
        *self.grab.borrow_mut() = Some(PointerGrab::Resize {
            win,
            edges,
            start,
            rect,
            last: start,
        });
    }

    fn grab_motion(self: &Rc<Self>, state: &Rc<State>, x: f64, y: f64) {
        enum Op {
            SetRect(Rc<crate::tree::Window>, crate::rect::Rect),
            Ratio(Rc<crate::tree::Window>, u32, f64, f64),
            /// dragging a tiled window: trade places with whatever the
            /// pointer crosses; the move animations carry the exchange
            Swap(Rc<crate::tree::Window>),
        }
        let op = {
            let mut slot = self.grab.borrow_mut();
            match slot.as_mut() {
                Some(PointerGrab::Move { win, start, rect }) => {
                    if win.floating.get() {
                        let (sw, sh) = crate::tree::output_extent(state);
                        let nx = ((rect.x1 as f64 + x - start.0) as i32)
                            .min(sw - rect.width())
                            .max(0);
                        let ny = ((rect.y1 as f64 + y - start.1) as i32)
                            .min(sh - rect.height())
                            .max(0);
                        Op::SetRect(win.clone(), rect.move_(nx - rect.x1, ny - rect.y1))
                    } else {
                        Op::Swap(win.clone())
                    }
                }
                Some(PointerGrab::Resize { win, edges, start, rect, last }) => {
                    if win.floating.get() {
                        let r = resize_rect(*rect, *edges, x - start.0, y - start.1, 50);
                        Op::SetRect(win.clone(), r)
                    } else {
                        let (dx, dy) = (x - last.0, y - last.1);
                        *last = (x, y);
                        Op::Ratio(win.clone(), *edges, dx, dy)
                    }
                }
                None => return,
            }
        };
        let win = match &op {
            Op::SetRect(w, _) | Op::Ratio(w, ..) | Op::Swap(w) => w.clone(),
        };
        // the window left the tree mid-grab (unmap, workspace move)
        let Some(ws) = crate::tree::workspace_of(state, &win) else {
            self.grab.borrow_mut().take();
            state.grab_active.set(false);
            return;
        };
        match op {
            Op::SetRect(_, r) => {
                win.rect.set(r);
                win.move_snap();
                win.configure_rect();
            }
            Op::Ratio(_, edges, dx, dy) => {
                let hit = match ws.tiling.mode() {
                    crate::config::LayoutMode::Dwindle => {
                        crate::tree::dwindle::resize_by_edges(&win, edges, dx, dy)
                    }
                    crate::config::LayoutMode::Scrolling => {
                        ws.tiling.strip.resize_by_edges(&win, edges, dx, dy)
                    }
                };
                if !hit {
                    return;
                }
            }
            Op::Swap(_) => {
                let Some((other, ..)) = crate::tree::window_at(state, x as i32, y as i32) else {
                    return;
                };
                if Rc::ptr_eq(&other, &win) || other.floating.get() || other.fullscreen.get() {
                    return;
                }
                let ows = crate::tree::workspace_of(state, &other)
                    .unwrap_or_else(|| crate::tree::active(state));
                let swapped = if Rc::ptr_eq(&ws, &ows) {
                    match ws.tiling.mode() {
                        crate::config::LayoutMode::Dwindle => {
                            crate::tree::dwindle::swap_windows(&win, &other)
                        }
                        crate::config::LayoutMode::Scrolling => {
                            ws.tiling.strip.swap_tiles(&win, &other)
                        }
                    }
                } else if ws.tiling.mode() == crate::config::LayoutMode::Dwindle
                    && ows.tiling.mode() == crate::config::LayoutMode::Dwindle
                {
                    // across outputs: leaves trade occupants tree-to-tree
                    crate::tree::dwindle::swap_windows(&win, &other)
                } else {
                    false
                };
                if !swapped {
                    return;
                }
                ws.tiling.note_focus_win(&win);
                if !Rc::ptr_eq(&ws, &ows) {
                    crate::tree::relayout(state, &ows);
                }
            }
        }
        crate::tree::relayout(state, &ws);
        state.damage.trigger();
    }

    fn end_grab(self: &Rc<Self>, state: &Rc<State>) {
        self.grab.borrow_mut().take();
        state.grab_active.set(false);
        // the pointer re-enters whatever it is over
        let usec = crate::util::Time::now().nsec() / 1_000;
        self.pointer_motion(state, usec, 0.0, 0.0, 0.0, 0.0);
    }

    pub fn pointer_axis(&self, time_usec: u64, horizontal: bool, dist: i32) {
        const SOURCE_WHEEL: u32 = 0;
        let axis = if horizontal { 1 } else { 0 };
        let focus = self.ptr_focus.borrow().clone();
        let Some(surface) = focus.filter(|s| !s.destroyed.get()) else {
            return;
        };
        let ms = (time_usec / 1000) as u32;
        // ~15 logical px per wheel detent
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

    /// give the keyboard to the window under the cursor, else the
    /// layout's default pick
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
            .or_else(|| ws.tiling.default_focus())
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

/// apply an edge drag to a floating rectangle; the opposite edges hold
/// still and the dragged ones never pull the box below the minimum size
fn resize_rect(r: crate::rect::Rect, edges: u32, dx: f64, dy: f64, min: i32) -> crate::rect::Rect {
    use crate::tree::dwindle::{EDGE_BOTTOM, EDGE_LEFT, EDGE_RIGHT, EDGE_TOP};
    let mut out = r;
    if edges & EDGE_LEFT != 0 {
        out.x1 = ((r.x1 as f64 + dx) as i32).min(r.x2 - min);
    } else if edges & EDGE_RIGHT != 0 {
        out.x2 = ((r.x2 as f64 + dx) as i32).max(r.x1 + min);
    }
    if edges & EDGE_TOP != 0 {
        out.y1 = ((r.y1 as f64 + dy) as i32).min(r.y2 - min);
    } else if edges & EDGE_BOTTOM != 0 {
        out.y2 = ((r.y2 as f64 + dy) as i32).max(r.y1 + min);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::{
        zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
    };
    use crate::protocol::pointer_constraints::{ConstraintsManager, LockedPointer};
    use wl_pointer::Handler as _;
    use wl_seat::Handler as _;
    use zwp_locked_pointer_v1::Handler as _;
    use zwp_pointer_constraints_v1::Handler as _;

    fn setup() -> (Rc<crate::state::State>, Rc<Client>, Rc<SeatGlobal>, Rc<WlSurface>) {
        let (state, client) = test_client();
        state.output_size.set((2560, 1440));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        s.mapped.set(true);
        // pointer parked on the surface at global 100,80; surface at 40,30
        *seat.ptr_focus.borrow_mut() = Some(s.clone());
        seat.ptr_origin.set((40, 30));
        seat.ptr_x.set(100.0);
        seat.ptr_y.set(80.0);
        s.size.set((800, 600));
        (state, client, seat, s)
    }

    fn lock(
        client: &Rc<Client>,
        surface: ObjectId,
        id: u32,
        lifetime: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mgr = ConstraintsManager {
            id: ObjectId(70),
            client: client.clone(),
            version: 1,
        };
        mgr.lock_pointer(zwp_pointer_constraints_v1::lock_pointer::Request {
            id: ObjectId(id),
            surface,
            pointer: ObjectId(0),
            region: ObjectId(0),
            lifetime,
        })
    }

    #[test]
    fn repeat_fires_only_the_chord_that_armed_it() {
        let (state, _client, seat, s) = setup();
        *seat.kb_focus.borrow_mut() = Some(s.clone());
        let mut cfg = (**state.config.borrow()).clone();
        cfg.binds.push(crate::config::Bind {
            mods: 1 << 6,
            key: 12,
            action: crate::config::Action::FocusWorkspace(5),
            on_release: false,
            repeat: true,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        // '-' held plain arms a client repeat under no mods
        seat.key(&state, 0, 12, true);
        assert_eq!(seat.repeat_key.get(), Some((12, 0)), "armed with its chord");
        // super pressed mid-hold must not re-aim the repeat at mod+minus
        seat.key(&state, 1000, 125, true);
        seat.repeat_fire(&state, 12, 0);
        assert_eq!(state.active_ws.get(), 0, "the repeat kept its armed chord");
        // the real chord re-arms with its mods and fires its bind
        seat.key(&state, 2000, 12, false);
        seat.key(&state, 3000, 12, true);
        assert_eq!(seat.repeat_key.get(), Some((12, 1 << 6)));
        seat.repeat_fire(&state, 12, 1 << 6);
        assert_eq!(state.active_ws.get(), 5, "the armed chord fires its bind");
    }

    #[test]
    fn an_interleaved_bind_press_disarms_a_release_bind() {
        let (state, _client, seat, _s) = setup();
        let mut cfg = (**state.config.borrow()).clone();
        let bind = crate::config::Bind {
            mods: 1 << 6,
            key: 45,
            action: crate::config::Action::FocusWorkspace(7),
            on_release: true,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        };
        cfg.binds.push(bind.clone());
        cfg.binds.push(crate::config::Bind {
            key: 28,
            on_release: false,
            action: crate::config::Action::FocusWorkspace(3),
            ..bind
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        seat.key(&state, 0, 125, true);
        seat.key(&state, 1000, 45, true);
        assert!(seat.armed_release.borrow().is_some(), "the release bind armed");
        // an interleaved normal bind returns early - it must still disarm
        let r = seat.key(&state, 2000, 28, true);
        assert!(matches!(r, KeyAction::Act(_)));
        assert!(seat.armed_release.borrow().is_none(), "the chord disarmed it");
        let r = seat.key(&state, 3000, 45, false);
        assert!(matches!(r, KeyAction::Handled), "the stale release stays quiet");
    }

    #[test]
    fn released_pointers_and_keyboards_stop_receiving() {
        use wl_keyboard::Handler as _;
        let (state, client, seat, _s) = setup();
        let _ = &state;
        let bind = Rc::new(WlSeat {
            id: ObjectId(80),
            client: client.clone(),
            version: 9,
            global: seat.clone(),
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        });
        client.add_client_obj(bind.clone()).unwrap();
        seat.bindings.borrow_mut().entry(client.id).or_default().push(bind.clone());
        bind.get_pointer(wl_seat::get_pointer::Request { id: ObjectId(81) }).unwrap();
        bind.get_keyboard(wl_seat::get_keyboard::Request { id: ObjectId(82) }).unwrap();
        let ptr = bind.pointers.borrow()[0].clone();
        let kb = bind.keyboards.borrow()[0].clone();
        ptr.release(wl_pointer::release::Request {}).unwrap();
        kb.release(wl_keyboard::release::Request {}).unwrap();
        assert!(bind.pointers.borrow().is_empty(), "released pointer unlisted");
        assert!(bind.keyboards.borrow().is_empty(), "released keyboard unlisted");
    }

    #[test]
    fn mouse_binds_respect_the_session_lock() {
        use crate::protocol::interfaces::ext_session_lock_manager_v1::{self, Handler as _};
        let (state, client, seat, _s) = setup();
        let mut cfg = (**state.config.borrow()).clone();
        cfg.binds.push(crate::config::Bind {
            mods: 0,
            key: 0x110,
            action: crate::config::Action::FocusWorkspace(3),
            on_release: false,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        let mgr = crate::protocol::session_lock::SessionLockManager {
            id: ObjectId(60),
            client: client.clone(),
            version: 1,
        };
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        seat.pointer_button(&state, 1000, 0x110, true);
        assert_eq!(state.active_ws.get(), 0, "a locked session ignores the bind");
        *state.lock.borrow_mut() = None;
        seat.pointer_button(&state, 2000, 0x110, true);
        assert_eq!(state.active_ws.get(), 3, "unlocked, the same click acts");
    }

    #[test]
    fn fullscreen_under_a_stationary_cursor_rebases_pointer_coords() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        // a bound seat with one wl_pointer so coordinate events hit the wire
        let bind = Rc::new(WlSeat {
            id: ObjectId(80),
            client: client.clone(),
            version: 9,
            global: seat.clone(),
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        });
        client.add_client_obj(bind.clone()).unwrap();
        seat.bindings.borrow_mut().entry(client.id).or_default().push(bind.clone());
        bind.get_pointer(wl_seat::get_pointer::Request { id: ObjectId(81) }).unwrap();

        // two tiled windows: mapping the second slides the first into the
        // right half at x=400. that first one gets a buffer big enough to
        // stay under the cursor at both origins
        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        let (sb, xb, _tb) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 11, 41, 51);
        crate::shell::xdg::tests::map(&state, &client, &sb, &xb, 21);
        seat.warp(&state, 410.0, 10.0);
        assert!(seat.pointer_focus().is_some_and(|f| Rc::ptr_eq(&f, &sa)));
        assert_eq!(seat.ptr_origin.get(), (400, 0));
        let ptr = ObjectId(81);
        let bytes = client.queued_out_bytes();
        let (leaves, motions) = (count_events(&bytes, ptr, 1), count_events(&bytes, ptr, 2));

        // fullscreen the window under the stationary cursor: same surface,
        // new painted origin. coordinates must follow, without a re-enter
        let win = crate::tree::window_for_surface(&state, &sa).unwrap();
        crate::tree::set_fullscreen(&state, &win, true);
        assert_eq!(seat.ptr_origin.get(), (0, 0), "origin follows the fullscreen rect");
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ptr, 1), leaves, "same surface, no leave");
        assert!(count_events(&bytes, ptr, 2) > motions, "a motion rebases the client's view");
    }

    #[test]
    fn fullscreen_under_a_locked_pointer_keeps_origin_live() {
        use crate::protocol::interfaces::{zwp_locked_pointer_v1, zwp_pointer_constraints_v1};
        use crate::protocol::pointer_constraints::{ConstraintsManager, LockedPointer};
        use zwp_locked_pointer_v1::Handler as _;
        use zwp_pointer_constraints_v1::Handler as _;

        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let bind = Rc::new(WlSeat {
            id: ObjectId(80),
            client: client.clone(),
            version: 9,
            global: seat.clone(),
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        });
        client.add_client_obj(bind.clone()).unwrap();
        seat.bindings.borrow_mut().entry(client.id).or_default().push(bind.clone());
        bind.get_pointer(wl_seat::get_pointer::Request { id: ObjectId(81) }).unwrap();

        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        let (sb, xb, _tb) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 11, 41, 51);
        crate::shell::xdg::tests::map(&state, &client, &sb, &xb, 21);
        seat.warp(&state, 410.0, 10.0);
        assert_eq!(seat.ptr_origin.get(), (400, 0));

        let mgr = ConstraintsManager { id: ObjectId(70), client: client.clone(), version: 1 };
        mgr.lock_pointer(zwp_pointer_constraints_v1::lock_pointer::Request {
            id: ObjectId(71),
            surface: sa.id,
            pointer: ObjectId(0),
            region: ObjectId(0),
            lifetime: 1,
        })
        .unwrap();
        let con = seat.constraint_for(&sa).unwrap();
        assert!(con.active.get(), "lock engages on the focused surface");
        let ptr = ObjectId(81);
        let m0 = count_events(&client.queued_out_bytes(), ptr, 2);

        // fullscreen while the lock pins focus: the cache heals silently
        let win = crate::tree::window_for_surface(&state, &sa).unwrap();
        crate::tree::set_fullscreen(&state, &win, true);
        assert_eq!(seat.ptr_origin.get(), (0, 0), "origin heals under the pin");
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (410.0, 10.0), "locked pointer never moves");
        assert_eq!(count_events(&client.queued_out_bytes(), ptr, 2), m0, "locked client hears no motion");

        // programmatic placement yields whole while the lock holds
        seat.place_pointer(&state, 5.0, 5.0);
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (410.0, 10.0));

        // unlock still-fullscreen: the client resyncs before any button
        let lp = LockedPointer { id: ObjectId(71), con };
        lp.destroy(zwp_locked_pointer_v1::destroy::Request {}).unwrap();
        let m1 = count_events(&client.queued_out_bytes(), ptr, 2);
        assert!(m1 > m0, "unlock delivers the owed rebase");

        // the re-lock cycle: grab again, leave fullscreen under it, unlock
        mgr.lock_pointer(zwp_pointer_constraints_v1::lock_pointer::Request {
            id: ObjectId(72),
            surface: sa.id,
            pointer: ObjectId(0),
            region: ObjectId(0),
            lifetime: 1,
        })
        .unwrap();
        let con = seat.constraint_for(&sa).unwrap();
        assert!(con.active.get(), "re-lock engages");
        crate::tree::set_fullscreen(&state, &win, false);
        assert_eq!(seat.ptr_origin.get(), (400, 0), "origin heals back to the tile");
        assert_eq!(count_events(&client.queued_out_bytes(), ptr, 2), m1, "still muzzled");
        let lp = LockedPointer { id: ObjectId(72), con };
        lp.destroy(zwp_locked_pointer_v1::destroy::Request {}).unwrap();
        assert!(count_events(&client.queued_out_bytes(), ptr, 2) > m1, "second unlock resyncs too");
    }

    #[test]
    fn fullscreen_pins_directional_and_cycle_focus() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        let (sb, xb, _tb) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 11, 12, 13);
        crate::shell::xdg::tests::map(&state, &client, &sb, &xb, 21);
        for i in 0..2u32 {
            let (s, x, _t) = crate::shell::xdg::tests::mk_toplevel(
                &client,
                &base,
                14 + i * 3,
                15 + i * 3,
                16 + i * 3,
            );
            crate::shell::xdg::tests::map(&state, &client, &s, &x, 22 + i);
        }
        // the scrolling strip keeps laying out off-view columns; with the
        // view scrolled to the strip's tail the head column sits fully
        // past the left edge - not a focus candidate while a fullscreen
        // surface buries everything
        let ws = crate::tree::active(&state);
        crate::tree::set_layout(&state, &ws, crate::config::LayoutMode::Scrolling);
        let win = crate::tree::window_for_surface(&state, &sa).unwrap();
        crate::tree::focus_window(&state, Some(&win));
        while ws.tiling.strip.move_column(&win, false) {}
        crate::tree::relayout(&state, &ws);
        crate::tree::set_fullscreen(&state, &win, true);
        crate::tree::focus_dir(&state, crate::config::Dir::Left);
        let cur = crate::tree::focused_window(&state).unwrap();
        assert!(Rc::ptr_eq(&cur, &win), "directional focus stays put");
        crate::tree::focus_cycle(&state, 1);
        let cur = crate::tree::focused_window(&state).unwrap();
        assert!(Rc::ptr_eq(&cur, &win), "cycle focus stays put");
        // focus stranded on a buried window is a softlock: any focus verb
        // snaps back to the one visible window instead of wandering
        let buried = crate::tree::window_for_surface(&state, &sb).unwrap();
        crate::tree::focus_window(&state, Some(&buried));
        crate::tree::focus_dir(&state, crate::config::Dir::Left);
        let cur = crate::tree::focused_window(&state).unwrap();
        assert!(Rc::ptr_eq(&cur, &win), "stranded focus snaps to the fullscreen window");
    }

    #[test]
    fn focus_moves_never_warp_a_locked_pointer() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        let (sb, xb, _tb) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 11, 41, 51);
        crate::shell::xdg::tests::map(&state, &client, &sb, &xb, 21);
        seat.warp(&state, 600.0, 300.0);
        assert!(seat.pointer_focus().is_some_and(|f| Rc::ptr_eq(&f, &sa)));
        lock(&client, sa.id, 71, 2).unwrap();
        let con = seat.constraint_for(&sa).unwrap();
        assert!(con.active.get());
        // keyboard focus may move on; the locked cursor must not budge
        crate::tree::focus_cycle(&state, 1);
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (600.0, 300.0), "pointer frozen");
        assert!(con.active.get(), "the lock survives a keyboard-only focus move");
    }

    #[test]
    fn a_workspace_switch_breaks_a_hidden_lock() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        seat.warp(&state, 400.0, 300.0);
        assert!(seat.pointer_focus().is_some_and(|f| Rc::ptr_eq(&f, &sa)));
        lock(&client, sa.id, 71, 2).unwrap();
        let con = seat.constraint_for(&sa).unwrap();
        assert!(con.active.get(), "locked while visible");
        // the game's workspace goes hidden: the lock must break, pointer
        // focus must leave, and deltas stop flowing to the crosshair
        crate::tree::switch_workspace(&state, 1);
        assert!(!con.active.get(), "hidden window cannot hold the lock");
        assert!(
            seat.pointer_focus().is_none_or(|f| !Rc::ptr_eq(&f, &sa)),
            "pointer focus left the hidden surface"
        );
        assert!(crate::tree::focused_window(&state).is_none(), "empty workspace, no focus");
        // coming back re-arms the persistent constraint
        crate::tree::switch_workspace(&state, 0);
        assert!(con.active.get(), "persistent lock re-locks on return");
    }

    #[test]
    fn a_lock_freezes_the_pointer_and_deltas_keep_flowing() {
        let (state, client, seat, s) = setup();
        let rp = Rc::new(crate::protocol::relative_pointer::RelativePointer {
            id: ObjectId(60),
            client: client.clone(),
        });
        seat.add_relative_pointer(client.id, rp);
        lock(&client, s.id, 71, 2).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(71), 0), 1, "locked");
        seat.pointer_motion(&state, 7_000, 25.0, -12.0, 25.0, -12.0);
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (100.0, 80.0));
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(60), 0), 1, "relative_motion");
    }

    #[test]
    fn unlock_hint_skips_a_surface_the_tree_cannot_place() {
        // the setup surface belongs to no window or layer, so the hint has
        // no global origin to resolve against; the pointer must stay put
        // rather than land somewhere stale
        let (_state, client, seat, s) = setup();
        lock(&client, s.id, 71, 1).unwrap();
        let con = seat.constraint_for(&s).unwrap();
        let lp = LockedPointer { id: ObjectId(71), con };
        lp.set_cursor_position_hint(zwp_locked_pointer_v1::set_cursor_position_hint::Request {
            surface_x: Fixed::from_int(400),
            surface_y: Fixed::from_int(300),
        })
        .unwrap();
        lp.destroy(zwp_locked_pointer_v1::destroy::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(71), 1), 1, "unlocked");
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (100.0, 80.0));
        // oneshot never re-arms
        assert!(seat.constraint_for(&s).is_none());
    }

    #[test]
    fn a_confine_clamps_motion_into_the_surface() {
        let (state, client, seat, s) = setup();
        let mgr = ConstraintsManager {
            id: ObjectId(70),
            client: client.clone(),
            version: 1,
        };
        mgr.confine_pointer(zwp_pointer_constraints_v1::confine_pointer::Request {
            id: ObjectId(72),
            surface: s.id,
            pointer: ObjectId(0),
            region: ObjectId(0),
            lifetime: 2,
        })
        .unwrap();
        // surface spans 40,30 .. 840,630; a huge delta pins to the edge
        seat.pointer_motion(&state, 7_000, 5000.0, 5000.0, 5000.0, 5000.0);
        assert_eq!((seat.ptr_x.get(), seat.ptr_y.get()), (839.0, 629.0));
    }

    #[test]
    fn one_constraint_per_surface() {
        let (_state, client, seat, s) = setup();
        lock(&client, s.id, 71, 2).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);
        lock(&client, s.id, 73, 2).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
        let _ = seat;
    }

    #[test]
    fn a_pointer_grab_key_consumes_its_release() {
        use crate::config::{Action, Bind};
        let (state, _client, seat, _s) = setup();
        let mut cfg = crate::config::empty();
        cfg.binds.push(Bind {
            mods: 0,
            key: 45,
            action: Action::PointerMove,
            on_release: false,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        // the press fires the bind and remembers the key
        assert!(matches!(
            seat.key(&state, 1_000, 45, true),
            KeyAction::Act(Action::PointerMove)
        ));
        assert_eq!(seat.grab_key.get(), Some(45));
        // the release ends the session instead of reaching the client
        assert!(matches!(seat.key(&state, 2_000, 45, false), KeyAction::Handled));
        assert_eq!(seat.grab_key.get(), None);
    }

    #[test]
    fn release_binds_fire_on_keyup_and_disarm_on_other_presses() {
        use crate::config::{Action, Bind};
        let (state, _client, seat, _s) = setup();
        let mut cfg = crate::config::empty();
        cfg.binds.push(Bind {
            mods: 0,
            key: 30,
            action: Action::Quit,
            on_release: true,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        // press arms, keyup fires
        assert!(matches!(seat.key(&state, 1_000, 30, true), KeyAction::Handled));
        assert!(matches!(seat.key(&state, 2_000, 30, false), KeyAction::Act(Action::Quit)));
        // an interleaved press disarms
        assert!(matches!(seat.key(&state, 3_000, 30, true), KeyAction::Handled));
        assert!(matches!(seat.key(&state, 4_000, 31, true), KeyAction::Handled));
        seat.key(&state, 5_000, 31, false);
        assert!(matches!(seat.key(&state, 6_000, 30, false), KeyAction::Handled));
    }

    #[test]
    fn a_drag_rides_the_grab_and_release_ends_it() {
        use crate::protocol::data_device::WlDataDevice;
        use crate::protocol::interfaces::wl_data_device::Handler as _;
        let (state, client, seat, s) = setup();
        let dev = Rc::new(WlDataDevice {
            id: ObjectId(70),
            client: client.clone(),
            version: 3,
        });
        client.add_client_obj(dev.clone()).unwrap();
        // press on the surface arms the grab and records the serial
        seat.pointer_button(&state, 1_000, 0x110, true);
        let serial = seat.last_press_serial.get();
        // a stale serial is ignored
        dev.start_drag(crate::protocol::interfaces::wl_data_device::start_drag::Request {
            source: ObjectId::NONE,
            origin: s.id,
            icon: ObjectId::NONE,
            serial: serial + 1,
        })
        .unwrap();
        assert!(seat.data.drag().is_none());
        // the real one starts an internal (source-less) session
        dev.start_drag(crate::protocol::interfaces::wl_data_device::start_drag::Request {
            source: ObjectId::NONE,
            origin: s.id,
            icon: ObjectId::NONE,
            serial,
        })
        .unwrap();
        assert!(seat.data.drag().is_some());
        // pointer focus moved to the session
        assert!(seat.ptr_focus.borrow().is_none());
        // releasing the grab ends it
        seat.pointer_button(&state, 2_000, 0x110, false);
        assert!(seat.data.drag().is_none());
    }

    #[test]
    fn set_cursor_needs_the_enter_serial() {
        let (_state, client, seat, _s) = setup();
        seat.ptr_enter_serial.set(9);
        let ptr = WlPointer {
            id: ObjectId(50),
            client: client.clone(),
            version: 9,
        };
        ptr.set_cursor(wl_pointer::set_cursor::Request {
            serial: 8,
            surface: ObjectId(0),
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .unwrap();
        assert!(matches!(&*seat.cursor.borrow(), CursorState::Default));
        ptr.set_cursor(wl_pointer::set_cursor::Request {
            serial: 9,
            surface: ObjectId(0),
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .unwrap();
        assert!(matches!(&*seat.cursor.borrow(), CursorState::Hidden));
    }

    #[test]
    fn resize_edges_move_only_the_dragged_sides() {
        use crate::rect::Rect;
        let r = Rect { x1: 100, y1: 100, x2: 500, y2: 400 };
        // bottom-right: those two edges follow the deltas
        let out = resize_rect(r, 2 | 8, 50.0, -20.0, 50);
        assert_eq!(out, Rect { x1: 100, y1: 100, x2: 550, y2: 380 });
        // top-left: the box shrinks from the other side
        let out = resize_rect(r, 1 | 4, 30.0, 40.0, 50);
        assert_eq!(out, Rect { x1: 130, y1: 140, x2: 500, y2: 400 });
        // no bit, no change on that axis
        let out = resize_rect(r, 8, 25.0, 999.0, 50);
        assert_eq!(out, Rect { x1: 100, y1: 100, x2: 525, y2: 400 });
    }

    #[test]
    fn resize_stops_at_the_minimum_size() {
        use crate::rect::Rect;
        let r = Rect { x1: 100, y1: 100, x2: 500, y2: 400 };
        // dragging left/top far past the opposite edge pins at min
        let out = resize_rect(r, 1 | 4, 1000.0, 1000.0, 50);
        assert_eq!(out, Rect { x1: 450, y1: 350, x2: 500, y2: 400 });
        // and right/bottom collapsing inward pins the same way
        let out = resize_rect(r, 2 | 8, -1000.0, -1000.0, 50);
        assert_eq!(out, Rect { x1: 100, y1: 100, x2: 150, y2: 150 });
    }

    #[test]
    fn a_cursor_surface_takes_the_role() {
        let (_state, client, seat, _s) = setup();
        seat.ptr_enter_serial.set(3);
        let cs = WlSurface::new(ObjectId(11), &client, 6);
        client.add_client_obj(cs.clone()).unwrap();
        client.objects.track_surface(cs.clone());
        let ptr = WlPointer {
            id: ObjectId(50),
            client: client.clone(),
            version: 9,
        };
        ptr.set_cursor(wl_pointer::set_cursor::Request {
            serial: 3,
            surface: cs.id,
            hotspot_x: 4,
            hotspot_y: 7,
        })
        .unwrap();
        assert_eq!(cs.role.get(), crate::surface::SurfaceRole::Cursor);
        assert_eq!(seat.cursor_hot.get(), (4, 7));
        assert!(matches!(&*seat.cursor.borrow(), CursorState::Surface(_)));
        // no buffer yet: hidden on the plane, but the state tracks the surface
        cs.commit_impl();
    }

    #[test]
    fn a_cast_pick_eats_the_click_and_escape_cancels() {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let base = crate::shell::xdg::tests::mk_base(&client, 30);
        let (sa, xa, _ta) = crate::shell::xdg::tests::mk_toplevel(&client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map_sized(&state, &client, &sa, &xa, 20, 800, 600);
        seat.warp(&state, 100.0, 100.0);

        let pick = Rc::new(crate::portal::PendingPick {
            types: 2, // windows
            done: Default::default(),
            result: RefCell::new(None),
        });
        *state.cast_pick.borrow_mut() = Some(pick.clone());
        seat.pointer_button(&state, 0, 0x110, true);
        assert!(state.cast_pick.borrow().is_none(), "the click answered the pick");
        assert!(matches!(
            pick.result.borrow().as_ref(),
            Some(crate::portal::cast::RestoreData::Window { .. })
        ));
        // the matching release stays consumed
        seat.pointer_button(&state, 0, 0x110, false);
        assert!(seat.pick_swallow.borrow().is_empty());

        let pick = Rc::new(crate::portal::PendingPick {
            types: 1,
            done: Default::default(),
            result: RefCell::new(None),
        });
        *state.cast_pick.borrow_mut() = Some(pick.clone());
        let act = seat.key(&state, 0, 1, true);
        assert!(matches!(act, KeyAction::Handled), "Escape goes no further");
        assert!(state.cast_pick.borrow().is_none());
        assert!(pick.result.borrow().is_none(), "cancel answers with nothing");
    }
}
