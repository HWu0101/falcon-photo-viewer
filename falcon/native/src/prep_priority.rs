//! Lower Windows priority for work Falcon does ahead of the user.
//!
//! Round 3 step (a) of [the sharing plan](../../../docs/planning/gpu-memory-sharing.md) (owner go,
//! 25 October 2026). The photos the user asked for (`support::ExplicitSet`: the photo on screen,
//! both Compare halves, the hovered Review tile, the frame a stopped browse waits for) and the
//! thumbnails on screen decode at normal priority, as before. Everything prepared ahead runs below
//! normal, so another program that wants the processor gets it first; when nothing else competes, a
//! below-normal thread still gets the whole machine.
//!
//! - **One place per worker** ([`Registry::worker`]): the worker's own thread and a rayon pool only
//!   that worker hands work to. `jpeg-decoder` (`in_place_scope`), `falcon-color`, rawler and
//!   jxl-oxide split their work across whichever rayon pool they run in, so lowering only the
//!   worker thread would leave that work at normal priority; inside the pool's `install` it stays
//!   on the worker's own threads.
//! - **Jobs never nest.** A rayon thread waiting for its helpers runs other work from its own pool,
//!   and with one pool for every worker that was another worker's whole job, on the same thread
//!   (the round's review, R1). One worker per pool and one job at a time per worker rule that
//!   out, so the thread-local contracts the jobs rely on (the HEIC supersession watch) hold.
//! - **Raising** ([`Registry::publish`], [`Registry::promote_thumb`]): the tick publishes the
//!   requested set once per tick, and a thumbnail tile that comes on screen promotes its job. Under
//!   the one lock, a running low job that is now asked for is raised with every thread doing its
//!   work (R2), and its guard puts them back when it ends, so the next job does not inherit it.
//!   A thumbnail leaves its queue through the registry ([`Worker::hand_over`]) and is held for its
//!   worker until it starts, so a tile that comes on screen as the worker takes it is never missed
//!   (the re-checks, R6 and R7).
//! - **Previews and thumbnails** run their whole job on the pool ([`Worker::run`]). **The
//!   full-resolution worker** owns thread-bound state (the GPU RAW developer, nvJPEG, the HEIC
//!   lane's supersession watch), so it stays put, lowers its own thread ([`Worker::enter`]) and
//!   hands its CPU decode and colour conversion to its pool ([`Guard::on_low_pool`]).
//!
//! Windows only. Elsewhere the registry stays off and every job runs exactly as before. Never
//! Windows' background-processing mode, never the whole process.
use crate::*;

/// The levels a job moves its threads between. `Usual` is the thread's own level, read once when it
/// first registers: normal for a worker's own thread, below normal for its pool's threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Low,
    Normal,
    Usual,
}

/// What a job is, as far as its priority goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    /// A preview or a full-resolution frame of photo `idx`: the published requested set decides.
    Frame(usize),
    /// A thumbnail, named the way its queue names it. `on_screen` is the request's own flag when
    /// the worker took it; a tile that comes on screen later raises it ([`Registry::promote_thumb`]).
    Thumb { idx: usize, gen: u64, delta: u8, on_screen: bool },
}

impl Job {
    fn idx(self) -> usize {
        match self {
            Job::Frame(idx) | Job::Thumb { idx, .. } => idx,
        }
    }
}

/// One thread, as the registry can reach it from another thread. `raw` is a real handle the thread
/// opened on itself (0 when there is none: off Windows, in the logic tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThreadRef {
    pub(crate) key: u64,
    raw: isize,
    usual: i32,
}

/// Sets one thread's level: the Windows call in production, a recorder in the logic tests.
pub(crate) type SetLevel = fn(ThreadRef, Level);

/// A worker's threads: its own, and its pool's as each starts. `held`: the thumbnail it has taken
/// from the queue and not yet started ([`Worker::hand_over`]).
struct WorkerThreads {
    owner: ThreadRef,
    helpers: Vec<ThreadRef>,
    held: Option<Held>,
}

/// A thumbnail between its queue and its start, named the way its queue names it (photo, folder
/// generation, rotation). `asked`: its tile came on screen meanwhile.
#[derive(Clone, Copy)]
struct Held {
    key: (usize, u64, u8),
    asked: bool,
}

/// A low job, from its start until its guard drops. At most one per worker.
struct Running {
    worker: usize,
    job: Job,
    low: Arc<AtomicBool>,
}

struct State {
    explicit: Option<support::ExplicitSet>,
    workers: Vec<WorkerThreads>,
    running: Vec<Running>,
}

pub(crate) struct Registry {
    enabled: AtomicBool,
    state: Mutex<State>,
    set: SetLevel,
}

/// The process's registry. Off until [`Registry::configure`] turns it on at boot (Windows).
pub(crate) static PREP: Registry = Registry::new(os_set);

/// One worker's place in the registry: made on the worker's own thread before its first job, and
/// used only there.
pub(crate) struct Worker {
    reg: &'static Registry,
    /// The worker's slot in the registry; `None` while the registry is off.
    id: Option<usize>,
    pool: Option<rayon::ThreadPool>,
}

/// A job's hold on its worker, released on every exit path, a caught panic included. It borrows
/// the worker mutably, so a worker cannot start a second job inside its first.
pub(crate) struct Guard<'a> {
    worker: &'a mut Worker,
    registered: bool,
    low: Arc<AtomicBool>,
}

impl Guard<'_> {
    /// Is this job still below normal? False once it has been raised.
    pub(crate) fn is_low(&self) -> bool {
        self.low.load(Ordering::Relaxed)
    }

    /// Work this job hands to rayon: on the worker's pool while the job is low, on the caller
    /// otherwise. A raise while it runs reaches the pool's threads too.
    pub(crate) fn on_low_pool<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        match &self.worker.pool {
            Some(pool) if self.is_low() => pool.install(f),
            _ => f(),
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let (Some(id), true) = (self.worker.id, self.registered) else { return };
        let reg = self.worker.reg;
        let mut s = reg.lock();
        s.running.retain(|r| r.worker != id);
        // Under the lock and after the entry is gone, so no raise can land after these restores. A
        // job that was raised took its pool's threads with it; they go back below normal here,
        // before the worker can start its next job.
        let w = &s.workers[id];
        (reg.set)(w.owner, Level::Usual);
        if !self.is_low() {
            for &h in &w.helpers {
                (reg.set)(h, Level::Usual);
            }
        }
    }
}

impl Registry {
    pub(crate) const fn new(set: SetLevel) -> Registry {
        Registry {
            enabled: AtomicBool::new(false),
            state: Mutex::new(State { explicit: None, workers: Vec::new(), running: Vec::new() }),
            set,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Boot, once, before the first worker takes its place.
    pub(crate) fn configure(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// A worker's place, with a pool of `threads` threads started below normal. Off: no pool,
    /// nothing registered, every job runs exactly as before. A pool that cannot start leaves the
    /// worker lowering its own thread alone.
    pub(crate) fn worker(&'static self, threads: usize) -> Worker {
        if !self.enabled() {
            return Worker { reg: self, id: None, pool: None };
        }
        let id = {
            let mut s = self.lock();
            s.workers.push(WorkerThreads { owner: ThreadRef::current(), helpers: Vec::new(), held: None });
            s.workers.len() - 1
        };
        let built = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(move |i| format!("falcon-prep-{id}-{i}"))
            .start_handler(move |_| self.helper_started(id))
            .build();
        let pool = match built {
            Ok(pool) => Some(pool),
            Err(e) => {
                log_event(&format!(
                    "work prepared ahead: worker {id}'s low-priority pool could not start ({e}) — it lowers its own thread instead"
                ));
                None
            }
        };
        Worker { reg: self, id: Some(id), pool }
    }

    /// One of a worker's pool threads, as it starts: below normal, and known to the registry so a
    /// raise reaches it. If the worker's job was raised before this thread was up, it joins it.
    fn helper_started(&self, worker: usize) {
        lower_current_thread();
        let me = ThreadRef::current();
        let mut s = self.lock();
        if s.running.iter().any(|r| r.worker == worker && !r.low.load(Ordering::Relaxed)) {
            (self.set)(me, Level::Normal);
        }
        s.workers[worker].helpers.push(me);
    }

    /// The tick, once per tick, right after the current photo is known: remember what the user
    /// asked for, and raise every running low job that is now part of it.
    pub(crate) fn publish(&self, explicit: support::ExplicitSet) {
        let mut s = self.lock();
        s.explicit = Some(explicit);
        self.raise(&s, |job| explicit.holds(job.idx()));
    }

    /// A tile showing this thumbnail came on screen while its job runs (`tick::feed_visible_thumb`,
    /// beside the queue's own promotion, which cannot reach a job that has left the queue). Raises
    /// that thumbnail job only: the photo's preview and full-resolution jobs stay with the
    /// published set. A job taken from the queue and not yet started is marked, and starts at
    /// normal ([`Worker::hand_over`]).
    pub(crate) fn promote_thumb(&self, idx: usize, gen: u64, delta: u8) {
        if !self.enabled() {
            return;
        }
        let mut s = self.lock();
        for w in &mut s.workers {
            if let Some(h) = w.held.as_mut().filter(|h| h.key == (idx, gen, delta)) {
                h.asked = true;
            }
        }
        self.raise(&s, |job| {
            matches!(job, Job::Thumb { idx: i, gen: g, delta: d, .. } if (i, g, d) == (idx, gen, delta))
        });
    }

    fn raise(&self, s: &State, asked: impl Fn(Job) -> bool) {
        for r in &s.running {
            if r.low.load(Ordering::Relaxed) && asked(r.job) {
                r.low.store(false, Ordering::Relaxed);
                let w = &s.workers[r.worker];
                (self.set)(w.owner, Level::Normal);
                for &h in &w.helpers {
                    (self.set)(h, Level::Normal);
                }
            }
        }
    }

    fn low_in(&self, s: &State, job: Job) -> bool {
        let requested = s.explicit.is_some_and(|e| e.holds(job.idx()));
        match job {
            // Before the first publish nothing is known, so nothing is lowered.
            Job::Frame(_) => s.explicit.is_some() && !requested,
            Job::Thumb { on_screen, .. } => !on_screen && !requested,
        }
    }
}

impl Worker {
    /// A thumbnail leaves its queue here: `look` sorts what has arrived and takes the next job
    /// (`support::ThumbQueue::take_via`) under the lock the promotions take, and the job is held
    /// for this worker until it starts. A tile's two promotions (`tick::feed_visible_thumb`: the
    /// queue's, then this registry's) therefore cannot both miss it. Before this point the queue's
    /// is sorted in, and the job leaves the queue on screen; after it, the registry's finds the job
    /// held or running (the re-checks, R6 and R7). One held job per worker, so nothing outlives the
    /// job it was for.
    pub(crate) fn hand_over(&self, look: impl FnOnce() -> Option<support::ThumbReq>) -> Option<support::ThumbReq> {
        let Some(id) = self.id else { return look() };
        let mut s = self.reg.lock();
        let job = look()?;
        s.workers[id].held = Some(Held { key: (job.idx, job.gen, job.delta), asked: false });
        Some(job)
    }

    /// The job starts. A low job is registered and the worker's own thread lowered, under the lock
    /// the raises take, so no change of the requested set falls between the check and the
    /// registration. The pool's threads are below normal already. A held thumbnail whose tile came
    /// on screen while it was held starts at normal.
    pub(crate) fn enter(&mut self, job: Job) -> Guard<'_> {
        self.enter_with_hook(job, || {})
    }

    // The hook makes a raise between the registration and the guard deterministic in the
    // regression test (the re-check, R5). The production empty closure optimizes away.
    fn enter_with_hook(&mut self, job: Job, after_lock: impl FnOnce()) -> Guard<'_> {
        let low = Arc::new(AtomicBool::new(false));
        // Whether the guard has an entry to remove, decided with the entry under the lock. `low`
        // cannot say: a raise can clear it as soon as the lock is released.
        let mut registered = false;
        if let Some(id) = self.id {
            let reg = self.reg;
            let mut s = reg.lock();
            // The job this worker was just handed, if it is a thumbnail.
            let asked = s.workers[id].held.take().is_some_and(|h| h.asked);
            if !asked && reg.low_in(&s, job) {
                low.store(true, Ordering::Relaxed);
                (reg.set)(s.workers[id].owner, Level::Low);
                s.running.push(Running { worker: id, job, low: low.clone() });
                registered = true;
            }
        }
        after_lock();
        Guard { worker: self, registered, low }
    }

    /// One preview or thumbnail job: on the worker's pool while nobody has asked for it, so the
    /// decoder's parallel work stays below normal with it; on this thread otherwise, as before.
    pub(crate) fn run<R: Send>(&mut self, job: Job, body: impl FnOnce() -> R + Send) -> R {
        self.enter(job).on_low_pool(body)
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// The thread's own handle, opened on first use and closed when the thread ends.
struct OwnThread(ThreadRef);

impl Drop for OwnThread {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.0.raw != 0 {
            // SAFETY: the handle was opened by `ThreadRef::current` on this thread and is closed once.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                    self.0.raw as *mut core::ffi::c_void,
                ));
            }
        }
    }
}

thread_local! {
    static OWN: OwnThread = OwnThread(ThreadRef::open_current());
}

impl ThreadRef {
    /// This thread, reachable from others. Never fails: without a handle the registry still tracks
    /// the job and the Windows call is skipped.
    pub(crate) fn current() -> ThreadRef {
        OWN.with(|t| t.0)
    }

    fn open_current() -> ThreadRef {
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        #[cfg(windows)]
        {
            use windows::Win32::System::Threading::{
                GetCurrentThread, GetCurrentThreadId, GetThreadPriority, OpenThread, THREAD_QUERY_INFORMATION,
                THREAD_SET_INFORMATION,
            };
            // SAFETY: plain thread queries on the calling thread; the handle is owned by `OwnThread`.
            unsafe {
                let usual = GetThreadPriority(GetCurrentThread());
                let raw = OpenThread(THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION, false, GetCurrentThreadId())
                    .map_or(0, |h| h.0 as isize);
                ThreadRef { key, raw, usual }
            }
        }
        #[cfg(not(windows))]
        ThreadRef { key, raw: 0, usual: 0 }
    }

    #[cfg(test)]
    pub(crate) fn fake(key: u64) -> ThreadRef {
        ThreadRef { key, raw: 0, usual: 0 }
    }
}

/// The production setter. A thread without a handle is left alone.
fn os_set(t: ThreadRef, level: Level) {
    #[cfg(windows)]
    if t.raw != 0 {
        use windows::Win32::System::Threading::{
            SetThreadPriority, THREAD_PRIORITY, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
        };
        let p = match level {
            Level::Low => THREAD_PRIORITY_BELOW_NORMAL,
            Level::Normal => THREAD_PRIORITY_NORMAL,
            Level::Usual => THREAD_PRIORITY(t.usual),
        };
        // SAFETY: `raw` is a live handle the thread opened on itself. The registry sets a worker's
        // threads only while its job runs (the worker, and so its pool, is alive then) and a pool
        // thread as it starts.
        unsafe {
            let _ = SetThreadPriority(windows::Win32::Foundation::HANDLE(t.raw as *mut core::ffi::c_void), p);
        }
    }
    #[cfg(not(windows))]
    let _ = (t, level);
}

/// A worker's pool threads start below normal, so that is their usual level.
fn lower_current_thread() {
    #[cfg(windows)]
    // SAFETY: the pseudo-handle names the calling thread.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// `FALCON_LOW_PRIORITY_PREP=0` turns it off for comparison runs, like the other `FALCON_*=0`
/// reverts. Read once at boot; anything else leaves it on. Off outside Windows.
pub(crate) fn enabled_from_env(value: Option<&str>) -> bool {
    cfg!(windows) && value.map(str::trim) != Some("0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static LOG: RefCell<Vec<(u64, Level)>> = const { RefCell::new(Vec::new()) };
    }

    fn record(t: ThreadRef, level: Level) {
        LOG.with(|l| l.borrow_mut().push((t.key, level)));
    }

    fn take_log() -> Vec<(u64, Level)> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    /// Current `c`, Compare off, nothing hovered or awaited.
    fn at(c: usize) -> support::ExplicitSet {
        support::ExplicitSet::new(c, false, 0, 0, 100, None, -1)
    }

    fn registry(set: SetLevel, enabled: bool) -> &'static Registry {
        let r: &'static Registry = Box::leak(Box::new(Registry::new(set)));
        r.configure(enabled);
        r
    }

    /// A worker whose threads are stand-ins: `owner`, and pool threads `helpers`. No real pool.
    fn fake_worker(r: &'static Registry, owner: u64, helpers: &[u64]) -> Worker {
        let mut s = r.lock();
        let helpers = helpers.iter().map(|&k| ThreadRef::fake(k)).collect();
        s.workers.push(WorkerThreads { owner: ThreadRef::fake(owner), helpers, held: None });
        Worker { reg: r, id: Some(s.workers.len() - 1), pool: None }
    }

    fn frame(idx: usize) -> Job {
        Job::Frame(idx)
    }

    fn thumb(idx: usize, gen: u64, delta: u8, on_screen: bool) -> Job {
        Job::Thumb { idx, gen, delta, on_screen }
    }

    /// Fails if a job nobody asked for is not lowered or not restored, if a requested photo or an
    /// on-screen thumbnail is touched, or if anything is lowered before the first publish.
    #[test]
    fn work_prepared_ahead_runs_low_and_requested_work_is_never_touched() {
        let r = registry(record, true);
        let mut w = fake_worker(r, 1, &[]);
        {
            let g = w.enter(frame(12));
            assert!(!g.is_low(), "nothing is known before the first publish");
        }
        assert_eq!(take_log(), vec![]);
        r.publish(at(10));
        {
            let g = w.enter(frame(12));
            assert!(g.is_low());
            assert_eq!(take_log(), vec![(1, Level::Low)]);
        }
        assert_eq!(take_log(), vec![(1, Level::Usual)], "restored when the job ends");
        drop(w.enter(frame(10)));
        drop(w.enter(thumb(12, 7, 0, true)));
        assert_eq!(take_log(), vec![], "the photo on screen and an on-screen thumbnail stay as they are");
        drop(w.enter(thumb(10, 7, 0, false)));
        assert_eq!(take_log(), vec![], "a thumbnail of the photo on screen is requested too");
        drop(w.enter(thumb(13, 7, 0, false)));
        assert_eq!(take_log(), vec![(1, Level::Low), (1, Level::Usual)]);
    }

    /// Fails if a running job that becomes the photo on screen stays low, if its pool's threads are
    /// left behind (R2), if they stay raised for the worker's next job, if a raise reaches another
    /// worker or a job that has ended, or if two workers' entries interfere (R1's shape).
    #[test]
    fn a_job_that_becomes_the_photo_on_screen_is_raised_with_every_thread_doing_its_work() {
        let r = registry(record, true);
        let (mut a, mut b) = (fake_worker(r, 1, &[11, 12]), fake_worker(r, 2, &[21]));
        let a_id = a.id.unwrap();
        r.publish(at(10));
        let ga = a.enter(frame(11));
        let gb = b.enter(frame(14));
        take_log();
        r.publish(at(11));
        assert_eq!(
            take_log(),
            vec![(1, Level::Normal), (11, Level::Normal), (12, Level::Normal)],
            "the job for the new photo on screen, with its pool, and nothing of the other worker's"
        );
        assert!(!ga.is_low() && gb.is_low());
        r.publish(at(11));
        assert_eq!(take_log(), vec![], "a raised job is not raised again");
        // A pool thread that starts after the raise joins it, and is put back with the others.
        let (late, late_log) = std::thread::spawn(move || {
            r.helper_started(a_id);
            (ThreadRef::current().key, take_log())
        })
        .join()
        .unwrap();
        assert_eq!(late_log, vec![(late, Level::Normal)]);
        drop(ga);
        assert_eq!(take_log(), vec![(1, Level::Usual), (11, Level::Usual), (12, Level::Usual), (late, Level::Usual)]);
        let ga = a.enter(frame(15));
        assert_eq!(take_log(), vec![(1, Level::Low)], "the next job starts low, its pool already back");
        drop(ga);
        take_log();
        r.publish(at(14));
        assert_eq!(take_log(), vec![(2, Level::Normal), (21, Level::Normal)], "the other worker's job, still registered");
        drop(gb);
        take_log();
        r.publish(at(15));
        assert_eq!(take_log(), vec![], "a job that has ended is never touched");
    }

    /// Fails if a thumbnail tile coming on screen does not raise its running job (R3), if the raise
    /// ignores the queue's identity (folder, rotation), or if it raises the photo's other jobs.
    #[test]
    fn a_thumbnail_that_comes_on_screen_raises_its_own_job_only() {
        let r = registry(record, true);
        let (mut t, mut f) = (fake_worker(r, 1, &[11]), fake_worker(r, 2, &[21]));
        r.publish(at(10));
        let gt = t.enter(thumb(20, 7, 0, false));
        let gf = f.enter(frame(20));
        take_log();
        r.promote_thumb(20, 8, 0);
        r.promote_thumb(20, 7, 1);
        r.promote_thumb(21, 7, 0);
        assert_eq!(take_log(), vec![], "another folder, another rotation or another photo");
        r.promote_thumb(20, 7, 0);
        assert_eq!(take_log(), vec![(1, Level::Normal), (11, Level::Normal)], "the thumbnail job and its pool only");
        assert!(!gt.is_low() && gf.is_low());
        drop((gt, gf));
        take_log();
        let off = registry(record, false);
        off.promote_thumb(20, 7, 0);
        assert_eq!(take_log(), vec![]);
    }

    /// Fails if a raise that lands between a job's registration and its guard (the re-check, R5)
    /// leaves the entry behind or the pool raised for the worker's next job.
    #[test]
    fn a_job_raised_as_it_registers_still_ends_cleanly() {
        let r = registry(record, true);
        let mut w = fake_worker(r, 1, &[11]);
        r.publish(at(10));
        let g = w.enter_with_hook(thumb(20, 7, 0, false), || r.promote_thumb(20, 7, 0));
        assert!(!g.is_low());
        assert_eq!(take_log(), vec![(1, Level::Low), (1, Level::Normal), (11, Level::Normal)]);
        drop(g);
        assert!(r.lock().running.is_empty(), "its entry is removed");
        assert_eq!(take_log(), vec![(1, Level::Usual), (11, Level::Usual)], "and its threads put back");
        drop(w.enter(thumb(20, 7, 0, false)));
        assert_eq!(take_log(), vec![(1, Level::Low), (1, Level::Usual)], "the next job starts low");
    }

    /// Fails if a tile that comes on screen as its thumbnail leaves the queue loses the promotion:
    /// the queue has let the job go, no job is registered yet, and a still grid or filmstrip does
    /// not promote again (the re-check, R6). Also fails if other thumbnails coming and going
    /// meanwhile can push the request out (the second re-check, R7), if the queue is read outside
    /// the registry's lock, or if a hold outlives its job or matches another folder or rotation.
    #[test]
    fn a_thumbnail_that_comes_on_screen_as_its_worker_takes_it_is_never_missed() {
        let r = registry(record, true);
        let (mut held, mut other) = (fake_worker(r, 1, &[11]), fake_worker(r, 2, &[21]));
        r.publish(at(10));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut queue = support::ThumbQueue::new(rx);
        tx.send(support::ThumbReq::speculative(31, 7, 0)).unwrap();
        let job = queue
            .take_via(|q| {
                held.hand_over(|| {
                    assert!(r.state.try_lock().is_err(), "the queue is read under the registry's lock");
                    q.try_take()
                })
            })
            .unwrap();
        assert!(!job.explicit);
        // The tile comes on screen now, once: `tick::feed_visible_thumb`'s two promotions.
        tx.send(support::ThumbReq::promote(31, 7, 0)).unwrap();
        r.promote_thumb(31, 7, 0);
        // Many other thumbnails come and go before the held job starts, each promoted while pending.
        for idx in 1000..1300 {
            tx.send(support::ThumbReq::explicit(idx, 7, 0)).unwrap();
            let next = queue.take_via(|q| other.hand_over(|| q.try_take())).unwrap();
            let g = other.enter(thumb(next.idx, next.gen, next.delta, next.explicit));
            tx.send(support::ThumbReq::promote(idx, 7, 0)).unwrap();
            r.promote_thumb(idx, 7, 0);
            drop(g);
        }
        assert!(queue.try_take().is_none(), "the queue's promotion cannot reach a job it has let go");
        let g = held.enter(thumb(job.idx, job.gen, job.delta, job.explicit));
        assert!(!g.is_low(), "the held job kept its tile's promotion");
        drop(g);
        assert_eq!(take_log(), vec![]);
        // Promoted while still queued: the queue moves it, as before.
        tx.send(support::ThumbReq::speculative(32, 7, 0)).unwrap();
        tx.send(support::ThumbReq::promote(32, 7, 0)).unwrap();
        r.promote_thumb(32, 7, 0);
        let job = queue.take_via(|q| held.hand_over(|| q.try_take())).unwrap();
        assert!(job.explicit);
        drop(held.enter(thumb(job.idx, job.gen, job.delta, job.explicit)));
        assert_eq!(take_log(), vec![]);
        // Nothing outlives its job, and another folder or rotation leaves the held job alone.
        tx.send(support::ThumbReq::speculative(31, 7, 0)).unwrap();
        let job = queue.take_via(|q| held.hand_over(|| q.try_take())).unwrap();
        r.promote_thumb(31, 8, 0);
        r.promote_thumb(31, 7, 1);
        drop(held.enter(thumb(job.idx, job.gen, job.delta, job.explicit)));
        assert_eq!(take_log(), vec![(1, Level::Low), (1, Level::Usual)]);
        assert!(r.lock().workers.iter().all(|w| w.held.is_none()), "no hold is left behind");
    }

    /// Fails if the switch-off or off-Windows registry lowers anything, or if a panic inside a job
    /// leaves its thread low or its entry behind.
    #[test]
    fn switched_off_nothing_moves_and_a_panic_restores_the_thread() {
        let off = registry(record, false);
        off.publish(at(10));
        let mut w = off.worker(4);
        assert!(w.id.is_none() && w.pool.is_none(), "no place and no pool while off");
        assert_eq!(w.run(frame(50), || 7), 7);
        assert_eq!(take_log(), vec![]);
        assert!(!enabled_from_env(Some(" 0 ")));
        assert_eq!(enabled_from_env(None), cfg!(windows));
        let r = registry(record, true);
        r.publish(at(10));
        let mut w = fake_worker(r, 4, &[]);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = w.enter(frame(40));
            panic!("a decoder panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(take_log(), vec![(4, Level::Low), (4, Level::Usual)]);
        assert!(r.lock().running.is_empty(), "the panicked job is no longer registered");
    }

    /// Windows, live, with the real Windows calls. Fails if a worker's pool is not below normal, if
    /// a low job's parallel work leaves its own worker's threads, if a raise misses the threads
    /// doing a preview's or a full-resolution job's work (R2), if they stay raised for the next
    /// job, or if one worker's job can run on another's thread (R1).
    #[cfg(windows)]
    #[test]
    fn each_worker_keeps_its_jobs_and_their_parallel_work_on_its_own_threads() {
        use rayon::prelude::*;
        use std::sync::mpsc::channel;
        use windows::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
        };
        const LOW: i32 = THREAD_PRIORITY_BELOW_NORMAL.0;
        const NORMAL: i32 = THREAD_PRIORITY_NORMAL.0;
        let r = registry(os_set, true);
        r.publish(at(10));
        let here = || unsafe { GetThreadPriority(GetCurrentThread()) };
        let name = || std::thread::current().name().unwrap_or("").to_string();
        // Each nested item reports the thread that ran it and that thread's priority.
        let nested = move || {
            (0..128)
                .into_par_iter()
                .map(|_| {
                    std::thread::sleep(Duration::from_micros(200));
                    (name(), here())
                })
                .collect::<Vec<_>>()
        };
        let all_at = |v: &[(String, i32)], p: i32| v.iter().all(|(_, q)| *q == p);
        let mine = |v: &[(String, i32)], id: usize| v.iter().all(|(n, _)| n.starts_with(&format!("falcon-prep-{id}-")));

        std::thread::scope(|s| {
            // A preview worker with a pool of four.
            s.spawn(|| {
                let mut w = r.worker(4);
                let id = w.id.unwrap();
                let (prio, work) = w.run(frame(30), || (here(), nested()));
                assert_eq!(prio, LOW);
                assert!(mine(&work, id), "nested work left the worker's pool: {work:?}");
                let threads: std::collections::BTreeSet<&str> = work.iter().map(|(n, _)| n.as_str()).collect();
                assert!(threads.len() > 1, "the nested work spread over the pool: {threads:?}");
                assert!(all_at(&work, LOW), "nested work ran at normal priority: {work:?}");
                let (prio, n) = w.run(frame(10), || (here(), name()));
                assert_eq!(prio, NORMAL, "the photo on screen runs where it always did");
                assert!(!n.starts_with("falcon-prep-"));
            });
        });

        // R2, a preview: raised while it runs, its parallel work after the raise is normal too, and
        // the worker's next job is low again, parallel work included.
        let (tx, rx) = channel();
        let (go_tx, go_rx) = channel::<()>();
        let ((raised, raised_work), (next, next_work)) = std::thread::scope(|s| {
            let job = s.spawn(|| {
                let mut w = r.worker(4);
                let raised = w.run(frame(32), move || {
                    tx.send(here()).unwrap();
                    go_rx.recv().unwrap();
                    (here(), nested())
                });
                (raised, w.run(frame(51), || (here(), nested())))
            });
            assert_eq!(rx.recv().unwrap(), LOW);
            r.publish(at(32));
            go_tx.send(()).unwrap();
            job.join().unwrap()
        });
        assert_eq!(raised, NORMAL, "the tick's raise reached the job");
        assert!(all_at(&raised_work, NORMAL), "a raised preview's parallel work stayed low: {raised_work:?}");
        assert_eq!(next, LOW);
        assert!(all_at(&next_work, LOW), "the next job inherited the raise: {next_work:?}");

        // R2, the full-resolution shape: the worker stays put and waits while its pool converts.
        let (tx, rx) = channel();
        let (go_tx, go_rx) = channel::<()>();
        let (before, delegated, after) = std::thread::scope(|s| {
            let job = s.spawn(|| {
                let mut w = r.worker(4);
                let g = w.enter(frame(33));
                let before = here();
                let delegated = g.on_low_pool(move || {
                    tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    (here(), nested())
                });
                (before, delegated, here())
            });
            rx.recv().unwrap();
            r.publish(at(33));
            go_tx.send(()).unwrap();
            job.join().unwrap()
        });
        assert_eq!(before, LOW, "the full-resolution worker lowers its own thread");
        assert_eq!(delegated.0, NORMAL, "the conversion already running was raised");
        assert!(all_at(&delegated.1, NORMAL), "its parallel work stayed low: {:?}", delegated.1);
        assert_eq!(after, NORMAL, "and the waiting worker with it");

        // R1: one worker's job waits for a helper inside its own pool while another worker runs a
        // job. The other job never lands on the waiting job's thread, and the waiting job, raised
        // meanwhile, resumes at normal priority.
        let (ready_tx, ready_rx) = channel();
        let (release_tx, release_rx) = channel::<()>();
        std::thread::scope(|s| {
            let outer = s.spawn(|| {
                let mut w = r.worker(2);
                w.run(frame(34), move || {
                    let me = std::thread::current().id();
                    rayon::scope(move |sc| {
                        sc.spawn(move |_| {
                            ready_tx.send(()).unwrap();
                            release_rx.recv().unwrap();
                        });
                    });
                    (me, here())
                })
            });
            ready_rx.recv().unwrap();
            let inner = s.spawn(|| r.worker(2).run(frame(35), || std::thread::current().id())).join().unwrap();
            r.publish(at(34));
            release_tx.send(()).unwrap();
            let (outer_id, prio) = outer.join().unwrap();
            assert_ne!(outer_id, inner, "another worker's job ran inside this one");
            assert_eq!(prio, NORMAL, "the waiting job lost its raise");
        });
        assert!(r.lock().running.is_empty());
    }
}
