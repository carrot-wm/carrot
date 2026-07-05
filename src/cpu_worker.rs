// blocking work off the main thread. the worker is a second copy of the
// runtime - its own engine and ring on its own os thread.
//
// a job splits in two: CpuJob stays main-thread (owns Rcs, gets completed()
// back); CpuWork crosses as a raw pointer in a Send envelope. INVARIANT:
// between submit and completion the main thread must not touch the work -
// that is what makes the Send impl sound; the mutex handoffs are the fences.
//
// doorbells are counting eventfds: main writes jobs_fd, the worker writes
// done_fd, each side reads its own bell through its own ring.

use crate::engine::{Engine, SpawnedFuture};
use crate::uring::Ring;
use crate::util::NumCell;
use rustix::event::{EventfdFlags, eventfd};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::mem;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CpuJobId(u64);

// -- the job traits --

/// main-thread half. work() exposes the crossing part; completed() runs back
/// on the main thread afterwards.
pub trait CpuJob {
    fn work(&mut self) -> &mut dyn CpuWork;
    fn completed(self: Box<Self>);
}

/// worker half. run() returning Some continues the job as a future on the
/// worker's own engine/ring.
pub trait CpuWork: Send {
    fn run(&mut self) -> Option<Box<dyn AsyncCpuWork>>;

    fn cancel_async(&mut self, ring: &Rc<Ring>) {
        let _ = ring;
        unreachable!("cancel_async on synchronous work");
    }

    fn async_work_done(&mut self, work: Box<dyn AsyncCpuWork>) {
        let _ = work;
        unreachable!("async_work_done on synchronous work");
    }
}

pub trait AsyncCpuWork: std::any::Any {
    fn run(
        self: Box<Self>,
        eng: &Rc<Engine>,
        ring: &Rc<Ring>,
        completion: WorkCompletion,
    ) -> SpawnedFuture<CompletedWork>;
}

/// type-state proof of completion; only constructible through complete()
pub struct CompletedWork(());

pub struct WorkCompletion {
    worker: Rc<Worker>,
    id: CpuJobId,
}

impl WorkCompletion {
    pub fn complete(self, work: Box<dyn AsyncCpuWork>) -> CompletedWork {
        let job = self.worker.async_jobs.borrow_mut().remove(&self.id);
        if let Some(j) = job {
            // hand the box back so the work can reclaim its state
            unsafe {
                (*j.work).async_work_done(work);
            }
        }
        self.worker.send_completion(self.id);
        CompletedWork(())
    }
}

// -- cross-thread plumbing --

enum Job {
    New { id: CpuJobId, work: *mut dyn CpuWork },
    Cancel { id: CpuJobId },
    Stop,
}

// the pointer is only dereferenced on the worker, between the mutex handoffs;
// New and Cancel share one queue so a cancel can never overtake its job
unsafe impl Send for Job {}

struct Shared {
    new_jobs: Mutex<VecDeque<Job>>,
    completions: Mutex<VecDeque<CpuJobId>>,
    sync_wake: Condvar,
}

// -- main-thread side --

pub struct CpuWorker {
    data: Rc<CpuWorkerData>,
}

struct CpuWorkerData {
    next_id: NumCell<u64>,
    shared: Arc<Shared>,
    jobs_fd: OwnedFd, // write side of the worker's doorbell
    pending: RefCell<HashMap<CpuJobId, Rc<PendingJobData>>>,
    listener: Cell<Option<SpawnedFuture<()>>>,
    thread: Cell<Option<thread::JoinHandle<()>>>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum PendingState {
    Waiting,
    Abandoned,
    Completed,
}

struct PendingJobData {
    id: CpuJobId,
    job: Cell<Option<*mut dyn CpuJob>>,
    state: Cell<PendingState>,
}

impl CpuWorker {
    pub fn new(eng: &Rc<Engine>, ring: &Rc<Ring>) -> Result<CpuWorker, std::io::Error> {
        let jobs_fd = eventfd(0, EventfdFlags::CLOEXEC)?;
        let done_fd = eventfd(0, EventfdFlags::CLOEXEC)?;
        let jobs_rx = jobs_fd.try_clone()?;
        let done_tx = done_fd.try_clone()?;
        let shared = Arc::new(Shared {
            new_jobs: Mutex::new(VecDeque::new()),
            completions: Mutex::new(VecDeque::new()),
            sync_wake: Condvar::new(),
        });
        let ws = shared.clone();
        let thread = thread::Builder::new()
            .name("cpu worker".into())
            .spawn(move || worker_main(ws, jobs_rx, done_tx))?;
        let data = Rc::new(CpuWorkerData {
            next_id: NumCell::new(1),
            shared,
            jobs_fd,
            pending: RefCell::new(HashMap::new()),
            listener: Cell::new(None),
            thread: Cell::new(Some(thread)),
        });
        let d = data.clone();
        let r = ring.clone();
        let bell = Rc::new(done_fd);
        data.listener.set(Some(eng.spawn("cpu completions", async move {
            let mut buf = vec![0u8; 8];
            loop {
                match r.read(&bell, buf).await {
                    Ok((b, _)) => {
                        buf = b;
                        d.dispatch_completions();
                    }
                    Err(_e) => {
                        crate::trace!("cpu completion bell died: {}", _e);
                        return;
                    }
                }
            }
        })));
        Ok(CpuWorker { data })
    }

    pub fn submit(&self, job: Box<dyn CpuJob>) -> PendingJob {
        let id = CpuJobId(self.data.next_id.fetch_add(1));
        let jp = Box::into_raw(job);
        let work: *mut dyn CpuWork = unsafe { (*jp).work() };
        let pd = Rc::new(PendingJobData {
            id,
            job: Cell::new(Some(jp)),
            state: Cell::new(PendingState::Waiting),
        });
        self.data.pending.borrow_mut().insert(id, pd.clone());
        self.data
            .shared
            .new_jobs
            .lock()
            .unwrap()
            .push_back(Job::New { id, work });
        self.data.ring_bell();
        PendingJob {
            data: pd,
            worker: self.data.clone(),
        }
    }
}

impl Drop for CpuWorker {
    fn drop(&mut self) {
        if !self.data.pending.borrow().is_empty() {
            // completed() will never run; the boxes leak on purpose
            crate::trace!("cpu worker dropped with jobs pending");
        }
        self.data.listener.take();
        self.data
            .shared
            .new_jobs
            .lock()
            .unwrap()
            .push_back(Job::Stop);
        self.data.ring_bell();
        if let Some(t) = self.data.thread.take() {
            let _ = t.join();
        }
    }
}

impl CpuWorkerData {
    fn ring_bell(&self) {
        // losing the bell means losing the worker; treat write failure as fatal
        if rustix::io::write(&self.jobs_fd, &1u64.to_ne_bytes()).is_err() {
            panic!("cpu worker doorbell write failed");
        }
    }

    fn dispatch_completions(&self) {
        let done = mem::take(&mut *self.shared.completions.lock().unwrap());
        for id in done {
            let Some(pd) = self.pending.borrow_mut().remove(&id) else {
                continue;
            };
            let Some(jp) = pd.job.take() else { continue };
            let job = unsafe { Box::from_raw(jp) };
            if pd.state.get() == PendingState::Waiting {
                pd.state.set(PendingState::Completed);
                job.completed();
            }
            // abandoned: drop the box, its Drop impls still run
        }
    }
}

// -- the pending handle --

#[must_use]
pub struct PendingJob {
    data: Rc<PendingJobData>,
    worker: Rc<CpuWorkerData>,
}

impl PendingJob {
    /// fire and forget: the job still runs, completed() is skipped
    pub fn detach(self) {
        if self.data.state.get() == PendingState::Waiting {
            self.data.state.set(PendingState::Abandoned);
        }
    }
}

impl Drop for PendingJob {
    fn drop(&mut self) {
        if self.data.state.get() != PendingState::Waiting {
            return;
        }
        // detach() plus a cancel hint so the worker can skip unwanted work.
        // the job box stays owned by `pending` until completion drains.
        crate::trace!("PendingJob dropped before completion, cancelling");
        self.data.state.set(PendingState::Abandoned);
        self.worker
            .shared
            .new_jobs
            .lock()
            .unwrap()
            .push_back(Job::Cancel { id: self.data.id });
        self.worker.ring_bell();
    }
}

// -- worker-thread side --

struct AsyncJob {
    _future: SpawnedFuture<CompletedWork>,
    work: *mut dyn CpuWork,
}

struct Worker {
    shared: Arc<Shared>,
    done_fd: OwnedFd,
    eng: Rc<Engine>,
    ring: Rc<Ring>,
    async_jobs: RefCell<HashMap<CpuJobId, AsyncJob>>,
}

fn worker_main(shared: Arc<Shared>, jobs_rx: OwnedFd, done_tx: OwnedFd) {
    let eng = Engine::new();
    let ring = match Ring::new(&eng, 32) {
        Ok(r) => r,
        Err(e) => panic!("cpu worker ring setup failed: {e}"),
    };
    let worker = Rc::new(Worker {
        shared,
        done_fd: done_tx,
        eng: eng.clone(),
        ring: ring.clone(),
        async_jobs: RefCell::new(HashMap::new()),
    });
    let w = worker.clone();
    let r = ring.clone();
    let _jobs = eng.spawn("cpu jobs", async move {
        w.handle_jobs(&r, jobs_rx).await;
    });
    if let Err(e) = ring.run() {
        panic!("cpu worker ring died: {e}");
    }
    eng.clear();
}

impl Worker {
    async fn handle_jobs(self: &Rc<Self>, ring: &Rc<Ring>, bell: OwnedFd) {
        let bell = Rc::new(bell);
        let mut buf = vec![0u8; 8];
        loop {
            // drain before waiting - the bell may have rung already
            loop {
                let jobs = mem::take(&mut *self.shared.new_jobs.lock().unwrap());
                if jobs.is_empty() {
                    break;
                }
                for j in jobs {
                    if self.handle_job(j) {
                        self.ring.stop();
                        return;
                    }
                }
            }
            match ring.read(&bell, buf).await {
                Ok((b, _)) => buf = b,
                Err(_) => return,
            }
        }
    }

    /// returns true on Stop
    fn handle_job(self: &Rc<Self>, j: Job) -> bool {
        match j {
            Job::New { id, work } => {
                let w = unsafe { &mut *work };
                match w.run() {
                    None => self.send_completion(id),
                    Some(aw) => {
                        let completion = WorkCompletion {
                            worker: self.clone(),
                            id,
                        };
                        let fut = aw.run(&self.eng, &self.ring, completion);
                        self.async_jobs
                            .borrow_mut()
                            .insert(id, AsyncJob { _future: fut, work });
                    }
                }
                false
            }
            Job::Cancel { id } => {
                // sync work can't be interrupted; only async work shortens
                let work = self.async_jobs.borrow().get(&id).map(|j| j.work);
                if let Some(work) = work {
                    unsafe {
                        (*work).cancel_async(&self.ring);
                    }
                }
                false
            }
            Job::Stop => {
                if !self.async_jobs.borrow().is_empty() {
                    crate::trace!("cpu worker stopping with async jobs in flight");
                }
                true
            }
        }
    }

    fn send_completion(&self, id: CpuJobId) {
        self.shared.completions.lock().unwrap().push_back(id);
        self.shared.sync_wake.notify_all();
        if rustix::io::write(&self.done_fd, &1u64.to_ne_bytes()).is_err() {
            panic!("cpu worker completion bell write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Time;
    use std::time::Duration;

    struct DoubleWork {
        input: u64,
        out: u64,
    }

    impl CpuWork for DoubleWork {
        fn run(&mut self) -> Option<Box<dyn AsyncCpuWork>> {
            thread::sleep(Duration::from_millis(2));
            self.out = self.input * 2;
            None
        }
    }

    struct DoubleJob {
        work: DoubleWork,
        done: Rc<Cell<u64>>,
    }

    impl CpuJob for DoubleJob {
        fn work(&mut self) -> &mut dyn CpuWork {
            &mut self.work
        }

        fn completed(self: Box<Self>) {
            self.done.set(self.work.out);
        }
    }

    #[test]
    fn roundtrip() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let cw = CpuWorker::new(&eng, &ring).unwrap();
        let done = Rc::new(Cell::new(0u64));
        let p = cw.submit(Box::new(DoubleJob {
            work: DoubleWork { input: 21, out: 0 },
            done: done.clone(),
        }));
        let d = done.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            // hold the handle so completed() runs; state is Completed by drop
            let _p = p;
            for _ in 0..500 {
                if d.get() != 0 {
                    break;
                }
                let _ = r.timeout(Time::now() + Duration::from_millis(2)).await;
            }
            r.stop();
        });
        ring.run().unwrap();
        drop(cw);
        assert_eq!(done.get(), 42);
    }

    struct FlagWork(Arc<std::sync::atomic::AtomicBool>);

    impl CpuWork for FlagWork {
        fn run(&mut self) -> Option<Box<dyn AsyncCpuWork>> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }

    struct FlagJob {
        work: FlagWork,
        completed: Rc<Cell<bool>>,
    }

    impl CpuJob for FlagJob {
        fn work(&mut self) -> &mut dyn CpuWork {
            &mut self.work
        }

        fn completed(self: Box<Self>) {
            self.completed.set(true);
        }
    }

    #[test]
    fn detached_job_still_runs_but_skips_completed() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let cw = CpuWorker::new(&eng, &ring).unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Rc::new(Cell::new(false));
        cw.submit(Box::new(FlagJob {
            work: FlagWork(ran.clone()),
            completed: completed.clone(),
        }))
        .detach();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            let _ = r.timeout(Time::now() + Duration::from_millis(30)).await;
            r.stop();
        });
        ring.run().unwrap();
        drop(cw);
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!completed.get());
    }
}
