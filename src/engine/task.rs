// task handles + the per-phase run queues. dropping the handle cancels the task.
//
// one heap alloc per task: a Task<T, F> holding either the live future or its
// result in a union, driven through hand-rolled vtables so SpawnedFuture<T> is
// type-erased over F. the flag discipline that keeps the unsafe sound:
//   RUNNING    a runnable is queued or mid-poll. at most one runnable per task -
//              wakes while RUNNING set RUN_AGAIN
//   COMPLETED  the union holds the result, not the future
//   EMPTIED    the result was moved out through the handle
//   CANCELLED  the handle was dropped
// the union payload is destroyed exactly once, by whichever of handle-drop /
// runnable-run / runnable-drop observes RUNNING clear + CANCELLED set (or by
// completion consuming the future in place).
// refcount holders: the handle (1 from spawn), each queued runnable, each live
// waker. all plain Cells - a waker built here must never leave this thread.

use super::{Engine, Phase};
use crate::util::NumCell;
use std::cell::{Cell, UnsafeCell};
use std::future::Future;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::ptr;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// -- state bits --

const RUNNING: u32 = 1;
const RUN_AGAIN: u32 = 2;
const COMPLETED: u32 = 4;
const EMPTIED: u32 = 8;
const CANCELLED: u32 = 16;

union TaskData<T, F> {
    future: ManuallyDrop<F>,
    result: ManuallyDrop<T>,
}

struct Task<T, F> {
    ref_count: NumCell<u64>,
    phase: Phase,
    state: NumCell<u32>,
    data: UnsafeCell<TaskData<T, F>>,
    /// waker of whoever awaits the handle
    waker: Cell<Option<Waker>>,
    engine: Rc<Engine>,
    #[allow(dead_code)]
    name: &'static str,
}

/// a panic through a half-polled task would leave the union in an indeterminate
/// variant - abort instead
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        std::process::abort();
    }
}

pub(super) fn spawn<T, F>(
    engine: &Rc<Engine>,
    name: &'static str,
    phase: Phase,
    f: F,
) -> SpawnedFuture<T>
where
    T: 'static,
    F: Future<Output = T> + 'static,
{
    let task = Box::new(Task {
        ref_count: NumCell::new(1),
        phase,
        state: NumCell::new(0),
        data: UnsafeCell::new(TaskData {
            future: ManuallyDrop::new(f),
        }),
        waker: Cell::new(None),
        engine: engine.clone(),
        name,
    });
    let ptr = Box::into_raw(task);
    unsafe {
        (*ptr).schedule_run();
    }
    crate::trace!("spawned task {}", name);
    SpawnedFuture {
        vtable: SpawnedFutureVTableProxy::<T, F>::VTABLE,
        data: ptr as *mut u8,
    }
}

// -- the handle --

/// awaiting yields the task's output; dropping cancels it
#[must_use]
pub struct SpawnedFuture<T: 'static> {
    vtable: &'static SpawnedFutureVtable<T>,
    data: *mut u8,
}

struct SpawnedFutureVtable<T> {
    poll: unsafe fn(*mut u8, &mut Context<'_>) -> Poll<T>,
    drop: unsafe fn(*mut u8),
}

impl<T> Future for SpawnedFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        unsafe { (self.vtable.poll)(self.data, cx) }
    }
}

impl<T> Drop for SpawnedFuture<T> {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self.data) }
    }
}

struct SpawnedFutureVTableProxy<T, F>(PhantomData<(T, F)>);

impl<T: 'static, F: Future<Output = T> + 'static> SpawnedFutureVTableProxy<T, F> {
    const VTABLE: &'static SpawnedFutureVtable<T> = &SpawnedFutureVtable {
        poll: Self::poll,
        drop: Self::drop,
    };

    unsafe fn poll(data: *mut u8, cx: &mut Context<'_>) -> Poll<T> {
        unsafe {
            let task = &*(data as *const Task<T, F>);
            if task.state.get() & COMPLETED == 0 {
                // single awaiter - newest waker wins
                task.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            } else if task.state.get() & EMPTIED == 0 {
                task.state.or_assign(EMPTIED);
                Poll::Ready(ptr::read(&*(*task.data.get()).result))
            } else {
                panic!("SpawnedFuture polled after its value was taken");
            }
        }
    }

    unsafe fn drop(data: *mut u8) {
        unsafe {
            let task = data as *const Task<T, F>;
            (*task).state.or_assign(CANCELLED);
            // if a runnable is queued or mid-poll, it owns cleanup
            if (*task).state.get() & RUNNING == 0 {
                (*task).drop_data();
            }
            Task::dec_ref(task);
        }
    }
}

// -- queue entries --

/// one pending execution of one task; owns +1 refcount. the bool picks run vs
/// cancelled-cleanup.
pub(super) struct Runnable {
    data: *const u8,
    run: unsafe fn(*const u8, bool),
}

impl Runnable {
    pub(super) fn run(self) {
        let this = ManuallyDrop::new(self);
        unsafe { (this.run)(this.data, true) }
    }
}

impl Drop for Runnable {
    // queue cleared without running (engine.clear after stop)
    fn drop(&mut self) {
        unsafe { (self.run)(self.data, false) }
    }
}

// -- task internals --

impl<T: 'static, F: Future<Output = T> + 'static> Task<T, F> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::waker_clone,
        Self::waker_wake,
        Self::waker_wake_by_ref,
        Self::waker_drop,
    );

    unsafe fn run_proxy(data: *const u8, run: bool) {
        let task = data as *const Task<T, F>;
        unsafe {
            if run {
                (*task).run();
            } else {
                (*task).runnable_dropped();
            }
            Self::dec_ref(task);
        }
    }

    unsafe fn runnable_dropped(&self) {
        self.state.and_assign(!RUNNING);
        if self.state.get() & CANCELLED != 0 {
            unsafe {
                self.drop_data();
            }
        }
    }

    unsafe fn schedule_run(&self) {
        if self.state.get() & (COMPLETED | CANCELLED) != 0 {
            // stale wakers are harmless
            return;
        }
        if self.state.get() & RUNNING != 0 {
            self.state.or_assign(RUN_AGAIN);
            return;
        }
        self.state.or_assign(RUNNING);
        self.ref_count.fetch_add(1);
        self.engine.push(
            self.phase,
            Runnable {
                data: self as *const _ as *const u8,
                run: Self::run_proxy,
            },
        );
    }

    unsafe fn run(&self) {
        // cancelled while queued - never poll a future whose owner believes it dead
        if self.state.get() & CANCELLED != 0 {
            self.state.and_assign(!RUNNING);
            unsafe {
                self.drop_data();
            }
            return;
        }
        unsafe {
            self.ref_count.fetch_add(1);
            let waker = Waker::from_raw(RawWaker::new(
                self as *const _ as *const (),
                &Self::WAKER_VTABLE,
            ));
            let mut cx = Context::from_waker(&waker);
            let guard = AbortOnUnwind;
            let t0 = crate::util::Time::now();
            let poll = Pin::new_unchecked(&mut *(*self.data.get()).future).poll(&mut cx);
            std::mem::forget(guard);
            // single-threaded loop: a long poll stalls everything, so name the offender
            let held = crate::util::Time::now().nsec().saturating_sub(t0.nsec());
            if held > 30_000_000 {
                eprintln!(
                    "carrot: stall: task '{}' held the loop {}ms",
                    self.name,
                    held / 1_000_000
                );
            }
            if let Poll::Ready(v) = poll {
                ManuallyDrop::drop(&mut (*self.data.get()).future);
                ptr::write(&raw mut (*self.data.get()).result, ManuallyDrop::new(v));
                self.state.or_assign(COMPLETED);
                if let Some(w) = self.waker.take() {
                    w.wake();
                }
            }
        }
        self.state.and_assign(!RUNNING);
        if self.state.get() & CANCELLED != 0 {
            unsafe {
                self.drop_data();
            }
        } else if self.state.get() & RUN_AGAIN != 0 {
            // clear first or the re-schedule loses the wake
            self.state.and_assign(!RUN_AGAIN);
            unsafe {
                self.schedule_run();
            }
        }
    }

    /// exactly-once payload destruction, variant picked by the flags
    unsafe fn drop_data(&self) {
        let state = self.state.get();
        unsafe {
            if state & COMPLETED == 0 {
                ManuallyDrop::drop(&mut (*self.data.get()).future);
            } else if state & EMPTIED == 0 {
                ManuallyDrop::drop(&mut (*self.data.get()).result);
            }
        }
    }

    unsafe fn dec_ref(task: *const Task<T, F>) {
        unsafe {
            if (*task).ref_count.fetch_sub(1) == 1 {
                drop(Box::from_raw(task as *mut Task<T, F>));
            }
        }
    }

    // -- waker vtable --

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        unsafe {
            (*(data as *const Task<T, F>)).ref_count.fetch_add(1);
        }
        RawWaker::new(data, &Self::WAKER_VTABLE)
    }

    unsafe fn waker_wake(data: *const ()) {
        unsafe {
            Self::waker_wake_by_ref(data);
            Self::waker_drop(data);
        }
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        unsafe {
            (*(data as *const Task<T, F>)).schedule_run();
        }
    }

    unsafe fn waker_drop(data: *const ()) {
        unsafe {
            Self::dec_ref(data as *const Task<T, F>);
        }
    }
}
