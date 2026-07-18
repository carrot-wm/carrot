// ext-session-lock-v1: the lock screen. state.lock is the single source of
// truth and it outlives its client - a dead locker must not unlock the
// session; a fresh lock request from a new client takes the slot over so a
// crashed locker can restart. `locked` is only sent once every output that
// existed at lock time has presented a locked frame.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::surface::{PendingState, SurfaceExt, SurfaceRole, WlSurface};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// ext_session_lock_v1 errors
const INVALID_DESTROY: u32 = 0;
const INVALID_UNLOCK: u32 = 1;
const ROLE: u32 = 2;
const DUPLICATE_OUTPUT: u32 = 3;
const ALREADY_CONSTRUCTED: u32 = 4;
// ext_session_lock_surface_v1 errors
const COMMIT_BEFORE_FIRST_ACK: u32 = 0;
const NULL_BUFFER: u32 = 1;
const DIMENSIONS_MISMATCH: u32 = 2;
const INVALID_SERIAL: u32 = 3;

pub fn locked(state: &State) -> bool {
    state.lock.borrow().is_some()
}

pub fn active(state: &State) -> Option<Rc<SessionLock>> {
    state.lock.borrow().clone()
}

/// the output rect for a connector name; headless falls back to the test size
fn output_rect(state: &State, name: &str) -> crate::rect::Rect {
    if let Some(d) = state.display.borrow().as_ref() {
        if let Some(o) = d.outputs.borrow().iter().find(|o| o.conn.name == name) {
            return o.rect();
        }
    }
    let (w, h) = state.output_size.get();
    crate::rect::Rect::new_sized_saturating(0, 0, w as i32, h as i32)
}

// -- manager --

pub struct SessionLockManagerGlobal;

impl Global for SessionLockManagerGlobal {
    fn interface(&self) -> &'static str {
        ext_session_lock_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(SessionLockManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct SessionLockManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl ext_session_lock_manager_v1::Handler for SessionLockManager {
    fn lock(
        &self,
        req: ext_session_lock_manager_v1::lock::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        let lk = Rc::new(SessionLock {
            id: req.id,
            client: self.client.clone(),
            locked_sent: Cell::new(false),
            finished: Cell::new(false),
            holder_dead: Cell::new(false),
            pending_outputs: RefCell::new(Vec::new()),
            staged_outputs: RefCell::new(Vec::new()),
            surfaces: RefCell::new(Vec::new()),
        });
        self.client.add_client_obj(lk.clone())?;

        // a live lock holder wins; a dead one is a crashed locker whose
        // session must stay locked until someone new takes the slot over
        let held_alive = state
            .lock
            .borrow()
            .as_ref()
            .is_some_and(|cur| !cur.holder_dead.get());
        if held_alive {
            lk.finished.set(true);
            self.client
                .event(|o| ext_session_lock_v1::finished::send(o, lk.id));
            return Ok(());
        }

        *state.lock.borrow_mut() = Some(lk.clone());
        // every output on glass right now owes a locked frame before the
        // locked event may be sent; a dark output would never present
        if state.dpms_off.get() {
            crate::output::dpms(state, true);
        }
        let outs: Vec<String> = state
            .display
            .borrow()
            .as_ref()
            .map(|d| d.outputs.borrow().iter().map(|o| o.conn.name.clone()).collect())
            .unwrap_or_default();
        if outs.is_empty() {
            lk.locked_sent.set(true);
            self.client
                .event(|o| ext_session_lock_v1::locked::send(o, lk.id));
        } else {
            *lk.pending_outputs.borrow_mut() = outs;
        }
        crate::tree::focus_window(state, None);
        if let Some(seat) = state.seat.borrow().clone() {
            seat.prepare_for_lock(state);
            seat.repick(state);
        }
        state.damage.trigger();
        Ok(())
    }

    fn destroy(
        &self,
        _req: ext_session_lock_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for SessionLockManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        ext_session_lock_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        ext_session_lock_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the lock --

pub struct SessionLock {
    pub id: ObjectId,
    pub client: Rc<Client>,
    /// locked sent: unlock_and_destroy is legal, destroy is not
    pub locked_sent: Cell<bool>,
    /// finished sent: the object is dead air
    pub finished: Cell<bool>,
    /// the holding client disconnected; the session stays locked
    pub holder_dead: Cell<bool>,
    /// outputs that still owe a presented locked frame
    pending_outputs: RefCell<Vec<String>>,
    /// outputs that composed a locked frame not yet presented
    staged_outputs: RefCell<Vec<String>>,
    pub surfaces: RefCell<Vec<Rc<LockSurface>>>,
}

impl ext_session_lock_v1::Handler for SessionLock {
    fn get_lock_surface(
        &self,
        req: ext_session_lock_v1::get_lock_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        let Some(output) = c.objects.output(req.output) else {
            c.invalid_object(req.output);
            return Ok(());
        };
        if surface.has_live_role() {
            c.protocol_error(self.id, ROLE, "the surface already has a role object");
            return Ok(());
        }
        if let Err(old) = surface.set_role(SurfaceRole::LockSurface) {
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
        if self
            .surfaces
            .borrow()
            .iter()
            .any(|l| l.output_name == output.name)
        {
            c.protocol_error(self.id, DUPLICATE_OUTPUT, "the output already has a lock surface");
            return Ok(());
        }
        let ls = Rc::new(LockSurface {
            id: req.id,
            client: c.clone(),
            surface: surface.clone(),
            output_name: output.name.clone(),
            next_serial: Cell::new(1),
            last_sent: Cell::new(0),
            acked: Cell::new(0),
            sent: RefCell::new(Vec::new()),
            acked_size: Cell::new((0, 0)),
        });
        c.add_client_obj(ls.clone())?;
        *surface.ext.borrow_mut() = Rc::new(LockExt { ls: ls.clone() });
        self.surfaces.borrow_mut().push(ls.clone());
        // inert after finished: the client is expected to destroy everything
        if !self.finished.get() {
            let r = output_rect(&c.state, &ls.output_name);
            ls.configure(r.width() as u32, r.height() as u32);
        }
        Ok(())
    }

    fn unlock_and_destroy(
        &self,
        _req: ext_session_lock_v1::unlock_and_destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        if !self.locked_sent.get() {
            self.client
                .protocol_error(self.id, INVALID_UNLOCK, "unlock before the locked event");
            return Ok(());
        }
        let held = state
            .lock
            .borrow()
            .as_ref()
            .is_some_and(|cur| cur.id == self.id && cur.client.id == self.client.id);
        if held {
            *state.lock.borrow_mut() = None;
            crate::tree::focus_window(state, crate::tree::focused_window(state).as_ref());
            if let Some(seat) = state.seat.borrow().clone() {
                seat.repick(state);
            }
            state.damage.trigger();
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn destroy(
        &self,
        _req: ext_session_lock_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.locked_sent.get() {
            self.client
                .protocol_error(self.id, INVALID_DESTROY, "destroy while locked");
            return Ok(());
        }
        let state = &self.client.state;
        // a withdrawn lock attempt (no locked, no finished) releases the slot
        let held = state
            .lock
            .borrow()
            .as_ref()
            .is_some_and(|cur| cur.id == self.id && cur.client.id == self.client.id);
        if held {
            *state.lock.borrow_mut() = None;
            state.damage.trigger();
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for SessionLock {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        ext_session_lock_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        ext_session_lock_v1::dispatch(&*self, 1, opcode, r)
    }
}

// -- lock surfaces --

pub struct LockSurface {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub surface: Rc<WlSurface>,
    pub output_name: String,
    next_serial: Cell<u32>,
    last_sent: Cell<u32>,
    acked: Cell<u32>,
    /// outstanding configures: (serial, w, h)
    sent: RefCell<Vec<(u32, u32, u32)>>,
    /// size of the last acked configure; commits must match it exactly
    acked_size: Cell<(u32, u32)>,
}

impl LockSurface {
    pub fn configure(&self, w: u32, h: u32) {
        let serial = self.next_serial.get();
        self.next_serial.set(serial.wrapping_add(1));
        self.last_sent.set(serial);
        self.sent.borrow_mut().push((serial, w, h));
        self.client.event(|o| {
            ext_session_lock_surface_v1::configure::send(o, self.id, serial, w, h)
        });
    }
}

impl ext_session_lock_surface_v1::Handler for LockSurface {
    fn ack_configure(
        &self,
        req: ext_session_lock_surface_v1::ack_configure::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if req.serial == 0 || req.serial > self.last_sent.get() {
            self.client
                .protocol_error(self.id, INVALID_SERIAL, "ack of a never-sent serial");
            return Ok(());
        }
        if req.serial <= self.acked.get() {
            return Ok(());
        }
        self.acked.set(req.serial);
        let mut sent = self.sent.borrow_mut();
        if let Some(&(_, w, h)) = sent.iter().find(|(s, _, _)| *s == req.serial) {
            self.acked_size.set((w, h));
        }
        sent.retain(|(s, _, _)| *s > req.serial);
        Ok(())
    }

    fn destroy(
        &self,
        _req: ext_session_lock_surface_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        if let Some(lk) = state.lock.borrow().as_ref() {
            lk.surfaces
                .borrow_mut()
                .retain(|l| !(l.id == self.id && l.client.id == self.client.id));
        }
        *self.surface.ext.borrow_mut() = Rc::new(crate::surface::NoneExt);
        self.client.remove_obj(self.id)?;
        state.damage.trigger();
        Ok(())
    }
}

impl Object for LockSurface {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        ext_session_lock_surface_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        ext_session_lock_surface_v1::dispatch(&*self, 1, opcode, r)
    }
}

struct LockExt {
    ls: Rc<LockSurface>,
}

impl SurfaceExt for LockExt {
    fn commit_requested(self: Rc<Self>, pending: Box<PendingState>) -> Option<Box<PendingState>> {
        let ls = &self.ls;
        if ls.acked.get() == 0 {
            ls.client.protocol_error(
                ls.id,
                COMMIT_BEFORE_FIRST_ACK,
                "commit before the first ack_configure",
            );
            return None;
        }
        // every commit keeps a buffer: an explicit detach is an error, and so
        // is an initial commit that never attached one
        let detaching = matches!(&pending.buffer, Some(None));
        let attaching = matches!(&pending.buffer, Some(Some(_)));
        if detaching || (!attaching && ls.surface.buffer.borrow().is_none()) {
            ls.client
                .protocol_error(ls.id, NULL_BUFFER, "lock surface committed without a buffer");
            return None;
        }
        // exact-size requirement, checked before the buffer can land; the
        // configured size lives in surface-local coordinates, so this
        // commit's scale and transform apply before comparing
        if let Some(Some(b)) = &pending.buffer {
            let (mut bw, mut bh) = (b.buf.rect.width(), b.buf.rect.height());
            let transform = pending.transform.unwrap_or(ls.surface.transform.get());
            if transform.swaps_dimensions() {
                std::mem::swap(&mut bw, &mut bh);
            }
            let scale = pending.scale.unwrap_or(ls.surface.scale.get()).max(1) as u32;
            let logical = ((bw as u32).div_ceil(scale), (bh as u32).div_ceil(scale));
            if logical != ls.acked_size.get() {
                ls.client.protocol_error(
                    ls.id,
                    DIMENSIONS_MISMATCH,
                    "buffer does not match the configured size",
                );
                return None;
            }
        }
        Some(pending)
    }

    fn after_apply(&self) {
        let ls = &self.ls;
        let state = &ls.client.state;
        // the newest mapped lock surface takes the keyboard; there is one
        // seat, and every lock surface belongs to the same client
        if ls.surface.mapped.get() && locked_by(state, &ls.client) {
            if let Some(seat) = state.seat.borrow().clone() {
                crate::input::focus::set_keyboard_focus(state, &seat, Some(ls.surface.clone()));
                seat.repick(state);
            }
        }
        state.damage.trigger();
    }

    fn on_surface_destroy(&self) -> Result<(), ()> {
        Err(())
    }
}

fn locked_by(state: &State, client: &Rc<Client>) -> bool {
    state
        .lock
        .borrow()
        .as_ref()
        .is_some_and(|lk| lk.client.id == client.id)
}

// -- compositor hooks --

/// compose asks what to draw while locked; recording the ask stages the
/// output for the locked-event confirmation (its next present is locked)
pub fn compose_locked(state: &Rc<State>, output: &str) -> Option<Rc<WlSurface>> {
    let lk = active(state)?;
    {
        let mut staged = lk.staged_outputs.borrow_mut();
        if !staged.iter().any(|n| n == output) {
            staged.push(output.to_string());
        }
    }
    lk.surfaces
        .borrow()
        .iter()
        .find(|l| l.output_name == output && l.surface.mapped.get())
        .map(|l| l.surface.clone())
}

/// present-loop tail: a staged output has now shown a locked frame
pub fn output_presented(state: &Rc<State>, output: &str) {
    let Some(lk) = active(state) else { return };
    if lk.locked_sent.get() || lk.holder_dead.get() {
        return;
    }
    if !lk.staged_outputs.borrow().iter().any(|n| n == output) {
        return;
    }
    let done = {
        let mut pending = lk.pending_outputs.borrow_mut();
        pending.retain(|n| n != output);
        pending.is_empty()
    };
    if done {
        lk.locked_sent.set(true);
        lk.client
            .event(|o| ext_session_lock_v1::locked::send(o, lk.id));
    }
}

/// a dead output owes nothing; new outputs stay blank until the client
/// gives them a lock surface
pub fn output_removed(state: &Rc<State>, output: &str) {
    let Some(lk) = active(state) else { return };
    lk.surfaces.borrow_mut().retain(|l| l.output_name != output);
    if lk.locked_sent.get() || lk.holder_dead.get() {
        return;
    }
    let done = {
        let mut pending = lk.pending_outputs.borrow_mut();
        pending.retain(|n| n != output);
        !pending.is_empty()
    };
    if !done {
        lk.locked_sent.set(true);
        lk.client
            .event(|o| ext_session_lock_v1::locked::send(o, lk.id));
    }
}

/// an output changed size while locked: the lock surface must follow
pub fn output_resized(state: &Rc<State>, output: &str) {
    let Some(lk) = active(state) else { return };
    let r = output_rect(state, output);
    for l in lk.surfaces.borrow().iter() {
        if l.output_name == output {
            l.configure(r.width() as u32, r.height() as u32);
        }
    }
}

/// the locking client died: the session stays locked, outputs go blank,
/// and the next lock request takes the slot over
pub fn drop_client(state: &State, id: ClientId) {
    let Some(lk) = state.lock.borrow().clone() else { return };
    if lk.client.id == id {
        lk.holder_dead.set(true);
        lk.surfaces.borrow_mut().clear();
        state.damage.trigger();
    }
}

/// pointer hit while locked: only this output's lock surface exists
pub fn surface_at(state: &Rc<State>, x: i32, y: i32) -> Option<(Rc<WlSurface>, i32, i32)> {
    let lk = active(state)?;
    let surfaces = lk.surfaces.borrow();
    for l in surfaces.iter() {
        if !l.surface.mapped.get() {
            continue;
        }
        let r = output_rect(state, &l.output_name);
        if r.contains(x, y) {
            let (s, sx, sy) = l.surface.find_surface_at(x - r.x1, y - r.y1)?;
            return Some((s, sx, sy));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::interfaces::wl_surface;
    use crate::protocol::shm::test_buffer;
    use ext_session_lock_manager_v1::Handler as _;
    use ext_session_lock_surface_v1::Handler as _;
    use ext_session_lock_v1::Handler as _;
    use wl_surface::Handler as _;

    const ERR: ObjectId = ObjectId(1);
    // ext_session_lock_v1 events
    const EV_LOCKED: u32 = 0;
    const EV_FINISHED: u32 = 1;

    fn setup() -> (Rc<State>, Rc<Client>, Rc<SessionLockManager>) {
        let (state, client) = test_client();
        state.output_size.set((800, 600));
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat);
        let out = crate::protocol::output::WlOutputGlobal {
            name: "TEST-1".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            refresh_mhz: 60_000,
        };
        crate::protocol::globals::Global::bind(&out, &client, ObjectId(90), 4).unwrap();
        let mgr = Rc::new(SessionLockManager {
            id: ObjectId(60),
            client: client.clone(),
            version: 1,
        });
        client.add_client_obj(mgr.clone()).unwrap();
        (state, client, mgr)
    }

    fn mk_surface(client: &Rc<Client>, id: u32) -> Rc<WlSurface> {
        let s = WlSurface::new(ObjectId(id), client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        s
    }

    #[test]
    fn locks_headless_and_a_second_lock_finishes() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        assert!(locked(&state));
        let bytes = client.queued_out_bytes();
        // no outputs on glass: locked arrives immediately
        assert_eq!(count_events(&bytes, ObjectId(61), EV_LOCKED), 1);
        // the slot is taken; a second attempt is finished on arrival
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(62) }).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(62), EV_FINISHED), 1);
        assert_eq!(active(&state).unwrap().id, ObjectId(61));
    }

    #[test]
    fn lock_surface_maps_takes_input_and_unlock_restores() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        let lk = active(&state).unwrap();
        let s = mk_surface(&client, 10);
        lk.get_lock_surface(ext_session_lock_v1::get_lock_surface::Request {
            id: ObjectId(70),
            surface: ObjectId(10),
            output: ObjectId(90),
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(70), 0), 1, "initial configure");
        let ls = lk.surfaces.borrow()[0].clone();
        ls.ack_configure(ext_session_lock_surface_v1::ack_configure::Request { serial: 1 })
            .unwrap();
        let b = test_buffer(&client, ObjectId(20), 800, 600);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 }).unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert!(s.mapped.get());
        // the lock surface is the only hittable thing and owns the keyboard
        let hit = crate::tree::surface_at(&state, 10, 10);
        assert!(hit.is_some_and(|(h, _, _)| Rc::ptr_eq(&h, &s)));
        let seat = state.seat.borrow().clone().unwrap();
        assert!(seat.kb_focus.borrow().as_ref().is_some_and(|f| Rc::ptr_eq(f, &s)));
        lk.unlock_and_destroy(ext_session_lock_v1::unlock_and_destroy::Request {}).unwrap();
        assert!(!locked(&state));
        assert!(crate::tree::surface_at(&state, 10, 10).is_none());
    }

    #[test]
    fn commit_before_first_ack_is_a_protocol_error() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        let lk = active(&state).unwrap();
        let s = mk_surface(&client, 10);
        lk.get_lock_surface(ext_session_lock_v1::get_lock_surface::Request {
            id: ObjectId(70),
            surface: ObjectId(10),
            output: ObjectId(90),
        })
        .unwrap();
        let b = test_buffer(&client, ObjectId(20), 800, 600);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 }).unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }

    #[test]
    fn a_wrong_size_buffer_is_a_protocol_error() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        let lk = active(&state).unwrap();
        let s = mk_surface(&client, 10);
        lk.get_lock_surface(ext_session_lock_v1::get_lock_surface::Request {
            id: ObjectId(70),
            surface: ObjectId(10),
            output: ObjectId(90),
        })
        .unwrap();
        let ls = lk.surfaces.borrow()[0].clone();
        ls.ack_configure(ext_session_lock_surface_v1::ack_configure::Request { serial: 1 })
            .unwrap();
        let b = test_buffer(&client, ObjectId(20), 64, 64);
        s.attach(wl_surface::attach::Request { buffer: b.id, x: 0, y: 0 }).unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
        assert!(!s.mapped.get() || crate::tree::surface_at(&state, 10, 10).is_none());
    }

    #[test]
    fn a_dead_locker_keeps_the_session_locked_and_can_be_replaced() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        assert!(locked(&state));
        drop_client(&state, client.id);
        assert!(locked(&state), "a crashed locker must not unlock the session");
        assert!(active(&state).unwrap().holder_dead.get());
        // the next lock takes the slot over so the locker can restart
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(63) }).unwrap();
        let lk = active(&state).unwrap();
        assert_eq!(lk.id, ObjectId(63));
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(63), EV_LOCKED), 1);
    }

    #[test]
    fn duplicate_output_is_a_protocol_error() {
        let (state, client, mgr) = setup();
        mgr.lock(ext_session_lock_manager_v1::lock::Request { id: ObjectId(61) }).unwrap();
        let lk = active(&state).unwrap();
        mk_surface(&client, 10);
        mk_surface(&client, 11);
        lk.get_lock_surface(ext_session_lock_v1::get_lock_surface::Request {
            id: ObjectId(70),
            surface: ObjectId(10),
            output: ObjectId(90),
        })
        .unwrap();
        lk.get_lock_surface(ext_session_lock_v1::get_lock_surface::Request {
            id: ObjectId(71),
            surface: ObjectId(11),
            output: ObjectId(90),
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ERR, 0), 1);
    }
}
