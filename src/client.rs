// per-client state. one Rc<Client> per connection; window keys are always
// (surface_id, client). server-side ids start at 0xff000000; protocol errors
// kill the client through wl_display.error, never a silent drop.

mod buffers;
mod objects;
mod tasks;

pub use objects::{Object, Objects};

use crate::protocol::display::WlDisplay;
use crate::protocol::interfaces::{IMPLEMENTATION, INVALID_METHOD, INVALID_OBJECT, wl_display};
use crate::protocol::wire::{EventOut, MAX_MESSAGE};
use crate::protocol::{DispatchError, ObjectId, WL_DISPLAY_ID};
use crate::state::State;
use crate::uring::RingError;
use crate::util::{AsyncEvent, NumCell};
use buffers::OutSwapchain;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::os::fd::OwnedFd;
use std::rc::Rc;

// -- errors --

#[derive(Debug)]
pub enum ClientError {
    Io(RingError),
    Closed,
    Timeout,
    UnalignedMessage,
    MessageTooLarge,
    MessageTooSmall,
    TooManyFds,
    CmsgTruncated,
    UnknownObject(ObjectId),
    IdAlreadyInUse(ObjectId),
    ClientIdOutOfBounds(ObjectId),
    Dispatch(&'static str, ObjectId, DispatchError),
}

impl ClientError {
    pub fn peer_closed(&self) -> bool {
        matches!(self, ClientError::Closed)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io error: {e}"),
            ClientError::Closed => write!(f, "the peer closed the connection"),
            ClientError::Timeout => write!(f, "the peer stopped reading"),
            ClientError::UnalignedMessage => write!(f, "message size is not a multiple of 4"),
            ClientError::MessageTooLarge => write!(f, "message exceeds the wire limit"),
            ClientError::MessageTooSmall => write!(f, "message smaller than its header"),
            ClientError::TooManyFds => write!(f, "too many queued fds"),
            ClientError::CmsgTruncated => write!(f, "control data truncated, fds were lost"),
            ClientError::UnknownObject(id) => write!(f, "object {id} does not exist"),
            ClientError::IdAlreadyInUse(id) => write!(f, "object id {id} is already in use"),
            ClientError::ClientIdOutOfBounds(id) => {
                write!(f, "object id {id} is outside the client range")
            }
            ClientError::Dispatch(iface, id, e) => write!(f, "{iface}{id}: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// -- the registry --

#[derive(Default)]
pub struct Clients {
    next: NumCell<u64>,
    active: RefCell<HashMap<ClientId, ClientHolder>>,
    /// shut-down clients drain their goodbye before the holder drops
    draining: RefCell<HashMap<ClientId, ClientHolder>>,
}

impl Clients {
    pub fn spawn(&self, state: &Rc<State>, socket: OwnedFd) -> Option<Rc<Client>> {
        // a peer that died mid-handshake just goes away
        let Ok(cred) = rustix::net::sockopt::socket_peercred(&socket) else {
            return None;
        };
        let id = ClientId(self.next.fetch_add(1) + 1);
        let client = Rc::new(Client {
            id,
            state: state.clone(),
            socket: Rc::new(socket),
            pid: cred.pid.as_raw_nonzero().get(),
            uid: cred.uid.as_raw(),
            objects: Objects::default(),
            swapchain: RefCell::new(OutSwapchain::default()),
            flush_request: AsyncEvent::default(),
            shutdown: AsyncEvent::default(),
            serials: RefCell::new(VecDeque::new()),
            checking_queue: Cell::new(false),
            is_xwayland: Cell::new(false),
        });
        client
            .objects
            .add_client_object(Rc::new(WlDisplay::new(&client)))
            .expect("display slot taken in a fresh table");
        let task = state.eng.spawn("client", tasks::client(client.clone()));
        self.active.borrow_mut().insert(
            id,
            ClientHolder {
                data: client,
                _task: task,
            },
        );
        crate::trace!("client {} connected", id);
        Some(self.active.borrow().get(&id).unwrap().data.clone())
    }

    pub fn for_each(&self, mut f: impl FnMut(&Rc<Client>)) {
        for holder in self.active.borrow().values() {
            f(&holder.data);
        }
    }

    /// teardown is the holder drop; a second kill is a no-op
    pub fn kill(&self, id: ClientId) {
        if self.active.borrow_mut().remove(&id).is_none() {
            self.draining.borrow_mut().remove(&id);
        }
    }

    pub fn shutdown(&self, id: ClientId) {
        if let Some(h) = self.active.borrow_mut().remove(&id) {
            h.data.shutdown.trigger();
            h.data.flush_request.trigger();
            self.draining.borrow_mut().insert(id, h);
        }
    }

    #[allow(dead_code)]
    pub fn broadcast(&self, f: impl Fn(&Rc<Client>)) {
        let clients: Vec<_> = self.active.borrow().values().map(|h| h.data.clone()).collect();
        for c in &clients {
            f(c);
        }
    }

    pub fn clear(&self) {
        self.active.borrow_mut().clear();
        self.draining.borrow_mut().clear();
    }
}

struct ClientHolder {
    data: Rc<Client>,
    _task: SpawnedFuture<()>,
}

use crate::engine::SpawnedFuture;

impl Drop for ClientHolder {
    fn drop(&mut self) {
        self.data.objects.destroy();
        // seat state can outlive every wl_seat object, so the client's
        // devices and sources go here, not in a break_loops
        if let Some(seat) = self.data.state.seat.borrow().clone() {
            seat.drop_client(self.data.id);
        }
        // ditto for layer surfaces: their exclusive zones must be handed
        // back even though the client never said destroy
        crate::shell::layer::drop_client(&self.data.state, self.data.id);
        crate::protocol::foreign_toplevel::drop_client(&self.data.state, self.data.id);
        self.data.state.idle.drop_client(self.data.id);
        self.data.flush_request.clear();
        self.data.shutdown.clear();
    }
}

// -- the client --

pub struct Client {
    pub id: ClientId,
    pub state: Rc<State>,
    pub socket: Rc<OwnedFd>,
    pub pid: i32,
    pub uid: u32,
    pub objects: Objects,
    /// the pre-connected xwayland peer; sole binder of xwayland_shell_v1
    pub is_xwayland: Cell<bool>,
    swapchain: RefCell<OutSwapchain>,
    flush_request: AsyncEvent,
    shutdown: AsyncEvent,
    serials: RefCell<VecDeque<SerialRange>>,
    checking_queue: Cell<bool>,
}

const MAX_SERIAL_RANGES: usize = 64;

struct SerialRange {
    lo: u64,
    hi: u64,
}

impl Client {
    /// append one event; the send task flushes once per engine iteration,
    /// after layout settles.
    pub fn event(self: &Rc<Self>, f: impl FnOnce(&mut EventOut)) {
        let mut sw = self.swapchain.borrow_mut();
        sw.record(f);
        let slow = sw.exceeds_limit();
        drop(sw);
        if slow && !self.checking_queue.replace(true) {
            self.state.slow_clients.push(self.clone());
        }
        self.flush_request.trigger();
    }

    /// recheck after a yield: transient bursts pass, stalled readers get killed
    pub async fn check_queue_size(self: Rc<Self>) {
        if self.swapchain.borrow().exceeds_limit() {
            self.state.eng.yield_now().await;
            if self.swapchain.borrow().exceeds_limit() {
                crate::trace!("client {} too slow fetching events", self.id);
                self.state.clients.kill(self.id);
                return;
            }
        }
        self.checking_queue.set(false);
    }

    pub fn add_client_obj(&self, obj: Rc<dyn Object>) -> Result<(), ClientError> {
        self.objects.add_client_object(obj)
    }

    #[allow(dead_code)]
    pub fn add_server_obj(&self, obj: Rc<dyn Object>) {
        self.objects.add_server_object(obj)
    }

    /// client-range ids get delete_id; server-range ids only free their slot
    pub fn remove_obj(self: &Rc<Self>, id: ObjectId) -> Result<(), ClientError> {
        self.objects.remove(id)?;
        if !id.is_server() {
            self.event(|o| wl_display::delete_id::send(o, WL_DISPLAY_ID, id.0));
        }
        Ok(())
    }

    // -- protocol errors: loud, then a graceful shutdown --

    pub fn protocol_error(self: &Rc<Self>, object: ObjectId, code: u32, msg: &str) {
        crate::trace!("client {}: protocol error on {}: {}", self.id, object, msg);
        // msg often quotes client input; clamp so the error event can't bust
        // the wire limit (header/object/code/len/nul/padding need 32 bytes)
        let mut msg = msg;
        const CAP: usize = MAX_MESSAGE - 32;
        if msg.len() > CAP {
            let mut end = CAP;
            while !msg.is_char_boundary(end) {
                end -= 1;
            }
            msg = &msg[..end];
        }
        self.event(|o| wl_display::error::send(o, WL_DISPLAY_ID, object, code, msg));
        self.state.clients.shutdown(self.id);
    }

    pub fn invalid_object(self: &Rc<Self>, id: ObjectId) {
        self.protocol_error(id, INVALID_OBJECT, &format!("object {id} does not exist"));
    }

    pub fn invalid_request(self: &Rc<Self>, obj: &dyn Object, opcode: u32) {
        let msg = format!("object {} has no request {}", obj.id(), opcode);
        self.protocol_error(obj.id(), INVALID_METHOD, &msg);
    }

    pub fn implementation_error(self: &Rc<Self>, msg: &str) {
        self.protocol_error(WL_DISPLAY_ID, IMPLEMENTATION, msg);
    }

    // -- serials --

    pub fn track_serial(&self, s: u64) {
        let mut ranges = self.serials.borrow_mut();
        if let Some(last) = ranges.back_mut() {
            if last.hi + 1 == s {
                last.hi = s;
                return;
            }
        }
        if ranges.len() == MAX_SERIAL_RANGES {
            ranges.pop_front();
        }
        ranges.push_back(SerialRange { lo: s, hi: s });
    }

    /// rebuild the 64-bit serial from 32 wire bits; verify we issued it
    #[allow(dead_code)]
    pub fn map_serial(&self, wire: u32) -> Option<u64> {
        let ranges = self.serials.borrow();
        let last = ranges.back()?;
        let mut v = (last.hi & !0xffff_ffff) | wire as u64;
        if v > last.hi {
            v = v.checked_sub(1 << 32)?;
        }
        for r in ranges.iter().rev() {
            if v > r.hi {
                return None;
            }
            if v >= r.lo {
                return Some(v);
            }
        }
        // older than the whole window - only plausible if history was truncated
        if ranges.len() == MAX_SERIAL_RANGES { Some(v) } else { None }
    }
}

// -- test scaffolding shared across module suites --

#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;
    use crate::engine::{Engine, Wheel};
    use crate::state::State;
    use crate::uring::Ring;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    /// full client over a real socketpair; nothing flushes without a ring loop,
    /// so queued events can be inspected in place
    pub(crate) fn test_client() -> (Rc<State>, Rc<Client>) {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let wheel = Wheel::new(&eng, &ring).unwrap();
        let state = State::new(&eng, &ring, wheel);
        let (a, _b) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        state.clients.spawn(&state, a);
        let client = state.clients.any();
        (state, client)
    }

    pub(crate) fn count_events(bytes: &[u8], object: ObjectId, opcode: u32) -> usize {
        let mut n = 0;
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let obj = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
            let w2 = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            let len = ((w2 >> 16) as usize).max(8);
            if obj == object.0 && w2 & 0xffff == opcode {
                n += 1;
            }
            off += len;
        }
        n
    }
}

#[cfg(test)]
impl Clients {
    pub(crate) fn any(&self) -> Rc<Client> {
        self.active
            .borrow()
            .values()
            .next()
            .expect("no active client")
            .data
            .clone()
    }
}

#[cfg(test)]
impl Client {
    /// committed buffers plus the current one
    pub(crate) fn queued_out_bytes(&self) -> Vec<u8> {
        self.swapchain.borrow().all_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, Wheel};
    use crate::protocol::globals::Global;
    use crate::uring::Ring;
    use crate::util::Time;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct TestGlobal;

    impl Global for TestGlobal {
        fn interface(&self) -> &'static str {
            "carrot_test"
        }

        fn version(&self) -> u32 {
            4
        }

        fn bind(&self, _c: &Rc<Client>, _id: ObjectId, _v: u32) -> Result<(), ClientError> {
            Ok(())
        }
    }

    fn w(bytes: &[u8], word: usize) -> u32 {
        u32::from_ne_bytes(bytes[word * 4..word * 4 + 4].try_into().unwrap())
    }

    #[test]
    fn registry_and_sync_roundtrip() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let wheel = Wheel::new(&eng, &ring).unwrap();
        let state = crate::state::State::new(&eng, &ring, wheel);
        state.globals.add(Rc::new(TestGlobal));

        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let peer = std::thread::spawn(move || {
            let mut s = UnixStream::from(theirs);
            // wl_display.get_registry(registry: 2), then wl_display.sync(callback: 3)
            let mut msg = Vec::new();
            msg.extend(1u32.to_ne_bytes());
            msg.extend(((12u32 << 16) | 1).to_ne_bytes());
            msg.extend(2u32.to_ne_bytes());
            msg.extend(1u32.to_ne_bytes());
            msg.extend((12u32 << 16).to_ne_bytes());
            msg.extend(3u32.to_ne_bytes());
            s.write_all(&msg).unwrap();
            // expect registry.global (32 bytes), callback.done (12), delete_id (12)
            let mut all = Vec::new();
            let mut tmp = [0u8; 256];
            while all.len() < 56 {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                all.extend(&tmp[..n]);
            }
            assert_eq!(all.len(), 56);
            // registry.global { name: 1, "carrot_test", version: 4 }
            assert_eq!(w(&all, 0), 2);
            assert_eq!(w(&all, 1), (32 << 16) | 0);
            assert_eq!(w(&all, 2), 1);
            assert_eq!(w(&all, 3), 12);
            assert_eq!(&all[16..28], b"carrot_test\0");
            assert_eq!(w(&all, 7), 4);
            // wl_callback.done(0) on id 3
            assert_eq!(w(&all, 8), 3);
            assert_eq!(w(&all, 9), (12 << 16) | 0);
            assert_eq!(w(&all, 10), 0);
            // wl_display.delete_id(3)
            assert_eq!(w(&all, 11), 1);
            assert_eq!(w(&all, 12), (12 << 16) | 1);
            assert_eq!(w(&all, 13), 3);
            d.store(true, Ordering::SeqCst);
        });

        state.clients.spawn(&state, ours);
        let flag = done.clone();
        let r = ring.clone();
        let _watch = eng.spawn("watch", async move {
            for _ in 0..600 {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let _ = r.timeout(Time::now() + Duration::from_millis(5)).await;
            }
            r.stop();
        });
        ring.run().unwrap();
        peer.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        state.clear();
        eng.clear();
    }
}
