// ext-workspace v1: groups follow outputs, workspaces follow the append-only
// list. every mutation site funnels through one changed() sweep that diffs
// protocol truth against what each client was last told, so a pager always
// receives one atomic burst ending in done, no matter which path moved the
// state. requests latch until commit, as the protocol demands.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::output::Output;
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    ext_workspace_group_handle_v1 as group_v1, ext_workspace_handle_v1 as workspace_v1,
    ext_workspace_manager_v1 as manager_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::tree::workspace::Workspace;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// the workspace is on glass on its own output
const STATE_ACTIVE: u32 = 1;
/// activate and assign are backed by real mutators; remove and deactivate
/// fight the append-only, one-per-output model and stay unadvertised
const WS_CAPS: u32 = 1 | 8;
/// create lands the next index on the group's output
const GROUP_CAPS: u32 = 1;

// -- the global --

pub struct ExtWorkspaceGlobal;

impl Global for ExtWorkspaceGlobal {
    fn interface(&self) -> &'static str {
        manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let mgr = Rc::new(ExtWorkspaceManager {
            id,
            client: client.clone(),
            version,
            stopped: Cell::new(false),
            groups: RefCell::new(Vec::new()),
            workspaces: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
        });
        client.add_client_obj(mgr.clone())?;
        let state = &client.state;
        state.ext_workspace_managers.borrow_mut().push(mgr.clone());
        // a fresh manager knows nothing; the first diff is the full replay
        sync(state, &mgr);
        Ok(())
    }
}

// -- the manager --

enum Pending {
    Activate(Weak<Workspace>),
    Assign(Weak<Workspace>, Weak<Output>),
    Create(Weak<Output>),
}

fn slot_of(state: &Rc<State>, out: &Rc<Output>) -> Option<usize> {
    let d = state.display.borrow();
    let d = d.as_ref()?;
    d.outputs.borrow().iter().position(|o| Rc::ptr_eq(o, out))
}

fn index_of(state: &Rc<State>, ws: &Rc<Workspace>) -> Option<usize> {
    state
        .workspaces
        .borrow()
        .iter()
        .position(|x| Rc::ptr_eq(x, ws))
}

pub struct ExtWorkspaceManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    stopped: Cell<bool>,
    groups: RefCell<Vec<Rc<ExtWorkspaceGroup>>>,
    workspaces: RefCell<Vec<Rc<ExtWorkspaceHandle>>>,
    /// requests wait here until commit makes them atomic
    pending: RefCell<Vec<Pending>>,
}

impl manager_v1::Handler for ExtWorkspaceManager {
    fn commit(&self, _req: manager_v1::commit::Request) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.client.state.clone();
        let ops: Vec<Pending> = self.pending.borrow_mut().drain(..).collect();
        for op in ops {
            match op {
                Pending::Activate(w) => {
                    let Some(ws) = w.upgrade() else { continue };
                    if let Some(idx) = index_of(&state, &ws) {
                        // the switch hook fans the result back out to
                        // every manager, this one included
                        crate::tree::switch_workspace(&state, idx);
                    }
                }
                Pending::Assign(w, o) => {
                    let Some(ws) = w.upgrade() else { continue };
                    let Some(out) = o.upgrade() else { continue };
                    let idx = index_of(&state, &ws);
                    let slot = slot_of(&state, &out);
                    if let (Some(idx), Some(slot)) = (idx, slot) {
                        crate::tree::move_workspace_to_output(&state, idx, slot);
                    }
                }
                Pending::Create(o) => {
                    let Some(out) = o.upgrade() else { continue };
                    if let Some(slot) = slot_of(&state, &out) {
                        crate::tree::create_workspace_on(&state, slot);
                    }
                }
            }
        }
        Ok(())
    }

    fn stop(&self, _req: manager_v1::stop::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.stopped.set(true);
        self.client.event(|o| manager_v1::finished::send(o, self.id));
        // finished is a destructor: the object dies here, the handles stay
        // until the client destroys them (their backpointers dangle safely)
        self.client
            .state
            .ext_workspace_managers
            .borrow_mut()
            .retain(|m| !(m.id == self.id && m.client.id == self.client.id));
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ExtWorkspaceManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        manager_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.groups.borrow_mut().clear();
        self.workspaces.borrow_mut().clear();
        self.pending.borrow_mut().clear();
    }
}

// -- the group handle --

pub struct ExtWorkspaceGroup {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    mgr: RefCell<Weak<ExtWorkspaceManager>>,
    output: RefCell<Weak<Output>>,
    /// the compositor lost the output; the object is inert but alive
    removed: Cell<bool>,
    /// the client destroyed the object; it stays listed so the same
    /// output is never re-announced to a client that dropped it
    dead: Cell<bool>,
}

impl ExtWorkspaceGroup {
    fn output(&self) -> Option<Rc<Output>> {
        self.output.borrow().upgrade()
    }

    fn is_for(&self, o: &Rc<Output>) -> bool {
        Weak::ptr_eq(&self.output.borrow(), &Rc::downgrade(o))
    }

    /// silent handles take no further events
    fn silent(&self) -> bool {
        self.removed.get() || self.dead.get()
    }
}

impl group_v1::Handler for ExtWorkspaceGroup {
    fn create_workspace(
        &self,
        _req: group_v1::create_workspace::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // the asked-for name is dropped: workspaces answer to their index,
        // which the spec permits. the mint itself waits for commit
        if self.silent() {
            return Ok(());
        }
        if let Some(mgr) = self.mgr.borrow().upgrade() {
            mgr.pending
                .borrow_mut()
                .push(Pending::Create(self.output.borrow().clone()));
        }
        Ok(())
    }

    fn destroy(&self, _req: group_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.dead.set(true);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ExtWorkspaceGroup {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        group_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        group_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        *self.mgr.borrow_mut() = Weak::new();
        *self.output.borrow_mut() = Weak::new();
    }
}

// -- the workspace handle --

pub struct ExtWorkspaceHandle {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    mgr: RefCell<Weak<ExtWorkspaceManager>>,
    workspace: RefCell<Weak<Workspace>>,
    /// last state bits on the wire; starts poisoned so creation always
    /// sends an initial state event
    sent_state: Cell<u32>,
    /// the output whose group this workspace last entered, by identity
    sent_group: RefCell<Weak<Output>>,
    dead: Cell<bool>,
}

impl ExtWorkspaceHandle {
    fn ws(&self) -> Option<Rc<Workspace>> {
        self.workspace.borrow().upgrade()
    }

    fn is_for(&self, ws: &Rc<Workspace>) -> bool {
        Weak::ptr_eq(&self.workspace.borrow(), &Rc::downgrade(ws))
    }
}

impl workspace_v1::Handler for ExtWorkspaceHandle {
    fn destroy(
        &self,
        _req: workspace_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dead.set(true);
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn activate(
        &self,
        _req: workspace_v1::activate::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.dead.get() {
            return Ok(());
        }
        if let Some(mgr) = self.mgr.borrow().upgrade() {
            mgr.pending
                .borrow_mut()
                .push(Pending::Activate(self.workspace.borrow().clone()));
        }
        Ok(())
    }

    fn deactivate(
        &self,
        _req: workspace_v1::deactivate::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // an output always shows exactly one workspace; nothing to do,
        // and the capability says so
        Ok(())
    }

    fn assign(&self, req: workspace_v1::assign::Request) -> Result<(), Box<dyn std::error::Error>> {
        if self.dead.get() {
            return Ok(());
        }
        let Some(mgr) = self.mgr.borrow().upgrade() else {
            return Ok(());
        };
        // a stale or foreign group id is a no-guarantee no-op, not an error
        let group = mgr
            .groups
            .borrow()
            .iter()
            .find(|g| g.id == req.workspace_group)
            .cloned();
        let Some(g) = group else { return Ok(()) };
        mgr.pending.borrow_mut().push(Pending::Assign(
            self.workspace.borrow().clone(),
            g.output.borrow().clone(),
        ));
        Ok(())
    }

    fn remove(&self, _req: workspace_v1::remove::Request) -> Result<(), Box<dyn std::error::Error>> {
        // workspaces are index-addressed and never removed; unadvertised
        Ok(())
    }
}

impl Object for ExtWorkspaceHandle {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        workspace_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        workspace_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        *self.mgr.borrow_mut() = Weak::new();
        *self.workspace.borrow_mut() = Weak::new();
        *self.sent_group.borrow_mut() = Weak::new();
    }
}

// -- the diff engine --

fn state_bits(outs: &[Rc<Output>], idx: usize, ws: &Workspace) -> u32 {
    match outs.get(ws.output.get()) {
        Some(o) if o.ws.get() == idx => STATE_ACTIVE,
        _ => 0,
    }
}

/// enter/leave against the client's wl_output binds, by connector name
fn each_bound_output(client: &Rc<Client>, out: &Rc<Output>, f: impl FnMut(ObjectId)) {
    let mut f = f;
    let name = out.conn.name.clone();
    client.objects.for_each_output(|w| {
        if w.name == name {
            f(w.id);
        }
    });
}

fn find_group(mgr: &Rc<ExtWorkspaceManager>, out: &Weak<Output>) -> Option<Rc<ExtWorkspaceGroup>> {
    mgr.groups
        .borrow()
        .iter()
        .find(|g| Weak::ptr_eq(&g.output.borrow(), out))
        .cloned()
}

/// one manager catches up with the world; every difference goes out as a
/// single burst ending in done
fn sync(state: &Rc<State>, mgr: &Rc<ExtWorkspaceManager>) {
    if mgr.stopped.get() {
        return;
    }
    let outs: Vec<Rc<Output>> = state
        .display
        .borrow()
        .as_ref()
        .map(|d| d.outputs.borrow().clone())
        .unwrap_or_default();
    let wss: Vec<Rc<Workspace>> = state.workspaces.borrow().clone();
    let mut wrote = false;

    // new groups first, so entries below have a target. a dead or removed
    // listing still counts as seen: the same output never re-announces
    for o in &outs {
        if mgr.groups.borrow().iter().any(|g| g.is_for(o)) {
            continue;
        }
        let g = Rc::new(ExtWorkspaceGroup {
            id: mgr.client.objects.alloc_server_id(),
            client: mgr.client.clone(),
            version: mgr.version,
            mgr: RefCell::new(Rc::downgrade(mgr)),
            output: RefCell::new(Rc::downgrade(o)),
            removed: Cell::new(false),
            dead: Cell::new(false),
        });
        mgr.client.add_server_obj(g.clone());
        mgr.groups.borrow_mut().push(g.clone());
        wrote = true;
        let (mid, gid) = (mgr.id, g.id);
        mgr.client.event(|ev| {
            manager_v1::workspace_group::send(ev, mid, gid);
            group_v1::capabilities::send(ev, gid, GROUP_CAPS);
        });
        each_bound_output(&mgr.client, o, |wl| {
            mgr.client
                .event(|ev| group_v1::output_enter::send(ev, gid, wl));
        });
    }

    // new workspaces get their identity burst; state and group entry
    // arrive through the diff below, off the poisoned initials
    for (i, ws) in wss.iter().enumerate() {
        if mgr.workspaces.borrow().iter().any(|h| h.is_for(ws)) {
            continue;
        }
        let h = Rc::new(ExtWorkspaceHandle {
            id: mgr.client.objects.alloc_server_id(),
            client: mgr.client.clone(),
            version: mgr.version,
            mgr: RefCell::new(Rc::downgrade(mgr)),
            workspace: RefCell::new(Rc::downgrade(ws)),
            sent_state: Cell::new(u32::MAX),
            sent_group: RefCell::new(Weak::new()),
            dead: Cell::new(false),
        });
        mgr.client.add_server_obj(h.clone());
        mgr.workspaces.borrow_mut().push(h.clone());
        wrote = true;
        let (mid, hid) = (mgr.id, h.id);
        // the index is the identity users address; it never shifts because
        // the list only ever grows, which is exactly what a session-stable
        // id asks for
        let tag = (i + 1).to_string();
        let coord = (i as u32).to_ne_bytes();
        mgr.client.event(|ev| {
            manager_v1::workspace::send(ev, mid, hid);
            workspace_v1::id::send(ev, hid, &tag);
            workspace_v1::name::send(ev, hid, &tag);
            workspace_v1::coordinates::send(ev, hid, &coord);
            workspace_v1::capabilities::send(ev, hid, WS_CAPS);
        });
    }

    // membership and state diffs; leaves land before the removals below
    // so a dying group empties out first, as the protocol requires
    let handles = mgr.workspaces.borrow().clone();
    for h in handles {
        if h.dead.get() {
            continue;
        }
        let Some(ws) = h.ws() else { continue };
        let idx = wss.iter().position(|w| Rc::ptr_eq(w, &ws));
        let cur = idx.and_then(|_| outs.get(ws.output.get()).cloned());
        let cur_weak = cur.as_ref().map(Rc::downgrade).unwrap_or_default();
        if !Weak::ptr_eq(&h.sent_group.borrow(), &cur_weak) {
            wrote = true;
            let old = h.sent_group.borrow().clone();
            if let Some(g) = find_group(mgr, &old)
                && !g.silent()
            {
                mgr.client
                    .event(|ev| group_v1::workspace_leave::send(ev, g.id, h.id));
            }
            if let Some(g) = find_group(mgr, &cur_weak)
                && !g.silent()
            {
                mgr.client
                    .event(|ev| group_v1::workspace_enter::send(ev, g.id, h.id));
            }
            *h.sent_group.borrow_mut() = cur_weak;
        }
        let bits = idx.map(|i| state_bits(&outs, i, &ws)).unwrap_or(0);
        if h.sent_state.get() != bits {
            h.sent_state.set(bits);
            wrote = true;
            mgr.client
                .event(|ev| workspace_v1::state::send(ev, h.id, bits));
        }
    }

    // groups whose output left the world, now that their workspaces are out
    let groups = mgr.groups.borrow().clone();
    for g in groups {
        if g.silent() {
            continue;
        }
        let alive = g
            .output()
            .is_some_and(|o| outs.iter().any(|n| Rc::ptr_eq(n, &o)));
        if alive {
            continue;
        }
        g.removed.set(true);
        wrote = true;
        mgr.client.event(|ev| group_v1::removed::send(ev, g.id));
    }

    if wrote {
        mgr.client.event(|ev| manager_v1::done::send(ev, mgr.id));
    }
}

// -- fan-out --

/// the one hook every workspace mutation site calls; diffing makes it
/// idempotent, so overlapping call sites cost nothing but a scan
pub fn changed(state: &Rc<State>) {
    let mgrs = state.ext_workspace_managers.borrow().clone();
    for mgr in mgrs {
        sync(state, &mgr);
    }
}

/// a fresh wl_output bind re-enters the group already pinned to that
/// connector, as the protocol promises for late binds
pub fn output_bound(client: &Rc<Client>, wlout: &Rc<crate::protocol::output::WlOutput>) {
    let mgrs = client.state.ext_workspace_managers.borrow().clone();
    for mgr in mgrs {
        if mgr.client.id != client.id || mgr.stopped.get() {
            continue;
        }
        let groups = mgr.groups.borrow().clone();
        let mut wrote = false;
        for g in groups {
            if g.silent() {
                continue;
            }
            let Some(o) = g.output() else { continue };
            if o.conn.name == wlout.name {
                wrote = true;
                client.event(|ev| group_v1::output_enter::send(ev, g.id, wlout.id));
            }
        }
        if wrote {
            client.event(|ev| manager_v1::done::send(ev, mgr.id));
        }
    }
}

pub fn drop_client(state: &Rc<State>, id: ClientId) {
    state
        .ext_workspace_managers
        .borrow_mut()
        .retain(|m| m.client.id != id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, event_seq, test_client};
    use crate::protocol::MIN_SERVER_ID;
    use manager_v1::Handler as _;
    use workspace_v1::Handler as _;

    fn bind_mgr(client: &Rc<Client>, id: u32) -> Rc<ExtWorkspaceManager> {
        ExtWorkspaceGlobal.bind(client, ObjectId(id), 1).unwrap();
        client
            .state
            .ext_workspace_managers
            .borrow()
            .iter()
            .find(|m| m.id == ObjectId(id))
            .cloned()
            .unwrap()
    }

    #[test]
    fn bind_bursts_identity_then_state_then_one_done() {
        let (state, client) = test_client();
        crate::tree::ensure_workspace(&state, 1);
        let before = client.queued_out_bytes().len();
        bind_mgr(&client, 60);
        let bytes = client.queued_out_bytes();
        let seq = event_seq(&bytes[before..]);
        let h0 = MIN_SERVER_ID;
        let h1 = MIN_SERVER_ID + 1;
        assert_eq!(
            seq,
            vec![
                (60, manager_v1::workspace::OPCODE),
                (h0, workspace_v1::id::OPCODE),
                (h0, workspace_v1::name::OPCODE),
                (h0, workspace_v1::coordinates::OPCODE),
                (h0, workspace_v1::capabilities::OPCODE),
                (60, manager_v1::workspace::OPCODE),
                (h1, workspace_v1::id::OPCODE),
                (h1, workspace_v1::name::OPCODE),
                (h1, workspace_v1::coordinates::OPCODE),
                (h1, workspace_v1::capabilities::OPCODE),
                (h0, workspace_v1::state::OPCODE),
                (h1, workspace_v1::state::OPCODE),
                (60, manager_v1::done::OPCODE),
            ]
        );
    }

    #[test]
    fn a_second_sweep_with_nothing_new_stays_silent() {
        let (state, client) = test_client();
        crate::tree::ensure_workspace(&state, 2);
        bind_mgr(&client, 60);
        let before = client.queued_out_bytes().len();
        changed(&state);
        changed(&state);
        assert_eq!(client.queued_out_bytes().len(), before, "diff is idempotent");
    }

    #[test]
    fn activation_latches_until_commit() {
        let (state, client) = test_client();
        crate::tree::ensure_workspace(&state, 2);
        let mgr = bind_mgr(&client, 60);
        let h = mgr.workspaces.borrow()[2].clone();
        h.activate(workspace_v1::activate::Request {}).unwrap();
        assert_eq!(state.active_ws.get(), 0, "nothing moves before commit");
        mgr.commit(manager_v1::commit::Request {}).unwrap();
        assert_eq!(state.active_ws.get(), 2, "commit applies the batch");
    }

    #[test]
    fn a_workspace_born_mid_session_is_announced_once() {
        let (state, client) = test_client();
        let mgr = bind_mgr(&client, 60);
        let before = client.queued_out_bytes().len();
        crate::tree::ensure_workspace(&state, 0);
        let bytes = client.queued_out_bytes();
        assert_eq!(
            count_events(&bytes[before..], mgr.id, manager_v1::workspace::OPCODE),
            1
        );
        assert_eq!(count_events(&bytes[before..], mgr.id, manager_v1::done::OPCODE), 1);
        // the sweep that follows the announcing one adds nothing
        let after = client.queued_out_bytes().len();
        changed(&state);
        assert_eq!(client.queued_out_bytes().len(), after);
    }

    #[test]
    fn stop_finishes_destroys_and_silences() {
        let (state, client) = test_client();
        crate::tree::ensure_workspace(&state, 0);
        let mgr = bind_mgr(&client, 60);
        mgr.stop(manager_v1::stop::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, mgr.id, manager_v1::finished::OPCODE), 1);
        assert!(client.objects.get(mgr.id).is_none(), "finished is a destructor");
        assert!(state.ext_workspace_managers.borrow().is_empty());
        let before = client.queued_out_bytes().len();
        crate::tree::ensure_workspace(&state, 3);
        assert_eq!(client.queued_out_bytes().len(), before, "stopped means silent");
    }

    #[test]
    fn a_destroyed_handle_never_returns() {
        let (state, client) = test_client();
        crate::tree::ensure_workspace(&state, 0);
        let mgr = bind_mgr(&client, 60);
        let h = mgr.workspaces.borrow()[0].clone();
        h.destroy(workspace_v1::destroy::Request {}).unwrap();
        assert!(client.objects.get(h.id).is_none());
        let before = client.queued_out_bytes().len();
        changed(&state);
        let bytes = client.queued_out_bytes();
        assert_eq!(
            count_events(&bytes[before..], mgr.id, manager_v1::workspace::OPCODE),
            0,
            "the zombie listing blocks re-announcement"
        );
    }
}
