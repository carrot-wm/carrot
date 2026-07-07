// zwlr-foreign-toplevel-management v3: taskbars, docks and overviews see
// the window list and drive it without being the compositor. every
// property burst ends in exactly one done - clients latch on it.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwlr_foreign_toplevel_handle_v1 as handle_v1,
    zwlr_foreign_toplevel_manager_v1 as manager_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::tree::Window;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;
const FULLSCREEN_SINCE: u32 = 2;

// -- the global --

pub struct ForeignToplevelGlobal;

impl Global for ForeignToplevelGlobal {
    fn interface(&self) -> &'static str {
        manager_v1::NAME
    }

    fn version(&self) -> u32 {
        3
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let mgr = Rc::new(FtlManager {
            id,
            client: client.clone(),
            version,
            stopped: Cell::new(false),
            handles: RefCell::new(Vec::new()),
        });
        client.add_client_obj(mgr.clone())?;
        let state = &client.state;
        state.ftl_managers.borrow_mut().push(mgr.clone());
        // one announce burst per currently-mapped window
        let wins = all_windows(state);
        for win in wins {
            publish(state, &mgr, &win);
        }
        Ok(())
    }
}

fn all_windows(state: &Rc<State>) -> Vec<Rc<Window>> {
    let mut out = Vec::new();
    for ws in state.workspaces.borrow().iter() {
        ws.for_each(|w| out.push(w.clone()));
    }
    out
}

// -- the manager --

pub struct FtlManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    /// stop() halts announcements; live handles keep their events
    stopped: Cell<bool>,
    handles: RefCell<Vec<Rc<FtlHandle>>>,
}

impl manager_v1::Handler for FtlManager {
    fn stop(&self, _req: manager_v1::stop::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.stopped.set(true);
        self.client
            .event(|o| manager_v1::finished::send(o, self.id));
        // the object dies; the Rc stays in state so handles keep flowing
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for FtlManager {
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
        self.handles.borrow_mut().clear();
    }
}

// -- the handle --

pub struct FtlHandle {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    window: RefCell<Weak<Window>>,
    /// output slot last announced; diffed on moves
    out_slot: Cell<usize>,
}

impl FtlHandle {
    fn win(&self) -> Option<Rc<Window>> {
        self.window.borrow().upgrade()
    }

    fn is_for(&self, win: &Rc<Window>) -> bool {
        self.win().is_some_and(|w| Rc::ptr_eq(&w, win))
    }

    fn state_bytes(&self, state: &Rc<State>, win: &Rc<Window>) -> Vec<u8> {
        let focused = crate::tree::focused_window(state)
            .is_some_and(|f| Rc::ptr_eq(&f, win));
        let mut out = Vec::with_capacity(8);
        if focused {
            out.extend_from_slice(&STATE_ACTIVATED.to_ne_bytes());
        }
        if win.fullscreen.get() && self.version >= FULLSCREEN_SINCE {
            out.extend_from_slice(&STATE_FULLSCREEN.to_ne_bytes());
        }
        out
    }

    /// enter/leave against the client's wl_output binds, by connector name.
    /// output tracking arrives with the multi-output work; a taskbar reads
    /// title/app_id/state without it
    fn each_output(&self, _state: &Rc<State>, _slot: usize, _f: impl Fn(ObjectId, ObjectId)) {}

    fn send_full(&self, state: &Rc<State>, win: &Rc<Window>) {
        let title = win.title();
        let app_id = win.app_id();
        let bytes = self.state_bytes(state, win);
        self.client.event(|o| {
            handle_v1::app_id::send(o, self.id, &app_id);
            handle_v1::title::send(o, self.id, &title);
        });
        self.each_output(state, self.out_slot.get(), |hid, oid| {
            self.client
                .event(|o| handle_v1::output_enter::send(o, hid, oid));
        });
        self.client.event(|o| {
            handle_v1::state::send(o, self.id, &bytes);
            handle_v1::done::send(o, self.id);
        });
    }
}

fn out_slot_of(_state: &Rc<State>, _win: &Rc<Window>) -> usize {
    0
}

fn publish(state: &Rc<State>, mgr: &Rc<FtlManager>, win: &Rc<Window>) {
    if mgr.stopped.get() {
        return;
    }
    let id = mgr.client.objects.alloc_server_id();
    let h = Rc::new(FtlHandle {
        id,
        client: mgr.client.clone(),
        version: mgr.version,
        window: RefCell::new(Rc::downgrade(win)),
        out_slot: Cell::new(out_slot_of(state, win)),
    });
    mgr.client.add_server_obj(h.clone());
    mgr.handles.borrow_mut().push(h.clone());
    mgr.client
        .event(|o| manager_v1::toplevel::send(o, mgr.id, id));
    h.send_full(state, win);
}

impl handle_v1::Handler for FtlHandle {
    fn activate(&self, _req: handle_v1::activate::Request) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.client.state.clone();
        if let Some(win) = self.win() {
            crate::tree::focus_window(&state, Some(&win));
        }
        Ok(())
    }

    fn close(&self, _req: handle_v1::close::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(win) = self.win() {
            win.send_close();
        }
        Ok(())
    }

    fn set_fullscreen(
        &self,
        _req: handle_v1::set_fullscreen::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.client.state.clone();
        if let Some(win) = self.win() {
            crate::tree::set_fullscreen(&state, &win, true);
        }
        Ok(())
    }

    fn unset_fullscreen(
        &self,
        _req: handle_v1::unset_fullscreen::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.client.state.clone();
        if let Some(win) = self.win() {
            crate::tree::set_fullscreen(&state, &win, false);
        }
        Ok(())
    }

    // no minimize concept and tiling owns geometry; accepted no-ops
    fn set_maximized(&self, _req: handle_v1::set_maximized::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn unset_maximized(&self, _req: handle_v1::unset_maximized::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_minimized(&self, _req: handle_v1::set_minimized::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn unset_minimized(&self, _req: handle_v1::unset_minimized::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_rectangle(&self, _req: handle_v1::set_rectangle::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn destroy(&self, _req: handle_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        for mgr in state.ftl_managers.borrow().iter() {
            mgr.handles
                .borrow_mut()
                .retain(|h| !(h.id == self.id && h.client.id == self.client.id));
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for FtlHandle {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        handle_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        handle_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        *self.window.borrow_mut() = Weak::new();
    }
}

// -- fan-out, called from the tree/focus hooks --

fn for_window(state: &Rc<State>, win: &Rc<Window>, f: impl Fn(&Rc<FtlHandle>)) {
    let managers = state.ftl_managers.borrow().clone();
    for mgr in managers {
        let handles = mgr.handles.borrow().clone();
        for h in handles {
            if h.is_for(win) {
                f(&h);
            }
        }
    }
}

pub fn window_mapped(state: &Rc<State>, win: &Rc<Window>) {
    let managers = state.ftl_managers.borrow().clone();
    for mgr in managers {
        publish(state, &mgr, win);
    }
}

pub fn window_unmapped(state: &Rc<State>, win: &Rc<Window>) {
    let managers = state.ftl_managers.borrow().clone();
    for mgr in managers {
        let handles = mgr.handles.borrow().clone();
        for h in handles {
            if h.is_for(win) {
                h.client.event(|o| handle_v1::closed::send(o, h.id));
                *h.window.borrow_mut() = Weak::new();
            }
        }
        mgr.handles.borrow_mut().retain(|h| h.win().is_some());
    }
}

pub fn title_changed(state: &Rc<State>, win: &Rc<Window>) {
    let title = win.title();
    for_window(state, win, |h| {
        h.client.event(|o| {
            handle_v1::title::send(o, h.id, &title);
            handle_v1::done::send(o, h.id);
        });
    });
}

pub fn app_id_changed(state: &Rc<State>, win: &Rc<Window>) {
    let app_id = win.app_id();
    for_window(state, win, |h| {
        h.client.event(|o| {
            handle_v1::app_id::send(o, h.id, &app_id);
            handle_v1::done::send(o, h.id);
        });
    });
}

pub fn state_changed(state: &Rc<State>, win: &Rc<Window>) {
    for_window(state, win, |h| {
        let bytes = h.state_bytes(state, win);
        h.client.event(|o| {
            handle_v1::state::send(o, h.id, &bytes);
            handle_v1::done::send(o, h.id);
        });
    });
}

/// activation moved; both ends re-state
pub fn focus_changed(state: &Rc<State>, old: Option<Rc<Window>>, new: Option<Rc<Window>>) {
    let same = match (&old, &new) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    if let Some(w) = old {
        state_changed(state, &w);
    }
    if let Some(w) = new {
        state_changed(state, &w);
    }
}

pub fn output_changed(state: &Rc<State>, win: &Rc<Window>) {
    let new = out_slot_of(state, win);
    for_window(state, win, |h| {
        let old = h.out_slot.replace(new);
        if old == new {
            return;
        }
        h.each_output(state, old, |hid, oid| {
            h.client.event(|o| handle_v1::output_leave::send(o, hid, oid));
        });
        h.each_output(state, new, |hid, oid| {
            h.client.event(|o| handle_v1::output_enter::send(o, hid, oid));
        });
        h.client.event(|o| handle_v1::done::send(o, h.id));
    });
}

pub fn drop_client(state: &Rc<State>, id: ClientId) {
    state.ftl_managers.borrow_mut().retain(|m| m.client.id != id);
}
