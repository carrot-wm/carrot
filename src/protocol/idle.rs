// idle tracking: ext-idle-notify-v1 (idle-timeout notifications) and
// zwp-idle-inhibit-v1 (windows holding the screen awake). one pump task
// ticks the deadlines; input activity resumes everyone and wakes dpms.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    ext_idle_notification_v1, ext_idle_notifier_v1, zwp_idle_inhibit_manager_v1,
    zwp_idle_inhibitor_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

// -- seat-wide idle state, hangs off State --

#[derive(Default)]
pub struct IdleState {
    notifications: RefCell<Vec<Rc<IdleNotification>>>,
    inhibitors: RefCell<Vec<Rc<IdleInhibitor>>>,
    /// nsec of the last input event
    last_activity: Cell<u64>,
    /// fast-path gate so per-event work stays a cell read
    any_idled: Cell<bool>,
    /// pump parks here while there is nothing to time
    pub kick: crate::util::AsyncEvent,
}

impl IdleState {
    pub fn drop_client(&self, id: ClientId) {
        self.notifications.borrow_mut().retain(|n| n.client.id != id);
        self.inhibitors.borrow_mut().retain(|i| i.client.id != id);
    }

    pub fn clear(&self) {
        self.notifications.borrow_mut().clear();
        self.inhibitors.borrow_mut().clear();
    }

    fn inhibited(&self, state: &Rc<State>) -> bool {
        self.inhibitors
            .borrow()
            .iter()
            .any(|i| i.surface.upgrade().is_some_and(|s| visible(state, &s)))
    }
}

/// an inhibitor is only in effect while its surface is visible, not merely
/// mapped: a player parked on another workspace must not hold the screen
/// awake. visible = the root is a mapped layer surface, or its window sits
/// on a workspace that is on glass and is not buried under a fullscreen
fn visible(state: &Rc<State>, s: &Rc<WlSurface>) -> bool {
    if !s.mapped.get() {
        return false;
    }
    let root = s.get_root();
    if root.role.get() == crate::surface::SurfaceRole::LayerSurface {
        return true;
    }
    let float_above = state.config.borrow().float_above_fullscreen;
    for ws in crate::tree::visible_workspaces(state) {
        if ws
            .fullscreen
            .borrow()
            .as_ref()
            .is_some_and(|w| Rc::ptr_eq(&w.surface(), &root))
        {
            return true;
        }
        let covered = ws.fullscreen.borrow().is_some();
        if ws.floats.borrow().iter().any(|w| Rc::ptr_eq(&w.surface(), &root)) {
            if !covered || float_above {
                return true;
            }
            continue;
        }
        if !covered {
            let mut tiled = false;
            ws.tiling.for_each(|w| tiled |= Rc::ptr_eq(&w.surface(), &root));
            if tiled {
                return true;
            }
        }
    }
    false
}

/// every input event lands here; must stay cheap
pub fn note_activity(state: &Rc<State>) {
    let idle = &state.idle;
    idle.last_activity.set(crate::util::Time::now().nsec());
    if idle.any_idled.replace(false) {
        for n in idle.notifications.borrow().iter() {
            if n.idled.replace(false) {
                n.client
                    .event(|o| ext_idle_notification_v1::resumed::send(o, n.id));
            }
        }
    }
    if state.dpms_off.get() {
        crate::output::dpms(state, true);
    }
}

/// deadline sweep
pub fn tick(state: &Rc<State>) {
    let idle = &state.idle;
    if idle.inhibited(state) {
        // a visible inhibitor counts as continuous activity
        idle.last_activity.set(crate::util::Time::now().nsec());
        return;
    }
    let idle_for_ms = (crate::util::Time::now()
        .nsec()
        .saturating_sub(idle.last_activity.get()))
        / 1_000_000;
    for n in idle.notifications.borrow().iter() {
        if !n.idled.get() && idle_for_ms >= n.timeout_ms as u64 {
            n.idled.set(true);
            idle.any_idled.set(true);
            n.client
                .event(|o| ext_idle_notification_v1::idled::send(o, n.id));
        }
    }
}

/// 1s granularity is plenty for minute-scale timeouts
pub async fn pump(state: Rc<State>) {
    use crate::util::Time;
    loop {
        if state.idle.notifications.borrow().is_empty() {
            state.idle.kick.triggered().await;
            continue;
        }
        let deadline = Time::from_nsec(Time::now().nsec() + 1_000_000_000);
        if state.ring.timeout(deadline).await.is_err() {
            return;
        }
        tick(&state);
    }
}

// -- ext-idle-notify --

pub struct IdleNotifierGlobal;

impl Global for IdleNotifierGlobal {
    fn interface(&self) -> &'static str {
        ext_idle_notifier_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(IdleNotifier {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct IdleNotifier {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl ext_idle_notifier_v1::Handler for IdleNotifier {
    fn get_idle_notification(
        &self,
        req: ext_idle_notifier_v1::get_idle_notification::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = Rc::new(IdleNotification {
            id: req.id,
            client: self.client.clone(),
            timeout_ms: req.timeout,
            idled: Cell::new(false),
        });
        self.client.add_client_obj(n.clone())?;
        let state = &self.client.state;
        state.idle.notifications.borrow_mut().push(n);
        state.idle.kick.trigger();
        Ok(())
    }

    fn destroy(
        &self,
        _req: ext_idle_notifier_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IdleNotifier {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        ext_idle_notifier_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        ext_idle_notifier_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct IdleNotification {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub timeout_ms: u32,
    pub idled: Cell<bool>,
}

impl ext_idle_notification_v1::Handler for IdleNotification {
    fn destroy(
        &self,
        _req: ext_idle_notification_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client
            .state
            .idle
            .notifications
            .borrow_mut()
            .retain(|n| !(n.id == self.id && n.client.id == self.client.id));
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IdleNotification {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        ext_idle_notification_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        ext_idle_notification_v1::dispatch(&*self, 1, opcode, r)
    }
}

// -- zwp-idle-inhibit --

pub struct IdleInhibitGlobal;

impl Global for IdleInhibitGlobal {
    fn interface(&self) -> &'static str {
        zwp_idle_inhibit_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(IdleInhibitManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct IdleInhibitManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwp_idle_inhibit_manager_v1::Handler for IdleInhibitManager {
    fn create_inhibitor(
        &self,
        req: zwp_idle_inhibit_manager_v1::create_inhibitor::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(s) = self.client.objects.surface(req.surface) else {
            self.client.invalid_object(req.surface);
            return Ok(());
        };
        let i = Rc::new(IdleInhibitor {
            id: req.id,
            client: self.client.clone(),
            surface: Rc::downgrade(&s),
        });
        self.client.add_client_obj(i.clone())?;
        self.client.state.idle.inhibitors.borrow_mut().push(i);
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwp_idle_inhibit_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IdleInhibitManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_idle_inhibit_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_idle_inhibit_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct IdleInhibitor {
    pub id: ObjectId,
    pub client: Rc<Client>,
    surface: Weak<WlSurface>,
}

impl zwp_idle_inhibitor_v1::Handler for IdleInhibitor {
    fn destroy(
        &self,
        _req: zwp_idle_inhibitor_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client
            .state
            .idle
            .inhibitors
            .borrow_mut()
            .retain(|i| !(i.id == self.id && i.client.id == self.client.id));
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for IdleInhibitor {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_idle_inhibitor_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_idle_inhibitor_v1::dispatch(&*self, 1, opcode, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};

    #[test]
    fn timeouts_idle_and_activity_resumes() {
        let (state, client) = test_client();
        let n = Rc::new(IdleNotification {
            id: ObjectId(70),
            client: client.clone(),
            timeout_ms: 0,
            idled: Cell::new(false),
        });
        client.add_client_obj(n.clone()).unwrap();
        state.idle.notifications.borrow_mut().push(n.clone());
        // zero timeout: the first tick idles it
        tick(&state);
        assert!(n.idled.get());
        assert_eq!(count_events(&client.queued_out_bytes(), n.id, 0), 1, "idled");
        // input resumes it
        note_activity(&state);
        assert!(!n.idled.get());
        assert_eq!(count_events(&client.queued_out_bytes(), n.id, 1), 1, "resumed");
    }

    fn inhibited_toplevel(
        state: &Rc<crate::state::State>,
        client: &Rc<Client>,
    ) -> (Rc<WlSurface>, Rc<IdleNotification>) {
        state.output_size.set((800, 600));
        let base = crate::shell::xdg::tests::mk_base(client, 30);
        let (s, x, _t) = crate::shell::xdg::tests::mk_toplevel(client, &base, 10, 40, 50);
        crate::shell::xdg::tests::map(state, client, &s, &x, 20);
        let i = Rc::new(IdleInhibitor {
            id: ObjectId(71),
            client: client.clone(),
            surface: Rc::downgrade(&s),
        });
        client.add_client_obj(i.clone()).unwrap();
        state.idle.inhibitors.borrow_mut().push(i);
        let n = Rc::new(IdleNotification {
            id: ObjectId(70),
            client: client.clone(),
            timeout_ms: 0,
            idled: Cell::new(false),
        });
        client.add_client_obj(n.clone()).unwrap();
        state.idle.notifications.borrow_mut().push(n.clone());
        (s, n)
    }

    #[test]
    fn a_mapped_inhibitor_blocks_idling() {
        use crate::protocol::interfaces::wl_surface;
        use wl_surface::Handler as _;
        let (state, client) = test_client();
        let (s, n) = inhibited_toplevel(&state, &client);
        tick(&state);
        assert!(!n.idled.get(), "visible inhibitor holds the screen awake");
        // unmap the surface: inhibition lapses
        s.attach(wl_surface::attach::Request { buffer: ObjectId::NONE, x: 0, y: 0 }).unwrap();
        s.commit(wl_surface::commit::Request {}).unwrap();
        state.idle.last_activity.set(0);
        tick(&state);
        assert!(n.idled.get());
    }

    #[test]
    fn an_inhibitor_stops_holding_when_its_workspace_leaves_the_glass() {
        let (state, client) = test_client();
        let (_s, n) = inhibited_toplevel(&state, &client);
        state.idle.last_activity.set(0);
        tick(&state);
        assert!(!n.idled.get(), "on glass, the inhibitor holds");
        // the window's workspace is no longer shown; per spec the mapped
        // but invisible surface stops inhibiting
        crate::tree::switch_workspace(&state, 1);
        state.idle.last_activity.set(0);
        tick(&state);
        assert!(n.idled.get(), "hidden window must not hold the screen awake");
    }
}
