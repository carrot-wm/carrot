// one type per op - accept, read/write, recvmsg/sendmsg, timeout, poll,
// cancel. buffers stay owned by the op until its cqe lands, then the
// pending future wakes with them.


use super::{CqeFuture, Oneshot, Op, OpId, Ring, RingError};
use crate::util::Time;
use rustix::event::PollFlags;
use rustix::io::Errno;
use rustix::io_uring::{IoringOp, IoringTimeoutFlags, io_uring_ptr, io_uring_sqe, io_uring_user_data};
use std::cell::Cell;
use std::mem;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

// -- timeout --

// matches the kernel's __kernel_timespec
#[repr(C)]
#[derive(Default)]
struct KernelTimespec {
    sec: i64,
    nsec: i64,
}

impl From<Time> for KernelTimespec {
    fn from(t: Time) -> Self {
        KernelTimespec {
            sec: (t.nsec() / 1_000_000_000) as i64,
            nsec: (t.nsec() % 1_000_000_000) as i64,
        }
    }
}

#[derive(Default)]
pub(super) struct TimeoutOp {
    id: OpId,
    // lives inside the box so its address stays valid for the kernel
    ts: KernelTimespec,
    data: Option<Rc<Oneshot>>,
}

unsafe impl Op for TimeoutOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::Timeout;
        sqe.fd = -1;
        sqe.addr_or_splice_off_in.addr =
            io_uring_ptr::new((&raw const self.ts) as *mut std::ffi::c_void);
        sqe.len.len = 1;
        sqe.op_flags.timeout_flags = IoringTimeoutFlags::ABS;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(slot) = self.data.take() {
            slot.complete(res);
        }
        ring.cached_timeouts.push(self);
    }
}

impl Ring {
    // resolves at `deadline` (absolute CLOCK_MONOTONIC). expiry and
    // cancellation both come back Ok - callers that need to tell them
    // apart race a version counter instead.
    pub async fn timeout(&self, deadline: Time) -> Result<(), RingError> {
        self.check_destroyed()?;
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let mut op = self.cached_timeouts.pop().unwrap_or_default();
        op.id = guard.id;
        op.ts = deadline.into();
        op.data = Some(slot.clone());
        self.schedule(op);
        match CqeFuture(slot).await {
            Ok(_) => Ok(()),
            Err(RingError::Os(e)) if e == Errno::TIME || e == Errno::CANCELED => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// -- read / write --

pub(super) struct RwOp {
    id: OpId,
    opcode: IoringOp,
    fd: i32,
    buf: Vec<u8>,
    len: u32,
    data: Option<RwData>,
}

struct RwData {
    // keeps the fd open until the cqe even if the caller lost interest
    _fd: Rc<OwnedFd>,
    ret: Rc<Cell<Option<Vec<u8>>>>,
    slot: Rc<Oneshot>,
}

impl Default for RwOp {
    fn default() -> Self {
        RwOp {
            id: OpId::default(),
            opcode: IoringOp::Nop,
            fd: -1,
            buf: Vec::new(),
            len: 0,
            data: None,
        }
    }
}

unsafe impl Op for RwOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = self.opcode;
        sqe.fd = self.fd;
        // stream position - a real offset here would pread/pwrite
        sqe.off_or_addr2.off = !0u64;
        sqe.addr_or_splice_off_in.addr =
            io_uring_ptr::new(self.buf.as_ptr() as *mut std::ffi::c_void);
        sqe.len.len = self.len;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(d) = self.data.take() {
            d.ret.set(Some(mem::take(&mut self.buf)));
            d.slot.complete(res);
        }
        ring.cached_rw.push(self);
    }
}

impl Ring {
    // the buffer moves into the op for the op's lifetime and comes back
    // with the byte count. dropping the future mid-flight cancels; the op
    // keeps the buffer alive until the kernel really lets go.
    pub async fn read(
        &self,
        fd: &Rc<OwnedFd>,
        buf: Vec<u8>,
    ) -> Result<(Vec<u8>, usize), RingError> {
        self.rw(IoringOp::Read, fd, buf).await
    }

    pub async fn write(
        &self,
        fd: &Rc<OwnedFd>,
        buf: Vec<u8>,
    ) -> Result<(Vec<u8>, usize), RingError> {
        self.rw(IoringOp::Write, fd, buf).await
    }

    async fn rw(
        &self,
        opcode: IoringOp,
        fd: &Rc<OwnedFd>,
        buf: Vec<u8>,
    ) -> Result<(Vec<u8>, usize), RingError> {
        self.check_destroyed()?;
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let ret = Rc::new(Cell::new(None));
        let mut op = self.cached_rw.pop().unwrap_or_default();
        op.id = guard.id;
        op.opcode = opcode;
        op.fd = fd.as_raw_fd();
        // sqe lengths are 32 bit; a giant buffer clamps and surfaces as a
        // normal short read/write instead of silently truncating to 0
        op.len = buf.len().min(u32::MAX as usize) as u32;
        op.buf = buf;
        op.data = Some(RwData {
            _fd: fd.clone(),
            ret: ret.clone(),
            slot: slot.clone(),
        });
        self.schedule(op);
        let n = CqeFuture(slot).await?;
        Ok((ret.take().expect("op returned no buffer"), n as usize))
    }
}

// -- poll --

pub(super) struct PollOp {
    id: OpId,
    fd: i32,
    events: u16,
    data: Option<PollData>,
}

struct PollData {
    _fd: Rc<OwnedFd>,
    slot: Rc<Oneshot>,
}

impl Default for PollOp {
    fn default() -> Self {
        PollOp {
            id: OpId::default(),
            fd: -1,
            events: 0,
            data: None,
        }
    }
}

unsafe impl Op for PollOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::PollAdd;
        sqe.fd = self.fd;
        sqe.op_flags.poll_events = self.events;
    }

    fn complete(mut self: Box<Self>, ring: &Ring, res: i32) {
        if let Some(d) = self.data.take() {
            d.slot.complete(res);
        }
        ring.cached_polls.push(self);
    }
}

impl Ring {
    // one-shot readiness. resolves with revents once the fd reports any
    // of the requested events.
    pub async fn readable(&self, fd: &Rc<OwnedFd>) -> Result<u16, RingError> {
        self.poll_fd(fd, PollFlags::IN.bits() as u16).await
    }

    #[allow(dead_code)]
    pub async fn writable(&self, fd: &Rc<OwnedFd>) -> Result<u16, RingError> {
        self.poll_fd(fd, PollFlags::OUT.bits() as u16).await
    }

    async fn poll_fd(&self, fd: &Rc<OwnedFd>, events: u16) -> Result<u16, RingError> {
        self.check_destroyed()?;
        let guard = self.guard();
        let slot = Rc::new(Oneshot::default());
        let mut op = self.cached_polls.pop().unwrap_or_default();
        op.id = guard.id;
        op.fd = fd.as_raw_fd();
        op.events = events;
        op.data = Some(PollData {
            _fd: fd.clone(),
            slot: slot.clone(),
        });
        self.schedule(op);
        let revents = CqeFuture(slot).await?;
        Ok(revents as u16)
    }
}

// -- async cancel --

#[derive(Default)]
pub(super) struct CancelOp {
    id: OpId,
    target: OpId,
}

unsafe impl Op for CancelOp {
    fn id(&self) -> OpId {
        self.id
    }

    fn is_cancel(&self) -> bool {
        true
    }

    fn encode(&self, sqe: &mut io_uring_sqe) {
        sqe.opcode = IoringOp::AsyncCancel;
        sqe.fd = -1;
        sqe.addr_or_splice_off_in.user_data = io_uring_user_data::from_u64(self.target.0);
    }

    fn complete(self: Box<Self>, ring: &Ring, res: i32) {
        // ENOENT just means the target completed first
        if res < 0 && res != -Errno::NOENT.raw_os_error() {
            crate::trace!("async cancel of {:?} failed: {}", self.target, res);
        }
        ring.cached_cancels.push(self);
    }
}

// the target's own cqe (usually -ECANCELED) resolves its future; this
// op's completion carries no caller-visible result
pub(super) fn schedule_cancel(ring: &Ring, target: OpId) {
    let mut op = ring.cached_cancels.pop().unwrap_or_default();
    op.id = ring.id_raw();
    op.target = target;
    ring.schedule(op);
}
