// eis - the server side of the ei protocol, so xtest clients (bridged
// through xwayland) and remote-desktop senders can inject input. sender
// contexts only: receivers finish the handshake but are never offered a
// seat. injected events run the same seat entry points as evdev, one
// connection task per client like the ipc socket.

mod wire;

use crate::engine::SpawnedFuture;
use crate::input::seat::{KeyAction, SeatGlobal};
use crate::state::State;
use wire::MsgBuilder;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;

/// ids below this belong to the client; ours count up from here
const MIN_SERVER_ID: u64 = 0xff00_0000_0000_0000;

const CAP_POINTER: u64 = 1 << 0;
const CAP_POINTER_ABS: u64 = 1 << 1;
const CAP_SCROLL: u64 = 1 << 2;
const CAP_BUTTON: u64 = 1 << 3;
const CAP_KEYBOARD: u64 = 1 << 4;

const CONTEXT_RECEIVER: u32 = 1;
const CONTEXT_SENDER: u32 = 2;
const DEVICE_VIRTUAL: u32 = 1;
const KEYMAP_XKB: u32 = 1;
const REASON_PROTOCOL: u32 = 3;

// -- the socket --

pub struct EiSocket {
    path: PathBuf,
    fd: OwnedFd,
}

/// bind $XDG_RUNTIME_DIR/eis-0 and advertise it. call before any worker
/// thread exists: the set_var is only sound single-threaded
pub fn bind_socket() -> Result<EiSocket, String> {
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, listen, socket_with,
    };
    let xrd = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "no XDG_RUNTIME_DIR".to_string())?;
    let path = PathBuf::from(xrd).join("eis-0");
    let _ = std::fs::remove_file(&path);
    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("ei socket: {e}"))?;
    let addr = SocketAddrUnix::new(&path).map_err(|e| format!("ei addr: {e}"))?;
    bind(&fd, &addr).map_err(|e| format!("ei bind {}: {e}", path.display()))?;
    listen(&fd, 8).map_err(|e| format!("ei listen: {e}"))?;
    // clients resolve the name relative to XDG_RUNTIME_DIR
    unsafe {
        std::env::set_var("LIBEI_SOCKET", "eis-0");
    }
    Ok(EiSocket { path, fd })
}

pub struct Ei {
    path: PathBuf,
    _accept: SpawnedFuture<()>,
    _conns: Rc<RefCell<HashMap<u64, SpawnedFuture<()>>>>,
}

impl Drop for Ei {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn start(state: &Rc<State>, sock: EiSocket) -> Ei {
    let listener = Rc::new(sock.fd);
    let conns: Rc<RefCell<HashMap<u64, SpawnedFuture<()>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let st = state.clone();
    let cs = conns.clone();
    let accept = state.eng.spawn("ei accept", async move {
        let mut next_id = 0u64;
        loop {
            match st.ring.accept(&listener).await {
                Ok(fd) => {
                    let id = next_id;
                    next_id += 1;
                    let st2 = st.clone();
                    let cs2 = cs.clone();
                    let task = st.eng.spawn("ei conn", async move {
                        conn(st2.clone(), Rc::new(fd)).await;
                        // drop our own entry from a fresh task
                        let cs3 = cs2.clone();
                        st2.run_toplevel.schedule(move || {
                            cs3.borrow_mut().remove(&id);
                        });
                    });
                    cs.borrow_mut().insert(id, task);
                }
                Err(e) => {
                    eprintln!("carrot: ei accept failed: {e}");
                    return;
                }
            }
        }
    });
    Ei { path: sock.path, _accept: accept, _conns: conns }
}

// -- the connection --

async fn conn(state: Rc<State>, fd: Rc<OwnedFd>) {
    let c = Conn::new(state.clone(), fd.clone());
    // server speaks first
    c.push(MsgBuilder::new(0, 0).u32(1));
    if c.flush().await.is_err() {
        return;
    }
    let mut pending = Vec::new();
    loop {
        let buf = vec![0u8; 4096];
        let Ok((buf, n)) = state.ring.read(&fd, buf).await else {
            break;
        };
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let open = c.drain(&mut pending);
        if c.flush().await.is_err() || !open {
            break;
        }
    }
    c.release_held();
}

#[derive(Clone, Copy, PartialEq)]
enum Iface {
    Handshake,
    Connection,
    Seat,
    Device,
    Pointer,
    PointerAbs,
    Button,
    Scroll,
    Keyboard,
}

/// the client's announced maximums; we cap everything at 1
#[derive(Default)]
struct Peer {
    connection: Cell<u32>,
    callback: Cell<u32>,
    seat: Cell<u32>,
    device: Cell<u32>,
    pointer: Cell<u32>,
    pointer_abs: Cell<u32>,
    button: Cell<u32>,
    scroll: Cell<u32>,
    keyboard: Cell<u32>,
}

impl Peer {
    fn set(&self, name: &str, v: u32) {
        match name {
            "ei_connection" => self.connection.set(v),
            "ei_callback" => self.callback.set(v),
            "ei_seat" => self.seat.set(v),
            "ei_device" => self.device.set(v),
            "ei_pointer" => self.pointer.set(v),
            "ei_pointer_absolute" => self.pointer_abs.set(v),
            "ei_button" => self.button.set(v),
            "ei_scroll" => self.scroll.set(v),
            "ei_keyboard" => self.keyboard.set(v),
            // touchscreen, pingpong, text: not offered
            _ => {}
        }
    }
}

struct Conn {
    state: Rc<State>,
    fd: Rc<OwnedFd>,
    out: RefCell<Vec<u8>>,
    out_fds: RefCell<Vec<Rc<OwnedFd>>>,
    objs: RefCell<HashMap<u64, Iface>>,
    next_id: Cell<u64>,
    serial: Cell<u32>,
    context: Cell<u32>,
    peer: Peer,
    conn_id: Cell<u64>,
    seat_id: Cell<u64>,
    device_id: Cell<u64>,
    /// capability mask advertised on the seat; bind must stay inside it
    caps: Cell<u64>,
    /// injected edges still down; a dead sender must not leave them pressed
    held_keys: RefCell<HashSet<u32>>,
    held_buttons: RefCell<HashSet<u32>>,
}

impl Conn {
    fn new(state: Rc<State>, fd: Rc<OwnedFd>) -> Conn {
        let mut objs = HashMap::new();
        objs.insert(0, Iface::Handshake);
        Conn {
            state,
            fd,
            out: RefCell::new(Vec::new()),
            out_fds: RefCell::new(Vec::new()),
            objs: RefCell::new(objs),
            next_id: Cell::new(MIN_SERVER_ID),
            serial: Cell::new(0),
            context: Cell::new(CONTEXT_RECEIVER),
            peer: Peer::default(),
            conn_id: Cell::new(0),
            seat_id: Cell::new(0),
            device_id: Cell::new(0),
            caps: Cell::new(0),
            held_keys: RefCell::new(HashSet::new()),
            held_buttons: RefCell::new(HashSet::new()),
        }
    }

    fn push(&self, msg: MsgBuilder) {
        self.out.borrow_mut().extend_from_slice(&msg.finish());
    }

    async fn flush(&self) -> Result<(), ()> {
        let mut buf = std::mem::take(&mut *self.out.borrow_mut());
        let mut fds = std::mem::take(&mut *self.out_fds.borrow_mut());
        let len = buf.len();
        let mut off = 0;
        while off < len {
            // fds ride the first send; a short write never splits them
            match self
                .state
                .ring
                .sendmsg(&self.fd, buf, (off, len), std::mem::take(&mut fds), None)
                .await
            {
                Ok((b, n)) if n > 0 => {
                    buf = b;
                    off += n;
                }
                _ => return Err(()),
            }
        }
        Ok(())
    }

    fn next_serial(&self) -> u32 {
        let s = self.serial.get().wrapping_add(1);
        self.serial.set(s);
        s
    }

    fn add(&self, iface: Iface) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.objs.borrow_mut().insert(id, iface);
        id
    }

    /// destroyed event, then forget the object
    fn destroyed(&self, id: u64) {
        let s = self.next_serial();
        self.push(MsgBuilder::new(id, 0).u32(s));
        self.objs.borrow_mut().remove(&id);
    }

    // -- dispatch --

    /// consume every complete message; false = close the connection
    fn drain(&self, pending: &mut Vec<u8>) -> bool {
        loop {
            let Some((object, len, opcode)) = wire::header(pending) else {
                return true;
            };
            if len < wire::HEADER || len % 4 != 0 || len > wire::MAX_MSG {
                return self.fail("bad message length");
            }
            if pending.len() < len {
                return true;
            }
            let msg: Vec<u8> = pending.drain(..len).collect();
            match self.handle(object, opcode, &msg[wire::HEADER..]) {
                Ok(open) => {
                    if !open {
                        return false;
                    }
                }
                Err(e) => return self.fail(e),
            }
        }
    }

    /// protocol violation: say why if a connection exists, then close
    fn fail(&self, why: &str) -> bool {
        if self.conn_id.get() != 0 {
            self.push(
                MsgBuilder::new(self.conn_id.get(), 0)
                    .u32(self.serial.get())
                    .u32(REASON_PROTOCOL)
                    .string(why),
            );
        }
        false
    }

    fn handle(&self, object: u64, opcode: u32, body: &[u8]) -> Result<bool, &'static str> {
        let iface = self.objs.borrow().get(&object).copied();
        let Some(iface) = iface else {
            // a request racing our destroyed event is survivable
            if self.conn_id.get() != 0 {
                self.push(
                    MsgBuilder::new(self.conn_id.get(), 2)
                        .u32(self.serial.get())
                        .u64(object),
                );
                return Ok(true);
            }
            return Err("unknown object");
        };
        let mut a = wire::Args::new(body);
        match iface {
            Iface::Handshake => self.handshake(opcode, &mut a),
            Iface::Connection => self.connection(opcode, &mut a),
            Iface::Seat => self.seat_req(object, opcode, &mut a),
            Iface::Device => self.device_req(opcode, &mut a),
            _ => self.inject(iface, object, opcode, &mut a),
        }
    }

    // -- handshake --

    fn handshake(&self, opcode: u32, a: &mut wire::Args) -> Result<bool, &'static str> {
        match opcode {
            0 => {
                if a.u32()? == 0 {
                    return Err("bad handshake version");
                }
            }
            1 => self.finish()?,
            2 => {
                let ctx = a.u32()?;
                if ctx != CONTEXT_RECEIVER && ctx != CONTEXT_SENDER {
                    return Err("bad context type");
                }
                self.context.set(ctx);
            }
            3 => {
                a.string()?;
            }
            4 => {
                let name = a.string()?;
                self.peer.set(name, a.u32()?);
            }
            _ => return Err("unknown handshake request"),
        }
        Ok(true)
    }

    fn finish(&self) -> Result<(), &'static str> {
        if self.peer.connection.get() == 0 || self.peer.callback.get() == 0 {
            return Err("ei_connection and ei_callback are required");
        }
        let shared = [
            ("ei_connection", self.peer.connection.get()),
            ("ei_callback", self.peer.callback.get()),
            ("ei_seat", self.peer.seat.get()),
            ("ei_device", self.peer.device.get()),
            ("ei_pointer", self.peer.pointer.get()),
            ("ei_pointer_absolute", self.peer.pointer_abs.get()),
            ("ei_button", self.peer.button.get()),
            ("ei_scroll", self.peer.scroll.get()),
            ("ei_keyboard", self.peer.keyboard.get()),
        ];
        for (name, v) in shared {
            if v > 0 {
                self.push(MsgBuilder::new(0, 1).string(name).u32(1));
            }
        }
        let conn = self.add(Iface::Connection);
        self.conn_id.set(conn);
        self.objs.borrow_mut().remove(&0);
        let serial = self.next_serial();
        self.push(MsgBuilder::new(0, 2).u32(serial).u64(conn).u32(1));
        // receivers get a connection and nothing more - we only inject
        if self.context.get() == CONTEXT_SENDER
            && self.peer.seat.get() > 0
            && self.peer.device.get() > 0
        {
            self.announce_seat();
        }
        Ok(())
    }

    fn announce_seat(&self) {
        let seat = self.add(Iface::Seat);
        self.seat_id.set(seat);
        self.push(MsgBuilder::new(self.conn_id.get(), 1).u64(seat).u32(1));
        self.push(MsgBuilder::new(seat, 1).string("default"));
        let table = [
            (CAP_POINTER, self.peer.pointer.get(), "ei_pointer"),
            (CAP_POINTER_ABS, self.peer.pointer_abs.get(), "ei_pointer_absolute"),
            (CAP_SCROLL, self.peer.scroll.get(), "ei_scroll"),
            (CAP_BUTTON, self.peer.button.get(), "ei_button"),
            (CAP_KEYBOARD, self.peer.keyboard.get(), "ei_keyboard"),
        ];
        let mut caps = 0;
        for (bit, v, name) in table {
            if v > 0 {
                caps |= bit;
                self.push(MsgBuilder::new(seat, 2).u64(bit).string(name));
            }
        }
        self.caps.set(caps);
        self.push(MsgBuilder::new(seat, 3));
    }

    // -- connection --

    fn connection(&self, opcode: u32, a: &mut wire::Args) -> Result<bool, &'static str> {
        match opcode {
            0 => {
                let cb = a.u64()?;
                a.u32()?;
                if cb == 0 || cb >= MIN_SERVER_ID {
                    return Err("sync callback id out of range");
                }
                // nothing is deferred, so done fires straight back
                self.push(MsgBuilder::new(cb, 0).u64(0));
                Ok(true)
            }
            1 => Ok(false),
            _ => Err("unknown connection request"),
        }
    }

    // -- seat and device --

    fn seat_req(&self, object: u64, opcode: u32, a: &mut wire::Args) -> Result<bool, &'static str> {
        match opcode {
            0 => {
                self.destroy_device();
                self.destroyed(object);
                self.seat_id.set(0);
                self.caps.set(0);
            }
            1 => {
                let caps = a.u64()?;
                if caps & !self.caps.get() != 0 {
                    return Err("bind outside the advertised capabilities");
                }
                self.destroy_device();
                if caps != 0 {
                    self.create_device(caps);
                }
            }
            _ => return Err("unknown seat request"),
        }
        Ok(true)
    }

    fn create_device(&self, mut caps: u64) {
        let (w, h) = self.state.output_size.get();
        // absolute motion needs a region to land in
        if w == 0 || h == 0 {
            caps &= !CAP_POINTER_ABS;
        }
        if caps == 0 {
            return;
        }
        let dev = self.add(Iface::Device);
        self.device_id.set(dev);
        self.push(MsgBuilder::new(self.seat_id.get(), 4).u64(dev).u32(1));
        self.push(MsgBuilder::new(dev, 2).u32(DEVICE_VIRTUAL));
        if caps & CAP_POINTER_ABS != 0 {
            self.push(MsgBuilder::new(dev, 4).u32(0).u32(0).u32(w).u32(h).f32(1.0));
        }
        let table = [
            (CAP_POINTER, Iface::Pointer, "ei_pointer"),
            (CAP_POINTER_ABS, Iface::PointerAbs, "ei_pointer_absolute"),
            (CAP_SCROLL, Iface::Scroll, "ei_scroll"),
            (CAP_BUTTON, Iface::Button, "ei_button"),
            (CAP_KEYBOARD, Iface::Keyboard, "ei_keyboard"),
        ];
        let mut kbd = 0;
        for (bit, iface, name) in table {
            if caps & bit != 0 {
                let id = self.add(iface);
                if iface == Iface::Keyboard {
                    kbd = id;
                }
                self.push(MsgBuilder::new(dev, 5).u64(id).string(name).u32(1));
            }
        }
        let seat = self.state.seat.borrow().clone();
        if kbd != 0 {
            if let Some(seat) = &seat {
                self.push(
                    MsgBuilder::new(kbd, 1)
                        .u32(KEYMAP_XKB)
                        .u32(seat.keymap.borrow().size),
                );
                self.out_fds.borrow_mut().push(seat.keymap.borrow().fd.clone());
            }
        }
        self.push(MsgBuilder::new(dev, 6));
        let serial = self.next_serial();
        self.push(MsgBuilder::new(dev, 7).u32(serial));
        if kbd != 0 {
            if let Some(seat) = &seat {
                let m = seat.mods.get();
                let serial = self.next_serial();
                self.push(
                    MsgBuilder::new(kbd, 3)
                        .u32(serial)
                        .u32(m.depressed)
                        .u32(m.locked)
                        .u32(m.latched)
                        .u32(m.group),
                );
            }
        }
    }

    fn destroy_device(&self) {
        if self.device_id.get() == 0 {
            return;
        }
        let subs: Vec<u64> = self
            .objs
            .borrow()
            .iter()
            .filter(|(_, i)| {
                matches!(
                    i,
                    Iface::Pointer
                        | Iface::PointerAbs
                        | Iface::Button
                        | Iface::Scroll
                        | Iface::Keyboard
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in subs {
            self.destroyed(id);
        }
        self.destroyed(self.device_id.get());
        self.device_id.set(0);
    }

    fn device_req(&self, opcode: u32, a: &mut wire::Args) -> Result<bool, &'static str> {
        match opcode {
            0 => self.destroy_device(),
            // start/stop_emulating carry no state we care about
            1 => {
                a.u32()?;
                a.u32()?;
            }
            2 => {
                a.u32()?;
            }
            3 => {
                a.u32()?;
                a.u64()?;
                if let Some(seat) = self.state.seat.borrow().clone() {
                    seat.pointer_frame();
                }
            }
            _ => return Err("unknown device request"),
        }
        Ok(true)
    }

    // -- injection --

    fn inject(
        &self,
        iface: Iface,
        object: u64,
        opcode: u32,
        a: &mut wire::Args,
    ) -> Result<bool, &'static str> {
        if opcode == 0 {
            self.destroyed(object);
            return Ok(true);
        }
        let now = crate::util::Time::now().nsec() / 1_000;
        let seat = self.state.seat.borrow().clone();
        match (iface, opcode) {
            (Iface::Pointer, 1) => {
                let dx = a.f32()? as f64;
                let dy = a.f32()? as f64;
                if let Some(seat) = &seat {
                    seat.pointer_motion(&self.state, now, dx, dy, dx, dy);
                    self.move_cursor(seat);
                }
            }
            (Iface::PointerAbs, 1) => {
                let x = a.f32()? as f64;
                let y = a.f32()? as f64;
                if let Some(seat) = &seat {
                    seat.warp(&self.state, x, y);
                    self.move_cursor(seat);
                }
            }
            (Iface::Button, 1) => {
                let button = a.u32()?;
                let pressed = a.u32()? != 0;
                if pressed {
                    self.held_buttons.borrow_mut().insert(button);
                } else {
                    self.held_buttons.borrow_mut().remove(&button);
                }
                if let Some(seat) = &seat {
                    seat.pointer_button(&self.state, now, button, pressed);
                }
            }
            (Iface::Scroll, 1) => {
                // px into detent units: carrot speaks 15 px per 120
                let x = a.f32()? as f64;
                let y = a.f32()? as f64;
                if let Some(seat) = &seat {
                    for (horizontal, v) in [(true, x), (false, y)] {
                        if v != 0.0 {
                            seat.pointer_axis(now, horizontal, (v / 15.0 * 120.0).round() as i32);
                        }
                    }
                }
            }
            (Iface::Scroll, 2) => {
                let x = a.i32()?;
                let y = a.i32()?;
                if let Some(seat) = &seat {
                    for (horizontal, v) in [(true, x), (false, y)] {
                        if v != 0 {
                            seat.pointer_axis(now, horizontal, v);
                        }
                    }
                }
            }
            (Iface::Scroll, 3) => {
                // no kinetic scrolling to stop
                a.u32()?;
                a.u32()?;
                a.u32()?;
            }
            (Iface::Keyboard, 1) => {
                let key = a.u32()?;
                let pressed = a.u32()? != 0;
                if pressed {
                    self.held_keys.borrow_mut().insert(key);
                } else {
                    self.held_keys.borrow_mut().remove(&key);
                }
                if let Some(seat) = &seat {
                    self.key(seat, now, key, pressed);
                }
            }
            _ => return Err("unknown request"),
        }
        Ok(true)
    }

    /// same path as the evdev consumer: binds fire, vt switches switch
    fn key(&self, seat: &Rc<SeatGlobal>, now: u64, key: u32, pressed: bool) {
        seat.ensure_focus(&self.state);
        match seat.key(&self.state, now, key, pressed) {
            KeyAction::SwitchVt(vt) => {
                let session = self.state.session.borrow().clone();
                if let Some(session) = session {
                    if vt == session.vtnr {
                        return;
                    }
                    if let Some(d) = self.state.display.borrow().as_ref() {
                        d.prepare_vt_switch(&self.state, vt);
                    }
                    session.switch_vt(vt);
                }
            }
            KeyAction::Act(action) => crate::ipc::dispatch_action(&self.state, &action),
            KeyAction::Handled => {}
        }
    }

    fn move_cursor(&self, seat: &Rc<SeatGlobal>) {
        if let Some(d) = self.state.display.borrow().as_ref() {
            d.move_cursor(&self.state, seat.ptr_x.get() as i32, seat.ptr_y.get() as i32);
        }
    }

    /// evdev synthesizes releases on pause and detach; the injected side
    /// owes the same when its sender dies mid-hold
    fn release_held(&self) {
        let Some(seat) = self.state.seat.borrow().clone() else {
            return;
        };
        let now = crate::util::Time::now().nsec() / 1_000;
        let keys: Vec<u32> = self.held_keys.borrow_mut().drain().collect();
        for key in keys {
            self.key(&seat, now, key, false);
        }
        let buttons: Vec<u32> = self.held_buttons.borrow_mut().drain().collect();
        for b in buttons {
            seat.pointer_button(&self.state, now, b, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, Wheel};
    use crate::uring::Ring;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    fn test_conn() -> Conn {
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
        Conn::new(state, Rc::new(a))
    }

    #[test]
    fn a_dead_sender_releases_its_held_edges() {
        let c = test_conn();
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *c.state.seat.borrow_mut() = Some(seat.clone());
        // a sender injects a key and a button, then dies mid-hold
        seat.key(&c.state, 0, 30, true);
        seat.pointer_button(&c.state, 0, 0x110, true);
        c.held_keys.borrow_mut().insert(30);
        c.held_buttons.borrow_mut().insert(0x110);
        assert!(seat.pressed.borrow().contains(&30));
        c.release_held();
        assert!(seat.pressed.borrow().is_empty(), "the key came back up");
        assert!(c.held_keys.borrow().is_empty());
        assert!(c.held_buttons.borrow().is_empty());
    }

    fn events(c: &Conn) -> Vec<(u64, u32)> {
        let out = c.out.borrow();
        let mut msgs = Vec::new();
        let mut off = 0;
        while let Some((object, len, opcode)) = wire::header(&out[off..]) {
            msgs.push((object, opcode));
            off += len;
        }
        msgs
    }

    fn feed(c: &Conn, pending: &mut Vec<u8>, msg: MsgBuilder) -> bool {
        pending.extend_from_slice(&msg.finish());
        c.drain(pending)
    }

    fn sender_handshake(c: &Conn, pending: &mut Vec<u8>) {
        assert!(feed(c, pending, MsgBuilder::new(0, 0).u32(1)));
        assert!(feed(c, pending, MsgBuilder::new(0, 2).u32(CONTEXT_SENDER)));
        assert!(feed(c, pending, MsgBuilder::new(0, 3).string("test")));
        for name in [
            "ei_connection",
            "ei_callback",
            "ei_seat",
            "ei_device",
            "ei_pointer",
            "ei_button",
        ] {
            assert!(feed(c, pending, MsgBuilder::new(0, 4).string(name).u32(1)));
        }
        assert!(feed(c, pending, MsgBuilder::new(0, 1)));
    }

    #[test]
    fn handshake_announces_seat_to_senders() {
        let c = test_conn();
        let mut pending = Vec::new();
        sender_handshake(&c, &mut pending);
        let evs = events(&c);
        // six shared interfaces, then the connection, all on object 0
        assert_eq!(evs.iter().filter(|e| **e == (0, 1)).count(), 6);
        assert_eq!(evs.iter().filter(|e| **e == (0, 2)).count(), 1);
        let conn = MIN_SERVER_ID;
        let seat = conn + 1;
        assert!(evs.contains(&(conn, 1)), "seat event");
        assert_eq!(evs.iter().filter(|e| **e == (seat, 2)).count(), 2, "pointer + button caps");
        assert!(evs.contains(&(seat, 3)), "seat done");
        assert_eq!(c.caps.get(), CAP_POINTER | CAP_BUTTON);
    }

    #[test]
    fn receivers_get_no_seat() {
        let c = test_conn();
        let mut pending = Vec::new();
        assert!(feed(&c, &mut pending, MsgBuilder::new(0, 0).u32(1)));
        for name in ["ei_connection", "ei_callback", "ei_seat", "ei_device"] {
            assert!(feed(&c, &mut pending, MsgBuilder::new(0, 4).string(name).u32(1)));
        }
        assert!(feed(&c, &mut pending, MsgBuilder::new(0, 1)));
        let evs = events(&c);
        assert!(evs.contains(&(0, 2)), "connection still granted");
        assert!(!evs.contains(&(MIN_SERVER_ID, 1)), "no seat for receivers");
    }

    #[test]
    fn finish_requires_connection_and_callback() {
        let c = test_conn();
        let mut pending = Vec::new();
        assert!(feed(&c, &mut pending, MsgBuilder::new(0, 0).u32(1)));
        assert!(!feed(&c, &mut pending, MsgBuilder::new(0, 1)));
    }

    #[test]
    fn unknown_object_fatal_only_before_connection() {
        let c = test_conn();
        let mut pending = Vec::new();
        assert!(!feed(&c, &mut pending, MsgBuilder::new(7, 0).u32(1)));
        let c = test_conn();
        let mut pending = Vec::new();
        sender_handshake(&c, &mut pending);
        c.out.borrow_mut().clear();
        assert!(feed(&c, &mut pending, MsgBuilder::new(999, 0).u32(1)));
        assert_eq!(events(&c), vec![(MIN_SERVER_ID, 2)], "invalid_object");
    }

    #[test]
    fn bind_creates_a_device() {
        let c = test_conn();
        let mut pending = Vec::new();
        sender_handshake(&c, &mut pending);
        c.out.borrow_mut().clear();
        let seat = MIN_SERVER_ID + 1;
        assert!(feed(
            &c,
            &mut pending,
            MsgBuilder::new(seat, 1).u64(CAP_POINTER | CAP_BUTTON),
        ));
        let dev = MIN_SERVER_ID + 2;
        let evs = events(&c);
        assert!(evs.contains(&(seat, 4)), "device event");
        assert!(evs.contains(&(dev, 2)), "device_type");
        assert_eq!(evs.iter().filter(|e| **e == (dev, 5)).count(), 2, "two interfaces");
        assert!(evs.contains(&(dev, 6)), "device done");
        assert!(evs.contains(&(dev, 7)), "resumed");
        // rebinding tears the old device down first
        c.out.borrow_mut().clear();
        assert!(feed(&c, &mut pending, MsgBuilder::new(seat, 1).u64(CAP_POINTER)));
        let evs = events(&c);
        assert!(evs.contains(&(dev, 0)), "old device destroyed");
        // binding outside the advertised mask is fatal
        assert!(!feed(&c, &mut pending, MsgBuilder::new(seat, 1).u64(CAP_KEYBOARD)));
    }
}
