// hand-rolled pipewire native client - no libpipewire anywhere. frames are
// 16-byte headers (object id | opcode<<24|size | seq | n_fds) with spa pods
// as bodies and fds over SCM_RIGHTS. io rides the ring: in the compositor
// the present tail feeds casts and must never block on the daemon.

pub mod client_node;
pub mod pod;

use crate::engine::Engine;
use crate::uring::{Ring, RingError};
use client_node::SourceNode;
use pod::{PodBuilder, PodParser};
use rustix::fd::OwnedFd;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

// core (object 0) methods and events
const CORE_HELLO: u8 = 1;
const CORE_SYNC: u8 = 2;
const CORE_PONG: u8 = 3;
const CORE_GET_REGISTRY: u8 = 5;
pub(crate) const CORE_CREATE_OBJECT: u8 = 6;
const EV_CORE_INFO: u8 = 0;
const EV_CORE_DONE: u8 = 1;
const EV_CORE_PING: u8 = 2;
const EV_CORE_ERROR: u8 = 3;
// client (object 1) methods
const CLIENT_UPDATE_PROPERTIES: u8 = 2;
// registry events
const EV_REGISTRY_GLOBAL: u8 = 0;

const CORE_VERSION: i32 = 3;
const REGISTRY_VERSION: i32 = 3;

#[derive(Debug)]
pub enum PwError {
    Env(&'static str),
    Io(rustix::io::Errno),
    Ring(RingError),
    Closed,
    Pod(pod::PodError),
    Remote(String),
}

impl std::fmt::Display for PwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PwError::Env(e) => write!(f, "{e}"),
            PwError::Io(e) => write!(f, "socket: {e}"),
            PwError::Ring(e) => write!(f, "socket: {e}"),
            PwError::Closed => write!(f, "the daemon hung up"),
            PwError::Pod(e) => write!(f, "{e}"),
            PwError::Remote(e) => write!(f, "daemon error: {e}"),
        }
    }
}

impl From<pod::PodError> for PwError {
    fn from(e: pod::PodError) -> PwError {
        PwError::Pod(e)
    }
}

impl From<rustix::io::Errno> for PwError {
    fn from(e: rustix::io::Errno) -> PwError {
        PwError::Io(e)
    }
}

impl From<RingError> for PwError {
    fn from(e: RingError) -> PwError {
        PwError::Ring(e)
    }
}

fn socket_path() -> Result<String, PwError> {
    let dir = std::env::var("PIPEWIRE_RUNTIME_DIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .map_err(|_| PwError::Env("no PIPEWIRE_RUNTIME_DIR or XDG_RUNTIME_DIR"))?;
    let name = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    Ok(format!("{dir}/{name}"))
}

/// a fresh daemon connection; OpenPipeWireRemote hands these straight to apps
pub(crate) fn open_socket() -> Result<OwnedFd, PwError> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with};
    let path = socket_path()?;
    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let addr = SocketAddrUnix::new(&*path)?;
    rustix::net::connect(&fd, &addr)?;
    Ok(fd)
}

pub struct Frame {
    pub id: u32,
    pub opcode: u8,
    pub seq: u32,
    pub body: Vec<u8>,
    /// options so handlers can take ownership fd by fd
    pub fds: Vec<Option<OwnedFd>>,
}

pub struct PwConn {
    ring: Rc<Ring>,
    fd: Rc<OwnedFd>,
    seq: Cell<u32>,
    /// partial inbound bytes; whole frames are cut off the front
    inbuf: RefCell<Vec<u8>>,
    /// fds arrive with whatever read was in flight; frames claim n_fds each
    pending_fds: RefCell<VecDeque<OwnedFd>>,
    /// recycled read buffer
    rbuf: Cell<Vec<u8>>,
}

impl PwConn {
    pub fn connect(ring: &Rc<Ring>) -> Result<PwConn, PwError> {
        Ok(PwConn {
            ring: ring.clone(),
            fd: Rc::new(open_socket()?),
            seq: Cell::new(0),
            inbuf: RefCell::new(Vec::new()),
            pending_fds: RefCell::new(VecDeque::new()),
            rbuf: Cell::new(Vec::new()),
        })
    }

    pub async fn send(&self, id: u32, opcode: u8, body: &[u8]) -> Result<(), PwError> {
        let seq = self.seq.get();
        self.seq.set(seq.wrapping_add(1));
        let mut msg = Vec::with_capacity(16 + body.len());
        msg.extend_from_slice(&id.to_le_bytes());
        msg.extend_from_slice(&(((opcode as u32) << 24) | body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&seq.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.extend_from_slice(body);
        let len = msg.len();
        let mut sent = 0;
        let mut buf = msg;
        while sent < len {
            let (b, n) = self
                .ring
                .sendmsg(&self.fd, buf, (sent, len), Vec::new(), None)
                .await?;
            buf = b;
            sent += n;
        }
        Ok(())
    }

    /// the next whole frame off the stream, reading as needed
    pub async fn recv(&self) -> Result<Frame, PwError> {
        loop {
            if let Some(f) = self.cut_frame() {
                return Ok(f);
            }
            let mut buf = self.rbuf.take();
            if buf.len() < 4096 {
                buf = vec![0u8; 4096];
            }
            let r = self.ring.recvmsg(&self.fd, buf, 0).await?;
            if r.truncated {
                return Err(PwError::Env("fd control data truncated"));
            }
            if r.n == 0 {
                return Err(PwError::Closed);
            }
            self.inbuf.borrow_mut().extend_from_slice(&r.buf[..r.n]);
            self.pending_fds.borrow_mut().extend(r.fds);
            self.rbuf.set(r.buf);
        }
    }

    fn cut_frame(&self) -> Option<Frame> {
        let mut ib = self.inbuf.borrow_mut();
        if ib.len() < 16 {
            return None;
        }
        let w2 = u32::from_le_bytes(ib[4..8].try_into().unwrap());
        let size = (w2 & 0xff_ffff) as usize;
        if ib.len() < 16 + size {
            return None;
        }
        let msg: Vec<u8> = ib.drain(..16 + size).collect();
        let id = u32::from_le_bytes(msg[0..4].try_into().unwrap());
        let seq = u32::from_le_bytes(msg[8..12].try_into().unwrap());
        let n_fds = u32::from_le_bytes(msg[12..16].try_into().unwrap()) as usize;
        // this frame's declared share of the fd stream, in arrival order
        let mut q = self.pending_fds.borrow_mut();
        let fds = (0..n_fds).map(|_| q.pop_front().map(Some).flatten()).collect();
        Some(Frame {
            id,
            opcode: (w2 >> 24) as u8,
            seq,
            body: msg[16..].to_vec(),
            fds,
        })
    }

    pub async fn hello(&self) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| b.int(CORE_VERSION));
        self.send(0, CORE_HELLO, &b.buf).await?;
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.dict(&[
                ("application.name", "carrot"),
                ("application.process.binary", "carrot"),
            ]);
        });
        self.send(1, CLIENT_UPDATE_PROPERTIES, &b.buf).await
    }

    pub async fn get_registry(&self, new_id: u32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(REGISTRY_VERSION);
            b.uint(new_id);
        });
        self.send(0, CORE_GET_REGISTRY, &b.buf).await
    }

    pub async fn sync(&self, cookie: i32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(0);
            b.int(cookie);
        });
        self.send(0, CORE_SYNC, &b.buf).await
    }

    pub async fn pong(&self, id: i32, seq: i32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(id);
            b.int(seq);
        });
        self.send(0, CORE_PONG, &b.buf).await
    }
}

fn remote_error(body: &[u8]) -> PwError {
    let parsed = (|| -> Result<(i32, i32, String), pod::PodError> {
        let mut p = PodParser::new(body);
        let mut s = p.struct_()?;
        let id = s.int()?;
        let _seq = s.int()?;
        let res = s.int()?;
        Ok((id, res, s.string()?.to_string()))
    })();
    match parsed {
        Ok((id, res, msg)) => PwError::Remote(format!("object {id}: {msg} ({res})")),
        Err(e) => PwError::Pod(e),
    }
}

async fn answer_ping(con: &PwConn, body: &[u8]) -> Result<(), PwError> {
    let mut p = PodParser::new(body);
    let mut s = p.struct_()?;
    let id = s.int()?;
    let seq = s.int()?;
    con.pong(id, seq).await
}

/// drive a node's control stream; returns only on error or hangup
pub(crate) async fn pump_node(
    con: &PwConn,
    node: &Rc<RefCell<SourceNode>>,
) -> Result<(), PwError> {
    loop {
        let mut f = con.recv().await?;
        if node.borrow_mut().handle(&mut f)? {
            continue;
        }
        match (f.id, f.opcode) {
            (0, EV_CORE_PING) => answer_ping(con, &f.body).await?,
            (0, EV_CORE_ERROR) => return Err(remote_error(&f.body)),
            _ => {}
        }
    }
}

/// `carrot pw-pattern [secs]`: a Video/Source client-node pushing a moving
/// test pattern - the P1 gate. connect a consumer (helvum/gstreamer/obs)
/// and watch it move
pub fn pattern() -> i32 {
    let secs: u64 = std::env::args()
        .skip_while(|a| a != "pw-pattern")
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(3600);
    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let code = Rc::new(Cell::new(0));
    let eng = engine.clone();
    let rng = ring.clone();
    let c = code.clone();
    let task = engine.spawn("pw pattern", async move {
        match pattern_run(&eng, &rng, secs).await {
            Ok(frames) => println!("pw-pattern: done, {frames} frames"),
            Err(e) => {
                eprintln!("pw-pattern: {e}");
                c.set(1);
            }
        }
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    code.get()
}

async fn pattern_run(eng: &Rc<Engine>, ring: &Rc<Ring>, secs: u64) -> Result<u64, PwError> {
    let con = Rc::new(PwConn::connect(ring)?);
    con.hello().await?;
    let node = SourceNode::create(con.clone(), 2, 640, 360, 30).await?;
    let (w, h, fps) = (node.width, node.height, node.fps);
    let node = Rc::new(RefCell::new(node));
    let failed: Rc<RefCell<Option<PwError>>> = Rc::new(RefCell::new(None));
    let pump = eng.spawn("pw net", {
        let con = con.clone();
        let node = node.clone();
        let failed = failed.clone();
        async move {
            if let Err(e) = pump_node(&con, &node).await {
                *failed.borrow_mut() = Some(e);
            }
        }
    });
    let started = crate::util::Time::now();
    let frame_ns = 1_000_000_000 / fps as u64;
    let mut announced = false;
    let mut bound = false;
    let mut tick = 0u64;
    loop {
        let now = crate::util::Time::now().nsec();
        if now.saturating_sub(started.nsec()) / 1_000_000_000 >= secs {
            break;
        }
        if let Some(e) = failed.borrow_mut().take() {
            return Err(e);
        }
        if !bound {
            if let Some(g) = node.borrow().bound_global {
                let ms = now.saturating_sub(started.nsec()) / 1_000_000;
                println!("pw-pattern: bound as global {g} after {ms}ms");
                bound = true;
            }
        }
        let _ = ring.timeout(crate::util::Time::from_nsec(now + frame_ns)).await;
        let mut n = node.borrow_mut();
        if !n.ready() {
            continue;
        }
        if !announced {
            announced = true;
            println!(
                "pw-pattern: streaming {w}x{h}@{fps} BGRx as global {:?}",
                n.bound_global
            );
        }
        n.produce(|px, stride| paint_bar(px, stride, w as usize, h as usize, tick));
        tick += 1;
    }
    drop(pump);
    Ok(tick)
}

fn paint_bar(px: &mut [u8], stride: usize, w: usize, h: usize, tick: u64) {
    let bar = (tick as usize * 4) % w;
    for y in 0..h.min(px.len() / stride) {
        let row = &mut px[y * stride..y * stride + stride];
        for x in 0..w {
            let o = x * 4;
            let on = x >= bar && x < bar + w / 8;
            row[o] = if on { 40 } else { (x * 255 / w) as u8 }; // b
            row[o + 1] = if on { 220 } else { (y * 255 / h) as u8 }; // g
            row[o + 2] = if on { 120 } else { 60 }; // r
            row[o + 3] = 255;
        }
    }
}

/// `carrot pw-probe`: hello + registry dump against the live daemon. proves
/// the framing, the pod codec, and the handshake end to end
pub fn probe() -> i32 {
    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let code = Rc::new(Cell::new(0));
    let rng = ring.clone();
    let c = code.clone();
    let task = engine.spawn("pw probe", async move {
        match probe_inner(&rng).await {
            Ok(n) => println!("pw-probe: {n} globals"),
            Err(e) => {
                eprintln!("pw-probe: {e}");
                c.set(1);
            }
        }
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    code.get()
}

async fn probe_inner(ring: &Rc<Ring>) -> Result<u32, PwError> {
    const REGISTRY_ID: u32 = 2;
    const COOKIE: i32 = 0x5eed;
    let con = PwConn::connect(ring)?;
    con.hello().await?;
    con.get_registry(REGISTRY_ID).await?;
    con.sync(COOKIE).await?;
    let mut globals = 0u32;
    loop {
        let f = con.recv().await?;
        match (f.id, f.opcode) {
            (0, EV_CORE_INFO) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let _id = s.int()?;
                let _cookie = s.int()?;
                let user = s.string()?.to_string();
                let host = s.string()?.to_string();
                let version = s.string()?.to_string();
                let name = s.string()?.to_string();
                println!("core: {name} {version} ({user}@{host})");
            }
            (0, EV_CORE_DONE) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let _id = s.int()?;
                if s.int()? == COOKIE {
                    return Ok(globals);
                }
            }
            (0, EV_CORE_PING) => answer_ping(&con, &f.body).await?,
            (0, EV_CORE_ERROR) => return Err(remote_error(&f.body)),
            (REGISTRY_ID, EV_REGISTRY_GLOBAL) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let id = s.uint()?;
                let _permissions = s.uint()?;
                let ty = s.string()?.to_string();
                let version = s.uint()?;
                let props = s.dict().unwrap_or_default();
                let tag = ["node.name", "media.class", "application.name", "metadata.name"]
                    .iter()
                    .find_map(|k| props.iter().find(|(pk, _)| pk == k))
                    .map(|(_, v)| format!(" {v}"))
                    .unwrap_or_default();
                println!("  {id:>3} v{version} {ty}{tag}");
                globals += 1;
            }
            _ => {}
        }
    }
}
