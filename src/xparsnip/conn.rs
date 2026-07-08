// the live x11 connection: pipelined requests over an already connected
// socket, u64 serials rebuilt from the 16 bit wire sequence, replies
// matched strictly in order. void requests set need_sync and the flusher
// injects a GetInputFocus fence so their errors stay attributable and the
// sequence counter can never silently wrap.

use super::wire;
use crate::engine::{Engine, SpawnedFuture};
use crate::uring::Ring;
use crate::util::{AsyncEvent, AsyncQueue};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::Entry;
use std::fmt;
use std::os::fd::OwnedFd;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum ParsnipError {
    Dead,
    Setup(String),
    Error(u8, u8),
}

impl fmt::Display for ParsnipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParsnipError::Dead => write!(f, "connection dead"),
            ParsnipError::Setup(e) => write!(f, "setup failed: {e}"),
            ParsnipError::Error(code, major) => {
                write!(f, "x error Bad{} from request {major}", wire::error_name(*code))
            }
        }
    }
}

// replies larger than this kill the connection; a wm never sees them
const MAX_REPLY: usize = 16 * 1024;
// GetProperty chunk, in 32 bit units
const PROP_CHUNK: u32 = 512;

pub struct Extensions {
    pub composite: u8,
    pub xfixes: u8,
    pub xfixes_first_event: u8,
    pub render: u8,
    pub res: u8,
}

struct Slot {
    done: AsyncEvent,
    val: RefCell<Option<Result<Vec<u8>, ParsnipError>>>,
    // fence replies get dropped instead of delivered
    discard: bool,
}

pub struct Parsnip {
    ring: Rc<Ring>,
    fd: Rc<OwnedFd>,
    pub root: u32,
    pub root_depth: u8,
    xid_next: Cell<u32>,
    xid_inc: u32,
    xid_max: u32,
    out: RefCell<Vec<u8>>,
    kick: AsyncEvent,
    // serial of the next request we will send; the wire carries only the
    // low 16 bits implicitly by position
    send_serial: Cell<u64>,
    recv_serial: Cell<u64>,
    need_sync: Cell<bool>,
    pending: RefCell<VecDeque<(u64, Rc<Slot>)>>,
    pub events: AsyncQueue<wire::XEvent>,
    atoms: RefCell<HashMap<String, u32>>,
    pub ext: RefCell<Option<Extensions>>,
    dead: Cell<bool>,
    tasks: RefCell<Vec<SpawnedFuture<()>>>,
}

impl Parsnip {
    // fd is already connected (the wm socketpair, or a display socket for
    // the probe); auth is whatever the caller scraped up, usually empty
    pub async fn connect(
        eng: &Rc<Engine>,
        ring: &Rc<Ring>,
        fd: OwnedFd,
        auth_name: &[u8],
        auth_data: &[u8],
    ) -> Result<Rc<Parsnip>, ParsnipError> {
        let fd = Rc::new(fd);
        let mut hello = Vec::new();
        wire::encode_setup_request(&mut hello, auth_name, auth_data);
        write_all(ring, &fd, hello).await.map_err(|e| ParsnipError::Setup(e))?;

        let prefix = read_exact(ring, &fd, 8).await.map_err(ParsnipError::Setup)?;
        let more = wire::setup_reply_len(&prefix).unwrap_or(0);
        let mut full = prefix;
        full.extend(read_exact(ring, &fd, more).await.map_err(ParsnipError::Setup)?);
        match full[0] {
            1 => {}
            0 => {
                let len = full.get(1).copied().unwrap_or(0) as usize;
                let msg = full.get(8..8 + len).unwrap_or(b"");
                return Err(ParsnipError::Setup(String::from_utf8_lossy(msg).into_owned()));
            }
            _ => return Err(ParsnipError::Setup("authenticate is not supported".into())),
        }
        let setup = wire::parse_setup(&full)
            .ok_or_else(|| ParsnipError::Setup("malformed setup reply".into()))?;
        let screen = setup
            .screens
            .first()
            .ok_or_else(|| ParsnipError::Setup("no screens".into()))?;

        let inc = setup.resource_id_mask & setup.resource_id_mask.wrapping_neg();
        let conn = Rc::new(Parsnip {
            ring: ring.clone(),
            fd,
            root: screen.root,
            root_depth: screen.root_depth,
            xid_next: Cell::new(setup.resource_id_base),
            xid_inc: inc,
            xid_max: setup.resource_id_base | setup.resource_id_mask,
            out: RefCell::new(Vec::new()),
            kick: AsyncEvent::default(),
            send_serial: Cell::new(1),
            recv_serial: Cell::new(0),
            need_sync: Cell::new(false),
            pending: RefCell::new(VecDeque::new()),
            events: AsyncQueue::default(),
            atoms: RefCell::new(HashMap::new()),
            ext: RefCell::new(None),
            dead: Cell::new(false),
            tasks: RefCell::new(Vec::new()),
        });

        let c = conn.clone();
        let outgoing = eng.spawn("parsnip out", async move { c.outgoing().await });
        let c = conn.clone();
        let incoming = eng.spawn("parsnip in", async move { c.incoming().await });
        conn.tasks.borrow_mut().push(outgoing);
        conn.tasks.borrow_mut().push(incoming);

        conn.bind_extensions().await?;
        Ok(conn)
    }

    pub fn alloc_xid(&self) -> u32 {
        let id = self.xid_next.get();
        // a wm allocates a handful of ids in its whole life
        assert!(id <= self.xid_max, "x resource ids exhausted");
        self.xid_next.set(id + self.xid_inc);
        id
    }

    // -- sending --

    // a request that never replies; errors surface through the next fence
    pub fn send(&self, build: impl FnOnce(&mut Vec<u8>)) {
        if self.dead.get() {
            return;
        }
        build(&mut self.out.borrow_mut());
        self.send_serial.set(self.send_serial.get() + 1);
        self.need_sync.set(true);
        self.kick.trigger();
    }

    // a request with a reply; resolution is strictly in send order
    pub async fn call(&self, build: impl FnOnce(&mut Vec<u8>)) -> Result<Vec<u8>, ParsnipError> {
        self.call_inner(build, false).await
    }

    async fn call_inner(
        &self,
        build: impl FnOnce(&mut Vec<u8>),
        discard: bool,
    ) -> Result<Vec<u8>, ParsnipError> {
        if self.dead.get() {
            return Err(ParsnipError::Dead);
        }
        build(&mut self.out.borrow_mut());
        let serial = self.send_serial.get();
        self.send_serial.set(serial + 1);
        self.need_sync.set(false);
        let slot = Rc::new(Slot {
            done: AsyncEvent::default(),
            val: RefCell::new(None),
            discard,
        });
        self.pending.borrow_mut().push_back((serial, slot.clone()));
        self.kick.trigger();
        slot.done.triggered().await;
        slot.val.borrow_mut().take().unwrap_or(Err(ParsnipError::Dead))
    }

    async fn outgoing(self: Rc<Self>) {
        loop {
            self.kick.triggered().await;
            // fence any voids in this batch so their errors stay bounded
            if self.need_sync.get() {
                self.need_sync.set(false);
                let serial = self.send_serial.get();
                self.send_serial.set(serial + 1);
                wire::get_input_focus(&mut self.out.borrow_mut());
                let slot = Rc::new(Slot {
                    done: AsyncEvent::default(),
                    val: RefCell::new(None),
                    discard: true,
                });
                self.pending.borrow_mut().push_back((serial, slot));
            }
            let buf: Vec<u8> = std::mem::take(&mut *self.out.borrow_mut());
            if buf.is_empty() {
                continue;
            }
            if write_all(&self.ring, &self.fd, buf).await.is_err() {
                self.kill();
                return;
            }
        }
    }

    // -- receiving --

    async fn incoming(self: Rc<Self>) {
        let mut pending = Vec::new();
        loop {
            let buf = vec![0u8; 4096];
            let Ok((buf, n)) = self.ring.read(&self.fd, buf).await else {
                self.kill();
                return;
            };
            if n == 0 {
                self.kill();
                return;
            }
            pending.extend_from_slice(&buf[..n]);
            loop {
                if pending.len() < 32 {
                    break;
                }
                let frame_len = if pending[0] == 1 {
                    let extra = wire::reply_extra_len(&pending);
                    if extra > MAX_REPLY {
                        eprintln!("carrot: xparsnip: oversized reply, killing the connection");
                        self.kill();
                        return;
                    }
                    32 + extra
                } else {
                    32
                };
                if pending.len() < frame_len {
                    break;
                }
                let frame: Vec<u8> = pending.drain(..frame_len).collect();
                if !self.dispatch(&frame) {
                    self.kill();
                    return;
                }
            }
        }
    }

    // sequence numbers only carry 16 bits on the wire; every frame we see
    // ratchets the full 64 bit counter forward
    fn ratchet(&self, wire_seq: u16) -> u64 {
        let last = self.recv_serial.get();
        let mut full = (last & !0xffff) | wire_seq as u64;
        if full < last {
            full += 0x10000;
        }
        self.recv_serial.set(full);
        full
    }

    fn dispatch(&self, frame: &[u8]) -> bool {
        let seq = u16::from_ne_bytes([frame[2], frame[3]]);
        match frame[0] {
            1 => {
                let serial = self.ratchet(seq);
                let front = self.pending.borrow_mut().pop_front();
                match front {
                    Some((s, slot)) if s == serial => {
                        if !slot.discard {
                            *slot.val.borrow_mut() = Some(Ok(frame.to_vec()));
                        }
                        slot.done.trigger();
                        true
                    }
                    _ => {
                        eprintln!("carrot: xparsnip: reply out of order, killing the connection");
                        false
                    }
                }
            }
            0 => {
                let serial = self.ratchet(seq);
                let Some(err) = wire::parse_error(frame) else {
                    return false;
                };
                let matches_front = self
                    .pending
                    .borrow()
                    .front()
                    .is_some_and(|(s, _)| *s == serial);
                if matches_front {
                    let (_, slot) = self.pending.borrow_mut().pop_front().unwrap();
                    *slot.val.borrow_mut() = Some(Err(ParsnipError::Error(err.code, err.major)));
                    slot.done.trigger();
                } else {
                    // a void request misbehaved; loud but not fatal
                    eprintln!(
                        "carrot: xparsnip: Bad{} from void request {} (value {:#x})",
                        wire::error_name(err.code),
                        err.major,
                        err.bad_value
                    );
                }
                true
            }
            _ => {
                // events also carry a sequence except KeymapNotify (11)
                if frame[0] & 0x7f != 11 {
                    self.ratchet(seq);
                }
                let first = self
                    .ext
                    .borrow()
                    .as_ref()
                    .map(|e| e.xfixes_first_event)
                    .unwrap_or(0);
                if let Some(ev) = wire::parse_event(frame, first) {
                    self.events.push(ev);
                }
                true
            }
        }
    }

    // -- conveniences --

    pub async fn intern(&self, name: &str) -> Result<u32, ParsnipError> {
        if let Some(a) = self.atoms.borrow().get(name) {
            return Ok(*a);
        }
        let bytes = name.as_bytes().to_vec();
        let reply = self.call(|b| wire::intern_atom(b, false, &bytes)).await?;
        let atom = wire::parse_intern_atom(&reply).ok_or(ParsnipError::Dead)?;
        if let Entry::Vacant(e) = self.atoms.borrow_mut().entry(name.to_string()) {
            e.insert(atom);
        }
        Ok(atom)
    }

    // the whole property, chunked; format and type come from the first round
    pub async fn get_property_full(
        &self,
        window: u32,
        property: u32,
        ty: u32,
    ) -> Result<wire::GetPropertyReply, ParsnipError> {
        let mut acc: Option<wire::GetPropertyReply> = None;
        let mut offset = 0u32;
        loop {
            let reply = self
                .call(|b| wire::get_property(b, 0, window, property, ty, offset, PROP_CHUNK))
                .await?;
            let part = wire::parse_get_property(&reply).ok_or(ParsnipError::Dead)?;
            let after = part.bytes_after;
            offset += part.data.len() as u32 / 4;
            match &mut acc {
                None => acc = Some(part),
                Some(a) => a.data.extend_from_slice(&part.data),
            }
            if after == 0 {
                return Ok(acc.unwrap());
            }
        }
    }

    async fn bind_extensions(&self) -> Result<(), ParsnipError> {
        // pipelined: all four queries go out in one batch
        let composite = self.call(|b| wire::query_extension(b, b"Composite"));
        let xfixes = self.call(|b| wire::query_extension(b, b"XFIXES"));
        let render = self.call(|b| wire::query_extension(b, b"RENDER"));
        let res = self.call(|b| wire::query_extension(b, b"X-Resource"));
        let parse = |r: Vec<u8>| wire::parse_query_extension(&r).ok_or(ParsnipError::Dead);
        let composite = parse(composite.await?)?;
        let xfixes = parse(xfixes.await?)?;
        let render = parse(render.await?)?;
        let res = parse(res.await?)?;
        for (name, e) in [
            ("Composite", &composite),
            ("XFIXES", &xfixes),
            ("RENDER", &render),
            ("X-Resource", &res),
        ] {
            if !e.present {
                return Err(ParsnipError::Setup(format!("{name} extension missing")));
            }
        }
        *self.ext.borrow_mut() = Some(Extensions {
            composite: composite.major_opcode,
            xfixes: xfixes.major_opcode,
            xfixes_first_event: xfixes.first_event,
            render: render.major_opcode,
            res: res.major_opcode,
        });
        Ok(())
    }

    pub fn kill(&self) {
        if self.dead.replace(true) {
            return;
        }
        let _ = rustix::net::shutdown(&*self.fd, rustix::net::Shutdown::Both);
        let mut pending = self.pending.borrow_mut();
        while let Some((_, slot)) = pending.pop_front() {
            *slot.val.borrow_mut() = Some(Err(ParsnipError::Dead));
            slot.done.trigger();
        }
    }

    // sever the task cycle; the owner calls this exactly once
    pub fn clear(&self) {
        self.kill();
        self.tasks.borrow_mut().clear();
        self.events.clear();
    }
}

async fn write_all(ring: &Rc<Ring>, fd: &Rc<OwnedFd>, mut buf: Vec<u8>) -> Result<(), String> {
    while !buf.is_empty() {
        match ring.write(fd, buf).await {
            Ok((b, n)) if n > 0 => {
                buf = b;
                buf.drain(..n);
            }
            Ok(_) => return Err("write stalled".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

async fn read_exact(ring: &Rc<Ring>, fd: &Rc<OwnedFd>, want: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(want);
    while out.len() < want {
        let buf = vec![0u8; want - out.len()];
        match ring.read(fd, buf).await {
            Ok((_, 0)) => return Err("eof during setup".into()),
            Ok((b, n)) => out.extend_from_slice(&b[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}
