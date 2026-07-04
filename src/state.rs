// compositor-wide state - the single Rc root every subsystem hangs off.

use crate::client::{Client, Clients};
use crate::engine::{Engine, SpawnedFuture, Wheel};
use std::cell::RefCell;
use crate::protocol::globals::Globals;
use crate::uring::Ring;
use crate::util::{AsyncEvent, AsyncQueue, NumCell};
use std::rc::Rc;

pub struct State {
    pub eng: Rc<Engine>,
    pub ring: Rc<Ring>,
    pub wheel: Wheel,
    pub clients: Clients,
    pub globals: Globals,
    pub run_toplevel: RunToplevel,
    // clients whose event queue grew past the limit; a policing task
    // double-checks after a yield and kills the ones still behind
    pub slow_clients: AsyncQueue<Rc<Client>>,
    // something visible changed; the present loop wakes on this
    pub damage: AsyncEvent,
    // active output dimensions; pointer clamping reads this
    pub output_size: std::cell::Cell<(u32, u32)>,
    pub workspaces: RefCell<Vec<Rc<crate::tree::workspace::Workspace>>>,
    pub active_ws: std::cell::Cell<usize>,
    // xdg surfaces with a scheduled configure; drained by an engine task
    pub configures: RefCell<Vec<Rc<crate::shell::xdg::XdgSurface>>>,
    pub configure_event: AsyncEvent,
    serial: NumCell<u64>,
}

impl State {
    pub fn new(eng: &Rc<Engine>, ring: &Rc<Ring>, wheel: Wheel) -> Rc<State> {
        Rc::new(State {
            eng: eng.clone(),
            ring: ring.clone(),
            wheel,
            clients: Clients::default(),
            globals: Globals::default(),
            run_toplevel: RunToplevel::install(eng),
            slow_clients: AsyncQueue::default(),
            damage: AsyncEvent::default(),
            output_size: std::cell::Cell::new((0, 0)),
            workspaces: RefCell::new(Vec::new()),
            active_ws: std::cell::Cell::new(0),
            configures: RefCell::new(Vec::new()),
            configure_event: AsyncEvent::default(),
            serial: NumCell::new(0),
        })
    }

    pub fn next_serial(&self, client: Option<&Client>) -> u64 {
        let s = self.serial.fetch_add(1) + 1;
        if let Some(c) = client {
            c.track_serial(s);
        }
        s
    }

    pub fn clear(&self) {
        self.clients.clear();
        self.slow_clients.clear();
        self.workspaces.borrow_mut().clear();
        self.configures.borrow_mut().clear();
        self.wheel.clear();
        self.run_toplevel.clear();
    }
}

// -- deferred closures --

// destructive operations (killing a client from its own send task, tree
// mutation from a late phase) bounce through here so they run from a
// fresh EventHandling task instead of whatever phase noticed the problem
pub struct RunToplevel {
    queue: Rc<AsyncQueue<Box<dyn FnOnce()>>>,
    _task: SpawnedFuture<()>,
}

impl RunToplevel {
    fn install(eng: &Rc<Engine>) -> RunToplevel {
        let queue: Rc<AsyncQueue<Box<dyn FnOnce()>>> = Rc::new(AsyncQueue::default());
        let q = queue.clone();
        let task = eng.spawn("run toplevel", async move {
            loop {
                let f = q.pop().await;
                f();
            }
        });
        RunToplevel {
            queue,
            _task: task,
        }
    }

    pub fn schedule(&self, f: impl FnOnce() + 'static) {
        self.queue.push(Box::new(f));
    }

    // queued closures capture Rc<State>; drop them or clear() leaves the
    // root cycle intact
    pub fn clear(&self) {
        self.queue.clear();
    }
}
