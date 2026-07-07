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
    /// clients over the queue limit; a policing task rechecks after a yield
    /// and kills the ones still behind
    pub slow_clients: AsyncQueue<Rc<Client>>,
    /// something visible changed; the present loop wakes on this
    pub damage: AsyncEvent,
    /// populated by the bring-up task once logind/display are up
    pub session: RefCell<Option<Rc<crate::dbus::LogindSession>>>,
    pub display: RefCell<Option<crate::output::Display>>,
    pub input: RefCell<Option<crate::input::InputStack>>,
    pub seat: RefCell<Option<Rc<crate::input::seat::SeatGlobal>>>,
    /// active output dimensions; pointer clamping reads this
    pub output_size: std::cell::Cell<(u32, u32)>,
    pub workspaces: RefCell<Vec<Rc<crate::tree::workspace::Workspace>>>,
    pub active_ws: std::cell::Cell<usize>,
    /// shell surfaces with a scheduled configure; drained by an engine task
    pub configures: RefCell<Vec<Rc<dyn crate::shell::Configurable>>>,
    pub configure_event: AsyncEvent,
    /// layer surfaces in mapping order, all four layers together
    pub layers: RefCell<Vec<Rc<crate::shell::layer::LayerSurface>>>,
    /// the output rect minus exclusive zones; the tiling root box
    pub usable: std::cell::Cell<crate::rect::Rect>,
    /// swapped whole on reload; readers grab an Rc and never hold it
    pub config: RefCell<Rc<crate::config::Config>>,
    pub xwayland: RefCell<Option<Rc<crate::xwayland::Xwayland>>>,
    /// ipc connections that asked for the event stream
    pub ipc_subs: RefCell<Vec<Rc<crate::ipc::Subscriber>>>,
    serial: NumCell<u64>,
    /// identity for cache keys: wire ids get reused, uids never do
    obj_uid: NumCell<u64>,
    /// dmabuf attachments sampled in place; released once the frame does
    pub retired: RefCell<Vec<crate::protocol::shm::AttachedBuffer>>,
    /// advertised drm formats + modifiers, filled once the renderer is up
    pub dmabuf_info: RefCell<Option<crate::protocol::dmabuf::DmabufInfo>>,
    pub ftl_managers: RefCell<Vec<Rc<crate::protocol::foreign_toplevel::FtlManager>>>,
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
            session: RefCell::new(None),
            display: RefCell::new(None),
            input: RefCell::new(None),
            seat: RefCell::new(None),
            output_size: std::cell::Cell::new((0, 0)),
            workspaces: RefCell::new(Vec::new()),
            active_ws: std::cell::Cell::new(0),
            configures: RefCell::new(Vec::new()),
            configure_event: AsyncEvent::default(),
            layers: RefCell::new(Vec::new()),
            usable: std::cell::Cell::new(crate::rect::Rect::default()),
            config: RefCell::new(Rc::new(crate::config::Config::default())),
            xwayland: RefCell::new(None),
            ipc_subs: RefCell::new(Vec::new()),
            serial: NumCell::new(0),
            obj_uid: NumCell::new(0),
            retired: RefCell::new(Vec::new()),
            dmabuf_info: RefCell::new(None),
            ftl_managers: RefCell::new(Vec::new()),
        })
    }

    pub fn next_serial(&self, client: Option<&Client>) -> u64 {
        let s = self.serial.fetch_add(1) + 1;
        if let Some(c) = client {
            c.track_serial(s);
        }
        s
    }

    pub fn next_uid(&self) -> u64 {
        self.obj_uid.fetch_add(1) + 1
    }

    /// break the Rc cycles so everything frees. called once, after ring stop.
    pub fn clear(&self) {
        self.clients.clear();
        self.slow_clients.clear();
        self.workspaces.borrow_mut().clear();
        self.configures.borrow_mut().clear();
        self.layers.borrow_mut().clear();
        self.ipc_subs.borrow_mut().clear();
        self.retired.borrow_mut().clear();
        self.ftl_managers.borrow_mut().clear();
        self.wheel.clear();
        self.run_toplevel.clear();
        self.display.borrow_mut().take();
        self.input.borrow_mut().take();
        if let Some(seat) = self.seat.borrow_mut().take() {
            seat.data.clear();
            seat.primary.clear();
            seat.popup_grab.borrow_mut().clear();
            seat.grab_prev_focus.borrow_mut().take();
        }
        if let Some(x) = self.xwayland.borrow_mut().take() {
            x.clear();
        }
        if let Some(s) = self.session.borrow_mut().take() {
            s.clear();
        }
    }
}

// -- deferred closures --

/// destructive ops (killing a client from its own send task, tree mutation
/// from a late phase) bounce through here to run from a fresh EventHandling
/// task instead of whatever phase noticed the problem
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

    /// queued closures capture Rc<State>; must drop them or the root cycle leaks
    pub fn clear(&self) {
        self.queue.clear();
    }
}
