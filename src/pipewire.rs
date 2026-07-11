// hand-rolled pipewire native client - no libpipewire anywhere. frames are
// 16-byte headers (object id | opcode<<24|size | seq | n_fds) with spa pods
// as bodies and fds over SCM_RIGHTS. P0 scope: connect, hello, registry.

pub mod client_node;
pub mod pod;

use pod::{PodBuilder, PodParser};
use rustix::fd::OwnedFd;
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
    Closed,
    Pod(pod::PodError),
    Remote(String),
}

impl std::fmt::Display for PwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PwError::Env(e) => write!(f, "{e}"),
            PwError::Io(e) => write!(f, "socket: {e}"),
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

fn socket_path() -> Result<String, PwError> {
    let dir = std::env::var("PIPEWIRE_RUNTIME_DIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .map_err(|_| PwError::Env("no PIPEWIRE_RUNTIME_DIR or XDG_RUNTIME_DIR"))?;
    let name = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    Ok(format!("{dir}/{name}"))
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
    fd: Rc<OwnedFd>,
    seq: std::cell::Cell<u32>,
    /// fds arrive with whatever read was in flight; frames claim n_fds each
    pending_fds: std::cell::RefCell<std::collections::VecDeque<OwnedFd>>,
}

impl PwConn {
    pub fn connect() -> Result<PwConn, PwError> {
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
        Ok(PwConn {
            fd: Rc::new(fd),
            seq: std::cell::Cell::new(0),
            pending_fds: std::cell::RefCell::new(std::collections::VecDeque::new()),
        })
    }

    pub fn send(&self, id: u32, opcode: u8, body: &[u8]) -> Result<(), PwError> {
        let seq = self.seq.get();
        self.seq.set(seq.wrapping_add(1));
        let mut msg = Vec::with_capacity(16 + body.len());
        msg.extend_from_slice(&id.to_le_bytes());
        msg.extend_from_slice(&(((opcode as u32) << 24) | body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&seq.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.extend_from_slice(body);
        let mut off = 0;
        while off < msg.len() {
            off += rustix::io::write(&*self.fd, &msg[off..])?;
        }
        Ok(())
    }

    fn read_exact(&self, buf: &mut [u8]) -> Result<(), PwError> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
        let mut off = 0;
        while off < buf.len() {
            let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
            let mut anc = RecvAncillaryBuffer::new(&mut space);
            let r = recvmsg(
                &*self.fd,
                &mut [rustix::io::IoSliceMut::new(&mut buf[off..])],
                &mut anc,
                RecvFlags::CMSG_CLOEXEC,
            )?;
            if r.bytes == 0 {
                return Err(PwError::Closed);
            }
            for m in anc.drain() {
                if let RecvAncillaryMessage::ScmRights(rights) = m {
                    self.pending_fds.borrow_mut().extend(rights);
                }
            }
            off += r.bytes;
        }
        Ok(())
    }

    pub fn recv(&self) -> Result<Frame, PwError> {
        let mut hdr = [0u8; 16];
        self.read_exact(&mut hdr)?;
        let id = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let w2 = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let seq = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let n_fds = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        let opcode = (w2 >> 24) as u8;
        let size = (w2 & 0xff_ffff) as usize;
        let mut body = vec![0u8; size];
        self.read_exact(&mut body)?;
        // this frame's declared share of the fd stream, in arrival order
        let mut q = self.pending_fds.borrow_mut();
        let fds = (0..n_fds).map(|_| q.pop_front().map(Some).flatten()).collect();
        Ok(Frame { id, opcode, seq, body, fds })
    }

    pub fn hello(&self) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| b.int(CORE_VERSION));
        self.send(0, CORE_HELLO, &b.buf)?;
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.dict(&[
                ("application.name", "carrot"),
                ("application.process.binary", "carrot"),
            ]);
        });
        self.send(1, CLIENT_UPDATE_PROPERTIES, &b.buf)
    }

    pub fn get_registry(&self, new_id: u32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(REGISTRY_VERSION);
            b.uint(new_id);
        });
        self.send(0, CORE_GET_REGISTRY, &b.buf)
    }

    pub fn sync(&self, cookie: i32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(0);
            b.int(cookie);
        });
        self.send(0, CORE_SYNC, &b.buf)
    }

    pub fn raw_fd(&self) -> &OwnedFd {
        &self.fd
    }

    pub fn pong(&self, id: i32, seq: i32) -> Result<(), PwError> {
        let mut b = PodBuilder::default();
        b.struct_(|b| {
            b.int(id);
            b.int(seq);
        });
        self.send(0, CORE_PONG, &b.buf)
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
    match pattern_inner(secs) {
        Ok(frames) => {
            println!("pw-pattern: done, {frames} frames");
            0
        }
        Err(e) => {
            eprintln!("pw-pattern: {e}");
            1
        }
    }
}

fn pattern_inner(secs: u64) -> Result<u64, PwError> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let con = Rc::new(PwConn::connect()?);
    con.hello()?;
    let mut node = client_node::SourceNode::create(con.clone(), 2, 640, 360, 30)?;
    let started = crate::util::Time::now();
    let mut announced = false;
    let mut tick = 0u64;
    let mut last = 0u64;
    let frame_ns = 1_000_000_000 / node.fps as u64;
    loop {
        let now = crate::util::Time::now().nsec();
        let elapsed = now.saturating_sub(started.nsec());
        if elapsed / 1_000_000_000 >= secs {
            return Ok(tick);
        }
        let next = last + frame_ns;
        let wait_ns = next.saturating_sub(now).min(200_000_000);
        let mut pfd = [PollFd::new(con.raw_fd(), PollFlags::IN)];
        let ts = Timespec {
            tv_sec: (wait_ns / 1_000_000_000) as i64,
            tv_nsec: (wait_ns % 1_000_000_000) as i64,
        };
        let n = poll(&mut pfd, Some(&ts)).unwrap_or(0);
        if n > 0 {
            let mut f = con.recv()?;
            if !node.handle(&mut f)? {
                match (f.id, f.opcode) {
                    (0, EV_CORE_PING) => {
                        let mut p = PodParser::new(&f.body);
                        let mut s = p.struct_()?;
                        let id = s.int()?;
                        let seq = s.int()?;
                        con.pong(id, seq)?;
                    }
                    (0, EV_CORE_ERROR) => {
                        let mut p = PodParser::new(&f.body);
                        let mut s = p.struct_()?;
                        let id = s.int()?;
                        let _seq = s.int()?;
                        let res = s.int()?;
                        let msg = s.string()?.to_string();
                        return Err(PwError::Remote(format!("object {id}: {msg} ({res})")));
                    }
                    _ => {}
                }
            }
            continue;
        }
        // frame due
        if node.ready() {
            if !announced {
                announced = true;
                println!(
                    "pw-pattern: streaming {}x{}@{} BGRx as global {:?}",
                    node.width, node.height, node.fps, node.bound_global
                );
            }
            node.produce(tick)?;
            tick += 1;
        }
        last = crate::util::Time::now().nsec();
    }
}

/// `carrot pw-probe`: hello + registry dump against the live daemon. proves
/// the framing, the pod codec, and the handshake end to end
pub fn probe() -> i32 {
    match probe_inner() {
        Ok(n) => {
            println!("pw-probe: {n} globals");
            0
        }
        Err(e) => {
            eprintln!("pw-probe: {e}");
            1
        }
    }
}

fn probe_inner() -> Result<u32, PwError> {
    const REGISTRY_ID: u32 = 2;
    const COOKIE: i32 = 0x5eed;
    let con = PwConn::connect()?;
    con.hello()?;
    con.get_registry(REGISTRY_ID)?;
    con.sync(COOKIE)?;
    let mut globals = 0u32;
    loop {
        let f = con.recv()?;
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
            (0, EV_CORE_PING) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let id = s.int()?;
                let seq = s.int()?;
                con.pong(id, seq)?;
            }
            (0, EV_CORE_ERROR) => {
                let mut p = PodParser::new(&f.body);
                let mut s = p.struct_()?;
                let id = s.int()?;
                let _seq = s.int()?;
                let res = s.int()?;
                let msg = s.string()?.to_string();
                return Err(PwError::Remote(format!("object {id}: {msg} ({res})")));
            }
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
