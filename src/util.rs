// single-threaded cell and time helpers shared by the runtime. nothing is Send.

use rustix::time::{ClockId, clock_gettime};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::ops::{Add, BitAnd, BitOr, Sub};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

// -- counting cell --

/// Cell<T> with fetch-style arithmetic, so counters skip the get/set pair
#[derive(Default)]
pub struct NumCell<T: Copy>(Cell<T>);

impl<T: Copy> NumCell<T> {
    pub fn new(v: T) -> Self {
        Self(Cell::new(v))
    }

    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, v: T) {
        self.0.set(v)
    }
}

impl<T: Copy + Add<Output = T>> NumCell<T> {
    pub fn fetch_add(&self, n: T) -> T {
        let old = self.0.get();
        self.0.set(old + n);
        old
    }
}

impl<T: Copy + Sub<Output = T>> NumCell<T> {
    pub fn fetch_sub(&self, n: T) -> T {
        let old = self.0.get();
        self.0.set(old - n);
        old
    }
}

impl<T: Copy + BitOr<Output = T>> NumCell<T> {
    pub fn or_assign(&self, bits: T) {
        self.0.set(self.0.get() | bits);
    }
}

impl<T: Copy + BitAnd<Output = T>> NumCell<T> {
    pub fn and_assign(&self, bits: T) {
        self.0.set(self.0.get() & bits);
    }
}

// -- free list --

pub struct Stack<T>(RefCell<Vec<T>>);

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self(RefCell::new(Vec::new()))
    }
}

impl<T> Stack<T> {
    pub fn push(&self, v: T) {
        self.0.borrow_mut().push(v);
    }

    pub fn pop(&self) -> Option<T> {
        self.0.borrow_mut().pop()
    }
}

// -- monotonic time --

/// nanoseconds of CLOCK_MONOTONIC. u64 gives us ~584 years of uptime.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time(u64);

impl Time {
    pub fn now() -> Time {
        let ts = clock_gettime(ClockId::Monotonic);
        Time(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
    }

    pub fn from_nsec(ns: u64) -> Time {
        Time(ns)
    }

    pub fn nsec(self) -> u64 {
        self.0
    }

    /// round up to the next full millisecond so nearby deadlines coalesce
    pub fn round_up_ms(self) -> Time {
        Time(self.0.div_ceil(1_000_000) * 1_000_000)
    }
}

impl Add<Duration> for Time {
    type Output = Time;

    fn add(self, d: Duration) -> Time {
        Time(self.0 + d.as_nanos() as u64)
    }
}

impl Sub<Time> for Time {
    type Output = Duration;

    fn sub(self, rhs: Time) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(rhs.0))
    }
}

// -- async event --

/// single-waiter trigger; multiple triggers coalesce, triggered() consumes all
#[derive(Default)]
pub struct AsyncEvent {
    triggers: NumCell<u32>,
    waker: Cell<Option<Waker>>,
}

impl AsyncEvent {
    pub fn trigger(&self) {
        self.triggers.fetch_add(1);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }

    pub fn clear(&self) {
        self.triggers.set(0);
        self.waker.take();
    }

    /// consume all pending triggers; true if there were any
    pub fn take(&self) -> bool {
        let had = self.triggers.get() > 0;
        self.triggers.set(0);
        had
    }

    pub fn triggered(&self) -> Triggered<'_> {
        Triggered(self)
    }
}

pub struct Triggered<'a>(&'a AsyncEvent);

impl Future for Triggered<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.triggers.get() > 0 {
            self.0.triggers.set(0);
            Poll::Ready(())
        } else {
            self.0.waker.set(Some(cx.waker().clone()));
            Poll::Pending
        }
    }
}

// -- async queue --

/// single-consumer queue; the popper parks until something arrives
pub struct AsyncQueue<T> {
    items: RefCell<VecDeque<T>>,
    waker: Cell<Option<Waker>>,
}

impl<T> Default for AsyncQueue<T> {
    fn default() -> Self {
        AsyncQueue {
            items: RefCell::new(VecDeque::new()),
            waker: Cell::new(None),
        }
    }
}

impl<T> AsyncQueue<T> {
    pub fn push(&self, v: T) {
        self.items.borrow_mut().push_back(v);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }

    pub fn pop(&self) -> Pop<'_, T> {
        Pop(self)
    }

    pub fn clear(&self) {
        self.items.borrow_mut().clear();
        self.waker.take();
    }
}

pub struct Pop<'a, T>(&'a AsyncQueue<T>);

impl<T> Future for Pop<'_, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.0.items.borrow_mut().pop_front() {
            Some(v) => Poll::Ready(v),
            None => {
                self.0.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

// -- drop guard --

pub struct OnDrop<F: FnMut()>(pub F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// wake when either event has triggers; consumes nothing - the caller take()s
/// each side to learn which fired. both slots hold the same task's waker.
pub struct EitherEvent<'a>(pub &'a AsyncEvent, pub &'a AsyncEvent);

impl Future for EitherEvent<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.triggers.get() > 0 || self.1.triggers.get() > 0 {
            return Poll::Ready(());
        }
        self.0.waker.set(Some(cx.waker().clone()));
        self.1.waker.set(Some(cx.waker().clone()));
        Poll::Pending
    }
}

// -- hashing --

/// maps keyed by our own monotonic ids don't need siphash's flood resistance;
/// one fibonacci multiply mixes plenty. client-controlled keys keep std.
#[derive(Default)]
pub struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

pub type IdHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<IdHasher>>;
pub type IdHashSet<K> = std::collections::HashSet<K, std::hash::BuildHasherDefault<IdHasher>>;

// -- deferred retirement --

/// a parking lot for values that must outlive the in-flight gpu frames
/// that may still reference them. parking snapshots the live frame set
/// (count + submission watermark); each frame completion releases the
/// batches whose frames have all fenced out. frames submitted after the
/// park latched after the replacement and never see the value, so they
/// don't extend the wait - under permanent pipelining (some frame always
/// in flight) a batch still drains one fence later, never "at idle"
pub struct RetireQueue<T> {
    batches: RefCell<Vec<RetireBatch<T>>>,
}

struct RetireBatch<T> {
    /// in-flight frames at park time still unfenced
    waits: u32,
    /// highest frame seq issued by park time; younger frames don't count
    watermark: u64,
    vals: Vec<T>,
}

impl<T> RetireQueue<T> {
    pub fn new() -> Self {
        RetireQueue { batches: RefCell::new(Vec::new()) }
    }

    /// park under the frames live right now; none in flight means nothing
    /// can reference the value and it drops on the spot
    pub fn park(&self, watermark: u64, inflight: u32, v: T) {
        if inflight == 0 {
            return;
        }
        let mut b = self.batches.borrow_mut();
        match b.last_mut() {
            Some(last) if last.waits == inflight && last.watermark == watermark => {
                last.vals.push(v);
            }
            _ => b.push(RetireBatch { waits: inflight, watermark, vals: vec![v] }),
        }
    }

    /// frame `seq`'s fence signaled (or its output died mid-await; holding
    /// forever would park the values for the session)
    pub fn frame_done(&self, seq: u64) {
        self.batches.borrow_mut().retain_mut(|b| {
            if seq <= b.watermark {
                b.waits -= 1;
            }
            b.waits > 0
        });
    }

    /// session teardown: drop everything regardless
    pub fn clear(&self) {
        self.batches.borrow_mut().clear();
    }

    #[cfg(test)]
    fn parked(&self) -> usize {
        self.batches.borrow().iter().map(|b| b.vals.len()).sum()
    }
}

#[cfg(test)]
mod retire_tests {
    use super::RetireQueue;

    #[test]
    fn nothing_in_flight_drops_on_the_spot() {
        let q: RetireQueue<u8> = RetireQueue::new();
        q.park(5, 0, 1);
        assert_eq!(q.parked(), 0);
    }

    #[test]
    fn a_batch_waits_out_exactly_the_frames_it_was_parked_under() {
        let q: RetireQueue<u8> = RetireQueue::new();
        // frames 1 and 2 in flight; the value may be sampled by both
        q.park(2, 2, 1);
        // frame 3 submits later and completes first: not a wait of ours
        q.frame_done(3);
        assert_eq!(q.parked(), 1, "a younger frame never releases the batch");
        q.frame_done(1);
        assert_eq!(q.parked(), 1);
        q.frame_done(2);
        assert_eq!(q.parked(), 0, "both parked-under frames fenced out");
    }

    #[test]
    fn pipelining_drains_one_fence_later_not_at_idle() {
        let q: RetireQueue<u8> = RetireQueue::new();
        // steady state: one frame always in flight. park under frame N,
        // frame N completes while N+1 is already flying: the batch drains
        // even though the in-flight count never touches zero
        for seq in 1..100u64 {
            q.park(seq, 1, seq as u8);
            q.frame_done(seq);
            assert_eq!(q.parked(), 0, "batch parked under frame {seq} drained at its fence");
        }
    }

    #[test]
    fn batches_under_the_same_frames_coalesce() {
        let q: RetireQueue<u8> = RetireQueue::new();
        q.park(4, 2, 1);
        q.park(4, 2, 2);
        assert_eq!(q.batches.borrow().len(), 1);
        q.park(5, 3, 3);
        assert_eq!(q.batches.borrow().len(), 2);
    }
}
