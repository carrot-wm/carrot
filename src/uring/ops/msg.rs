// message-shaped ops: accept, recvmsg, sendmsg, and the linked timeout giving
// sendmsg a deadline. fds ride out of band as SCM_RIGHTS control messages - the
// cmsg header below is the one piece of kernel abi we lay out ourselves.

use super::KernelTimespec;
use super::super::{CqeFuture, Oneshot, Op, OpId, Ring, RingError};
use crate::util::Time;
use rustix::io_uring::{IoringOp, IoringTimeoutFlags, MsgHdr, io_uring_ptr, io_uring_sqe, iovec};
use rustix::net::{RecvFlags, SendFlags, SocketFlags};
use std::cell::RefCell;
use std::ffi::c_void;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

// -- SCM_RIGHTS plumbing --

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
/// kernel sets this in msg_flags when control data didn't fit
const MSG_CTRUNC: u32 = 8;

/// struct cmsghdr on 64-bit: size 16, align 8
#[repr(C)]
#[derive(Copy, Clone)]
struct CmsgHdr {
    len: usize,
    level: i32,
    ty: i32,
}

const CMSG_HDR: usize = size_of::<CmsgHdr>();

const fn cmsg_align(n: usize) -> usize {
    (n + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// 1024 bytes holds SCM_RIGHTS for 252 fds - far past what a wayland msg carries
#[repr(C, align(8))]
struct CmsgBuf([u8; 1024]);

impl Default for CmsgBuf {
    fn default() -> Self {
        CmsgBuf([0; 1024])
    }
}

/// null pointers + zero lengths are the valid empty state
fn zeroed_msghdr() -> MsgHdr {
    unsafe { mem::zeroed() }
}

fn zeroed_iovec() -> iovec {
    unsafe { mem::zeroed() }
}

fn collect_rights(buf: &[u8], controllen: usize, out: &mut Vec<OwnedFd>) {
    let len = controllen.min(buf.len());
    let mut off = 0;
    while off + CMSG_HDR <= len {
        // buf is 8-aligned and off advances by cmsg_align, so this holds
        let hdr = unsafe { *(buf.as_ptr().add(off) as *const CmsgHdr) };
        if hdr.len < CMSG_HDR || off + hdr.len > len {
            break;
        }
        if hdr.level == SOL_SOCKET && hdr.ty == SCM_RIGHTS {
            for raw in buf[off + CMSG_HDR..off + hdr.len].chunks_exact(4) {
                let fd = i32::from_ne_bytes(raw.try_into().unwrap());
                out.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
        off += cmsg_align(hdr.len);
    }
}

// -- accept --

pub(in crate::uring) struct AcceptOp {
    id: OpId,
    fd: i32,
    data: Option<AcceptData>,
}

struct AcceptData {
    _fd: Rc<OwnedFd>,
    slot: Rc<Oneshot>,
}

impl Default for AcceptOp {
    fn default() -> Self {
        AcceptOp {
            id: OpId::default(),
            fd: -1,
            data: None,
        }
    }
}

unsafe impl Op for AcceptOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::Accept;
        sqe.fd = self.fd;
        sqe.op_flags.accept_flags = SocketFlags::CLOEXEC;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(d) = self.data.take() {
            if res >= 0 && Rc::strong_count(&d.slot) == 1 {
                // accept beat its own cancellation but nobody waits - close, don't leak
                drop(unsafe { OwnedFd::from_raw_fd(res) });
            } else {
                d.slot.complete(res);
            }
        }
        ring.cached_accepts.push(self);
    }
}

impl Ring {
    pub async fn accept(&self, fd: &Rc<OwnedFd>) -> Result<OwnedFd, RingError> {
        self.check_destroyed()?;
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let mut op = self.cached_accepts.pop().unwrap_or_default();
        op.id = guard.id;
        op.fd = fd.as_raw_fd();
        op.data = Some(AcceptData {
            _fd: fd.clone(),
            slot: slot.clone(),
        });
        self.schedule(op);
        let n = CqeFuture(slot).await?;
        Ok(unsafe { OwnedFd::from_raw_fd(n) })
    }
}

// -- recvmsg --

pub struct RecvMsg {
    pub buf: Vec<u8>,
    pub n: usize,
    pub fds: Vec<OwnedFd>,
    /// control data didn't fit and the kernel dropped fds; callers treat this
    /// as a protocol error, never ignore it
    pub truncated: bool,
}

pub(in crate::uring) struct RecvmsgOp {
    id: OpId,
    fd: i32,
    buf: Vec<u8>,
    offset: usize,
    iov: iovec,
    hdr: MsgHdr,
    cmsg: Box<CmsgBuf>,
    data: Option<RecvmsgData>,
}

struct RecvmsgData {
    _fd: Rc<OwnedFd>,
    ret: Rc<RefCell<Option<RecvMsg>>>,
    slot: Rc<Oneshot>,
}

impl Default for RecvmsgOp {
    fn default() -> Self {
        RecvmsgOp {
            id: OpId::default(),
            fd: -1,
            buf: Vec::new(),
            offset: 0,
            iov: zeroed_iovec(),
            hdr: zeroed_msghdr(),
            cmsg: Box::default(),
            data: None,
        }
    }
}

impl RecvmsgOp {
    /// wire up the self-referential pointers. only valid once the box has its
    /// final heap address, i.e. right before schedule()
    fn prep(&mut self) {
        self.iov.iov_base = unsafe { self.buf.as_mut_ptr().add(self.offset) } as *mut c_void;
        self.iov.iov_len = self.buf.len() - self.offset;
        self.hdr = zeroed_msghdr();
        self.hdr.msg_iov = &raw mut self.iov;
        self.hdr.msg_iovlen = 1;
        self.hdr.msg_control = self.cmsg.0.as_mut_ptr() as *mut c_void;
        self.hdr.msg_controllen = self.cmsg.0.len();
    }
}

unsafe impl Op for RecvmsgOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::Recvmsg;
        sqe.fd = self.fd;
        sqe.addr_or_splice_off_in.addr =
            io_uring_ptr::new((&raw const self.hdr) as *mut c_void);
        sqe.op_flags.recv_flags = RecvFlags::CMSG_CLOEXEC;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(d) = self.data.take() {
            let mut fds = Vec::new();
            let mut truncated = false;
            if res >= 0 {
                // kernel wrote the real control length back into hdr
                truncated = self.hdr.msg_flags.bits() as u32 & MSG_CTRUNC != 0;
                collect_rights(&self.cmsg.0, self.hdr.msg_controllen, &mut fds);
            }
            *d.ret.borrow_mut() = Some(RecvMsg {
                buf: mem::take(&mut self.buf),
                n: res.max(0) as usize,
                fds,
                truncated,
            });
            d.slot.complete(res);
        }
        ring.cached_recvmsg.push(self);
    }
}

impl Ring {
    /// reads into buf[offset..]; fds come back with the bytes. n == 0 is EOF.
    pub async fn recvmsg(
        &self,
        fd: &Rc<OwnedFd>,
        buf: Vec<u8>,
        offset: usize,
    ) -> Result<RecvMsg, RingError> {
        self.check_destroyed()?;
        assert!(offset < buf.len());
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let ret = Rc::new(RefCell::new(None));
        let mut op = self.cached_recvmsg.pop().unwrap_or_default();
        op.id = guard.id;
        op.fd = fd.as_raw_fd();
        op.offset = offset;
        op.buf = buf;
        op.data = Some(RecvmsgData {
            _fd: fd.clone(),
            ret: ret.clone(),
            slot: slot.clone(),
        });
        op.prep();
        self.schedule(op);
        CqeFuture(slot).await?;
        Ok(ret.borrow_mut().take().expect("recvmsg returned nothing"))
    }
}

// -- sendmsg --

pub(in crate::uring) struct SendmsgOp {
    id: OpId,
    fd: i32,
    buf: Vec<u8>,
    range: (usize, usize),
    /// keeps the passed fds open until the kernel is done with them
    fds: Vec<Rc<OwnedFd>>,
    iov: iovec,
    hdr: MsgHdr,
    cmsg: Box<CmsgBuf>,
    link: bool,
    data: Option<SendmsgData>,
}

struct SendmsgData {
    _fd: Rc<OwnedFd>,
    ret: Rc<RefCell<Option<Vec<u8>>>>,
    slot: Rc<Oneshot>,
}

impl Default for SendmsgOp {
    fn default() -> Self {
        SendmsgOp {
            id: OpId::default(),
            fd: -1,
            buf: Vec::new(),
            range: (0, 0),
            fds: Vec::new(),
            iov: zeroed_iovec(),
            hdr: zeroed_msghdr(),
            cmsg: Box::default(),
            link: false,
            data: None,
        }
    }
}

impl SendmsgOp {
    fn prep(&mut self) {
        self.iov.iov_base = unsafe { self.buf.as_mut_ptr().add(self.range.0) } as *mut c_void;
        self.iov.iov_len = self.range.1 - self.range.0;
        self.hdr = zeroed_msghdr();
        self.hdr.msg_iov = &raw mut self.iov;
        self.hdr.msg_iovlen = 1;
        if !self.fds.is_empty() {
            let dlen = self.fds.len() * 4;
            assert!(CMSG_HDR + dlen <= self.cmsg.0.len(), "too many fds in one message");
            let hdr = CmsgHdr {
                len: CMSG_HDR + dlen,
                level: SOL_SOCKET,
                ty: SCM_RIGHTS,
            };
            unsafe {
                *(self.cmsg.0.as_mut_ptr() as *mut CmsgHdr) = hdr;
            }
            for (i, fd) in self.fds.iter().enumerate() {
                let raw = fd.as_raw_fd().to_ne_bytes();
                self.cmsg.0[CMSG_HDR + i * 4..CMSG_HDR + i * 4 + 4].copy_from_slice(&raw);
            }
            self.hdr.msg_control = self.cmsg.0.as_mut_ptr() as *mut c_void;
            self.hdr.msg_controllen = CMSG_HDR + cmsg_align(dlen);
        }
    }
}

unsafe impl Op for SendmsgOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn has_link(&self) -> bool {
        self.link
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::Sendmsg;
        sqe.fd = self.fd;
        sqe.addr_or_splice_off_in.addr =
            io_uring_ptr::new((&raw const self.hdr) as *mut c_void);
        sqe.op_flags.send_flags = SendFlags::NOSIGNAL;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(d) = self.data.take() {
            *d.ret.borrow_mut() = Some(mem::take(&mut self.buf));
            d.slot.complete(res);
        }
        // drop fd refs even when the future is gone
        self.fds.clear();
        ring.cached_sendmsg.push(self);
    }
}

impl Ring {
    /// writes buf[range.0..range.1], attaching fds to the first byte. a deadline
    /// rides as a linked timeout: expiry surfaces as ECANCELED on this op.
    pub async fn sendmsg(
        &self,
        fd: &Rc<OwnedFd>,
        buf: Vec<u8>,
        range: (usize, usize),
        fds: Vec<Rc<OwnedFd>>,
        deadline: Option<Time>,
    ) -> Result<(Vec<u8>, usize), RingError> {
        self.check_destroyed()?;
        assert!(range.0 <= range.1 && range.1 <= buf.len());
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let ret = Rc::new(RefCell::new(None));
        let mut op = self.cached_sendmsg.pop().unwrap_or_default();
        op.id = guard.id;
        op.fd = fd.as_raw_fd();
        op.range = range;
        op.buf = buf;
        op.fds = fds;
        op.link = deadline.is_some();
        op.data = Some(SendmsgData {
            _fd: fd.clone(),
            ret: ret.clone(),
            slot: slot.clone(),
        });
        op.prep();
        // scheduled back to back with no await between, so encode() sees them adjacent
        self.schedule(op);
        if let Some(t) = deadline {
            schedule_link(self, t);
        }
        let n = CqeFuture(slot).await?;
        Ok((ret.borrow_mut().take().expect("sendmsg kept the buffer"), n as usize))
    }
}

// -- linked timeout --

#[derive(Default)]
pub(in crate::uring) struct LinkTimeoutOp {
    id: OpId,
    ts: KernelTimespec,
}

unsafe impl Op for LinkTimeoutOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::LinkTimeout;
        sqe.fd = -1;
        sqe.addr_or_splice_off_in.addr =
            io_uring_ptr::new((&raw const self.ts) as *mut c_void);
        sqe.len.len = 1;
        sqe.op_flags.timeout_flags = IoringTimeoutFlags::ABS;
    }

    fn complete(self: Box<Self>, ring: &Ring, _res: i32) {
        // parent finished first (we got cancelled) or we fired (parent sees
        // ECANCELED); neither is worth reporting
        ring.cached_links.push(self);
    }
}

fn schedule_link(ring: &Ring, deadline: Time) {
    let mut op = ring.cached_links.pop().unwrap_or_default();
    op.id = ring.id_raw();
    op.ts = deadline.into();
    ring.schedule(op);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use rustix::event::{EventfdFlags, eventfd};
    use rustix::net::{AddressFamily, SocketType, socketpair};

    #[test]
    fn sendmsg_recvmsg_roundtrip_with_fd() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let (a, b) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let (a, b) = (Rc::new(a), Rc::new(b));
        // ride an eventfd through SCM_RIGHTS, read its counter back out
        let efd = eventfd(7, EventfdFlags::empty()).unwrap();
        let out = Rc::new(RefCell::new(None));
        let o = out.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            let payload = b"hello wire".to_vec();
            let len = payload.len();
            if r.sendmsg(&a, payload, (0, len), vec![Rc::new(efd)], None).await.is_ok() {
                let got = r.recvmsg(&b, vec![0u8; 64], 0).await;
                *o.borrow_mut() = Some(got);
            }
            r.stop();
        });
        ring.run().unwrap();
        let got = out.borrow_mut().take().unwrap().unwrap();
        assert_eq!(&got.buf[..got.n], b"hello wire");
        assert_eq!(got.fds.len(), 1);
        assert!(!got.truncated);
        let mut b8 = [0u8; 8];
        rustix::io::read(&got.fds[0], &mut b8).unwrap();
        assert_eq!(u64::from_ne_bytes(b8), 7);
    }

    #[test]
    fn accept_returns_a_connection() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let path = std::env::temp_dir().join(format!("carrot-accept-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // connect completes against the backlog before we accept
        let _peer = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let lfd = Rc::new(std::os::fd::OwnedFd::from(listener));
        let ok = Rc::new(std::cell::Cell::new(false));
        let k = ok.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            if r.accept(&lfd).await.is_ok() {
                k.set(true);
            }
            r.stop();
        });
        ring.run().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(ok.get());
    }
}
