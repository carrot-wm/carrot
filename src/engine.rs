// the async engine. single threaded, everything is Rc, nothing is Send.
//
// each dispatch iteration drains four phases in order: EventHandling ->
// Layout -> PostLayout -> Present. client reads land in the first, client
// writes in PostLayout, so replies flush once per iteration after layout
// has settled.
//
// NOTE: never hold a RefCell borrow across an .await.

mod task;

pub use task::SpawnedFuture;

use crate::util::{NumCell, Time};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use task::Runnable;

// -- phases --

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    EventHandling,
    Layout,
    PostLayout,
    Present,
}

const NUM_PHASES: usize = 4;

// -- engine --

pub struct Engine {
    queues: [RefCell<VecDeque<Runnable>>; NUM_PHASES],
    // runnables sitting in the queues (the stash doesn't count)
    num_queued: NumCell<usize>,
    // completed sweeps; yield_now waits for this to move
    iteration: NumCell<u64>,
    yields: RefCell<VecDeque<Waker>>,
    // drain buffers, swapped against the live queues so a batch can run
    // while tasks keep pushing. borrowed for all of dispatch(), which is
    // what makes dispatch non-reentrant.
    stash: RefCell<VecDeque<Runnable>>,
    yield_stash: RefCell<VecDeque<Waker>>,
    stopped: Cell<bool>,
    // one timestamp per sweep - every task in a sweep sees the same now
    now: Cell<Option<Time>>,
}

impl Engine {
    pub fn new() -> Rc<Engine> {
        Rc::new(Engine {
            queues: std::array::from_fn(|_| RefCell::new(VecDeque::new())),
            num_queued: NumCell::new(0),
            iteration: NumCell::new(0),
            yields: RefCell::new(VecDeque::new()),
            stash: RefCell::new(VecDeque::new()),
            yield_stash: RefCell::new(VecDeque::new()),
            stopped: Cell::new(false),
            now: Cell::new(None),
        })
    }

    pub fn spawn<T, F>(self: &Rc<Self>, name: &'static str, f: F) -> SpawnedFuture<T>
    where
        T: 'static,
        F: Future<Output = T> + 'static,
    {
        task::spawn(self, name, Phase::EventHandling, f)
    }

    pub fn spawn2<T, F>(self: &Rc<Self>, name: &'static str, phase: Phase, f: F) -> SpawnedFuture<T>
    where
        T: 'static,
        F: Future<Output = T> + 'static,
    {
        task::spawn(self, name, phase, f)
    }

    // run every queued task to quiescence. each phase drains to a fixed
    // point before the next one starts; a wake into an earlier phase waits
    // for the next sweep.
    pub fn dispatch(&self) {
        let mut stash = self.stash.borrow_mut();
        let mut yield_stash = self.yield_stash.borrow_mut();
        while self.num_queued.get() > 0 {
            self.now.set(None);
            let mut phase = 0;
            while phase < NUM_PHASES {
                std::mem::swap(&mut *self.queues[phase].borrow_mut(), &mut *stash);
                if stash.is_empty() {
                    phase += 1;
                    continue;
                }
                self.num_queued.fetch_sub(stash.len());
                while let Some(r) = stash.pop_front() {
                    r.run();
                    if self.stopped.get() {
                        self.now.set(None);
                        return;
                    }
                }
                // no phase += 1: re-swap the same phase until it comes up
                // empty, so same-phase wakes run in this sweep
            }
            self.iteration.fetch_add(1);
            std::mem::swap(&mut *self.yields.borrow_mut(), &mut *yield_stash);
            while let Some(w) = yield_stash.pop_front() {
                w.wake();
            }
        }
        // the sweep timestamp is only coherent inside dispatch; anyone
        // asking between dispatches (arming a timer from the outside, say)
        // gets a fresh read instead of however-old the last sweep is
        self.now.set(None);
    }

    pub fn stop(&self) {
        self.stopped.set(true);
    }

    // drop everything still queued. must follow stop() - dispatch bails
    // mid-batch and the leftovers only get cleaned up here.
    pub fn clear(&self) {
        // take-then-drop: a runnable's destructor can wake a live task,
        // which pushes into the very queue being cleared - dropping under
        // the borrow would panic. dropped payloads can also enqueue fresh
        // runnables, so sweep until everything stays empty.
        loop {
            let mut any = false;
            for q in &self.queues {
                let drained = std::mem::take(&mut *q.borrow_mut());
                any |= !drained.is_empty();
                drop(drained);
            }
            let drained = std::mem::take(&mut *self.stash.borrow_mut());
            any |= !drained.is_empty();
            drop(drained);
            if !any {
                break;
            }
        }
        self.num_queued.set(0);
        drop(std::mem::take(&mut *self.yields.borrow_mut()));
        drop(std::mem::take(&mut *self.yield_stash.borrow_mut()));
    }

    pub fn iteration(&self) -> u64 {
        self.iteration.get()
    }

    pub fn now(&self) -> Time {
        match self.now.get() {
            Some(t) => t,
            None => {
                let t = Time::now();
                self.now.set(Some(t));
                t
            }
        }
    }

    pub fn yield_now(self: &Rc<Self>) -> Yield {
        Yield {
            seen: self.iteration.get(),
            eng: self.clone(),
        }
    }

    fn push(&self, phase: Phase, r: Runnable) {
        self.queues[phase as usize].borrow_mut().push_back(r);
        self.num_queued.fetch_add(1);
    }

    fn push_yield(&self, w: Waker) {
        self.yields.borrow_mut().push_back(w);
    }
}

// -- yield --

// resolves once a full sweep has completed after creation. this is the
// fairness valve: a task that re-wakes itself without ever yielding keeps
// its phase queue busy and dispatch never returns to the ring.
pub struct Yield {
    seen: u64,
    eng: Rc<Engine>,
}

impl Future for Yield {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.eng.iteration.get() > self.seen {
            Poll::Ready(())
        } else {
            self.eng.push_yield(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_order() {
        let eng = Engine::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut keep = Vec::new();
        for (phase, tag) in [
            (Phase::Present, "present"),
            (Phase::PostLayout, "post"),
            (Phase::EventHandling, "event"),
            (Phase::Layout, "layout"),
        ] {
            let log = log.clone();
            keep.push(eng.spawn2("test", phase, async move {
                log.borrow_mut().push(tag);
            }));
        }
        eng.dispatch();
        assert_eq!(*log.borrow(), ["event", "layout", "post", "present"]);
    }

    #[test]
    fn completion_value() {
        let eng = Engine::new();
        let child = eng.spawn("child", async { 7 });
        let got = Rc::new(Cell::new(0));
        let got2 = got.clone();
        let _parent = eng.spawn("parent", async move {
            got2.set(child.await);
        });
        eng.dispatch();
        assert_eq!(got.get(), 7);
    }

    #[test]
    fn drop_is_cancel() {
        struct SetOnDrop(Rc<Cell<bool>>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let eng = Engine::new();
        let ran = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let guard = SetOnDrop(dropped.clone());
        let ran2 = ran.clone();
        let t = eng.spawn("test", async move {
            let _guard = guard;
            ran2.set(true);
        });
        drop(t);
        eng.dispatch();
        assert!(!ran.get());
        assert!(dropped.get());
    }

    #[test]
    fn clear_survives_wake_on_drop() {
        // a cancelled task's destructor wakes a live task from inside
        // clear() - the push must not hit a held queue borrow
        struct WakeOnDrop(Rc<Cell<Option<std::task::Waker>>>);
        impl Drop for WakeOnDrop {
            fn drop(&mut self) {
                if let Some(w) = self.0.take() {
                    w.wake();
                }
            }
        }
        struct Park(Rc<Cell<Option<std::task::Waker>>>);
        impl Future for Park {
            type Output = ();
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                self.0.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
        let eng = Engine::new();
        let slot = Rc::new(Cell::new(None));
        let _parked = eng.spawn("parked", Park(slot.clone()));
        eng.dispatch();
        let guard = WakeOnDrop(slot);
        let owner = eng.spawn("owner", async move {
            let _guard = guard;
            std::future::pending::<()>().await
        });
        drop(owner);
        eng.stop();
        eng.clear();
    }

    #[test]
    fn yield_waits_for_full_sweep() {
        let eng = Engine::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let l1 = log.clone();
        let e1 = eng.clone();
        let _a = eng.spawn("a", async move {
            l1.borrow_mut().push("a1");
            e1.yield_now().await;
            l1.borrow_mut().push("a2");
        });
        let l2 = log.clone();
        let _b = eng.spawn2("b", Phase::Present, async move {
            l2.borrow_mut().push("b");
        });
        eng.dispatch();
        assert_eq!(*log.borrow(), ["a1", "b", "a2"]);
    }
}
