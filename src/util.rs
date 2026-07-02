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
