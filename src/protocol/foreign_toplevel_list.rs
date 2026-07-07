// ext-foreign-toplevel-list v1: the window list without the management verbs.
// identifier goes out once per handle, every property burst ends in one done,
// a closed handle goes silent.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    ext_foreign_toplevel_handle_v1 as handle_v1,
    ext_foreign_toplevel_list_v1 as list_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use crate::tree::Window;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// random per-boot half of the identifier, so an ident from an earlier run
/// can't alias a live window
fn boot_token() -> u32 {
    use std::sync::OnceLock;
    static TOKEN: OnceLock<u32> = OnceLock::new();
    *TOKEN.get_or_init(|| {
        use std::io::Read;
        let mut b = [0u8; 4];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut b).is_ok() {
                return u32::from_ne_bytes(b);
            }
        }
        crate::util::Time::now().nsec() as u32
    })
}

/// 25 printable ascii bytes, stable per window, never reused
pub fn identifier(win: &Window) -> String {
    format!("{:08x}-{:016x}", boot_token(), win.ident)
}

// -- the global --

pub struct ForeignToplevelListGlobal;

impl Global for ForeignToplevelListGlobal {
    fn interface(&self) -> &'static str {
        list_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let list = Rc::new(ExtToplevelList {
            id,
            client: client.clone(),
            version,
            stopped: Cell::new(false),
            handles: RefCell::new(Vec::new()),
        });
        client.add_client_obj(list.clone())?;
        let state = &client.state;
        state.ext_toplevel_lists.borrow_mut().push(list.clone());
        // one announce burst per mapped window, flushed with the bind
        let wins = all_windows(state);
        for win in wins {
            publish(&list, &win);
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

// -- the list --

pub struct ExtToplevelList {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    /// no further toplevel events; the object itself waits for destroy
    stopped: Cell<bool>,
    handles: RefCell<Vec<Rc<ExtToplevelHandle>>>,
}

impl list_v1::Handler for ExtToplevelList {
    fn stop(&self, _req: list_v1::stop::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.stopped.set(true);
        self.client.event(|o| list_v1::finished::send(o, self.id));
        Ok(())
    }

    fn destroy(&self, _req: list_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // the Rc stays in state so the surviving handles keep their events
        self.stopped.set(true);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ExtToplevelList {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        list_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        list_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.handles.borrow_mut().clear();
    }
}

// -- the handle --

pub struct ExtToplevelHandle {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    window: RefCell<Weak<Window>>,
}

impl ExtToplevelHandle {
    fn win(&self) -> Option<Rc<Window>> {
        self.window.borrow().upgrade()
    }

    fn is_for(&self, win: &Rc<Window>) -> bool {
        self.win().is_some_and(|w| Rc::ptr_eq(&w, win))
    }

    pub fn window(&self) -> Weak<Window> {
        self.window.borrow().clone()
    }
}

fn publish(list: &Rc<ExtToplevelList>, win: &Rc<Window>) {
    if list.stopped.get() {
        return;
    }
    let id = list.client.objects.alloc_server_id();
    let h = Rc::new(ExtToplevelHandle {
        id,
        client: list.client.clone(),
        version: list.version,
        window: RefCell::new(Rc::downgrade(win)),
    });
    list.client.add_server_obj(h.clone());
    list.handles.borrow_mut().push(h.clone());
    let ident = identifier(win);
    let title = win.title();
    let app_id = win.app_id();
    let lid = list.id;
    list.client.event(|o| {
        list_v1::toplevel::send(o, lid, id);
        handle_v1::identifier::send(o, id, &ident);
        // empty strings are still definite initial state
        handle_v1::title::send(o, id, &title);
        handle_v1::app_id::send(o, id, &app_id);
        handle_v1::done::send(o, id);
    });
}

impl handle_v1::Handler for ExtToplevelHandle {
    fn destroy(&self, _req: handle_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        let state = &self.client.state;
        for list in state.ext_toplevel_lists.borrow().iter() {
            list.handles
                .borrow_mut()
                .retain(|h| !(h.id == self.id && h.client.id == self.client.id));
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ExtToplevelHandle {
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

/// by-id lookup for the toplevel image-capture source factory
pub(crate) fn find_handle(
    state: &Rc<State>,
    client: ClientId,
    id: ObjectId,
) -> Option<Rc<ExtToplevelHandle>> {
    let lists = state.ext_toplevel_lists.borrow().clone();
    for list in lists {
        if list.client.id != client {
            continue;
        }
        let found = list
            .handles
            .borrow()
            .iter()
            .find(|h| h.id == id)
            .cloned();
        if found.is_some() {
            return found;
        }
    }
    None
}

// -- fan-out --

fn for_window(state: &Rc<State>, win: &Rc<Window>, f: impl Fn(&Rc<ExtToplevelHandle>)) {
    let lists = state.ext_toplevel_lists.borrow().clone();
    for list in lists {
        let handles = list.handles.borrow().clone();
        for h in handles {
            if h.is_for(win) {
                f(&h);
            }
        }
    }
}

pub fn window_mapped(state: &Rc<State>, win: &Rc<Window>) {
    let lists = state.ext_toplevel_lists.borrow().clone();
    for list in lists {
        publish(&list, win);
    }
}

pub fn window_unmapped(state: &Rc<State>, win: &Rc<Window>) {
    let lists = state.ext_toplevel_lists.borrow().clone();
    for list in lists {
        let handles = list.handles.borrow().clone();
        for h in handles {
            if h.is_for(win) {
                // closed, then silence: no done follows
                h.client.event(|o| handle_v1::closed::send(o, h.id));
                *h.window.borrow_mut() = Weak::new();
            }
        }
        list.handles.borrow_mut().retain(|h| h.win().is_some());
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

pub fn drop_client(state: &Rc<State>, id: ClientId) {
    state
        .ext_toplevel_lists
        .borrow_mut()
        .retain(|l| l.client.id != id);
}

