// hand-rolled dbus client for logind: system-bus calls and signals only.
// fds ride out of band; each message drains exactly UNIX_FDS off the front
// of one arrival-ordered queue - get it wrong and a device fd lands on the
// wrong reply.

mod logind;
pub(crate) mod wire;

pub use logind::{DeviceEvent, LogindSession};

use crate::engine::{Engine, SpawnedFuture};
use crate::uring::{Ring, RingError};
use crate::util::{AsyncEvent, AsyncQueue, NumCell};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::os::fd::OwnedFd;
use std::rc::Rc;
pub(crate) use wire::MsgBuilder;
use wire::Rd;

/// a local bus answers in microseconds; anything past this is a wedge,
/// and the caller can recover (skip a device, try the next card)
const CALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug)]
pub enum DbusError {
    Env(&'static str),
    Connect(rustix::io::Errno),
    Auth(&'static str),
    BigEndianPeer,
    Malformed(&'static str),
    Ring(RingError),
    Closed,
    TooFewFds,
    Call { name: String, text: String },
    BadReply(&'static str),
    /// the deadline fired first; the reply may never have been sent
    NoReply,
}

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbusError::Env(e) => write!(f, "{e}"),
            DbusError::Connect(e) => write!(f, "connecting to the bus failed: {e}"),
            DbusError::Auth(e) => write!(f, "bus auth failed: expected {e}"),
            DbusError::BigEndianPeer => write!(f, "peer speaks big-endian dbus"),
            DbusError::Malformed(e) => write!(f, "malformed dbus message: {e}"),
            DbusError::Ring(e) => write!(f, "bus io failed: {e}"),
            DbusError::Closed => write!(f, "the bus connection is closed"),
            DbusError::TooFewFds => write!(f, "message claims more fds than arrived"),
            DbusError::Call { name, text } => write!(f, "call failed: {name}: {text}"),
            DbusError::BadReply(e) => write!(f, "unexpected reply shape: {e}"),
            DbusError::NoReply => write!(f, "no reply within the deadline"),
        }
    }
}

impl std::error::Error for DbusError {}

impl From<RingError> for DbusError {
    fn from(e: RingError) -> Self {
        DbusError::Ring(e)
    }
}

pub struct Reply {
    pub sig: String,
    pub body: Vec<u8>,
    pub fds: Vec<Rc<OwnedFd>>,
}

impl Reply {
    pub fn rd(&self) -> Rd<'_> {
        Rd::new(&self.body, &self.fds)
    }
}

pub struct SignalMsg {
    pub path: Option<String>,
    pub sig: String,
    pub body: Vec<u8>,
    pub fds: Vec<Rc<OwnedFd>>,
}

impl SignalMsg {
    pub fn rd(&self) -> Rd<'_> {
        Rd::new(&self.body, &self.fds)
    }
}

pub enum Arg<'a> {
    U32(u32),
    Str(&'a str),
    Bool(bool),
    StrDict(&'a [(&'a str, &'a str)]),
    StrArray(&'a [&'a str]),
}

struct PendingReply {
    done: AsyncEvent,
    result: Cell<Option<Result<Reply, DbusError>>>,
}

type SignalCb = Box<dyn Fn(&SignalMsg)>;

/// an inbound method call, ready for reply()/reply_err()
pub struct MethodCall {
    pub path: String,
    pub interface: String,
    pub member: String,
    pub sender: String,
    pub serial: u32,
    pub sig: String,
    pub body: Vec<u8>,
    pub fds: Vec<Rc<OwnedFd>>,
}

type MethodCb = Box<dyn Fn(&DbusConn, &MethodCall)>;

impl MethodCall {
    pub fn rd(&self) -> wire::Rd<'_> {
        wire::Rd::new(&self.body, &self.fds)
    }
}

pub struct DbusConn {
    ring: Rc<Ring>,
    eng: Rc<Engine>,
    fd: Rc<OwnedFd>,
    serial: NumCell<u64>,
    closed: Cell<bool>,
    replies: RefCell<HashMap<u32, Rc<PendingReply>>>,
    signal_handlers: RefCell<Vec<(&'static str, &'static str, SignalCb)>>,
    method_handlers: RefCell<Vec<(&'static str, MethodCb)>>,
    out: AsyncQueue<(Vec<u8>, Vec<Rc<OwnedFd>>)>,
    tasks: Cell<Option<(SpawnedFuture<()>, SpawnedFuture<()>)>>,
    pub unique_name: RefCell<String>,
    /// probe-only byte transcript
    dump: RefCell<Option<std::fs::File>>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

const AUTH_BURST: &[u8] = b"\0AUTH EXTERNAL\r\nDATA\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n";

fn system_bus_path() -> Result<String, DbusError> {
    match std::env::var("DBUS_SYSTEM_BUS_ADDRESS") {
        Ok(addr) => {
            // first entry only; strip trailing ,guid=...
            let first = addr.split(';').next().unwrap_or("");
            let Some(rest) = first.strip_prefix("unix:path=") else {
                return Err(DbusError::Env(
                    "DBUS_SYSTEM_BUS_ADDRESS is not a unix:path= address",
                ));
            };
            Ok(rest.split(',').next().unwrap_or(rest).to_string())
        }
        Err(_) => Ok("/var/run/dbus/system_bus_socket".to_string()),
    }
}

fn session_bus_path() -> Result<String, DbusError> {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        let first = addr.split(';').next().unwrap_or("");
        if let Some(rest) = first.strip_prefix("unix:path=") {
            return Ok(rest.split(',').next().unwrap_or(rest).to_string());
        }
        return Err(DbusError::Env(
            "DBUS_SESSION_BUS_ADDRESS is not a unix:path= address",
        ));
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(rt) => Ok(format!("{rt}/bus")),
        Err(_) => Err(DbusError::Env("no DBUS_SESSION_BUS_ADDRESS or XDG_RUNTIME_DIR")),
    }
}

impl DbusConn {
    pub async fn connect(eng: &Rc<Engine>, ring: &Rc<Ring>) -> Result<Rc<DbusConn>, DbusError> {
        Self::connect_path(eng, ring, system_bus_path()?).await
    }

    pub async fn connect_session(eng: &Rc<Engine>, ring: &Rc<Ring>) -> Result<Rc<DbusConn>, DbusError> {
        Self::connect_path(eng, ring, session_bus_path()?).await
    }

    async fn connect_path(
        eng: &Rc<Engine>,
        ring: &Rc<Ring>,
        path: String,
    ) -> Result<Rc<DbusConn>, DbusError> {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with};
        let fd = socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(DbusError::Connect)?;
        let addr = SocketAddrUnix::new(&*path).map_err(DbusError::Connect)?;
        rustix::net::connect(&fd, &addr).map_err(DbusError::Connect)?;
        let fd = Rc::new(fd);

        let leftover = auth(ring, &fd).await?;

        let conn = Rc::new(DbusConn {
            ring: ring.clone(),
            eng: eng.clone(),
            fd,
            serial: NumCell::new(1),
            closed: Cell::new(false),
            replies: RefCell::new(HashMap::new()),
            signal_handlers: RefCell::new(Vec::new()),
            method_handlers: RefCell::new(Vec::new()),
            out: AsyncQueue::default(),
            tasks: Cell::new(None),
            unique_name: RefCell::new(String::new()),
            dump: RefCell::new(None),
        });
        let c = conn.clone();
        let incoming = eng.spawn("dbus in", async move {
            let res = c.incoming_loop(leftover).await;
            if let Err(e) = res {
                eprintln!("carrot: dbus connection lost: {e}");
            }
            c.fail_all(DbusError::Closed);
        });
        let c = conn.clone();
        let outgoing = eng.spawn("dbus out", async move {
            // a dead writer strands every queued call just as surely as a
            // dead reader; both exits fail the pendings out loud
            if let Err(e) = c.outgoing_loop().await {
                eprintln!("carrot: dbus writer lost: {e}");
            }
            c.fail_all(DbusError::Closed);
        });
        conn.tasks.set(Some((incoming, outgoing)));

        let hello = conn
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
                "",
                &[],
            )
            .await?;
        if hello.sig != "s" {
            return Err(DbusError::BadReply("Hello"));
        }
        *conn.unique_name.borrow_mut() = hello.rd().str()?;
        Ok(conn)
    }

    /// serve an interface: the callback owns member dispatch and replies
    pub fn serve(&self, interface: &'static str, cb: MethodCb) {
        self.method_handlers.borrow_mut().push((interface, cb));
    }

    pub fn reply(&self, call: &MethodCall, sig: &str, f: impl FnOnce(&mut wire::MsgBuilder)) {
        self.reply_to(call.serial, &call.sender, sig, f);
    }

    /// reply by saved (serial, sender): Start answers from its cast task,
    /// long after the handler returned
    pub fn reply_to(
        &self,
        reply_serial: u32,
        dest: &str,
        sig: &str,
        f: impl FnOnce(&mut wire::MsgBuilder),
    ) {
        let serial = self.next_serial();
        let mut b = wire::MsgBuilder::method_return(serial, reply_serial, dest);
        if !sig.is_empty() {
            b.signature(sig);
        }
        b.finish_header();
        f(&mut b);
        self.out.push((b.finish(), Vec::new()));
    }

    /// a reply carrying fds; "h" values in the body index into the fd array
    pub fn reply_fds(
        &self,
        call: &MethodCall,
        sig: &str,
        fds: Vec<Rc<OwnedFd>>,
        f: impl FnOnce(&mut wire::MsgBuilder),
    ) {
        let serial = self.next_serial();
        let mut b = wire::MsgBuilder::method_return(serial, call.serial, &call.sender);
        if !sig.is_empty() {
            b.signature(sig);
        }
        b.unix_fds(fds.len() as u32);
        b.finish_header();
        f(&mut b);
        self.out.push((b.finish(), fds));
    }

    /// broadcast a bodyless signal; the portal's Session.Closed rides this
    pub fn emit_signal(&self, path: &str, iface: &str, member: &str) {
        let serial = self.next_serial();
        let mut b = wire::MsgBuilder::signal(serial, path, iface, member);
        b.finish_header();
        self.out.push((b.finish(), Vec::new()));
    }

    pub fn reply_err(&self, call: &MethodCall, name: &str, text: &str) {
        let serial = self.next_serial();
        let mut b = wire::MsgBuilder::error_msg(serial, call.serial, &call.sender, name);
        b.signature("s");
        b.finish_header();
        b.put_str(text);
        self.out.push((b.finish(), Vec::new()));
    }

    /// claim a well-known name; 4 = DBUS_NAME_FLAG_DO_NOT_QUEUE
    pub async fn request_name(self: &Rc<Self>, name: &str) -> Result<(), DbusError> {
        let r = self
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                "su",
                // 4|2|1: DO_NOT_QUEUE, REPLACE_EXISTING, ALLOW_REPLACEMENT.
                // a lingering previous instance loses the name to us
                // instead of blocking screen sharing until a reboot
                &[Arg::Str(name), Arg::U32(7)],
            )
            .await?;
        // 1 = became primary owner, 4 = already were
        if r.sig == "u" && matches!(r.rd().u32()?, 1 | 4) {
            Ok(())
        } else {
            Err(DbusError::Env("could not become the portal name owner"))
        }
    }

    fn next_serial(&self) -> u32 {
        (self.serial.fetch_add(1) & 0xffff_ffff) as u32
    }

    fn build(
        &self,
        serial: u32,
        flags: u8,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        sig: &str,
        args: &[Arg<'_>],
    ) -> Vec<u8> {
        let mut b = MsgBuilder::call(serial, flags);
        b.path(path);
        b.destination(dest);
        b.interface(iface);
        b.member(member);
        if !sig.is_empty() {
            b.signature(sig);
        }
        b.finish_header();
        for a in args {
            match a {
                Arg::U32(v) => b.put_u32(*v),
                Arg::Str(v) => b.put_str(v),
                Arg::Bool(v) => b.put_bool(*v),
                Arg::StrDict(v) => b.put_str_dict(v),
                Arg::StrArray(v) => b.put_str_array(v),
            }
        }
        b.finish()
    }

    pub async fn call(
        self: &Rc<Self>,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        sig: &str,
        args: &[Arg<'_>],
    ) -> Result<Reply, DbusError> {
        if self.closed.get() {
            return Err(DbusError::Closed);
        }
        let serial = self.next_serial();
        let pending = Rc::new(PendingReply {
            done: AsyncEvent::default(),
            result: Cell::new(None),
        });
        self.replies.borrow_mut().insert(serial, pending.clone());
        self.out
            .push((self.build(serial, 0, dest, path, iface, member, sig, args), Vec::new()));
        // a reply that never comes must fail the call, not park it forever:
        // the whole compositor once sat headless on a TakeDevice whose
        // answer went missing. the bus is local, so the deadline is beyond
        // any honest round trip. the guard drops with the call, and the
        // ring cancels a dropped timeout, so the settled path costs nothing
        let conn = self.clone();
        let _deadline = self.eng.spawn("dbus deadline", async move {
            if conn.ring.timeout(crate::util::Time::now() + CALL_DEADLINE).await.is_ok()
                && let Some(p) = conn.replies.borrow_mut().remove(&serial)
            {
                p.result.set(Some(Err(DbusError::NoReply)));
                p.done.trigger();
            }
        });
        pending.done.triggered().await;
        pending.result.take().unwrap_or(Err(DbusError::Closed))
    }

    pub fn call_noreply(
        &self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        sig: &str,
        args: &[Arg<'_>],
    ) {
        if self.closed.get() {
            return;
        }
        let serial = self.next_serial();
        self.out.push((
            self.build(
                serial,
                wire::NO_REPLY_EXPECTED,
                dest,
                path,
                iface,
                member,
                sig,
                args,
            ),
            Vec::new(),
        ));
    }

    // handler first, then the bus-side match, so nothing slips between
    pub async fn subscribe(
        self: &Rc<Self>,
        iface: &'static str,
        member: &'static str,
        sender: &str,
        path: &str,
        cb: SignalCb,
    ) -> Result<(), DbusError> {
        self.signal_handlers.borrow_mut().push((iface, member, cb));
        let rule = format!(
            "type='signal',interface='{iface}',member='{member}',sender='{sender}',path='{path}'"
        );
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            "s",
            &[Arg::Str(&rule)],
        )
        .await?;
        Ok(())
    }

    pub fn set_dump(&self, f: std::fs::File) {
        *self.dump.borrow_mut() = Some(f);
    }

    pub(super) fn dump_line(&self, line: &str) {
        if let Some(f) = self.dump.borrow_mut().as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    async fn incoming_loop(&self, leftover: Vec<u8>) -> Result<(), DbusError> {
        let mut pending = leftover;
        let mut fdq: VecDeque<Rc<OwnedFd>> = VecDeque::new();
        let mut buf = vec![0u8; 4096];
        if !pending.is_empty() {
            self.dump_line(&format!("RX leftover {} {}", pending.len(), hex(&pending)));
        }
        loop {
            let res = self.drain_messages(&mut pending, &mut fdq);
            if let Err(e) = &res {
                self.dump_line(&format!("DRAIN ERR {e}"));
            }
            res?;
            let r = self.ring.recvmsg(&self.fd, buf, 0).await?;
            if r.truncated {
                return Err(DbusError::Malformed("fd control data truncated"));
            }
            if r.n == 0 {
                self.dump_line("RX eof");
                return Err(DbusError::Closed);
            }
            self.dump_line(&format!(
                "RX {} fds={} {}",
                r.n,
                r.fds.len(),
                hex(&r.buf[..r.n])
            ));
            pending.extend_from_slice(&r.buf[..r.n]);
            fdq.extend(r.fds.into_iter().map(Rc::new));
            buf = r.buf;
        }
    }

    fn drain_messages(
        &self,
        pending: &mut Vec<u8>,
        fdq: &mut VecDeque<Rc<OwnedFd>>,
    ) -> Result<(), DbusError> {
        while let Some(len) = wire::message_len(pending) {
            if pending.len() < len {
                break;
            }
            let msg: Vec<u8> = pending.drain(..len).collect();
            // the length framing already isolated this message: one
            // malformed body skips, it must not kill the connection
            // (and with it every portal session)
            let h = match wire::parse(&msg) {
                Ok(h) => h,
                Err(e) => {
                    self.dump_line(&format!("SKIP malformed message: {e}"));
                    continue;
                }
            };
            self.dump_line(&format!(
                "MSG type={} serial={} reply_to={:?} sig={:?} fds={} err={:?} member={:?}",
                h.mtype, h.serial, h.reply_serial, h.signature, h.unix_fds, h.error_name, h.member
            ));
            let nfds = h.unix_fds as usize;
            if fdq.len() < nfds {
                return Err(DbusError::TooFewFds);
            }
            let fds: Vec<Rc<OwnedFd>> = fdq.drain(..nfds).collect();
            let body = msg[h.body.0..h.body.1].to_vec();
            match h.mtype {
                wire::METHOD_RETURN => {
                    if let Some(serial) = h.reply_serial {
                        match self.replies.borrow_mut().remove(&serial) {
                            Some(p) => {
                                p.result.set(Some(Ok(Reply {
                                    sig: h.signature,
                                    body,
                                    fds,
                                })));
                                p.done.trigger();
                            }
                            None => {
                                // late (post-deadline) or misrouted; either
                                // way someone waited on this and gave up
                                eprintln!("carrot: dbus: return for unknown serial {serial} dropped");
                                self.dump_line(&format!("RETURN for unknown serial {serial}"))
                            }
                        }
                    }
                }
                wire::ERROR => {
                    if let Some(serial) = h.reply_serial {
                        if let Some(p) = self.replies.borrow_mut().remove(&serial) {
                            let text = if h.signature.starts_with('s') {
                                Rd::new(&body, &[]).str().unwrap_or_default()
                            } else {
                                String::new()
                            };
                            p.result.set(Some(Err(DbusError::Call {
                                name: h.error_name.unwrap_or_default(),
                                text,
                            })));
                            p.done.trigger();
                        } else {
                            eprintln!(
                                "carrot: dbus: error {} for unknown serial {serial} dropped",
                                h.error_name.as_deref().unwrap_or("?")
                            );
                        }
                    }
                }
                wire::SIGNAL => {
                    let (Some(iface), Some(member)) = (&h.interface, &h.member) else {
                        continue;
                    };
                    let sm = SignalMsg {
                        path: h.path,
                        sig: h.signature,
                        body,
                        fds,
                    };
                    for (i, m, cb) in self.signal_handlers.borrow().iter() {
                        if *i == iface && *m == member {
                            cb(&sm);
                        }
                    }
                }
                wire::METHOD_CALL => {
                    let call = MethodCall {
                        path: h.path.unwrap_or_default(),
                        interface: h.interface.unwrap_or_default(),
                        member: h.member.unwrap_or_default(),
                        sender: h.sender.unwrap_or_default(),
                        serial: h.serial,
                        sig: h.signature,
                        body,
                        fds,
                    };
                    let handled = {
                        let hs = self.method_handlers.borrow();
                        let mut hit = false;
                        for (iface, cb) in hs.iter() {
                            if *iface == call.interface {
                                cb(self, &call);
                                hit = true;
                                break;
                            }
                        }
                        hit
                    };
                    if !handled {
                        self.reply_err(
                            &call,
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "no such interface here",
                        );
                    }
                }
                _ => crate::trace!("ignoring dbus message type {}", h.mtype),
            }
        }
        Ok(())
    }

    async fn outgoing_loop(&self) -> Result<(), DbusError> {
        loop {
            let (msg, mut fds) = self.out.pop().await;
            self.dump_line(&format!("TX {} fds={} {}", msg.len(), fds.len(), hex(&msg)));
            let len = msg.len();
            let mut sent = 0;
            let mut buf = msg;
            while sent < len {
                // fds attach to the first byte
                let (b, n) = self
                    .ring
                    .sendmsg(&self.fd, buf, (sent, len), std::mem::take(&mut fds), None)
                    .await?;
                buf = b;
                sent += n;
            }
        }
    }

    fn fail_all(&self, _e: DbusError) {
        self.closed.set(true);
        for (_, p) in self.replies.borrow_mut().drain() {
            p.result.set(Some(Err(DbusError::Closed)));
            p.done.trigger();
        }
    }

    /// break the task<->conn cycle; idempotent
    pub fn clear(&self) {
        self.closed.set(true);
        self.tasks.take();
        self.signal_handlers.borrow_mut().clear();
        self.fail_all(DbusError::Closed);
    }
}

/// hand the session's addresses to bus activation and the systemd user
/// manager: portals (xdg-desktop-portal and its backends) are bus-activated
/// and cannot reach the compositor without WAYLAND_DISPLAY in their env
pub async fn export_session_env(
    eng: Rc<Engine>,
    ring: Rc<Ring>,
    state: Rc<crate::state::State>,
) {
    // xwayland picks its display number early in its own task; give it a
    // moment so DISPLAY rides along, then export without it
    for _ in 0..20 {
        if state.xwayland.borrow().is_some() {
            break;
        }
        let deadline = crate::util::Time::from_nsec(crate::util::Time::now().nsec() + 100_000_000);
        if ring.timeout(deadline).await.is_err() {
            return;
        }
    }
    let conn = match DbusConn::connect_session(&eng, &ring).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("carrot: dbus: no session bus, portals will not find us: {e}");
            return;
        }
    };
    let wd = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let desk = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "carrot".into());
    let xdisp = state
        .xwayland
        .borrow()
        .as_ref()
        .map(|x| format!(":{}", x.display));
    let mut pairs: Vec<(&str, &str)> = vec![
        ("WAYLAND_DISPLAY", wd.as_str()),
        ("XDG_CURRENT_DESKTOP", desk.as_str()),
    ];
    if let Some(d) = &xdisp {
        pairs.push(("DISPLAY", d.as_str()));
    }
    let r1 = conn
        .call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "UpdateActivationEnvironment",
            "a{ss}",
            &[Arg::StrDict(&pairs)],
        )
        .await;
    let assigns: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let refs: Vec<&str> = assigns.iter().map(|s| s.as_str()).collect();
    let r2 = conn
        .call(
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "SetEnvironment",
            "as",
            &[Arg::StrArray(&refs)],
        )
        .await;
    match (&r1, &r2) {
        (Ok(_), Ok(_)) => eprintln!("carrot: dbus: session env exported (activation + systemd)"),
        (Ok(_), Err(_)) => {
            eprintln!("carrot: dbus: session env exported (activation only, no systemd user manager)")
        }
        (Err(e), _) => eprintln!("carrot: dbus: session env export failed: {e}"),
    }
    // only after the env lands: a unit started before it would inherit a
    // session with no WAYLAND_DISPLAY and connect to nothing
    if r2.is_ok() {
        start_session_target(&conn).await;
    }
    conn.clear();
}


/// units bound to graphical-session.target, tried in order. graphical-session
/// itself refuses a manual start: it is only ever pulled in by a unit that
/// BindsTo it, so a compositor has to raise one of these instead.
///
/// this matters because xdg-desktop-portal 1.22+ declares
/// `Requisite=graphical-session.target`. Requisite is the strict form: if the
/// target is not already active the activation job fails outright rather than
/// starting it, so a session that never raises the target has no portal at
/// all, and the only symptom is "Dependency failed for Portal service" in the
/// user journal.
const SESSION_TARGETS: [&str; 2] = ["carrot-session.target", "nixos-fake-graphical-session.target"];

/// raise the session target so portals and anything else bound to
/// graphical-session.target can start. best effort: a session without a
/// systemd user manager is a supported way to run, it just has no portal
async fn start_session_target(conn: &Rc<DbusConn>) {
    for unit in SESSION_TARGETS {
        let r = conn
            .call(
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1",
                "org.freedesktop.systemd1.Manager",
                "StartUnit",
                "ss",
                &[Arg::Str(unit), Arg::Str("replace")],
            )
            .await;
        match r {
            Ok(_) => {
                eprintln!("carrot: dbus: session target up ({unit})");
                return;
            }
            // not installed: try the next name rather than giving up
            Err(e) => crate::trace!("dbus: {unit}: {e}"),
        }
    }
    eprintln!(
        "carrot: dbus: no session target could be started; units bound to \
graphical-session.target (xdg-desktop-portal among them) will stay down"
    );
}

pub fn probe() -> i32 {
    logind::probe()
}

async fn auth(ring: &Rc<Ring>, fd: &Rc<OwnedFd>) -> Result<Vec<u8>, DbusError> {
    let mut buf = AUTH_BURST.to_vec();
    let len = buf.len();
    let mut sent = 0;
    while sent < len {
        let (b, n) = ring.sendmsg(fd, buf, (sent, len), Vec::new(), None).await?;
        buf = b;
        sent += n;
    }

    // three lines back: DATA, OK <guid>, AGREE_UNIX_FD. bytes after the
    // third line are binary and belong to the message stream.
    let mut acc: Vec<u8> = Vec::new();
    let mut lines = 0;
    let mut consumed = 0;
    let mut read_buf = vec![0u8; 256];
    loop {
        while lines < 3 {
            let Some(nl) = acc[consumed..].iter().position(|&b| b == b'\n') else {
                break;
            };
            let line = &acc[consumed..consumed + nl];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let word = line.split(|&b| b == b' ').next().unwrap_or(b"");
            match (lines, word) {
                (0, b"DATA") | (1, b"OK") | (2, b"AGREE_UNIX_FD") => lines += 1,
                (0, _) => return Err(DbusError::Auth("DATA")),
                (1, _) => return Err(DbusError::Auth("OK")),
                _ => return Err(DbusError::Auth("AGREE_UNIX_FD")),
            }
            consumed += nl + 1;
        }
        if lines == 3 {
            return Ok(acc[consumed..].to_vec());
        }
        let (b, n) = ring.read(fd, read_buf).await?;
        if n == 0 {
            return Err(DbusError::Auth("more auth data"));
        }
        acc.extend_from_slice(&b[..n]);
        read_buf = b;
    }
}
