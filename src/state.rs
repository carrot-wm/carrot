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
    /// minted activation tokens by string, with mint time; single use
    pub activation_tokens: RefCell<std::collections::HashMap<String, u64>>,
    pub active_ws: std::cell::Cell<usize>,
    /// which output has focus; follows the pointer and workspace switches
    pub focused_output: std::cell::Cell<usize>,
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
    /// foreign-toplevel watchers (taskbars/overviews), announce fan-out
    pub ftl_managers: RefCell<Vec<Rc<crate::protocol::foreign_toplevel::FtlManager>>>,
    /// ext-foreign-toplevel-list watchers; same fan-out, fewer verbs
    pub ext_toplevel_lists:
        RefCell<Vec<Rc<crate::protocol::foreign_toplevel_list::ExtToplevelList>>>,
    /// live image-copy-capture sessions, serviced from present/commit
    pub icc_sessions: RefCell<Vec<Rc<crate::protocol::image_copy_capture::IccSession>>>,
    /// live portal screencasts, fed from the present tail
    pub casts: RefCell<Vec<Rc<crate::portal::cast::Cast>>>,
    /// the last config-loaded ipc event, replayed to new subscribers
    pub last_config_event: RefCell<Option<String>>,
    /// a hidden-cast source committed; the cast tick task drains it
    pub cast_kick: AsyncEvent,
    /// the tick task, spawned with the first cast
    pub cast_tick: std::cell::Cell<Option<SpawnedFuture<()>>>,
    /// a screencast consent click waiting on the seat
    pub cast_pick: RefCell<Option<Rc<crate::portal::PendingPick>>>,
    /// idle notifications + inhibitors; the pump task ticks deadlines
    pub idle: crate::protocol::idle::IdleState,
    /// the session lock; survives its client (a dead locker must not unlock)
    pub lock: RefCell<Option<Rc<crate::protocol::session_lock::SessionLock>>>,
    /// heads are dark (dpms); any input wakes them
    pub dpms_off: std::cell::Cell<bool>,
    /// replaced dmabuf attachments, parked until every frame that was in
    /// flight at replacement time has fenced out. a global "wait for
    /// idle" here starved clients whose own gpu load kept a frame in
    /// flight permanently: no release ever, swapchain exhausted, viz
    /// thread blocked (the discord first-frame hang)
    pub retired: crate::util::RetireQueue<crate::protocol::shm::AttachedBuffer>,
    /// monotonic render-frame submission counter; retire watermarks
    pub frame_seq: std::cell::Cell<u64>,
    /// attachments whose buffer sits on a plane (direct scanout); they
    /// release when the buffer leaves it, not when render frames drain
    pub scanout_hold: RefCell<Vec<crate::protocol::shm::AttachedBuffer>>,
    /// buffer uids currently on (or queued to) a plane, one entry per output
    pub scanout_uids: RefCell<Vec<u64>>,
    /// surfaces drawn since the last callback sweep; the sweep drains this
    /// instead of walking every surface of every client each vblank
    pub shown_surfaces: RefCell<Vec<std::rc::Rc<crate::surface::WlSurface>>>,
    /// frames between render submit and fence; the retire park snapshot
    pub frames_in_flight: std::cell::Cell<u32>,
    /// the renderer imports sealed shm pools: those buffers sample in
    /// place and release like dmabufs instead of copying at commit
    pub host_import: std::cell::Cell<bool>,
    /// the render device + (fourcc, modifier) set the dmabuf global speaks
    /// for; filled when the display comes up
    pub dmabuf_info: RefCell<Option<crate::protocol::dmabuf::DmabufInfo>>,
    /// frozen at each output's predicted present; all sampling agrees on
    /// when the frame will glass
    pub anim_clock: crate::anim::AnimClock,
    /// gpu textures dropped by animations; compose drains these into the
    /// per-output retire queue so nothing dies mid-sample
    pub retire_tex: RefCell<Vec<crate::render::renderer::Texture>>,
    /// an interactive move/resize grab is live; targets track 1:1 and no
    /// animation may spawn or retarget from its relayouts
    pub grab_active: std::cell::Cell<bool>,
    /// children the spawn sites own; only these ever get reaped, other
    /// waiters (xwayland) keep their exit statuses
    pub reap_pids: RefCell<Vec<rustix::process::Pid>>,
    serial: NumCell<u64>,
    /// identity for cache keys: wire ids get reused, uids never do
    obj_uid: NumCell<u64>,
}

impl State {
    pub fn new(eng: &Rc<Engine>, ring: &Rc<Ring>, wheel: Wheel) -> Rc<State> {
        Rc::new(State {
            eng: eng.clone(),
            ring: ring.clone(),
            wheel,
            clients: Clients::default(),
            globals: Globals::default(),
            activation_tokens: RefCell::new(std::collections::HashMap::new()),
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
            focused_output: std::cell::Cell::new(0),
            configures: RefCell::new(Vec::new()),
            configure_event: AsyncEvent::default(),
            layers: RefCell::new(Vec::new()),
            usable: std::cell::Cell::new(crate::rect::Rect::default()),
            config: RefCell::new(Rc::new(crate::config::Config::default())),
            xwayland: RefCell::new(None),
            ipc_subs: RefCell::new(Vec::new()),
            ftl_managers: RefCell::new(Vec::new()),
            ext_toplevel_lists: RefCell::new(Vec::new()),
            icc_sessions: RefCell::new(Vec::new()),
            casts: RefCell::new(Vec::new()),
            last_config_event: RefCell::new(None),
            cast_kick: AsyncEvent::default(),
            cast_tick: std::cell::Cell::new(None),
            cast_pick: RefCell::new(None),
            idle: Default::default(),
            lock: RefCell::new(None),
            dpms_off: std::cell::Cell::new(false),
            retired: crate::util::RetireQueue::new(),
            frame_seq: std::cell::Cell::new(0),
            scanout_hold: RefCell::new(Vec::new()),
            scanout_uids: RefCell::new(Vec::new()),
            shown_surfaces: RefCell::new(Vec::new()),
            frames_in_flight: std::cell::Cell::new(0),
            host_import: std::cell::Cell::new(false),
            dmabuf_info: RefCell::new(None),
            anim_clock: crate::anim::AnimClock::new(),
            retire_tex: RefCell::new(Vec::new()),
            grab_active: std::cell::Cell::new(false),
            reap_pids: RefCell::new(Vec::new()),
            serial: NumCell::new(0),
            obj_uid: NumCell::new(0),
        })
    }

    pub fn next_uid(&self) -> u64 {
        self.obj_uid.fetch_add(1) + 1
    }

    pub fn next_serial(&self, client: Option<&Client>) -> u64 {
        let s = self.serial.fetch_add(1) + 1;
        if let Some(c) = client {
            c.track_serial(s);
        }
        s
    }

    /// break the Rc cycles so everything frees. called once, after ring stop.
    pub fn clear(&self) {
        self.clients.clear();
        self.slow_clients.clear();
        self.workspaces.borrow_mut().clear();
        self.configures.borrow_mut().clear();
        self.layers.borrow_mut().clear();
        self.ipc_subs.borrow_mut().clear();
        self.ext_toplevel_lists.borrow_mut().clear();
        self.ftl_managers.borrow_mut().clear();
        self.icc_sessions.borrow_mut().clear();
        self.casts.borrow_mut().clear();
        self.cast_tick.take();
        self.cast_pick.borrow_mut().take();
        self.lock.borrow_mut().take();
        self.idle.clear();
        self.retired.clear();
        self.wheel.clear();
        self.run_toplevel.clear();
        self.display.borrow_mut().take();
        self.input.borrow_mut().take();
        if let Some(seat) = self.seat.borrow_mut().take() {
            seat.data.clear();
            seat.primary.clear();
            seat.popup_grab.borrow_mut().clear();
            seat.grab_prev_focus.borrow_mut().take();
            seat.relative.borrow_mut().clear();
            seat.constraints.borrow_mut().clear();
            *seat.cursor.borrow_mut() = crate::input::seat::CursorState::Default;
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
