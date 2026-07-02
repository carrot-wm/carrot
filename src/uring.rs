// io_uring, proactor style - ops go in owning their buffers, come back with
// results. setup is SINGLE_ISSUER | DEFER_TASKRUN | SUBMIT_ALL; NODROP is
// required, so a full sq backpressures into a userspace queue rather than
// silently dropping. run() is the main loop: engine + cqe drain to a joint
// fixpoint, encode queued ops, then one enter parking on min_complete=1 when
// idle. every wakeup is a cqe - no side channel, so the park is sound.

mod ops;

pub use ops::msg::RecvMsg;

use crate::engine::Engine;
use crate::util::{IdHashMap, IdHashSet, NumCell, Stack};
use rustix::io::Errno;
use rustix::io_uring::{
    IORING_OFF_SQ_RING, IORING_OFF_SQES, IoringEnterFlags, IoringFeatureFlags, IoringSetupFlags,
    IoringSqeFlags, io_uring_cqe, io_uring_enter, io_uring_params, io_uring_setup, io_uring_sqe,
    io_uring_user_data,
};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

// -- errors --

#[derive(Debug)]
pub enum RingError {
    Setup(Errno),
    MissingFeature(&'static str),
    Mmap(Errno),
    Enter(Errno),
    Destroyed,
    Os(Errno),
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingError::Setup(e) => write!(f, "io_uring_setup failed: {e}"),
            RingError::MissingFeature(what) => {
                write!(f, "kernel lacks a required io_uring feature: {what}")
            }
            RingError::Mmap(e) => write!(f, "mapping the ring failed: {e}"),
            RingError::Enter(e) => write!(f, "io_uring_enter failed: {e}"),
            RingError::Destroyed => write!(f, "the ring is shut down"),
            RingError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RingError {}

// -- op identity --

/// monotonic u64 from 1, never reused, never a storage index. 0 is the sentinel.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct OpId(u64);

// -- the op trait --

/// one impl per op kind. the box lives in Ring.ops from schedule() until its
/// cqe drains, and owns every byte the kernel can touch - that address
/// stability is the whole soundness argument, hence unsafe.
unsafe trait Op {
    fn id(&self) -> OpId;
    fn encode(&self, sqe: &mut io_uring_sqe);
    fn complete(self: Box<Self>, ring: &Ring, res: i32);
    fn is_cancel(&self) -> bool {
        false
    }
    /// a linked-timeout pair needs two contiguous sq slots
    fn has_link(&self) -> bool {
        false
    }
}

// -- completion plumbing --

#[derive(Default)]
struct Oneshot {
    res: Cell<Option<i32>>,
    waker: Cell<Option<Waker>>,
}

impl Oneshot {
    fn complete(&self, res: i32) {
        self.res.set(Some(res));
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

struct CqeFuture(Rc<Oneshot>);

impl Future for CqeFuture {
    type Output = Result<i32, RingError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.res.take() {
            Some(res) if res < 0 => Poll::Ready(Err(RingError::Os(Errno::from_raw_os_error(-res)))),
            Some(res) => Poll::Ready(Ok(res)),
            None => {
                self.0.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

/// cancel-on-drop: an op dies with its future. after normal completion the id
/// is gone from the map, so drop is a no-op.
struct OpGuard<'a> {
    id: OpId,
    ring: &'a Ring,
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        self.ring.cancel_op(self.id);
    }
}

// -- ring memory --

struct Mmap {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

impl Mmap {
    fn map(fd: &OwnedFd, len: usize, offset: u64) -> Result<Mmap, RingError> {
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED | MapFlags::POPULATE,
                fd,
                offset,
            )
        }
        .map_err(RingError::Mmap)?;
        Ok(Mmap { ptr, len })
    }

    unsafe fn at<T>(&self, off: u32) -> *const T {
        unsafe { self.ptr.cast::<u8>().add(off as usize).cast() }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

// -- the ring --

pub struct Ring {
    destroyed: Cell<bool>,
    eng: Rc<Engine>,

    /// sq and cq share the first region (SINGLE_MMAP)
    _rings: Mmap,
    _sqes_map: Mmap,
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_len: u32,
    /// legacy indirection array, written as identity per sqe
    sq_array: *const Cell<u32>,
    sqes: *const UnsafeCell<io_uring_sqe>,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const io_uring_cqe,

    fd: OwnedFd,

    next_id: NumCell<u64>,
    /// scheduled but not yet encoded. unbounded: a full sq backpressures here
    to_encode: RefCell<VecDeque<OpId>>,
    /// ids whose sqe reached the kernel; only these need ASYNC_CANCEL
    in_kernel: RefCell<IdHashSet<OpId>>,
    ops: RefCell<IdHashMap<OpId, Box<dyn Op>>>,

    /// freelists so the op hot path stops allocating once warm
    cached_timeouts: Stack<Box<ops::TimeoutOp>>,
    cached_rw: Stack<Box<ops::RwOp>>,
    cached_polls: Stack<Box<ops::PollOp>>,
    cached_cancels: Stack<Box<ops::CancelOp>>,
    cached_accepts: Stack<Box<ops::msg::AcceptOp>>,
    cached_recvmsg: Stack<Box<ops::msg::RecvmsgOp>>,
    cached_sendmsg: Stack<Box<ops::msg::SendmsgOp>>,
    cached_links: Stack<Box<ops::msg::LinkTimeoutOp>>,
}

impl Ring {
    pub fn new(eng: &Rc<Engine>, entries: u32) -> Result<Rc<Ring>, RingError> {
        let mut params = io_uring_params::default();
        // no COOP_TASKRUN: it only tunes the path DEFER_TASKRUN removes
        params.flags = IoringSetupFlags::SINGLE_ISSUER
            | IoringSetupFlags::DEFER_TASKRUN
            | IoringSetupFlags::SUBMIT_ALL;
        let fd = unsafe { io_uring_setup(entries, &mut params) }.map_err(|e| match e {
            Errno::INVAL => RingError::MissingFeature(
                "setup flags SINGLE_ISSUER | DEFER_TASKRUN | SUBMIT_ALL (kernel 6.1+)",
            ),
            e => RingError::Setup(e),
        })?;
        if !params.features.contains(IoringFeatureFlags::NODROP) {
            return Err(RingError::MissingFeature("IORING_FEAT_NODROP"));
        }
        if !params.features.contains(IoringFeatureFlags::SINGLE_MMAP) {
            return Err(RingError::MissingFeature("IORING_FEAT_SINGLE_MMAP"));
        }

        let sq_size = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_size =
            params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<io_uring_cqe>();
        let rings = Mmap::map(&fd, sq_size.max(cq_size), IORING_OFF_SQ_RING)?;
        let sqes_map = Mmap::map(
            &fd,
            params.sq_entries as usize * size_of::<io_uring_sqe>(),
            IORING_OFF_SQES,
        )?;

        unsafe {
            Ok(Rc::new(Ring {
                destroyed: Cell::new(false),
                eng: eng.clone(),
                sq_head: rings.at(params.sq_off.head),
                sq_tail: rings.at(params.sq_off.tail),
                sq_mask: *rings.at::<u32>(params.sq_off.ring_mask),
                sq_len: params.sq_entries,
                sq_array: rings.at(params.sq_off.array),
                sqes: sqes_map.at(0),
                cq_head: rings.at(params.cq_off.head),
                cq_tail: rings.at(params.cq_off.tail),
                cq_mask: *rings.at::<u32>(params.cq_off.ring_mask),
                cqes: rings.at(params.cq_off.cqes),
                _rings: rings,
                _sqes_map: sqes_map,
                fd,
                next_id: NumCell::new(1),
                to_encode: RefCell::new(VecDeque::new()),
                in_kernel: RefCell::new(IdHashSet::default()),
                ops: RefCell::new(IdHashMap::default()),
                cached_timeouts: Stack::default(),
                cached_rw: Stack::default(),
                cached_polls: Stack::default(),
                cached_cancels: Stack::default(),
                cached_accepts: Stack::default(),
                cached_recvmsg: Stack::default(),
                cached_sendmsg: Stack::default(),
                cached_links: Stack::default(),
            }))
        }
    }

    /// returns on stop() (Ok) or a non-transient enter failure (Err). always
    /// drains on the way out so no kernel op outlives the memory it points into.
    pub fn run(&self) -> Result<(), RingError> {
        let res = self.run_loop();
        self.kill();
        res
    }

    pub fn stop(&self) {
        self.kill();
    }

    fn run_loop(&self) -> Result<(), RingError> {
        let mut to_submit: u64 = 0;
        loop {
            // engine and cq to a joint fixpoint before sleeping
            loop {
                self.eng.dispatch();
                if self.destroyed.get() {
                    return Ok(());
                }
                if !self.drain_cqes() {
                    break;
                }
            }
            to_submit += self.encode() as u64;
            let (n, min_complete, flags) = if to_submit == 0 {
                // park until one cqe arrives
                (0, 1, IoringEnterFlags::GETEVENTS)
            } else if self.to_encode.borrow().is_empty() {
                (to_submit as u32, 1, IoringEnterFlags::GETEVENTS)
            } else {
                // sq was full - submit what fits, don't sleep
                (u32::MAX, 0, IoringEnterFlags::empty())
            };
            let mut submitted = 0;
            match unsafe { io_uring_enter(&self.fd, n, min_complete, flags) } {
                Ok(k) => submitted = k,
                Err(Errno::INTR | Errno::AGAIN | Errno::BUSY) => {}
                Err(e) => return Err(RingError::Enter(e)),
            }
            to_submit = to_submit.saturating_sub(submitted as u64);
            // backpressure: sqes pending but none went in. sleep for one
            // completion so the kernel can retire work, else this loop spins
            if to_submit > 0 && submitted == 0 {
                match unsafe { io_uring_enter(&self.fd, 0, 1, IoringEnterFlags::GETEVENTS) } {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(e) => return Err(RingError::Enter(e)),
                }
            }
        }
    }

    fn drain_cqes(&self) -> bool {
        unsafe {
            let mut head = (*self.cq_head).load(Ordering::Relaxed);
            let tail = (*self.cq_tail).load(Ordering::Acquire);
            if head == tail {
                return false;
            }
            while head != tail {
                let cqe = self.cqes.add((head & self.cq_mask) as usize);
                let id = OpId((*cqe).user_data.u64_());
                let res = (*cqe).res;
                head = head.wrapping_add(1);
                // publish per entry so an overflowed kernel can flush backlog
                // into the freed slots while we work
                (*self.cq_head).store(head, Ordering::Release);
                let op = self.ops.borrow_mut().remove(&id);
                self.in_kernel.borrow_mut().remove(&id);
                if let Some(op) = op {
                    op.complete(self, res);
                }
            }
        }
        true
    }

    fn encode(&self) -> usize {
        // ops borrow held across the loop is fine: no user code runs here
        let ops = self.ops.borrow();
        let mut queue = self.to_encode.borrow_mut();
        let mut encoded = 0;
        unsafe {
            loop {
                let head = (*self.sq_head).load(Ordering::Acquire);
                let tail = (*self.sq_tail).load(Ordering::Relaxed);
                let free = self.sq_len - tail.wrapping_sub(head);
                if free == 0 {
                    break;
                }
                let Some(id) = queue.pop_front() else { break };
                // cancelled between schedule and encode
                let Some(op) = ops.get(&id) else { continue };
                if op.has_link() && free < 2 {
                    // never split an op from its linked timeout
                    queue.push_front(id);
                    break;
                }
                self.in_kernel.borrow_mut().insert(id);
                let idx = (tail & self.sq_mask) as usize;
                (*self.sq_array.add(idx)).set(idx as u32);
                let sqe = &mut *(*self.sqes.add(idx)).get();
                *sqe = io_uring_sqe::default();
                sqe.user_data = io_uring_user_data::from_u64(id.0);
                op.encode(sqe);
                if op.has_link() {
                    sqe.flags |= IoringSqeFlags::IO_LINK;
                }
                (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release);
                encoded += 1;
            }
        }
        encoded
    }

    fn id_raw(&self) -> OpId {
        OpId(self.next_id.fetch_add(1))
    }

    fn guard(&self) -> OpGuard<'_> {
        OpGuard {
            id: self.id_raw(),
            ring: self,
        }
    }

    fn check_destroyed(&self) -> Result<(), RingError> {
        if self.destroyed.get() {
            Err(RingError::Destroyed)
        } else {
            Ok(())
        }
    }

    fn schedule(&self, op: Box<dyn Op>) {
        assert!(!self.destroyed.get(), "op scheduled on a dead ring");
        self.to_encode.borrow_mut().push_back(op.id());
        self.ops.borrow_mut().insert(op.id(), op);
    }

    fn cancel_op(&self, id: OpId) {
        if !self.ops.borrow().contains_key(&id) {
            // already completed
            return;
        }
        if !self.in_kernel.borrow().contains(&id) {
            // sqe never reached the kernel - resolve in userspace; encode()
            // skips the stale to_encode entry later
            let op = self.ops.borrow_mut().remove(&id).unwrap();
            op.complete(self, -Errno::CANCELED.raw_os_error());
            return;
        }
        ops::schedule_cancel(self, id);
    }

    fn kill(&self) {
        self.eng.stop();
        let targets: Vec<OpId> = self
            .ops
            .borrow()
            .iter()
            .filter(|(_, op)| !op.is_cancel())
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.cancel_op(id);
        }
        if self.destroyed.replace(true) {
            return;
        }
        // drain to empty: the kernel may still write through pointers into the
        // op boxes. crashing here beats a use-after-free.
        while !self.ops.borrow().is_empty() {
            self.encode();
            let _ = unsafe { io_uring_enter(&self.fd, u32::MAX, 0, IoringEnterFlags::empty()) };
            match unsafe { io_uring_enter(&self.fd, 0, 1, IoringEnterFlags::GETEVENTS) } {
                Ok(_) | Err(Errno::INTR) => {}
                Err(e) => panic!("could not drain the ring at shutdown: {e}"),
            }
            self.drain_cqes();
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // backstop for panics/early exits - idempotent after kill()
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Time;
    use rustix::event::{EventfdFlags, eventfd};
    use std::time::Duration;

    // NOTE: no asserts inside spawned tasks - a panicking poll aborts the whole
    // test binary. stash results, assert after run() returns.

    #[test]
    fn timeout_fires() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let done = Rc::new(Cell::new(false));
        let d = done.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            if r.timeout(Time::now() + Duration::from_millis(5)).await.is_ok() {
                d.set(true);
            }
            r.stop();
        });
        ring.run().unwrap();
        assert!(done.get());
    }

    #[test]
    fn read_returns_data() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let efd = Rc::new(eventfd(0, EventfdFlags::empty()).unwrap());
        rustix::io::write(&*efd, &1u64.to_ne_bytes()).unwrap();
        let out = Rc::new(RefCell::new(None));
        let o = out.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            let res = r.read(&efd, vec![0u8; 8]).await;
            *o.borrow_mut() = Some(res);
            r.stop();
        });
        ring.run().unwrap();
        let (buf, n) = out.borrow_mut().take().unwrap().unwrap();
        assert_eq!(n, 8);
        assert_eq!(u64::from_ne_bytes(buf.try_into().unwrap()), 1);
    }

    #[test]
    fn dropped_read_cancels() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        // counter 0 - this read never completes on its own
        let efd = Rc::new(eventfd(0, EventfdFlags::empty()).unwrap());
        let r = ring.clone();
        let victim = eng.spawn("victim", async move {
            let _ = r.read(&efd, vec![0u8; 8]).await;
        });
        let r = ring.clone();
        let _root = eng.spawn("root", async move {
            drop(victim);
            let _ = r.timeout(Time::now() + Duration::from_millis(5)).await;
            r.stop();
        });
        // a hung cancel path hangs run() itself
        ring.run().unwrap();
    }

    #[test]
    fn poll_readable() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let efd = Rc::new(eventfd(3, EventfdFlags::empty()).unwrap());
        let ok = Rc::new(Cell::new(false));
        let k = ok.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            if r.readable(&efd).await.is_ok() {
                k.set(true);
            }
            r.stop();
        });
        ring.run().unwrap();
        assert!(ok.get());
    }
}
