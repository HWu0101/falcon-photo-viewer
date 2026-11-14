// Delete round, Stage B (3 November 2026): the file queue. A delete, a Recover and an Undo move run here,
// one at a time, in the order they were pressed, on one thread that starts when there is work and ends
// when there is none. So the interface never waits for the Recycle Bin, and no press refuses another.
//
// A press (`press_delete`, `press_recover`, `press_undo_move`, on the interface thread) changes the
// history at once and queues the job, so the next Ctrl+Z reaches the act before. A delete's handler only
// stashes a [`TakeOut`]: the tick's first drain takes the photo out of the list and then presses, so the
// next photo shows at once, and the file goes behind it (`take_out` in `main.rs`). The job (`run_job`) does
// the disk work and only reports. The tick folds the report (`fold_file_done`) at its top, and that asks
// for a reread ([`Reread`]), which lists the folder on its own thread once the queue is quiet: at once
// after a failure, a Recover, a Move or an Undo move, and after a quiet second behind a clean delete, so a
// burst lists once. It lands on the tick only if the folder really changed, and nothing a person has open
// holds it up. The bin check ([`BinCheck`]) runs at the same quiet second, also on its own thread, so no
// job waits behind a listing of the Recycle Bin.
//
// A press decides its route once ([`support::DeleteRoute`]): at once to the bin on a fixed disk, or Falcon's
// dialog first where the photo can't come back, and then Falcon deletes the file itself (R21 to R24). A
// for-good photo has no record, so nothing offers it back and Ctrl+Z reaches the act before.
//
// Exclusion: `start_op` refuses while the queue isn't quiet, and a delete and an Undo move refuse while
// another operation holds `sel_busy`. A Recover has never checked `sel_busy`. No job raises `sel_busy`,
// so the Review panel's buttons don't flicker. See `PLAN.md` R15, R16, R17 in this round's folder.

use crate::platform::GoneFrom;
use crate::support::{CleanRecover, CompareSeat, DeleteOutcome, DeleteRecord, DeleteRoute, FileHistory, LeftPhoto, ListingStamps, Named, OpResult};
use crate::tick::NotifEntry;
use crate::*;
use std::ffi::OsStr;
use std::sync::OnceLock;

/// R17: how long a person waits on a job before a message says it's working. If nothing changes on
/// screen they think nothing happened, and press again (`docs/design/ux-mental-models.md`, "My pace").
pub(crate) const WORKING_AFTER: Duration = Duration::from_secs(1);

/// R13, R14: how long the queue stays quiet after a job's report before a clean delete's reread and the
/// bin check run, so a culling burst lists once and not on every press. A failure, a Recover, a Move and
/// an Undo move don't wait for it: someone is looking for their photo.
pub(crate) const QUIET_BEFORE_LISTING: Duration = Duration::from_secs(1);

pub(crate) enum FileJob {
    /// `targets` are the photos as they were at the press, cloned there after the identity checks, so
    /// the job can reach nothing else. Their files are listed on the worker, after the barrier.
    /// `for_good` are the press's photos that skip the bin, after Falcon's dialog has said so (R22).
    Delete { seq: u64, dir: String, targets: Vec<Shot>, for_good: ForGood, outcome: Arc<OnceLock<DeleteOutcome>> },
    /// `nav_at_press` is read at the actual press (R2): a step taken while the job runs counts.
    /// `not_in_bin` is what the bin check has found missing by then (R14), which can't come back.
    /// `seats` are the Compare halves its delete emptied (R5). `waited` goes on to the report, because a
    /// person is waiting for the photo until it lands (R17).
    Recover { seq: u64, dir: String, name: String, plural: bool, outcome: Arc<OnceLock<DeleteOutcome>>, not_in_bin: Vec<PathBuf>, nav_at_press: u64, seats: Vec<CompareSeat>, waited: Waited },
    /// `subject` is what it says it brought back (`support::moved_subject`).
    UndoMove { seq: u64, dir: String, pairs: Vec<(PathBuf, PathBuf)>, subject: String },
}

/// R22, R24: the photos of a press that skip the bin, and where the dialog said they go.
#[derive(Default)]
pub(crate) struct ForGood {
    pub(crate) photos: Vec<Shot>,
    pub(crate) from: GoneFrom,
}

impl FileJob {
    fn about(&self) -> (u64, String) {
        match self {
            FileJob::Delete { seq, dir, .. } | FileJob::Recover { seq, dir, .. } | FileJob::UndoMove { seq, dir, .. } => (*seq, dir.clone()),
        }
    }
}

/// What a job reports to the tick.
pub(crate) enum FileDone {
    /// `fail`: how many of its photos are still in the folder. `confirmed`: what it sent is known to be in the
    /// bin, so the bin check has nothing to ask (R14). `for_good`: part of the press skipped the bin, and its
    /// steps wait for this report (R22). `fold`: what its row folds into in the bell list (R32, R33).
    Delete { seq: u64, dir: String, msg: String, fail: usize, can_recover: bool, left: Vec<String>, confirmed: bool, for_good: bool, fold: Option<FoldSeed> },
    /// `msg` is `None` when the delete it reverses sent nothing: that delete's result has already spoken.
    /// `failed` is how many of its files didn't come back.
    Recover { seq: u64, dir: String, files: Vec<PathBuf>, recovered: usize, failed: usize, msg: Option<String>, nav_at_press: u64, seats: Vec<CompareSeat>, waited: Waited },
    /// `severity`: a failed photo is an error, and one with nothing skipped or left behind is clean (R48).
    /// `back`: the photo files it was asked to bring back, for the blue edge (R46).
    UndoMove { seq: u64, dir: String, msg: String, severity: support::OpSeverity, back: Vec<PathBuf> },
    /// R15: a job panicked. Its photos are still on disk or in the bin, so the reread tells the truth.
    Crashed { seq: u64, dir: String },
}

/// R32, R33: what a clean delete's row folds into (`tick::Fold`), as its job found it. Only a photo that went
/// whole: anything else said something a person has to read, and a problem is never folded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FoldSeed {
    /// One photo, on its own: it can join the run of deletes on top of the list. `to_bin`: with a record, so
    /// it can come back; otherwise it went for good from a drive with no bin. One only in the cloud gets a row
    /// of its own, as a dialog asked about it.
    One { name: String, subject: String, to_bin: bool },
    /// A selection, all to the bin: its photos' names and how a message names each, in its order.
    Selection(Vec<(String, String)>),
}

impl FileDone {
    /// The wait this report ends (R17), by the stamp its press gave it. A delete and its Recover share a
    /// stamp, and nobody waits on a delete. A clean Recover says nothing here (R36), so the person is still
    /// waiting for the photo: its wait ends at the landing.
    pub(crate) fn ends_wait(&self) -> Option<u64> {
        match self {
            FileDone::Delete { .. } => None,
            FileDone::Recover { recovered, failed: 0, .. } if *recovered > 0 => None,
            FileDone::Recover { seq, .. } | FileDone::UndoMove { seq, .. } | FileDone::Crashed { seq, .. } => Some(*seq),
        }
    }

    #[cfg(test)]
    fn seq(&self) -> u64 {
        match self {
            FileDone::Delete { seq, .. } | FileDone::Recover { seq, .. } | FileDone::UndoMove { seq, .. } | FileDone::Crashed { seq, .. } => *seq,
        }
    }
}

/// R17: a job a person is waiting on, since when, and what to say if it takes a while.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Waited {
    pub(crate) seq: u64,
    pub(crate) pressed: Instant,
    pub(crate) text: String,
}

impl Waited {
    /// It has taken [`WORKING_AFTER`] and hasn't said so yet (`said` is the stamp that has).
    pub(crate) fn due(&self, now: Instant, said: Option<u64>) -> bool {
        now.saturating_duration_since(self.pressed) >= WORKING_AFTER && said != Some(self.seq)
    }
}

struct Queued {
    job: FileJob,
    waited: Option<Waited>,
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Queued>,
    /// The running job's wait, for [`FileQueue::working_due`].
    running_wait: Option<Waited>,
    /// A thread is on its way to the next job, or running one.
    worker: bool,
}

struct Shared {
    state: Mutex<State>,
    results: Mutex<Vec<FileDone>>,
    /// True from a push until the last job's result is posted. An atomic, so the tick's reads are free
    /// and `support::await_file_op_boundary` can wait on it at quit, as it does on `sel_busy`.
    busy: AtomicBool,
    /// How many jobs have ever been pushed. A listing older than a press is out of date (R2).
    pushes: AtomicU64,
    run: Box<dyn Fn(FileJob) -> FileDone + Send + Sync>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Results first, then `busy`: once the queue reads quiet, every result is there to fold.
    fn finish(&self, done: FileDone) {
        self.results.lock().unwrap_or_else(|e| e.into_inner()).push(done);
        let mut st = self.state();
        st.running_wait = None;
        self.busy.store(!st.jobs.is_empty(), Ordering::SeqCst);
    }
}

/// The queue. Cloning shares it.
#[derive(Clone)]
pub(crate) struct FileQueue(Arc<Shared>);

impl FileQueue {
    pub(crate) fn new() -> Self {
        Self::with_runner(Box::new(|job| {
            // The Mac's `NSFileManager` hands back autoreleased objects, and this thread has no pool of
            // its own. Compile-checked only.
            #[cfg(target_os = "macos")]
            {
                objc2::rc::autoreleasepool(|_| run_job(job, &TheDisk))
            }
            #[cfg(not(target_os = "macos"))]
            {
                run_job(job, &TheDisk)
            }
        }))
    }

    pub(crate) fn with_runner(run: Box<dyn Fn(FileJob) -> FileDone + Send + Sync>) -> Self {
        Self(Arc::new(Shared { state: Default::default(), results: Default::default(), busy: AtomicBool::new(false), pushes: AtomicU64::new(0), run }))
    }

    /// Never refuses, whatever is running. The press's order is the jobs' order, which is what lets a
    /// Recover queue straight behind its own delete.
    pub(crate) fn push(&self, job: FileJob, waited: Option<Waited>) {
        let mut st = self.0.state();
        st.jobs.push_back(Queued { job, waited });
        self.0.busy.store(true, Ordering::SeqCst);
        self.0.pushes.fetch_add(1, Ordering::SeqCst);
        if !st.worker {
            st.worker = true;
            start_worker(&self.0, &mut st);
        }
    }

    /// No job waiting and none running.
    pub(crate) fn quiet(&self) -> bool {
        !self.0.busy.load(Ordering::SeqCst)
    }

    /// R2: how many jobs have been pushed. A listing remembers it, and one older than a press is discarded.
    pub(crate) fn pushes(&self) -> u64 {
        self.0.pushes.load(Ordering::SeqCst)
    }

    /// What [`Self::quiet`] reads, for `support::await_file_op_boundary` at quit.
    pub(crate) fn busy_flag(&self) -> &AtomicBool {
        &self.0.busy
    }

    pub(crate) fn take_results(&self) -> Vec<FileDone> {
        std::mem::take(&mut *self.0.results.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// R17: the first job a person is waiting on, once it has taken [`WORKING_AFTER`] and hasn't said so
    /// yet (`said` is the stamp that has). A delete is nobody's wait: the next photo is already there.
    pub(crate) fn working_due(&self, now: Instant, said: Option<u64>) -> Option<Waited> {
        let st = self.0.state();
        let first = st.running_wait.as_ref().or_else(|| st.jobs.iter().find_map(|q| q.waited.as_ref()))?;
        first.due(now, said).then(|| first.clone())
    }
}

fn start_worker(shared: &Arc<Shared>, st: &mut State) {
    let on_thread = shared.clone();
    if let Err(e) = std::thread::Builder::new().name("falcon-file-queue".into()).spawn(move || work(on_thread)) {
        // The jobs wait, and the next press tries again. They are not lost, and nothing has happened yet.
        log_event(&format!("file queue: couldn't start its thread ({e})"));
        st.worker = false;
    }
}

fn work(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut st = shared.state();
            let Some(Queued { job, waited }) = st.jobs.pop_front() else {
                st.worker = false;
                return;
            };
            st.running_wait = waited;
            job
        };
        let (seq, dir) = job.about();
        let guard = PanicGuard { shared: &shared, seq, dir };
        let done = (shared.run)(job);
        drop(guard);
        shared.finish(done);
    }
}

/// R15: a job that panics (`CoCreateInstance(..).unwrap()` in the `trash` crate) ends this thread. The
/// queue goes on without it: the crash is reported, and a new thread takes any jobs left, so `quiet()`
/// comes true and a queued Recover still runs, finding the outcome unset.
struct PanicGuard<'a> {
    shared: &'a Arc<Shared>,
    seq: u64,
    dir: String,
}

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        log_event(&format!("file queue: job {} crashed", self.seq));
        self.shared.finish(FileDone::Crashed { seq: self.seq, dir: std::mem::take(&mut self.dir) });
        let mut st = self.shared.state();
        if st.jobs.is_empty() {
            st.worker = false;
        } else {
            start_worker(self.shared, &mut st);
        }
    }
}

/// What the recycle reports: `files` is the reconciled subset, the ONE input to the record and the pill
/// (v0.8.131, F-P11 rules 3+4). `confirmed`: it has been checked against the bin already, which only the
/// Mac has, from its Trash URLs. Otherwise the bin check looks at it a moment later (R14).
pub(crate) struct Recycled {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) confirmed: bool,
    #[cfg(target_os = "macos")]
    pub(crate) trash_paths: Vec<(PathBuf, PathBuf)>,
}

impl Recycled {
    /// A press with nothing for the bin: every photo of it skips the bin (R22).
    fn nothing() -> Self {
        Self {
            files: Vec::new(),
            confirmed: true,
            #[cfg(target_os = "macos")]
            trash_paths: Vec::new(),
        }
    }
}

/// The disk, for the jobs. The methods are the real thing, and a test's rig overrides them, so no test
/// here touches the Recycle Bin (L28: the jobs' own decisions stay reachable).
pub(crate) trait FileIo {
    fn barrier(&self) {
        support::writer_barrier(Duration::from_secs(10));
    }

    fn list_files(&self, targets: &[Shot]) -> Vec<PathBuf> {
        files_to_send(targets)
    }

    /// `recycle_shot_files` runs whole and unchanged, re-stat included.
    fn recycle(&self, paths: &[PathBuf]) -> Recycled {
        // v0.9.18: the Mac arm returns one more value — the (original, in-Trash) pairs it captured per file
        // — so Recover can rename each file straight back. v0.9.63:
        // BOTH arms' third value is now the RECONCILED subset (F-P11 rule 2) — the Mac arm re-stats the
        // folder exactly as the Windows one does.
        #[cfg(not(target_os = "macos"))]
        {
            let (_ok, _fail, recycled) = support::recycle_shot_files(paths);
            // v0.8.131 (F-P11 rules 3+4): the reconciled subset is the ONE input to the record and the
            // pill. R14: the check against the bin comes after, at a quiet second, on its own thread.
            Recycled { files: recycled, confirmed: false }
        }
        // v0.9.63: macOS confirms rule 3 with the captured Trash URLs (`mac_confirm_trashed`) rather than
        // a bin listing it has no API for, so the reconciled subset IS the confirmed one and `trash_pairs`
        // is already filtered to it — index-parallel with the files.
        #[cfg(target_os = "macos")]
        {
            let (_ok, _fail, recycled, trash_paths) = support::recycle_shot_files(paths);
            Recycled { files: recycled, confirmed: true, trash_paths }
        }
    }

    /// R22: one photo's files, by Falcon itself, in the order given.
    fn remove_for_good(&self, files: &[PathBuf]) -> (usize, usize, Vec<PathBuf>, String) {
        support::delete_photo_for_good(files)
    }

    /// v0.9.18: Windows lists the Recycle Bin and matches by original path + op_time; macOS renames each
    /// captured (original, in-Trash) pair back — no bin scan, so it takes the pairs instead. Returns how
    /// many came back and what to say.
    fn restore(&self, name: &str, shown: &str, sent: &DeleteOutcome) -> (usize, String) {
        #[cfg(not(target_os = "macos"))]
        let (recovered, _failed, msg) = support::recover_recycled_files(name, shown, &sent.files, sent.op_time);
        #[cfg(target_os = "macos")]
        let (recovered, _failed, msg) = {
            let _ = name;
            support::recover_recycled_files(shown, &sent.files, &sent.trash_paths)
        };
        (recovered, msg)
    }

    fn undo_moves(&self, pairs: &[(PathBuf, PathBuf)]) -> (usize, usize, usize, usize) {
        support::undo_moves(pairs)
    }

    /// R14: one listing of the bin, for all it's asked about: which of those files aren't in it. If it
    /// can't be listed (a listing can fail) the bare re-stat stands: the pessimistic fallback is the
    /// shipped behaviour, not a new risk. The Mac never asks (`Recycled::confirmed`), and has no such API.
    fn missing_from_bin(&self, asks: &[BinAsk]) -> Vec<BinFound> {
        #[cfg(not(target_os = "macos"))]
        {
            let began = Instant::now();
            let Ok(bin) = trash::os_limited::list() else {
                log_event("delete-recycle: the bin could not be listed — the re-stat's verdict stands");
                return Vec::new();
            };
            let found: Vec<BinFound> = asks.iter().filter_map(|ask| {
                let missing = support::missing_from_bin(&bin, &ask.files, ask.op_time);
                (!missing.is_empty()).then_some(BinFound { seq: ask.seq, missing })
            }).collect();
            log_event(&format!("delete: bin check {} ms ({} items, {} missing)", began.elapsed().as_millis(), bin.len(), found.iter().map(|f| f.missing.len()).sum::<usize>()));
            found
        }
        #[cfg(target_os = "macos")]
        {
            let _ = asks;
            Vec::new()
        }
    }
}

struct TheDisk;
impl FileIo for TheDisk {}

/// One job, start to finish. It reports and never touches interface state.
pub(crate) fn run_job(job: FileJob, io: &impl FileIo) -> FileDone {
    match job {
        FileJob::Delete { seq, dir, targets, for_good, outcome } => run_delete(io, seq, dir, &targets, &for_good, &outcome),
        FileJob::Recover { seq, dir, name, plural, outcome, not_in_bin, nav_at_press, seats, waited } => run_recover(io, seq, dir, &name, plural, &outcome, &not_in_bin, nav_at_press, seats, waited),
        FileJob::UndoMove { seq, dir, pairs, subject } => run_undo_move(io, seq, dir, &pairs, &subject),
    }
}

/// ONLY these photos' file(s) + their Falcon-owned sidecars, all regular files.
fn files_to_send(targets: &[Shot]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for shot in targets {
        if let Some(raw) = &shot.raw {
            if raw.is_file() {
                paths.push(raw.clone());
            }
            let sc = sidecar_path_for(raw, true);
            if sc.is_file() {
                paths.push(sc);
            }
        }
        if let Some(jpg) = &shot.jpg {
            if jpg.is_file() {
                paths.push(jpg.clone());
            }
            let sc = sidecar_path_for(jpg, false);
            if sc.is_file() {
                paths.push(sc);
            }
        }
    }
    paths
}

/// How one photo went: what its re-stat says of its photo files. A side file that stayed is in the log and
/// doesn't change what a person is told about the photo.
#[derive(Debug, PartialEq, Eq)]
enum Went {
    Whole,
    /// RAW or the finished file went, and the other didn't.
    Part { gone: String, stayed: String },
    Stayed,
    /// Its files had left already.
    AlreadyGone,
}

/// `files` is what was listed to go, and `gone` what the re-stat says went.
fn how_it_went(shot: &Shot, files: &HashSet<&Path>, gone: &HashSet<&Path>) -> Went {
    let raw_word = || "RAW".to_string();
    let finished_word = || shot.finished_file_format().unwrap_or_else(|| "image".to_string());
    let went = |f: &Option<PathBuf>| f.as_ref().filter(|p| files.contains(p.as_path())).map(|p| gone.contains(p.as_path()));
    match (went(&shot.raw), went(&shot.jpg)) {
        (None, None) => Went::AlreadyGone,
        (Some(true), Some(false)) => Went::Part { gone: raw_word(), stayed: finished_word() },
        (Some(false), Some(true)) => Went::Part { gone: finished_word(), stayed: raw_word() },
        (Some(false), _) | (_, Some(false)) => Went::Stayed,
        _ => Went::Whole,
    }
}

/// What a press says of its photos, in the owner's approved words, and how many are still in the folder. A
/// person reads photos here, never files. The route's sentences are `one(photo)`, `part(gone, name, stayed)`,
/// `all(n)` for a selection and `some(ok, total)` for part of one. Falcon can't know why a file refused, so
/// it's "may have it open" (R47), and the system's error stays in the log.
fn press_text(went: &[(&Shot, Went)], one: impl Fn(&str) -> String, part: impl Fn(&str, &str, &str) -> String, all: impl Fn(usize) -> String, some: impl Fn(usize, usize) -> String) -> (String, usize) {
    let whole = went.iter().filter(|(_, w)| matches!(w, Went::Whole | Went::AlreadyGone)).count();
    let still = went.len() - whole;
    let text = match went {
        [(shot, Went::Whole)] => one(&support::photo_subject(shot)),
        [(shot, Went::Part { gone, stayed })] => part(gone, &shot.name, stayed),
        [(shot, Went::Stayed)] => platform::could_not_delete_one_text(&support::photo_subject(shot)),
        _ if went.iter().all(|(_, w)| *w == Went::AlreadyGone) => i18n::tr("Nothing to delete — files already gone.").to_string(),
        _ if still == 0 => all(went.len()),
        _ if went.iter().all(|(_, w)| *w == Went::Stayed) => platform::could_not_delete_many_text(went.len()),
        _ => format!("{} {}", some(whole, went.len()), platform::still_in_folder_text(still)),
    };
    (text, still)
}

/// R29, R47: what a press whose photos went to the bin says.
fn bin_text(went: &[(&Shot, Went)]) -> (String, usize) {
    press_text(went, platform::sent_one_text, platform::sent_part_text, platform::sent_selected_text, platform::sent_some_text)
}

/// R22, R24: what a press that skipped the bin says. `to_bin` is `Some((sent, stayed))` for the other half
/// of a mixed selection. The photo's own sentences are the owner's approved words.
fn for_good_text(from: GoneFrom, went: &[(&Shot, Went)], to_bin: Option<(usize, usize)>) -> (String, usize) {
    let whole = went.iter().filter(|(_, w)| matches!(w, Went::Whole | Went::AlreadyGone)).count();
    let still_for_good = went.len() - whole;
    if let Some((sent, bin_stayed)) = to_bin {
        let still = still_for_good + bin_stayed;
        let mut said: Vec<String> = Vec::new();
        if sent > 0 {
            said.push(platform::sent_photos_text(sent));
        }
        if whole > 0 {
            said.push(platform::deleted_from_cloud_count_text(whole));
        }
        if sent == 0 && whole == 0 {
            return (platform::could_not_delete_many_text(went.len() + bin_stayed), still);
        }
        if still > 0 {
            said.push(platform::still_in_folder_text(still));
        }
        return (said.join(" "), still);
    }
    press_text(
        went,
        |photo| platform::deleted_one_text(from, photo),
        |gone, name, stayed| platform::deleted_part_text(from, gone, name, stayed),
        |n| platform::deleted_many_text(from, n),
        |ok, total| platform::deleted_some_text(from, ok, total),
    )
}

fn run_delete(io: &impl FileIo, seq: u64, dir: String, targets: &[Shot], for_good: &ForGood, outcome: &OnceLock<DeleteOutcome>) -> FileDone {
    let began = Instant::now();
    // 1) v0.8.70 (FIX 4): the press has flushed, and this DRAINS the writer BEFORE enumerating these
    //    photos' files. A rating changed in the last quiescence window is synced (sync_xmp_ratings, inside
    //    flush_old) to a possibly FRESH `NAME.xmp` on this very barrier; enumerating first would miss that
    //    just-born sidecar and strand it as an orphan beside the recycled photo. Ordering is otherwise the
    //    proven sync move-op discipline: the flush persists the OTHER shots' ratings/marks (reload's own
    //    flush_old is then a harmless same-value re-flush), and the barrier (v0.8.36 ITEM 1, barrier a)
    //    makes it durable BEFORE we recycle — a crash between the async enqueue and the recycle can't lose
    //    them, and log+proceeds on timeout.
    io.barrier();
    let barrier_ms = began.elapsed().as_millis();
    // 2) The freshly-synced sidecar (if any) is on disk now, so it is included here — never orphaned.
    let paths = io.list_files(targets);
    // v0.8.37 (FIX A): stamp the op time BEFORE the recycle. The Recycle-Bin item's `time_deleted` is
    // at/after this instant, so recovery's bin-item time filter uses it as the lower bound
    // (match_recycled_items adds a small slack for the shell's whole-second clock).
    let op_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 3) recycle. trash 5.2.6 = IFileOperation: SEQUENTIAL, so a mid-batch lock can leave EARLIER files
    //    already recycled (partial completion, NOT all-or-nothing) even though its Err path reports (0, N).
    //    v0.8.16 (skeptic note): the rescan is UNCONDITIONAL on any attempt — including a (0, 0) no-op
    //    (e.g. the shot's files vanished externally before this batch ran) — so no zombie shot with
    //    dangling paths survives until a manual reopen. `fold_file_done` asks for it.
    let recycling = Instant::now();
    let recycled = if targets.is_empty() { Recycled::nothing() } else { io.recycle(&paths) };
    let recycle_ms = recycling.elapsed().as_millis();
    // R22: the photos that skip the bin, each on its own: a failure stops inside that photo, and the next
    // goes on. Its files are listed here for the reason above, so a side file saved at the last moment goes.
    let removing = Instant::now();
    let went: Vec<(&Shot, Went)> = for_good.photos.iter().map(|shot| {
        let files = io.list_files(std::slice::from_ref(shot));
        let (_, _, gone, why) = io.remove_for_good(&files);
        if !why.is_empty() {
            log_event(&format!("delete: job {seq} for good stopped at {why}"));
        }
        let how = how_it_went(shot, &files.iter().map(PathBuf::as_path).collect(), &gone.iter().map(PathBuf::as_path).collect());
        (shot, how)
    }).collect();
    // v0.8.131 (F-P11 rule 5): the record describes the SUBSET that actually left — its photo count —
    // never the batch that was attempted.
    let sent: HashSet<&OsStr> = recycled.files.iter().filter_map(|p| p.file_name()).collect();
    let left: Vec<LeftPhoto> = targets
        .iter()
        .filter_map(|sh| {
            let files: Vec<PathBuf> = [&sh.raw, &sh.jpg].into_iter().flatten()
                .filter(|f| f.file_name().is_some_and(|n| sent.contains(n)))
                .cloned()
                .collect();
            // Half a pair isn't the pair: a message about what left names the photo without its label.
            let whole = files.len() == [&sh.raw, &sh.jpg].into_iter().flatten().count();
            let subject = if whole { support::photo_subject(sh) } else { sh.name.clone() };
            (!files.is_empty()).then(|| LeftPhoto { name: sh.name.clone(), subject, files })
        })
        .collect();
    let left_names: Vec<String> = left.iter().map(|photo| photo.name.clone()).collect();
    let confirmed = recycled.confirmed;
    // R32, R33: a press whose photos all went whole folds in the bell list, and nothing else does.
    let fold = if went.is_empty() {
        let (listed, gone): (HashSet<&Path>, HashSet<&Path>) = (paths.iter().map(PathBuf::as_path).collect(), recycled.files.iter().map(PathBuf::as_path).collect());
        let whole = targets.iter().all(|shot| how_it_went(shot, &listed, &gone) == Went::Whole);
        match targets {
            [one] if whole => Some(FoldSeed::One { name: one.name.clone(), subject: support::photo_subject(one), to_bin: true }),
            [_, _, ..] if whole => Some(FoldSeed::Selection(targets.iter().map(|shot| (shot.name.clone(), support::photo_subject(shot))).collect())),
            _ => None,
        }
    } else {
        match (&went[..], for_good.from, targets.is_empty()) {
            ([(one, Went::Whole)], GoneFrom::Drive, true) => Some(FoldSeed::One { name: one.name.clone(), subject: support::photo_subject(one), to_bin: false }),
            _ => None,
        }
    };
    // What's said is in photos, and `fail` is how many are still in the folder: a side file that stayed is in
    // the log. A partial failure has been reconciled to disk by now (F5, the re-stat), and the reread follows.
    let (msg, fail) = if went.is_empty() {
        let (listed, gone): (HashSet<&Path>, HashSet<&Path>) = (paths.iter().map(PathBuf::as_path).collect(), recycled.files.iter().map(PathBuf::as_path).collect());
        let to_bin: Vec<(&Shot, Went)> = targets.iter().map(|shot| (shot, how_it_went(shot, &listed, &gone))).collect();
        bin_text(&to_bin)
    } else {
        log_event(&format!(
            "delete: job {seq} for good={} ms: {} of {} photo(s) gone",
            removing.elapsed().as_millis(), went.iter().filter(|(_, w)| matches!(w, Went::Whole)).count(), went.len()
        ));
        let to_bin = (!targets.is_empty()).then(|| (left.len(), targets.len().saturating_sub(left.len())));
        for_good_text(for_good.from, &went, to_bin)
    };
    // v0.8.37 (FIX A1): record everything a recovery needs and OFFER the Recover button.
    // v0.8.131 (F-P11 rule 4): can_recover = something actually reached the bin. The old
    // `ok > 0 && fail == 0` read a pair the OS never verified, so a partial recycle — the commonest real
    // failure — offered no Recover for the files that DID leave.
    let can_recover = !recycled.files.is_empty();
    // v0.8.134 (F-5): the LINE says what the RECORD holds. It quoted the counts we ATTEMPTED beside a
    // record built from the bin-confirmed subset, so a partial recycle logged "recorded 12 photo(s) / 24
    // file(s)" over a record naming three.
    log_event(&format!(
        "delete: job {seq} barrier={barrier_ms} ms recycle={recycle_ms} ms: {} photo(s) / {} file(s) reached the bin (of {} photo(s) / {} file(s) attempted)",
        left.len(), recycled.files.len(), targets.len(), paths.len()
    ));
    let _ = outcome.set(DeleteOutcome {
        files: recycled.files,
        left,
        op_time,
        // v0.9.18: the captured (original, in-Trash) pairs — the exact list Recover renames back on Mac.
        // v0.9.63 (F-P11 rules 3+4): what keeps the two lists in step is that both come out of the same
        // `mac_confirm_trashed` intersection: every file recorded has a pair, and a partial delete records
        // exactly the ones that reached the Trash.
        #[cfg(target_os = "macos")]
        trash_paths: recycled.trash_paths,
    });
    FileDone::Delete { seq, dir, msg, fail, can_recover, left: left_names, confirmed, for_good: !for_good.photos.is_empty(), fold }
}

#[allow(clippy::too_many_arguments)]
fn run_recover(io: &impl FileIo, seq: u64, dir: String, name: &str, plural: bool, outcome: &OnceLock<DeleteOutcome>, not_in_bin: &[PathBuf], nav_at_press: u64, seats: Vec<CompareSeat>, waited: Waited) -> FileDone {
    // Its delete ran first. It sent nothing, or crashed and set nothing (R15): there's nothing to bring
    // back, and that delete has already spoken, so this says nothing more.
    let Some(sent) = outcome.get().filter(|o| !o.files.is_empty()) else {
        return FileDone::Recover { seq, dir, files: Vec::new(), recovered: 0, failed: 0, msg: None, nav_at_press, seats, waited };
    };
    // R14: what the bin check found missing can't come back, and its amber line has already said so.
    let sent = sent.without(not_in_bin);
    let began = Instant::now();
    let subject = sent.left.first().map_or(name, |photo| photo.subject.as_str());
    let shown = support::deleted_subject(subject, if plural { sent.left.len().max(1) } else { 0 });
    let (recovered, msg) = io.restore(name, &shown, &sent);
    log_event(&format!("recover: job {seq} restored {recovered} of {} file(s) in {} ms", sent.files.len(), began.elapsed().as_millis()));
    let failed = sent.files.len().saturating_sub(recovered);
    FileDone::Recover { seq, dir, files: sent.files.clone(), recovered, failed, msg: Some(msg), nav_at_press, seats, waited }
}

fn run_undo_move(io: &impl FileIo, seq: u64, dir: String, pairs: &[(PathBuf, PathBuf)], subject: &str) -> FileDone {
    // Fix round 1 (1 November, CS-26): photos first, then their XMP sidecars; the counts
    // are photo files, `side_not` the sidecars that could not come back.
    let (ok, sk, fail, side_not) = io.undo_moves(pairs);
    // Every file back says it in photos, the owner's approved words. Anything that didn't come back is in
    // files, because that's what was counted.
    let msg = if (sk, fail, side_not) == (0, 0, 0) && !subject.is_empty() {
        platform::brought_back_text(subject)
    } else {
        format!(
            "{}{}{}{}",
            // Language packs (round 2): whole counted messages ("file(s)" for every count).
            tr_plural!(ok, "Undid — restored {n} file(s) to the folder", "Undid — restored {n} file(s) to the folder"),
            support::clause_skipped(sk),
            support::clause_side_files(side_not, support::SideVerb::Restored),
            support::clause_failed(fail)
        )
    };
    // R46: its photos, for the blue edge. A side file isn't a photo, and one that didn't come back isn't in
    // the list the landing reads them against.
    let back = pairs.iter().filter(|(from, _)| !support::is_side_file(from)).map(|(from, _)| from.clone()).collect();
    FileDone::UndoMove { seq, dir, msg, severity: support::OpSeverity::of_result(fail, sk + side_not), back }
}

/// What a delete's handler stashes, after its gates and identity checks, for the tick's drain. One tick
/// away at most: input wakes the timer. The drain is the one place a landing keeps shots, ratings and
/// generation one consistent set, and the Mac's menu arm presses Delete from inside the tick.
pub(crate) struct TakeOut {
    pub(crate) dir: String,
    pub(crate) gen: u64,
    pub(crate) name: String,
    pub(crate) photos: usize,
    /// Each photo, and its place in the list the undo stack addresses.
    pub(crate) targets: Vec<(usize, Shot)>,
    /// Decided when the dialog opened, or at the press if none did, and only read from here on (R22).
    pub(crate) route: DeleteRoute,
}

/// The press of a delete, in the tick's drain: the record exists from here, so Ctrl+Z reaches it with its
/// job still queued, and each press takes one step back. The photos' undo steps go with it now (R8), by
/// the places of the list on screen, and any that didn't leave after all come back when the job has said
/// so. The caller has cleared redo (R7), and queues the job this returns only when the landing has
/// flushed: its barrier then covers a star saved on the photo's way out (`run_delete`).
///
/// R22: a photo that skips the bin has no record, so no Recover and no Ctrl+Z entry: Ctrl+Z reaches the act
/// before. Its steps wait for the job the way a Recover's do (`on_the_way`), so a delete that fails loses
/// no step, and the reread drops them if the photo has really gone. A selection with some for the bin keeps
/// a record of those (R24), and a record of one reads by name (R25).
///
/// R5: `seats` are the Compare halves this press empties, which its record remembers for its Ctrl+Z.
///
/// R29: this is the newest press from here, so an older clean result that arrives after it doesn't raise a card.
pub(crate) fn press_delete(history: &RefCell<FileHistory>, undo: &RefCell<UndoRedo>, seq: u64, press: TakeOut, seats: Vec<CompareSeat>) -> FileJob {
    history.borrow_mut().newest_press = seq;
    let TakeOut { dir, gen, name, photos, targets, route } = press;
    let from = if matches!(route, DeleteRoute::CloudOnly(_)) { GoneFrom::Cloud } else { GoneFrom::Drive };
    let (skipping, binned): (Vec<_>, Vec<_>) = targets.into_iter().partition(|(_, s)| route.goes_for_good(s));
    let park = |group: &[(usize, Shot)]| {
        let leaving: Vec<(usize, &str)> = group.iter().map(|(i, s)| (*i, s.name.as_str())).collect();
        support::park_steps(&mut undo.borrow_mut(), &leaving)
    };
    if !skipping.is_empty() {
        let steps = park(&skipping);
        history.borrow_mut().start_return(seq, steps);
    }
    let outcome = Arc::new(OnceLock::new());
    if !binned.is_empty() {
        let (name, photos) = match (skipping.is_empty(), &binned[..]) {
            (true, _) => (name, photos),
            (false, [(_, one)]) => (one.name.clone(), 0),
            (false, many) => (format!("{} photos", many.len()), many.len()),
        };
        let steps = park(&binned);
        let subject = if let [(_, one)] = &binned[..] { support::photo_subject(one) } else { name.clone() };
        history.borrow_mut().push_delete(DeleteRecord { dir: dir.clone(), gen, name, photos, subject, seq, steps, outcome: outcome.clone(), unconfirmed: false, not_in_bin: Vec::new(), seats });
    }
    let for_good = ForGood { photos: skipping.into_iter().map(|(_, s)| s).collect(), from };
    let targets = binned.into_iter().map(|(_, s)| s).collect();
    FileJob::Delete { seq, dir, targets, for_good, outcome }
}

/// The press of a Recover: it takes the record at once, so the next press reaches the act before. FIFO
/// puts the job behind its delete. `false`: no such record. There's deliberately no redo of a delete.
pub(crate) fn press_recover(history: &RefCell<FileHistory>, queue: &FileQueue, seq: u64, nav_at_press: u64, now: Instant) -> bool {
    let mut hist = history.borrow_mut();
    let Some(rec) = hist.take_delete(seq) else { return false };
    let waited = Waited { seq, pressed: now, text: tr_format!("Recovering {name}…", name = rec.shown_name()) };
    hist.start_return(seq, rec.steps);
    queue.push(
        FileJob::Recover { seq, dir: rec.dir, name: rec.name, plural: rec.photos > 0, outcome: rec.outcome, not_in_bin: rec.not_in_bin, nav_at_press, seats: rec.seats, waited: waited.clone() },
        Some(waited),
    );
    true
}

/// The press of an Undo move, the same as a Recover's. It names no photo (R1), so the person stays where
/// they are by the time it lands. Its steps come back too (R44). `false`: no such record.
pub(crate) fn press_undo_move(history: &RefCell<FileHistory>, queue: &FileQueue, seq: u64, now: Instant) -> bool {
    let mut hist = history.borrow_mut();
    let Some(rec) = hist.take_move(seq) else { return false };
    hist.start_return(seq, rec.steps);
    queue.push(
        FileJob::UndoMove { seq, dir: rec.folder, pairs: rec.pairs, subject: rec.subject },
        Some(Waited { seq, pressed: now, text: i18n::tr("Undoing move…").to_string() }),
    );
    true
}

/// Where a job's report goes on the interface thread.
pub(crate) struct FoldTo<'a> {
    pub(crate) history: &'a RefCell<FileHistory>,
    pub(crate) pending: &'a RefCell<Vec<NotifEntry>>,
    pub(crate) landing_name: &'a RefCell<Option<Named>>,
    pub(crate) reread: &'a RefCell<Reread>,
}

impl FoldTo<'_> {
    /// THE CALLER-SIDE RESCAN CONTRACT: `recycle_shot_files` is NON-ATOMIC (IFileOperation is sequential),
    /// so the reload is UNCONDITIONAL on success OR failure — including a (0,0) no-op — and a partial
    /// failure says the folder was re-checked. The photo left the list at the press, and this reread,
    /// after the job, is what brings it back if it never left the disk. `at_once` is for someone looking
    /// for a photo; a clean delete's waits for a quiet second. The tick re-checks the folder too.
    fn ask_for_reread(&self, dir: &str, at_once: bool) {
        if !dir.is_empty() {
            self.reread.borrow_mut().want(PathBuf::from(dir), at_once);
        }
    }
}

/// One job's report, folded. The tick runs this just before its reread drain.
pub(crate) fn fold_file_done(done: FileDone, to: &FoldTo, now: Instant) {
    to.reread.borrow_mut().note_report(now);
    match done {
        FileDone::Delete { seq, dir, msg, fail, can_recover, left, confirmed, for_good, fold } => {
            {
                let mut hist = to.history.borrow_mut();
                hist.settle_delete(seq, can_recover, &left);
                if !confirmed {
                    hist.note_unconfirmed(seq); // R14
                }
                if for_good {
                    hist.send_back(seq); // R22: only that press's steps have waited under this stamp so far
                }
            }
            to.ask_for_reread(&dir, fail > 0 || !can_recover || for_good); // a photo may be coming back
            // Success or failure, surface a transient toast (falcon.log already carries the durable record
            // via recycle_shot_files). Failure = danger kind; the rescan has reconciled the shot list to
            // disk (including any partial recycle), so the view matches reality.
            // v0.8.120 (design-sweep O5): through `notif_pending`, like every other op result.
            // This block used to write the toast properties DIRECTLY and push nothing, so the
            // delete never reached `notif_log` at all: the bell badge and the events panel showed
            // no trace of it, and ANY later notification (a save-failure warning, a finished
            // background copy, the unapplied-rotations notice) replaced the text AND cleared
            // `toast_recover` inside step_notifs — closing the ~30 s recovery window silently and
            // permanently. As an entry the result is durable (failures included), the Recover pill
            // rides the entry onto BOTH the toast and the persisted row (gated there by the live
            // `can-recover`), the 1800-frame linger is the entry ladder's own, and the severity is
            // the op-result convention every sibling uses (error red / success accent).
            // The pill takes this row's own record, by stamp, so every row keeps a working Recover
            // while its photo can come back (Delete round; older rows were demoted while the record
            // was a single slot). If Ctrl+Z has taken the record already,
            // `tick::delete_result_still_true` drops a clean result in the same tick (R9).
            // v0.8.121 (Round-B fix F23a = audit A15): the entry SHAPE is a pure fn now, so the
            // events-centre test can drive the real construction instead of hand-building an entry
            // whose named falsifier ("drop `.with_recover()` from the delete push") could never fire.
            let mut entry = tick::delete_result_entry(msg, fail, &dir, can_recover, seq, now);
            // R29, R35: the pictures from the press. The card has one only when it names one photo, and each
            // line of a fold has its own (R33). Those nothing shows are dropped here.
            let mut pictures = to.history.borrow_mut().take_pictures(seq);
            if let ([(_, picture)], 0) = (&pictures[..], fail) {
                entry.picture = Some(picture.clone());
            }
            entry.fold = fold.map(|seed| tick::Fold::from_seed(seed, seq, &mut pictures, now));
            to.pending.borrow_mut().push(entry);
        }
        FileDone::Recover { seq, dir, files, recovered, failed, msg, nav_at_press, seats, waited } => {
            // R8: the undo steps go back at the landing that lists the photo again.
            to.history.borrow_mut().send_back(seq);
            // R2 (Delete round): the photo that came back shows, unless the person navigates before it
            // lands. The newest press's name wins: results arrive in press order. R5: in Compare every
            // press still to land counts, because the second goes back in a half the first has put right.
            let clean = recovered > 0 && failed == 0;
            if recovered > 0 {
                // R46: the blue edge's one other source. Every file it was asked for: one that didn't come back
                // isn't in the list the landing reads them against.
                to.history.borrow_mut().brought_back.extend(files.iter().cloned());
                let mut named = to.landing_name.borrow_mut();
                let (mut all, earlier_clean) = named.take().map_or_else(|| (Vec::new(), None), |earlier| (earlier.seats, earlier.clean));
                all.extend(seats);
                let words = if clean { msg.clone() } else { None };
                let clean = words.map(|words| CleanRecover { words, wait: waited }).or(earlier_clean);
                *named = Some(Named { files, nav_at_press, seats: all, clean });
            }
            to.ask_for_reread(&dir, true);
            // R36: a clean Recover pushes no row, because the photo coming back says it, and the bell list is
            // what's still gone. It has its say at the landing (`tick::say_recovered`). What failed speaks, in
            // the events centre beside the delete it reverses (v0.8.120, O5): red if nothing came back, and
            // amber if part did, such as a file of that name saved beside the photo since.
            if !clean {
                if let Some(msg) = msg {
                    to.pending.borrow_mut().push(NotifEntry::new(msg, if recovered == 0 { 3 } else { 2 }, now).in_dir(&dir));
                }
            }
        }
        FileDone::UndoMove { seq, dir, msg, severity, back } => {
            to.history.borrow_mut().send_back(seq);
            // R46: the person brought these back, so the blue edge may point at them if they land off what
            // the strip or the grid shows. Read at the landing that lists them.
            to.history.borrow_mut().brought_back.extend(back);
            // R1 (Delete round): Undo move never moves you. It names nothing, so the person stays on
            // their photo, wherever they have browsed to by now.
            to.ask_for_reread(&dir, true);
            // v0.8.120 (design-sweep Y44): kind 3, not 1. `result_subfolder(1)` = "Rejected", so the
            // undo's toast read "Undid — restored 12 file(s) to the folder" beside a "Show" that opened
            // ./Rejected — where none of the restored files are — and the same wrong target was baked
            // permanently into the events-panel row. Kind 3 reveals the shoot folder
            // (tick::result_reveal_path). It also stops the row offering "Undo": `undoable` requires kind
            // 1, and an undo has no move of its own to take back.
            let result = (msg, severity, 3, Some(dir.clone()));
            to.pending.borrow_mut().push(tick::op_result_notification(result, &dir, false, None, now));
        }
        FileDone::Crashed { seq, dir } => {
            // R15: a delete's record goes, as nothing is known about it. A Recover's or an Undo move's
            // steps go on to the reread, which keeps those whose photo is there.
            let mut hist = to.history.borrow_mut();
            hist.settle_delete(seq, false, &[]);
            hist.send_back(seq);
            hist.take_pictures(seq);
            drop(hist);
            to.ask_for_reread(&dir, true);
            to.pending.borrow_mut().push(NotifEntry::error(i18n::tr("Operation failed — the worker crashed mid-run (see log)"), now).in_dir(&dir));
        }
    }
}

/// R14: what the bin check is asked about one delete: the files its job says left, and when. The Mac's job
/// has confirmed against the Trash by then, so nothing there reads these.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) struct BinAsk {
    pub(crate) seq: u64,
    pub(crate) op_time: i64,
    pub(crate) files: Vec<PathBuf>,
}

/// R14: which of those files aren't in the bin. Only a delete with something missing has one of these.
pub(crate) struct BinFound {
    pub(crate) seq: u64,
    pub(crate) missing: Vec<PathBuf>,
}

/// R17: the wait that's due to say it's working, at the tick. A job's while the queue runs, then a clean
/// Recover's (`named`), since its job has said nothing and the person is still waiting for the photo. One
/// wait counts from one press, however the time divides between the job and the listing, so a person is
/// never left wondering. `said` is the stamp that has spoken: it stops it speaking again.
pub(crate) fn working_due(queue: &FileQueue, named: Option<&Named>, now: Instant, said: Option<u64>) -> Option<Waited> {
    if queue.quiet() {
        named.and_then(|named| named.clean.as_ref()).map(|clean| &clean.wait).filter(|wait| wait.due(now, said)).cloned()
    } else {
        queue.working_due(now, said)
    }
}

/// What reads the bin: the real thing in the app, a row's own in a test.
pub(crate) type BinRunner = Box<dyn Fn(&[BinAsk]) -> Vec<BinFound> + Send + Sync>;

struct BinShared {
    running: Arc<AtomicBool>,
    found: Mutex<Vec<BinFound>>,
    run: BinRunner,
}

/// R14: the check that what left the folder really is in the Recycle Bin, so Falcon doesn't offer a
/// Recover that can't work (F-P11 rule 3). Reading the whole bin can take a while when the bin is big,
/// so it runs at a quiet second, once for the whole burst, on a thread of its own, and no job waits
/// behind it. It only reads, so it doesn't stop anything else either, and quit doesn't wait for it.
#[derive(Clone)]
pub(crate) struct BinCheck(Arc<BinShared>);

impl BinCheck {
    pub(crate) fn new() -> Self {
        Self::with_runner(Box::new(|asks| TheDisk.missing_from_bin(asks)))
    }

    pub(crate) fn with_runner(run: BinRunner) -> Self {
        Self(Arc::new(BinShared { running: Default::default(), found: Default::default(), run }))
    }

    pub(crate) fn running(&self) -> bool {
        self.0.running.load(Ordering::SeqCst)
    }

    pub(crate) fn take_found(&self) -> Vec<BinFound> {
        std::mem::take(&mut *self.0.found.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// A check that panics (`CoCreateInstance(..).unwrap()` in the `trash` crate) ends its thread and says
    /// nothing, so the re-stat's verdict stands, as it does when the bin can't be listed.
    pub(crate) fn start(&self, asks: Vec<BinAsk>) {
        self.0.running.store(true, Ordering::SeqCst);
        let shared = self.0.clone();
        let started = std::thread::Builder::new().name("falcon-bin-check".into()).spawn(move || {
            let _running = support::FlagGuard(shared.running.clone());
            let found = (shared.run)(&asks);
            shared.found.lock().unwrap_or_else(|e| e.into_inner()).extend(found);
        });
        if let Err(e) = started {
            log_event(&format!("delete: couldn't start the bin check ({e}) — the re-stat's verdict stands"));
            self.0.running.store(false, Ordering::SeqCst);
        }
    }
}

/// R14: what the bin check found, folded on the interface thread. A file missing from the bin means the
/// photo can't come back, so its record shrinks or goes, its row stops offering Recover, and an amber line
/// says so. A delete Ctrl+Z has taken in the meantime isn't in the history any more, so the file the
/// check couldn't find is simply back in the folder, and nothing speaks: Del then Recover in one burst
/// gives no amber line. `true`: a row stopped offering, and the panel's rows need building again.
pub(crate) fn fold_bin_check(found: Vec<BinFound>, to: &FoldTo, log: &RefCell<VecDeque<NotifEntry>>, now: Instant) -> bool {
    let mut retired = false;
    for BinFound { seq, missing } in found {
        let Some(verdict) = to.history.borrow_mut().settle_not_in_bin(seq, missing) else { continue };
        if verdict.record_gone {
            tick::retire_row_offer(&mut log.borrow_mut(), seq);
            retired = true;
        }
        if let Some(line) = platform::not_in_bin_text(&verdict.subjects) {
            to.pending.borrow_mut().push(NotifEntry::warn(line, now).in_dir(&verdict.dir).about_delete(seq));
        }
        // A half that stayed gets its steps back, and a photo that's gone drops them.
        to.ask_for_reread(&verdict.dir, true);
    }
    retired
}

/// What a listing brings back, for the tick's acceptance.
pub(crate) struct Listed {
    pub(crate) result: std::io::Result<ScanDone>,
    /// R13: a photo's cloud flag differs from the list on screen. Stamps can't see it: a file freed by
    /// OneDrive keeps its size and date, and every reread used to heal it.
    pub(crate) cloud_flipped: bool,
    pub(crate) warn: Option<OpResult>,
    pub(crate) took: Duration,
}

struct Wanted {
    dir: PathBuf,
    at_once: bool,
}

struct Flight {
    dir: PathBuf,
    at_once: bool,
    gen: u64,
    pushes: u64,
    stale: Arc<AtomicBool>,
    arrived: Arc<Mutex<Option<Listed>>>,
    /// Arrived, and waiting for what a landing would close.
    listed: Option<Listed>,
}

/// What a new listing's thread needs from [`Reread::start`].
pub(crate) struct ListingTicket {
    pub(crate) dir: PathBuf,
    pub(crate) gen: u64,
    pub(crate) pushes: u64,
    pub(crate) stale: Arc<AtomicBool>,
    pub(crate) arrived: Arc<Mutex<Option<Listed>>>,
}

/// What the tick does with a listing this time round.
pub(crate) enum Acceptance {
    /// Nothing has arrived, or what has is waiting for a hold to end.
    Nothing,
    /// A landing or a press has moved on from what it read. It lists again.
    Discarded,
    /// The folder couldn't be read. M2: the list on screen stays.
    Failed,
    /// The folder is what the list on screen expects. No landing, no blink, nothing closes. It still says
    /// what the drive answered, since each reread refreshes that (R21).
    Unchanged(Option<bool>),
    Land(Box<ScanDone>, Option<OpResult>),
}

/// R13: the reread after a file operation, off the interface thread (it took 0.23 to 0.29 s there at
/// 16,000 photos, on every delete). On the interface thread, from the tick:
/// - A job's report, or a Move's worker, says it [`Self::want`]s one. So does Falcon coming back to the
///   front ([`ask_on_return`]), for what another program saved meanwhile.
/// - It [`Self::start`]s when [`Self::due`], and the thread lists the folder ([`spawn_listing`]).
/// - The tick asks [`Self::accept`] each tick for what to do with the result.
///
/// A listing is discarded at acceptance and not at arrival, because a hold can outlast a press: it's
/// out of date if the generation has moved (every landing bumps it, so a take-out, a sort pick and a
/// reopen all count), if a job has been pushed since it started (R2, so a Recover press invalidates it), or
/// if another reread has been asked for since. So a listing older than a take-out can't bring a photo back.
#[derive(Default)]
pub(crate) struct Reread {
    wanted: Option<Wanted>,
    flight: Option<Flight>,
    last_report: Option<Instant>,
}

impl Reread {
    /// A newer ask means something changed after any listing out there started.
    pub(crate) fn want(&mut self, dir: PathBuf, at_once: bool) {
        if let Some(flight) = &self.flight {
            flight.stale.store(true, Ordering::SeqCst);
        }
        let at_once = at_once || self.wanted.as_ref().is_some_and(|w| w.dir == dir && w.at_once);
        self.wanted = Some(Wanted { dir, at_once });
    }

    /// A job has reported. The quiet second counts from here, also for the bin check.
    pub(crate) fn note_report(&mut self, now: Instant) {
        self.last_report = Some(now);
    }

    /// R14: has the queue had its quiet second? The caller asks the queue itself.
    pub(crate) fn quiet_a_second(&self, now: Instant) -> bool {
        self.last_report.is_none_or(|at| now.saturating_duration_since(at) >= QUIET_BEFORE_LISTING)
    }

    /// One listing at a time, once the queue is quiet and no Move or Empty is under way, so it reads what
    /// they did.
    pub(crate) fn due(&self, now: Instant, queue_quiet: bool, position_op: bool) -> bool {
        self.flight.is_none() && queue_quiet && !position_op
            && self.wanted.as_ref().is_some_and(|w| w.at_once || self.quiet_a_second(now))
    }

    pub(crate) fn start(&mut self, gen: u64, pushes: u64) -> Option<ListingTicket> {
        let Wanted { dir, at_once } = self.wanted.take()?;
        let (stale, arrived): (Arc<AtomicBool>, Arc<Mutex<Option<Listed>>>) = Default::default();
        self.flight = Some(Flight { dir: dir.clone(), at_once, gen, pushes, stale: stale.clone(), arrived: arrived.clone(), listed: None });
        Some(ListingTicket { dir, gen, pushes, stale, arrived })
    }

    /// Wanted, listing, or waiting to land: only a landing that arrives on its own waits for this (the
    /// Date taken landing). The reread itself doesn't: it would wait for itself.
    pub(crate) fn wanted_or_running(&self) -> bool {
        self.wanted.is_some() || self.flight.is_some()
    }

    /// The tick stays awake for this. A listing that has arrived and waits for a hold doesn't count: what
    /// ends the hold wakes the tick by itself, and an open menu shouldn't hold the full rate.
    pub(crate) fn working(&self) -> bool {
        self.wanted.is_some() || self.flight.as_ref().is_some_and(|f| f.listed.is_none())
    }

    /// N7: a folder off screen has nothing to land in. `true`: something was forgotten, which goes with
    /// its name and its steps ([`no_landing`]).
    pub(crate) fn forget_unless(&mut self, on_screen: impl Fn(&Path) -> bool) -> bool {
        let mut forgot = false;
        if self.wanted.as_ref().is_some_and(|w| !on_screen(&w.dir)) {
            self.wanted = None;
            forgot = true;
        }
        if self.flight.as_ref().is_some_and(|f| !on_screen(&f.dir)) {
            if let Some(flight) = self.flight.take() {
                flight.stale.store(true, Ordering::SeqCst);
            }
            forgot = true;
        }
        forgot
    }

    /// The acceptance, in this order. `held` is what a landing would close or end
    /// (`support::LandingHolds`), asked only if the folder really changed. It's the person's, and there
    /// is no timeout, because nothing is lost by waiting.
    pub(crate) fn accept(&mut self, gen_now: u64, pushes_now: u64, stamps: Option<&ListingStamps>, held: impl FnOnce() -> bool) -> Acceptance {
        let Some(flight) = self.flight.as_mut() else { return Acceptance::Nothing };
        if flight.listed.is_none() {
            flight.listed = flight.arrived.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        if flight.listed.is_none() {
            return Acceptance::Nothing;
        }
        if flight.stale.load(Ordering::SeqCst) || flight.gen != gen_now || flight.pushes != pushes_now {
            let (dir, at_once) = (flight.dir.clone(), flight.at_once);
            self.flight = None;
            log_event("reread: discarded, older than a landing or a press");
            if self.wanted.is_none() {
                self.wanted = Some(Wanted { dir, at_once });
            }
            return Acceptance::Discarded;
        }
        let Some(listed) = flight.listed.as_ref() else { return Acceptance::Nothing };
        let took = listed.took.as_millis();
        let unchanged = match &listed.result {
            Ok(done) => !listed.cloud_flipped && done.stamps.as_ref() == stamps,
            Err(e) => {
                log_event(&format!("scan: couldn't read {} ({e}) — keeping the current folder", flight.dir.display()));
                self.flight = None;
                return Acceptance::Failed;
            }
        };
        if unchanged {
            let delete_asks = listed.result.as_ref().ok().and_then(|done| done.delete_asks);
            self.flight = None;
            log_event(&format!("reread: unchanged in {took} ms"));
            return Acceptance::Unchanged(delete_asks);
        }
        if held() {
            return Acceptance::Nothing;
        }
        match self.flight.take().and_then(|f| f.listed) {
            Some(Listed { result: Ok(done), warn, .. }) => {
                log_event(&format!("reread: changed, listed in {took} ms"));
                Acceptance::Land(Box::new(done), warn)
            }
            _ => Acceptance::Nothing,
        }
    }
}

/// The refresh on coming back (11 November 2026). The owner: "it should try to diff the folder and refresh if
/// changes happen as usually I would save a photo next to the original", and of zoom and Compare when it
/// lands: "Yes keep don't reset". PURE: has Falcon just become the active program? Only on not in front,
/// then in front. `was` is the last known answer and `now` this tick's (`background_memory::falcon_is_active`),
/// where `None` is unknown (mid-switch, a locked desktop): the caller keeps the last known answer through an
/// unknown, so in front, unknown, in front is no return, and away, unknown, in front is one. Nothing known
/// yet (the first tick after launch) is no return. Falcon's own dialogs count as Falcon, so a file dialog
/// closing isn't one either.
pub(crate) fn came_back(was: Option<bool>, now: Option<bool>) -> bool {
    was == Some(false) && now == Some(true)
}

/// The refresh on coming back: once a tick, from the tick's file block, ahead of the reread's acceptance. On a
/// return it asks for a reread of the folder on screen (`on_screen`, the reread's own folder guard's
/// `LastScannedDir`, empty when nothing has been scanned), at once, without the quiet second a clean delete
/// waits. That's all it does: the listing, its holds, its out-of-date checks and its landing by photo are the
/// reread's, so an unchanged folder lands nothing, and a listing reads names and dates only, never a photo
/// (the cloud rule). `in_front` is the last known answer, updated here. `true`: it asked.
pub(crate) fn ask_on_return(in_front: &Cell<Option<bool>>, now: Option<bool>, on_screen: &str, reread: &RefCell<Reread>) -> bool {
    let was = in_front.get();
    if now.is_some() {
        in_front.set(now);
    }
    if !came_back(was, now) || on_screen.is_empty() {
        return false;
    }
    reread.borrow_mut().want(PathBuf::from(on_screen), true);
    log_event("reread: Falcon is in front again");
    true
}

/// R13: a listing that lands nothing, or is forgotten, still ends the wait for it. A photo's steps go back
/// on the stack if it's in the list on screen (`list`), since no landing is coming to put them there, and
/// drop if it isn't. A list that couldn't be read has nothing to give them. A name has nothing to land on,
/// and it goes back to the caller, because a clean Recover still owes the person a word (R36).
pub(crate) fn no_landing(history: &RefCell<FileHistory>, undo: &RefCell<UndoRedo>, landing_name: &RefCell<Option<Named>>, list: Option<&[Shot]>) -> Option<Named> {
    let named = landing_name.borrow_mut().take();
    history.borrow_mut().brought_back.clear(); // R46: nothing landed, so no edge points at it
    let returned = std::mem::take(&mut history.borrow_mut().returning);
    if let Some(list) = list {
        support::return_parked_steps(&mut undo.borrow_mut(), returned, list);
    }
    named
}

/// What a listing reads the folder with, taken on the interface thread when it starts.
pub(crate) struct ListingInput {
    /// The list on screen, for the cloud flags.
    pub(crate) live: Arc<Vec<Shot>>,
    /// `reload`'s same-folder sort rule: the live attr first.
    pub(crate) live_sort: Option<String>,
    pub(crate) sort_lru: Vec<(String, String)>,
    pub(crate) sort_cap: usize,
    pub(crate) inline_dates: bool,
}

/// The listing itself: an explicit reopen's own read (`support::reopen_scan`), which sorts by the folder's
/// live sort and lands by photo at acceptance, carrying what changed meanwhile.
pub(crate) fn run_listing(dir: &Path, input: ListingInput, read: impl FnOnce(&Path) -> std::io::Result<falcon_decode::FolderScan>) -> Listed {
    let began = Instant::now();
    let warn = Mutex::new(None);
    let result = support::reopen_scan(0, dir.to_path_buf(), None, input.live_sort, &input.sort_lru, input.sort_cap, None, input.inline_dates, &warn, read);
    let cloud_flipped = result.as_ref().is_ok_and(|done| support::cloud_flag_differs(&input.live, &done.shots));
    Listed { result, cloud_flipped, warn: warn.into_inner().unwrap_or_else(|e| e.into_inner()), took: began.elapsed() }
}

/// Posts a failed listing if the thread panics, so nothing waits for it for ever: not the next listing,
/// not a Date taken landing, not a Ctrl+Z waiting for its photo.
struct ListingGuard(Arc<Mutex<Option<Listed>>>);

impl Drop for ListingGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(failed_listing("the listing crashed"));
        }
    }
}

fn failed_listing(why: &str) -> Listed {
    Listed { result: Err(std::io::Error::other(why.to_string())), cloud_flipped: false, warn: None, took: Duration::ZERO }
}

/// The thread, with what it runs handed in. A listing that has gone out of date stops reading.
pub(crate) fn spawn_listing_with(ticket: ListingTicket, run: impl FnOnce(&Path, &AtomicBool) -> Listed + Send + 'static) {
    let ListingTicket { dir, stale, arrived, .. } = ticket;
    let on_thread = arrived.clone();
    let started = std::thread::Builder::new().name("falcon-reread".into()).spawn(move || {
        let _guard = ListingGuard(on_thread.clone());
        let listed = run(&dir, &stale);
        *on_thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(listed);
    });
    if let Err(e) = started {
        *arrived.lock().unwrap_or_else(|e| e.into_inner()) = Some(failed_listing(&format!("couldn't start the reread's thread ({e})")));
    }
}

/// R13: the listing on its own thread. The generation and the queue say when it's out of date, for a
/// listing the tick would discard anyway, so the next one starts sooner.
pub(crate) fn spawn_listing(ticket: ListingTicket, input: ListingInput, generation: Arc<AtomicU64>, queue: FileQueue) {
    let (gen, pushes) = (ticket.gen, ticket.pushes);
    spawn_listing_with(ticket, move |dir, stale| {
        let moved_on = || stale.load(Ordering::SeqCst) || generation.load(Ordering::Relaxed) != gen || queue.pushes() != pushes;
        let listed = run_listing(dir, input, |d| falcon_decode::scan_folder_with_metadata_cancellable(d, moved_on));
        support::reap_stale_temps(dir);
        listed
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue that writes each job down, as it starts, in words naming the photo. Each job waits for
    /// its own stamp's gate if the row has one, so a row can hold one job and let the rest go.
    fn writing_queue(said: &Arc<Mutex<Vec<String>>>, gates: &'static [(u64, Mutex<()>)]) -> FileQueue {
        let said = said.clone();
        FileQueue::with_runner(Box::new(move |job| {
            let (seq, dir) = job.about();
            let _held = gates.iter().find(|(s, _)| *s == seq).map(|(_, g)| g.lock().unwrap_or_else(|e| e.into_inner()));
            let words = match &job {
                FileJob::Delete { targets, .. } => format!("delete {}", targets[0].name),
                FileJob::Recover { name, .. } => format!("recover {name}"),
                FileJob::UndoMove { .. } => "undo move".to_string(),
            };
            said.lock().unwrap().push(words);
            FileDone::Crashed { seq, dir }
        }))
    }

    fn photo(name: &str) -> Shot {
        Shot { id: 0, name: name.into(), raw: None, jpg: Some(PathBuf::from(format!("{name}.png"))),
            has_raw: false, has_jpg: true, kind: falcon_decode::SrcKind::Png, cloud_placeholder: false, sniffed: None }
    }

    /// The real file calls, with no writer to wait for. Nothing here reaches the bin: every row's press has
    /// nothing for it.
    struct NoBinDisk;
    impl FileIo for NoBinDisk {
        fn barrier(&self) {}
        fn recycle(&self, _paths: &[PathBuf]) -> Recycled {
            panic!("a press that skips the bin never reaches it");
        }
    }

    /// A generated folder, gone with the row.
    struct GeneratedFolder(PathBuf);
    impl GeneratedFolder {
        fn new(row: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("falcon_for_good_{row}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"generated").unwrap();
            path
        }
        /// A photo, RAW first when it has one, with an XMP sidecar beside each of its files.
        fn pair(&self, name: &str) -> Shot {
            let (raw, jpg) = (self.file(&format!("{name}.CR3")), self.file(&format!("{name}.JPG")));
            std::fs::write(sidecar_path_for(&raw, true), b"<x/>").unwrap();
            std::fs::write(sidecar_path_for(&jpg, false), b"<x/>").unwrap();
            Shot { id: 0, name: name.into(), raw: Some(raw), jpg: Some(jpg), has_raw: true, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None }
        }
        fn left(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        }
    }
    impl Drop for GeneratedFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Only the photos named, by Falcon itself: what a press on a drive with no bin hands the job.
    fn for_good_job(dir: &Path, photos: Vec<Shot>) -> (FileDone, Arc<OnceLock<DeleteOutcome>>) {
        let outcome: Arc<OnceLock<DeleteOutcome>> = Default::default();
        let job = FileJob::Delete { seq: 1, dir: dir.display().to_string(), targets: Vec::new(), for_good: ForGood { photos, from: GoneFrom::Drive }, outcome: outcome.clone() };
        (run_job(job, &NoBinDisk), outcome)
    }

    fn what_it_said(done: FileDone) -> (String, usize, bool, bool) {
        let FileDone::Delete { msg, fail, can_recover, for_good, .. } = done else { panic!("a delete's report") };
        (msg, fail, can_recover, for_good)
    }

    /// R22: after Falcon's dialog, `remove_file` per target, not the crate. A clean pair goes whole, side
    /// files too, and nothing of it reaches the bin, so nothing offers it back. On generated files, twice,
    /// and the other photo in the folder is never touched.
    ///
    /// FALSIFIER (L28): `run_delete` recycles the for-good photos: `NoBinDisk::recycle` panics. It forgets
    /// their side files (`list_files` of the shot's photo files only): the folder row. `can_recover` true
    /// for a for-good photo: the pill. `for_good` false in the report: the last term.
    #[test]
    fn a_for_good_job_takes_a_clean_pair_and_its_side_files() {
        let folder = GeneratedFolder::new("clean");
        let bee = folder.file("BEE.JPG");
        for photo in ["ANT", "CAT"] {
            let shot = folder.pair(photo);
            let (done, outcome) = for_good_job(&folder.0, vec![shot]);
            let (msg, fail, can_recover, for_good) = what_it_said(done);
            assert_eq!((fail, can_recover, for_good), (0, false, true), "{photo}: clean, with no way back, and its steps can move on");
            assert!(msg.contains(photo) && msg.contains("[RAW+JPG]") && msg.ends_with("for good"), "{photo}: {msg}");
            assert!(outcome.get().is_none_or(|o| o.files.is_empty()), "{photo}: nothing is recorded as in the bin");
            assert_eq!(folder.left(), ["BEE.JPG"], "{photo}: every one of its files has gone, and only its");
        }
        assert!(bee.is_file());
    }

    /// R22: stop at the first failure inside a photo, so a locked RAW leaves the pair whole. The re-stat is
    /// what's reported. Twice, and the second time another program has let go.
    ///
    /// FALSIFIER (L28): `delete_photo_for_good_with` carries on past a failure (`break` gone): the JPEG
    /// goes, and the folder row sees it. `how_it_went` reads the attempt rather than the re-stat: the words.
    #[cfg(windows)]
    #[test]
    fn a_locked_raw_leaves_the_whole_pair() {
        use std::os::windows::fs::OpenOptionsExt;
        let folder = GeneratedFolder::new("locked_raw");
        let shot = folder.pair("IMG_7");
        let raw = shot.raw.clone().unwrap();
        let all = folder.left();
        assert_eq!(all.len(), 4, "the pair and its side files: {all:?}");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&raw).unwrap();
        for _ in 0..2 {
            let (done, _) = for_good_job(&folder.0, vec![shot.clone()]);
            let (msg, fail, can_recover, _) = what_it_said(done);
            assert_eq!(folder.left(), all, "nothing went");
            assert_eq!((fail, can_recover), (1, false));
            assert!(msg.starts_with("Couldn't delete IMG_7 [RAW+JPG], another program may have it open."), "{msg}");
        }
        drop(held);
        let (done, _) = for_good_job(&folder.0, vec![shot]);
        assert_eq!(what_it_said(done).1, 0);
        assert!(folder.left().is_empty(), "with the file free, it all goes");
    }

    /// R22: a locked JPEG gives a partial result: the RAW and its side file have gone, and the sentence
    /// says which half is still there. The re-stat agrees with the folder.
    ///
    /// FALSIFIER (L28): the job reports every file as gone (`gone` from the attempt, not the re-stat): the
    /// words. The sentence names no format: the words. `Part` swaps its two halves: the words.
    #[cfg(windows)]
    #[test]
    fn a_locked_jpeg_gives_a_partial_result_and_the_re_stat_agrees() {
        use std::os::windows::fs::OpenOptionsExt;
        let folder = GeneratedFolder::new("locked_jpeg");
        let shot = folder.pair("IMG_7");
        let jpg = shot.jpg.clone().unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&jpg).unwrap();
        let (done, _) = for_good_job(&folder.0, vec![shot]);
        drop(held);
        let (msg, fail, can_recover, _) = what_it_said(done);
        assert_eq!(msg, "Deleted the RAW of IMG_7 for good. The JPG is still in the folder.");
        assert_eq!((fail, can_recover), (1, false), "red, with no way back for the RAW");
        let left = folder.left();
        assert_eq!(left.len(), 2, "the JPEG and its own side file: {left:?}");
        assert!(left.iter().all(|f| f.starts_with("IMG_7.JPG")), "{left:?}");
    }

    /// R22: "Stop at the first failure inside a photo, then go on to the next photo." A selection of three
    /// with the first one's file locked: the other two go, and it says two of three.
    ///
    /// FALSIFIER (L28): one failure ends the job (`list` all, `remove_for_good` once on the lot): the
    /// second and third stay.
    #[cfg(windows)]
    #[test]
    fn a_for_good_job_goes_on_to_the_next_photo() {
        use std::os::windows::fs::OpenOptionsExt;
        let folder = GeneratedFolder::new("next_photo");
        let (ant, bee, cat) = (folder.pair("ANT"), folder.pair("BEE"), folder.pair("CAT"));
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(ant.raw.as_ref().unwrap()).unwrap();
        let (done, _) = for_good_job(&folder.0, vec![ant, bee, cat]);
        drop(held);
        let (msg, fail, _, _) = what_it_said(done);
        assert_eq!(msg, "Deleted 2 of 3 photos for good. 1 is still in the folder.");
        assert_eq!(fail, 1);
        assert!(folder.left().iter().all(|f| f.starts_with("ANT.")), "only ANT's are left: {:?}", folder.left());
    }

    /// R22, R24: what a press that skipped the bin says, in the owner's approved words where they exist
    /// (`QUESTIONS.md`, second set, question 6), and the rest in the same grammar. None says it went to the
    /// bin, or that it can come back, unless it did (the mixed line's first half).
    ///
    /// FALSIFIER (L28): change any sentence, or a branch of `for_good_text`: the line naming it.
    #[cfg(windows)]
    #[test]
    fn a_for_good_result_says_what_happened() {
        fn photo_of(name: &str, pair: bool) -> Shot {
            Shot { id: 0, name: name.into(), raw: pair.then(|| PathBuf::from(format!("{name}.CR3"))), jpg: Some(PathBuf::from(format!("{name}.JPG"))),
                has_raw: pair, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None }
        }
        let (solo, double) = (photo_of("IMG_1234", false), photo_of("IMG_1234", true));
        let one = |from, shot, how| for_good_text(from, &[(shot, how)], None);
        let part = || Went::Part { gone: "RAW".into(), stayed: "JPG".into() };
        // The approved ones.
        assert_eq!(one(GoneFrom::Drive, &double, Went::Whole), ("Deleted IMG_1234 [RAW+JPG] for good".to_string(), 0));
        assert_eq!(one(GoneFrom::Drive, &solo, Went::Whole), ("Deleted IMG_1234 for good".to_string(), 0));
        assert_eq!(one(GoneFrom::Cloud, &solo, Went::Whole), ("Deleted IMG_1234 from OneDrive".to_string(), 0));
        assert_eq!(one(GoneFrom::Drive, &double, Went::Stayed), ("Couldn't delete IMG_1234 [RAW+JPG], another program may have it open. It's still in the folder.".to_string(), 1));
        // In their grammar.
        assert_eq!(one(GoneFrom::Drive, &double, part()), ("Deleted the RAW of IMG_1234 for good. The JPG is still in the folder.".to_string(), 1));
        assert_eq!(one(GoneFrom::Cloud, &double, part()), ("Deleted the RAW of IMG_1234 from OneDrive. The JPG is still in the folder.".to_string(), 1));
        assert_eq!(one(GoneFrom::Drive, &solo, Went::AlreadyGone), ("Nothing to delete — files already gone.".to_string(), 0));
        let (ant, bee, cat) = (photo_of("ANT", false), photo_of("BEE", true), photo_of("CAT", false));
        let many = |from, a: Went, b: Went, c: Went, to_bin| for_good_text(from, &[(&ant, a), (&bee, b), (&cat, c)], to_bin);
        assert_eq!(many(GoneFrom::Drive, Went::Whole, Went::Whole, Went::Whole, None), ("Deleted 3 photos for good".to_string(), 0));
        assert_eq!(many(GoneFrom::Cloud, Went::Whole, Went::Whole, Went::Whole, None), ("Deleted 3 photos from OneDrive".to_string(), 0));
        assert_eq!(many(GoneFrom::Drive, Went::Whole, Went::Stayed, Went::Whole, None), ("Deleted 2 of 3 photos for good. 1 is still in the folder.".to_string(), 1));
        assert_eq!(many(GoneFrom::Cloud, Went::Whole, Went::Stayed, Went::Stayed, None), ("Deleted 1 of 3 photos from OneDrive. 2 are still in the folder.".to_string(), 2));
        assert_eq!(many(GoneFrom::Drive, Went::Stayed, Went::Stayed, Went::Stayed, None), ("Couldn't delete 3 photos, another program may have them open. They're still in the folder.".to_string(), 3));
        // R24: the mixed selection, for the owner to read when clicking.
        assert_eq!(many(GoneFrom::Cloud, Went::Whole, Went::Whole, Went::Whole, Some((2, 0))), ("Sent 2 photos to the Recycle Bin. Deleted 3 from OneDrive.".to_string(), 0));
        assert_eq!(many(GoneFrom::Cloud, Went::Whole, Went::Stayed, Went::Whole, Some((1, 1))), ("Sent 1 photo to the Recycle Bin. Deleted 2 from OneDrive. 2 are still in the folder.".to_string(), 2));
        assert_eq!(many(GoneFrom::Cloud, Went::Stayed, Went::Stayed, Went::Stayed, Some((1, 0))), ("Sent 1 photo to the Recycle Bin. 3 are still in the folder.".to_string(), 3));
        assert_eq!(many(GoneFrom::Cloud, Went::Whole, Went::Whole, Went::Whole, Some((0, 1))), ("Deleted 3 from OneDrive. 1 is still in the folder.".to_string(), 1));
        assert_eq!(many(GoneFrom::Cloud, Went::Stayed, Went::Stayed, Went::Stayed, Some((0, 1))).0, "Couldn't delete 4 photos, another program may have them open. They're still in the folder.");
    }

    /// R29, R47: what a press whose photos went to the bin says, in the owner's approved words: "Sent IMG_1234
    /// [RAW+JPG] to the Recycle Bin", "Couldn't delete IMG_1234 [RAW+JPG], another program may have it open. It's
    /// still in the folder." Never a file count, never the system's error.
    ///
    /// FALSIFIER (L28): change any sentence, or a branch of `press_text`: the line naming it.
    #[cfg(windows)]
    #[test]
    fn a_bin_result_says_what_happened() {
        fn photo_of(name: &str, pair: bool) -> Shot {
            Shot { id: 0, name: name.into(), raw: pair.then(|| PathBuf::from(format!("{name}.CR3"))), jpg: Some(PathBuf::from(format!("{name}.JPG"))),
                has_raw: pair, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None }
        }
        let (solo, double) = (photo_of("IMG_1234", false), photo_of("IMG_1234", true));
        let one = |shot, how| bin_text(&[(shot, how)]);
        let part = || Went::Part { gone: "RAW".into(), stayed: "JPG".into() };
        assert_eq!(one(&solo, Went::Whole), ("Sent IMG_1234 to the Recycle Bin".to_string(), 0));
        assert_eq!(one(&double, Went::Whole), ("Sent IMG_1234 [RAW+JPG] to the Recycle Bin".to_string(), 0));
        assert_eq!(one(&double, part()), ("Sent the RAW of IMG_1234 to the Recycle Bin. The JPG is still in the folder.".to_string(), 1));
        assert_eq!(one(&double, Went::Stayed), ("Couldn't delete IMG_1234 [RAW+JPG], another program may have it open. It's still in the folder.".to_string(), 1));
        assert_eq!(one(&solo, Went::Stayed), ("Couldn't delete IMG_1234, another program may have it open. It's still in the folder.".to_string(), 1));
        assert_eq!(one(&solo, Went::AlreadyGone), ("Nothing to delete — files already gone.".to_string(), 0));
        // The approved selection's sentences, with its own numbers.
        let thirty: Vec<Shot> = (0..30).map(|i| photo_of(&format!("IMG_{i}"), i % 2 == 0)).collect();
        let of = |stay: usize| bin_text(&thirty.iter().enumerate().map(|(i, s)| (s, if i < stay { Went::Stayed } else { Went::Whole })).collect::<Vec<_>>());
        assert_eq!(of(0), ("Sent 30 selected photos to the Recycle Bin".to_string(), 0));
        assert_eq!(of(2), ("Sent 28 of 30 photos to the Recycle Bin. 2 are still in the folder.".to_string(), 2));
        assert_eq!(of(29), ("Sent 1 of 30 photos to the Recycle Bin. 29 are still in the folder.".to_string(), 29));
        assert_eq!(of(30), ("Couldn't delete 30 photos, another program may have them open. They're still in the folder.".to_string(), 30));
        let (ant, bee) = (photo_of("ANT", false), photo_of("BEE", true));
        assert_eq!(bin_text(&[(&ant, Went::Whole), (&bee, part())]), ("Sent 1 of 2 photos to the Recycle Bin. 1 is still in the folder.".to_string(), 1), "half a pair isn't a photo that went");
        assert_eq!(bin_text(&[(&ant, Went::AlreadyGone), (&bee, Went::AlreadyGone)]).0, "Nothing to delete — files already gone.");
    }

    /// A side file that stays after its photo has gone is in the log: what a person reads is about the photo. The
    /// next row is a real sentence from a real job, through a re-stat that left the sidecar. Only the record's files go back.
    ///
    /// FALSIFIER (L28): `fail` counts files again: the row reads red. `bin_text` is given files instead of photos: its words.
    #[test]
    fn a_side_file_that_stays_doesnt_turn_the_sentence_red() {
        struct SideFileStays;
        impl FileIo for SideFileStays {
            fn barrier(&self) {}
            fn list_files(&self, targets: &[Shot]) -> Vec<PathBuf> {
                targets.iter().flat_map(|s| [s.jpg.clone(), Some(PathBuf::from("ANT.JPG.xmp"))]).flatten().collect()
            }
            fn recycle(&self, paths: &[PathBuf]) -> Recycled {
                Recycled {
                    files: paths.iter().filter(|p| p.extension().is_none_or(|e| e != "xmp")).cloned().collect(), confirmed: true,
                    #[cfg(target_os = "macos")]
                    trash_paths: Vec::new(),
                }
            }
        }
        let shot = Shot { id: 0, name: "ANT".into(), raw: None, jpg: Some(PathBuf::from("ANT.JPG")), has_raw: false, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        let job = FileJob::Delete { seq: 1, dir: "folder".into(), targets: vec![shot], for_good: Default::default(), outcome: Default::default() };
        let FileDone::Delete { msg, fail, can_recover, .. } = run_job(job, &SideFileStays) else { panic!("a delete") };
        assert_eq!((msg, fail, can_recover), (platform::sent_one_text("ANT"), 0, true));
    }

    /// R36, R44: Undo move says it in the owner's words when everything came back: "Brought back 30 photos",
    /// "Brought back IMG_1234 [RAW+JPG]". If anything didn't, it says so in what was counted, as before.
    /// R48: everything back is clean, so it doesn't light the bell; a file skipped or a side file left behind is
    /// a plain success, which does; a failed one is red.
    ///
    /// FALSIFIER (L28): `run_undo_move` always says the plural: the first row. It says it with a file skipped or
    /// failed: those rows. It lets an empty subject through: the last. It leaves out `sk` or `side_not` from what
    /// was said: those rows' severity.
    #[test]
    fn an_undo_move_says_it_in_photos() {
        use support::OpSeverity::{Clean, Error, Success};
        struct Back(usize, usize, usize, usize);
        impl FileIo for Back {
            fn barrier(&self) {}
            fn undo_moves(&self, _pairs: &[(PathBuf, PathBuf)]) -> (usize, usize, usize, usize) {
                (self.0, self.1, self.2, self.3)
            }
        }
        let say = |subject: &str, back: Back| {
            let job = FileJob::UndoMove { seq: 1, dir: "folder".into(), pairs: Vec::new(), subject: subject.into() };
            let FileDone::UndoMove { msg, severity, .. } = run_job(job, &back) else { panic!("an undo move") };
            (msg, severity)
        };
        let brought_back = |what: &str| platform::brought_back_text(what);
        assert_eq!(say("IMG_1234 [RAW+JPG]", Back(2, 0, 0, 0)), (brought_back("IMG_1234 [RAW+JPG]"), Clean));
        assert_eq!(say("30 photos", Back(60, 0, 0, 0)), (brought_back("30 photos"), Clean));
        let (skipped, severity) = say("30 photos", Back(59, 1, 0, 0));
        assert!(skipped.contains("59") && !skipped.starts_with(&brought_back("")), "a file skipped: {skipped}");
        assert_eq!(severity, Success, "a file skipped still asks to be read");
        let (failed, severity) = say("30 photos", Back(59, 0, 1, 0));
        assert!(failed.contains("59") && severity == Error, "a file failed: {failed}");
        let (side, severity) = say("30 photos", Back(60, 0, 0, 1));
        assert!(side.contains("60") && !side.starts_with(&brought_back("")), "a side file left behind: {side}");
        assert_eq!(severity, Success, "a side file left behind still asks to be read");
        let (nameless, severity) = say("", Back(2, 0, 0, 0));
        assert!(nameless.contains('2') && !nameless.starts_with(&brought_back("")), "no photo on the list to name: {nameless}");
        assert_eq!(severity, Clean, "everything came back, so nothing is wrong");
    }

    /// R17: "a person is never left wondering". A clean Recover's report ends no wait: it's the same wait, from the
    /// press, whichever way the time falls between the job and the listing. A wait that spoke while its job ran
    /// isn't said twice. A report that speaks (red or amber) ends it, as ever.
    ///
    /// FALSIFIER (L28): `ends_wait` answers a clean Recover: the first row. `working_due` ignores the name: the slow
    /// listing row. It counts from the report: the second before it. It reads the name while a job runs too: the flip row,
    /// which says the two in turn, each tick.
    #[test]
    fn a_clean_recovers_wait_lasts_until_the_landing() {
        let t0 = Instant::now();
        let wait = |seq| Waited { seq, pressed: t0, text: format!("Recovering {seq}…") };
        let recover = |recovered, failed| FileDone::Recover { seq: 1, dir: String::new(), files: Vec::new(), recovered, failed, msg: Some("said".into()), nav_at_press: 0, seats: Vec::new(), waited: wait(1) };
        assert_eq!(recover(2, 0).ends_wait(), None, "clean: the person is still waiting for the photo");
        assert_eq!(recover(1, 1).ends_wait(), Some(1), "part of it: an amber row speaks");
        assert_eq!(recover(0, 2).ends_wait(), Some(1), "none of it: a red row speaks");
        assert_eq!(FileDone::UndoMove { seq: 3, dir: String::new(), msg: String::new(), severity: support::OpSeverity::Clean, back: Vec::new() }.ends_wait(), Some(3), "Undo move has its card");

        // The job took no time, and the listing takes its time.
        let queue = FileQueue::with_runner(Box::new(|_| panic!("no job in this row")));
        let named = Named { clean: Some(CleanRecover { words: "Recovered".into(), wait: wait(1) }), ..Named::default() };
        let after = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(working_due(&queue, Some(&named), after(999), None), None, "not yet");
        assert_eq!(working_due(&queue, Some(&named), after(1_000), None), Some(wait(1)), "a second from the press, not from the report");
        assert_eq!(working_due(&queue, Some(&named), after(9_000), Some(1)), None, "once");
        assert_eq!(working_due(&queue, None, after(9_000), None), None, "nothing's waiting");
        let partial = Named { clean: None, ..Named::default() };
        assert_eq!(working_due(&queue, Some(&partial), after(9_000), None), None, "its row has spoken");
        // Another Recover still running: its wait says so, and the name's waits behind it. Never the two in turn.
        static GATE: Mutex<()> = Mutex::new(());
        let queue = FileQueue::with_runner(Box::new(|job| {
            let _held = GATE.lock().unwrap_or_else(|e| e.into_inner());
            let (seq, dir) = job.about();
            FileDone::Crashed { seq, dir }
        }));
        let held = GATE.lock().unwrap();
        queue.push(recovering(2, "BEE"), Some(wait(2)));
        let mut said = None;
        for _ in 0..4 {
            if let Some(w) = working_due(&queue, Some(&named), after(2_000), said) {
                assert_eq!(w.seq, 2, "the running job's");
                assert_eq!(said, None, "and once");
                said = Some(w.seq);
            }
        }
        drop(held);
        until_quiet(&queue);
    }

    /// `how_it_went` reads what the job's re-stat found, of the files it found to delete.
    ///
    /// FALSIFIER (L28): a file that had already left counts as one that stayed: the half-a-pair row. The two
    /// halves swapped: the words. A photo with nothing to delete counts as whole: the last row.
    #[test]
    fn how_a_for_good_photo_went_comes_from_the_re_stat() {
        let (raw, jpg) = (PathBuf::from("IMG_7.CR3"), PathBuf::from("IMG_7.JPG"));
        let shot = Shot { id: 0, name: "IMG_7".into(), raw: Some(raw.clone()), jpg: Some(jpg.clone()), has_raw: true, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        let set = |files: &[&PathBuf]| -> HashSet<PathBuf> { files.iter().map(|f| (*f).clone()).collect() };
        let how = |listed: &[&PathBuf], gone: &[&PathBuf]| {
            let (listed, gone) = (set(listed), set(gone));
            how_it_went(&shot, &listed.iter().map(PathBuf::as_path).collect(), &gone.iter().map(PathBuf::as_path).collect())
        };
        assert_eq!(how(&[&raw, &jpg], &[&raw, &jpg]), Went::Whole);
        assert_eq!(how(&[&raw, &jpg], &[]), Went::Stayed);
        assert_eq!(how(&[&raw, &jpg], &[&raw]), Went::Part { gone: "RAW".into(), stayed: "JPG".into() });
        assert_eq!(how(&[&raw, &jpg], &[&jpg]), Went::Part { gone: "JPG".into(), stayed: "RAW".into() });
        assert_eq!(how(&[&jpg], &[&jpg]), Went::Whole, "its RAW had left already, so nothing stayed");
        assert_eq!(how(&[], &[]), Went::AlreadyGone);
    }

    fn a_wait(seq: u64) -> Waited {
        Waited { seq, pressed: Instant::now(), text: "Recovering…".to_string() }
    }

    fn deleting(seq: u64, name: &str) -> FileJob {
        FileJob::Delete { seq, dir: "folder".into(), targets: vec![photo(name)], for_good: Default::default(), outcome: Default::default() }
    }

    fn recovering(seq: u64, photo: &str) -> FileJob {
        FileJob::Recover { seq, dir: "folder".into(), name: photo.into(), plural: false, outcome: Default::default(), not_in_bin: Vec::new(), nav_at_press: 0, seats: Vec::new(), waited: a_wait(seq) }
    }

    fn until_quiet(queue: &FileQueue) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !queue.quiet() {
            assert!(Instant::now() < deadline, "the queue should go quiet");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// One worker, press order, and a push never refused, whatever is running. Quiet only when the last
    /// has finished, and by then every result is there to fold. A second burst starts a new thread.
    ///
    /// FALSIFIER (L28): `pop_back` in `work`: the jobs run backwards. `push` doesn't raise `busy`: it reads
    /// quiet with jobs waiting. `finish` lowers `busy` first with jobs left: the same. `worker` stays
    /// true when the thread ends: the second burst never runs.
    #[test]
    fn queue_runs_in_press_order_and_never_refuses() {
        static GATES: [(u64, Mutex<()>); 1] = [(4, Mutex::new(()))];
        let said = Arc::new(Mutex::new(Vec::new()));
        let queue = writing_queue(&said, &GATES);
        assert!(queue.quiet(), "nothing yet");
        let held = GATES[0].1.lock().unwrap();
        queue.push(deleting(1, "ANT"), None);
        queue.push(deleting(2, "BEE"), None);
        queue.push(deleting(3, "CAT"), None);
        queue.push(recovering(4, "CAT"), None);
        assert!(!queue.quiet(), "work in hand");
        let deadline = Instant::now() + Duration::from_secs(20);
        while said.lock().unwrap().len() < 3 {
            assert!(Instant::now() < deadline, "the three deletes run");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!queue.quiet(), "not until the last");
        queue.push(deleting(5, "DOG"), None);
        drop(held);
        until_quiet(&queue);
        assert_eq!(*said.lock().unwrap(), ["delete ANT", "delete BEE", "delete CAT", "recover CAT", "delete DOG"]);
        assert_eq!(queue.take_results().iter().map(FileDone::seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5], "every result is there, in press order");
        assert!(queue.take_results().is_empty(), "and taken once");
        // Later: the thread has gone, and a new press still runs.
        let deadline = Instant::now() + Duration::from_secs(20);
        while queue.0.state().worker {
            assert!(Instant::now() < deadline, "the thread ends when there's no work");
            std::thread::sleep(Duration::from_millis(1));
        }
        queue.push(recovering(6, "DOG"), None);
        until_quiet(&queue);
        assert_eq!(said.lock().unwrap().last().map(String::as_str), Some("recover DOG"));
    }

    /// R15, the queue's half: the thread dies with the job, and a new one takes the rest. With nothing
    /// left it just ends, and the next press starts one.
    ///
    /// FALSIFIER (L28): `PanicGuard` doesn't respawn: the queue never goes quiet. It leaves `worker` set
    /// with nothing left: the last push never runs. It posts no result: the row of stamps.
    #[test]
    fn a_panicking_job_hands_on_to_a_new_thread() {
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let said_q = said.clone();
        let queue = FileQueue::with_runner(Box::new(move |job| {
            let (seq, dir) = job.about();
            assert_ne!(seq % 2, 0, "a job crashing, on purpose, for this row");
            said_q.lock().unwrap().push(format!("{seq}"));
            FileDone::Crashed { seq, dir }
        }));
        for seq in [1, 2, 3, 4, 5] {
            queue.push(recovering(seq, "ANT"), None);
        }
        until_quiet(&queue);
        assert_eq!(*said.lock().unwrap(), ["1", "3", "5"], "the jobs after each crash still ran");
        assert_eq!(queue.take_results().iter().map(FileDone::seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5], "and each reported");
        queue.push(recovering(6, "ANT"), None);
        until_quiet(&queue);
        assert!(matches!(queue.take_results()[..], [FileDone::Crashed { seq: 6, .. }]), "a crash with nothing left doesn't stop the next press");
        queue.push(recovering(7, "ANT"), None);
        until_quiet(&queue);
        assert_eq!(said.lock().unwrap().last().map(String::as_str), Some("7"));
    }

    /// The order the comments have held since v0.8.70: the barrier, and only then the list. A star on ANT
    /// creates a fresh `ANT.xmp` on that barrier, and enumerating first strands it beside the recycled
    /// photo. A job lists only its own photo's files, and a pair goes together.
    ///
    /// FALSIFIER (L28): list before the barrier in `run_delete`, or at the press: no `ANT.xmp`. Skip a
    /// sidecar in `files_to_send`: its row.
    #[test]
    fn delete_job_enumerates_after_the_barrier() {
        struct WriterSavesAStar(PathBuf, Mutex<Vec<PathBuf>>);
        impl FileIo for WriterSavesAStar {
            fn barrier(&self) {
                std::fs::write(self.0.join("ANT.xmp"), b"<x/>").unwrap();
            }
            fn recycle(&self, paths: &[PathBuf]) -> Recycled {
                self.1.lock().unwrap().extend_from_slice(paths);
                Recycled {
                    files: paths.to_vec(), confirmed: false,
                    #[cfg(target_os = "macos")]
                    trash_paths: Vec::new(),
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("falcon_delete_job_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["ANT.CR3", "ANT.JPG", "ANT.JPG.xmp", "BEE.JPG", "BEE.JPG.xmp"] {
            std::fs::write(dir.join(f), b"bytes").unwrap();
        }
        let ant = Shot { id: 0, name: "ANT".into(), raw: Some(dir.join("ANT.CR3")), jpg: Some(dir.join("ANT.JPG")),
            has_raw: true, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        let outcome: Arc<OnceLock<DeleteOutcome>> = Default::default();
        let io = WriterSavesAStar(dir.clone(), Mutex::new(Vec::new()));
        let job = FileJob::Delete { seq: 1, dir: dir.display().to_string(), targets: vec![ant], for_good: Default::default(), outcome: outcome.clone() };
        let FileDone::Delete { can_recover: true, left, .. } = run_job(job, &io) else { panic!("a clean delete") };
        assert_eq!(left, ["ANT"]);
        let want = ["ANT.CR3", "ANT.xmp", "ANT.JPG", "ANT.JPG.xmp"].map(|f| dir.join(f));
        assert_eq!(*io.1.lock().unwrap(), want, "the fresh side file too, and nothing of BEE's");
        assert_eq!(outcome.get().unwrap().files, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R16 and the shutdown sites: a queued delete isn't lost by closing. The process waits for the queue
    /// at its boundary, a close finishes it first, and the idle rate doesn't starve it. `start_op` waits
    /// for the queue too, and says so.
    ///
    /// FALSIFIER (L28): `quiet()` always says yes: the first row. Drop the queue's term from the exit's
    /// wait, the close handler, quit-when-idle, the idle term or `start_op`: the wiring rows. Stop waiting
    /// on `busy_flag`: its row. Let `Waited` jobs raise `sel_busy`: the row that says none does.
    #[test]
    fn shutdown_waits_for_a_queued_delete() {
        static GATES: [(u64, Mutex<()>); 1] = [(1, Mutex::new(()))];
        let said = Arc::new(Mutex::new(Vec::new()));
        let queue = writing_queue(&said, &GATES);
        assert!(!support::shutdown_waits_for_file_op(false, !queue.quiet()), "nothing to wait for");
        let held = GATES[0].1.lock().unwrap();
        queue.push(deleting(1, "ANT"), None);
        queue.push(deleting(2, "BEE"), None);
        assert!(support::shutdown_waits_for_file_op(false, !queue.quiet()), "a job is running, and one not started");
        let at_quit = queue.clone();
        // "Having waited" is judged by order, not by a clock: the wait may only return once the held job
        // has been let go. (A wall-clock bound measured from the waiter's own start failed whenever a busy
        // machine started that thread late.)
        let let_go = Arc::new(AtomicBool::new(false));
        let seen = let_go.clone();
        let waiting = std::thread::spawn(move || {
            let reached = support::await_file_op_boundary(at_quit.busy_flag(), Duration::from_secs(20));
            (reached, seen.load(Ordering::SeqCst))
        });
        std::thread::sleep(Duration::from_millis(120));
        let_go.store(true, Ordering::SeqCst);
        drop(held);
        let (reached, after_let_go) = waiting.join().unwrap();
        assert!(reached, "it reached the boundary");
        assert!(after_let_go, "having waited for the job that was held");
        assert_eq!(*said.lock().unwrap(), ["delete ANT", "delete BEE"], "the job not yet started ran too");

        let src = include_str!("main.rs").replace("\r\n", "\n");
        let main = src.split_once("\nmod tests {").map_or(src.as_str(), |(p, _)| p);
        assert!(main.contains("let wait_for_op = support::shutdown_waits_for_file_op(immediate, sel_busy.load(Ordering::SeqCst) || !file_queue.quiet());"), "the exit asks");
        assert!(main.contains("support::await_file_op_boundary(file_queue.busy_flag(), std::time::Duration::from_secs(10).saturating_sub(waiting_since.elapsed()));"), "and waits, within the same bound");
        assert!(main.contains("let ops_running = busy_cl.load(Ordering::Relaxed) || !queue_cl.quiet() || prog_cl.running.load(Ordering::Relaxed);"), "R16: a close finishes it first");
        assert!(main.contains("        if quit_when_idle_t.get()\n            && !sel_busy_qt.load(Ordering::Relaxed)\n            && file_queue_t.quiet()\n"), "and quits when it's done");
        assert!(main.contains("else if sel_busy_qt.load(Ordering::Relaxed) || !file_queue_t.quiet() { \"file-op\" }"), "the tick keeps its pace meanwhile");
        let start = main.split_once("    let start_op = {").expect("start_op").1;
        let start = start.split_once("if busy.swap(true, Ordering::SeqCst) {").expect("its latch").0;
        assert!(start.contains("if !queue_so.quiet() {") && start.contains("show_transient_toast(") && start.contains("return false;"), "the operations that can touch the same files wait, and say so");
        assert!(main.contains("position_op: position_op_t.load(Ordering::SeqCst) || !file_queue_t.quiet(),"), "a Date taken landing waits for it");
        let this = include_str!("file_queue.rs").replace("\r\n", "\n");
        let product = this.split_once("\nmod tests {").map_or(this.as_str(), |(p, _)| p);
        assert!(!product.contains("sel_busy."), "no job raises `sel_busy`");
    }

    /// Exclusion, the design's two directions: a delete and an Undo move are refused only by another
    /// operation holding `sel_busy`, in words, and a Recover never is. None of them is refused because the
    /// queue is running, which is the point of it. (`start_op`'s side is the row above.)
    ///
    /// FALSIFIER (L28): Undo move refuses on nothing, or says nothing: its row. Any of them asks `quiet()`, or
    /// goes back through `start_op`: the rows below.
    #[test]
    fn only_another_operation_refuses_a_delete_or_an_undo_move() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let main = src.split_once("\nmod tests {").map_or(src.as_str(), |(p, _)| p);
        let part = |from: &str, to: &str| main.split_once(from).expect("the handler").1.split_once(to).expect("its end").0.to_string();
        let undo = part("app.on_undo_move(move |seq| {", "app.on_empty_rejected(move || {");
        let refusal = undo.split_once("if busy.load(Ordering::SeqCst) {").expect("Undo move refuses while another operation runs").1;
        let (refusal, after) = refusal.split_once("return;").expect("and stops");
        assert!(refusal.contains("say_edit_result(") && refusal.contains("Busy with another file operation — undo cancelled"), "in words, through the router");
        assert!(after.contains("file_queue::press_undo_move("), "and the press comes after it");
        let delete = part("app.on_delete_recycle(move || {", "app.on_recover_delete(move |seq| {");
        assert!(delete.contains("if busy_dr.load(Ordering::SeqCst) {"), "a delete refuses the same");
        let recover = part("app.on_recover_delete(move |seq| {", "let shots_fr = shots.clone();");
        assert!(!recover.contains("busy"), "a Recover never has");
        for (what, handler) in [("Undo move", &undo), ("delete", &delete), ("Recover", &recover)] {
            assert!(!handler.contains("quiet()") && !handler.contains("start("), "{what} joins the queue, whatever is running");
        }
    }

    /// A row's pill goes at the press. The job's report may be a moment away, and a second click on a pill
    /// still lit would find nothing and say nothing (L21). The model is rebuilt there, because the report
    /// that used to rebuild it comes later, and from commit 9 may not come at all.
    ///
    /// FALSIFIER (L28): drop either handler's rebuild, or retire after it: its row.
    ///
    /// Commit 10: a Recover's row is retired through `tick::taken_back`, which also takes its line out of a fold
    /// (R33, R37). The fold rows in `tipgeom_tests.rs` drive it.
    #[test]
    fn a_rows_pill_goes_at_the_press() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let main = src.split_once("\nmod tests {").map_or(src.as_str(), |(p, _)| p);
        let part = |from: &str, to: &str| main.split_once(from).expect("the handler").1.split_once(to).expect("its end").0.to_string();
        for (what, handler, log, model, retires) in [
            ("Recover", part("if file_queue::press_recover(&history_rc", "let shots_fr = shots.clone();"), "log_rc", "model_rc", "tick::taken_back(&mut log_rc.borrow_mut(), seq, None, app.get_notif_open());"),
            ("Undo move", part("if file_queue::press_undo_move(&history_um", "app.on_empty_rejected(move || {"), "log_um", "model_um", "tick::retire_row_offer(&mut log_um.borrow_mut(), seq);"),
        ] {
            let retire = handler.find(retires).unwrap_or_else(|| panic!("{what}: its row stops offering"));
            let rebuild = handler.find(&format!("rebuild_notif_model(&{model}, &{log}.borrow(), Instant::now(), ")).unwrap_or_else(|| panic!("{what}: at the press"));
            assert!(retire < rebuild, "{what}: the rebuild reads the retired row");
        }
    }

    /// R17: a person waiting on a job hears that it's working after a second, once, for a Recover, a
    /// selection's Recover and an Undo move, whichever route pressed them. A delete is nobody's wait. It
    /// counts from the press, also behind other jobs, and goes on while the job runs.
    ///
    /// FALSIFIER (L28): the comparison `>=` becomes `>` or the constant changes: the second's rows. `said`
    /// is ignored: it speaks every tick. A running job's wait is dropped: the running row. A delete
    /// carries a wait: its row. The tick stops asking: the wiring row.
    #[test]
    fn a_job_a_person_waits_on_says_so_after_a_second() {
        static GATES: [(u64, Mutex<()>); 2] = [(1, Mutex::new(())), (2, Mutex::new(()))];
        let said = Arc::new(Mutex::new(Vec::new()));
        let queue = writing_queue(&said, &GATES);
        let history = RefCell::new(FileHistory::default());
        let undo = RefCell::new(UndoRedo::default());
        let t0 = Instant::now();
        let after = |ms: u64| t0 + Duration::from_millis(ms);
        let due = |at: Instant, said: Option<u64>| queue.working_due(at, said).map(|w| (w.seq, w.text));

        // Two jobs held: a delete, which nobody waits on, and its Recover behind it.
        let (hold_delete, hold_recover) = (GATES[0].1.lock().unwrap(), GATES[1].1.lock().unwrap());
        queue.push(press_delete(&history, &undo, 1, TakeOut { dir: "folder".into(), gen: 1, name: "ANT".into(), photos: 0, targets: vec![(0, photo("ANT"))], route: DeleteRoute::Bin { asks: false } }, Vec::new()), None);
        assert_eq!(due(after(60_000), None), None, "a delete is nobody's wait");
        history.borrow_mut().deletes[0].seq = 2; // Its Recover has its own gate here, so its own stamp.
        assert!(press_recover(&history, &queue, 2, 0, t0));
        assert_eq!(due(after(999), None), None, "not yet");
        assert_eq!(due(after(1_000), None), Some((2, "Recovering ANT…".to_string())), "behind the delete, from its own press");
        assert_eq!(due(after(1_000), Some(2)), None, "once");
        drop(hold_delete);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !queue.0.state().jobs.is_empty() {
            assert!(Instant::now() < deadline, "the delete runs, and the Recover starts");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(due(after(2_000), None), Some((2, "Recovering ANT…".to_string())), "while it runs too");
        drop(hold_recover);
        until_quiet(&queue);
        assert_eq!(due(after(9_000), None), None, "and not after it");
        queue.take_results();

        // A selection's Recover and an Undo move, in the words the person hears.
        history.borrow_mut().push_delete(DeleteRecord::for_test("folder", "3 photos", 3, Vec::new(), 7, Vec::new()));
        let moved = |seq| support::MoveRecord { folder: "folder".into(), pairs: Vec::new(), seq, steps: Vec::new(), subject: String::new() };
        history.borrow_mut().push_move(moved(8));
        let hold = GATES[0].1.lock().unwrap();
        queue.push(deleting(1, "HOLD"), None);
        assert!(press_recover(&history, &queue, 7, 0, t0));
        assert_eq!(due(after(1_000), None), Some((7, "Recovering 3 photos…".to_string())));
        assert!(press_undo_move(&history, &queue, 8, t0));
        assert_eq!(due(after(1_000), Some(7)), None, "the first one's turn comes first");
        drop(hold);
        until_quiet(&queue);
        history.borrow_mut().push_move(moved(9));
        let hold = GATES[0].1.lock().unwrap();
        queue.push(deleting(1, "HOLD"), None);
        assert!(press_undo_move(&history, &queue, 9, t0));
        assert_eq!(due(after(1_000), None), Some((9, "Undoing move…".to_string())));
        drop(hold);
        until_quiet(&queue);

        let src = include_str!("main.rs").replace("\r\n", "\n");
        let main = src.split_once("\nmod tests {").map_or(src.as_str(), |(p, _)| p);
        assert!(main.contains("let due = file_queue::working_due(&file_queue_t, landing_name_t.borrow().as_ref(), now, said);"), "the tick asks");
        let tick = main.split_once("if let Some(waited) = due {").expect("and speaks").1;
        assert!(tick.split_once("*working_said_t").expect("and remembers").0.contains("say_edit_result("), "through the one router, so an open panel hears it too");
        let fold = main.split_once("for done in file_queue_t.take_results() {").expect("the fold").1;
        let fold = fold.split_once("file_queue::fold_file_done(").expect("its report").0;
        assert!(fold.contains("done.ends_wait()") && fold.contains("tick::retire_working_message("), "and it goes when the job has reported");
    }

    /// R14: the check runs on a thread of its own: a delete and a Recover pressed while it reads the bin
    /// run to their end, and nothing waits behind it. It looks once at what it's asked, its answer waits to be
    /// taken, and a check that panics says nothing, so the re-stat stands and the next one still runs.
    ///
    /// FALSIFIER (L28): `start` waits for its thread: "it's reading the bin". `running` isn't cleared by a
    /// guard: the panic row. The answer is dropped: the found row.
    #[test]
    fn the_bin_check_runs_beside_the_jobs() {
        static GATE: Mutex<()> = Mutex::new(());
        let check = BinCheck::with_runner(Box::new(|asks| {
            // Waits for the row, for a while. A check run on the caller's thread gives up, and the row sees it.
            let deadline = Instant::now() + Duration::from_secs(5);
            while GATE.try_lock().is_err() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(asks.first().map(|a| a.seq), Some(99), "a check crashing, on purpose, for this row");
            asks.iter().map(|a| BinFound { seq: a.seq, missing: a.files.clone() }).collect()
        }));
        let ask = |seq: u64| BinAsk { seq, op_time: 0, files: vec![PathBuf::from(format!("{seq}.png"))] };
        let until_done = |check: &BinCheck| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while check.running() {
                assert!(Instant::now() < deadline, "the check should end");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let held = GATE.lock().unwrap();
        assert!(!check.running());
        check.start(vec![ask(1), ask(2)]);
        assert!(check.running(), "it's reading the bin");
        // A delete and its Recover run to their end while it does.
        let said = Arc::new(Mutex::new(Vec::new()));
        static NO_GATES: [(u64, Mutex<()>); 0] = [];
        let queue = writing_queue(&said, &NO_GATES);
        queue.push(deleting(1, "ANT"), None);
        queue.push(recovering(2, "ANT"), None);
        until_quiet(&queue);
        assert_eq!(*said.lock().unwrap(), ["delete ANT", "recover ANT"], "no job waits behind the bin");
        assert!(check.running() && check.take_found().is_empty(), "still reading");
        drop(held);
        until_done(&check);
        let found = check.take_found();
        assert_eq!(found.iter().map(|f| f.seq).collect::<Vec<_>>(), [1, 2], "what it found waits to be taken");
        assert!(check.take_found().is_empty(), "once");
        // A panic: it says nothing, and the next one runs.
        check.start(vec![ask(99)]);
        until_done(&check);
        assert!(check.take_found().is_empty(), "the re-stat stands");
        check.start(vec![ask(3)]);
        until_done(&check);
        assert_eq!(check.take_found().len(), 1);
    }

    /// A listing's thread that panics, or can't start, still posts a result, as nothing else would: the
    /// next listing, a Date taken landing and a Ctrl+Z waiting for its photo all wait for one.
    ///
    /// FALSIFIER (L28): `ListingGuard` posts nothing: the deadline fails the row.
    #[test]
    fn a_listing_that_panics_still_reports() {
        let mut reread = Reread::default();
        reread.want(PathBuf::from("folder"), true);
        let ticket = reread.start(1, 0).expect("wanted");
        spawn_listing_with(ticket, |_, _| panic!("a listing crashing, on purpose, for this row"));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match reread.accept(1, 0, None, || false) {
                Acceptance::Failed => break,
                Acceptance::Nothing => assert!(Instant::now() < deadline, "it should report"),
                _ => panic!("a failed listing lands nothing"),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!reread.wanted_or_running(), "nothing waits for it now");
    }

    /// The refresh on coming back: a return is not in front, then in front. Every pair of answers, where `None`
    /// is unknown (mid-switch, a locked desktop).
    ///
    /// FALSIFIER (L28): `came_back` is true on any tick in front (`now == Some(true)`): the rows that start in
    /// front. It counts unknown as away (`was != Some(true)`): the rows that start unknown.
    #[test]
    fn came_back_is_only_away_then_in_front() {
        let answers = [None, Some(false), Some(true)];
        for was in answers {
            for now in answers {
                assert_eq!(came_back(was, now), was == Some(false) && now == Some(true), "{was:?} then {now:?}");
            }
        }
    }

    /// The refresh on coming back, through the tick's own ask: the last known answer is kept through an unknown,
    /// so in front, unknown, in front is no return, and away, unknown, in front is one. The first tick after
    /// launch is no return, whatever it reads. A return asks once for the folder on screen, at once, and the
    /// next tick in front doesn't ask again. No folder on screen: nothing. Each twice (L53). A launch that starts
    /// behind another program and is then clicked is a return too: that listing is held by the folder's own open
    /// and lists again after it (`coming_back_while_a_folder_opens_lands_nothing_over_it`).
    ///
    /// FALSIFIER (L28): `ask_on_return` writes the unknown into its cell (`in_front.set(now)` always): the
    /// away, unknown, in front row. It asks without the quiet second's exemption (`want(.., false)`): `due` before
    /// the second. It skips the empty folder test: the last row.
    #[test]
    fn a_return_asks_once_for_the_folder_on_screen_at_once() {
        let reread = RefCell::new(Reread::default());
        let front = Cell::new(None);
        let ticks = |answers: &[Option<bool>], on: &str| -> Vec<bool> {
            answers.iter().map(|&now| ask_on_return(&front, now, on, &reread)).collect()
        };
        assert_eq!(ticks(&[Some(true), Some(true)], "D:/shoot"), [false, false], "the first ticks after launch, in front: no return");
        let on_screen = PathBuf::from("D:/shoot");
        let started = |reread: &RefCell<Reread>| reread.borrow_mut().start(1, 0).map(|ticket| ticket.dir);
        let first = Instant::now();
        for _ in 0..2 {
            assert_eq!(ticks(&[Some(false), Some(true), Some(true)], "D:/shoot"), [false, true, false], "one ask per return");
            assert!(reread.borrow().due(first, true, false), "at once: no quiet second");
            assert_eq!(started(&reread), Some(on_screen.clone()), "for the folder on screen");
            *reread.borrow_mut() = Reread::default();
            assert_eq!(ticks(&[Some(true), None, Some(true)], "D:/shoot"), [false, false, false], "a moment unknown in front is no return");
            assert_eq!(ticks(&[Some(false), None, Some(true)], "D:/shoot"), [false, false, true], "away, unknown, back is");
            assert_eq!(started(&reread), Some(on_screen.clone()));
            *reread.borrow_mut() = Reread::default();
        }
        // Asked during a clean delete's quiet second, the one listing goes at once.
        reread.borrow_mut().note_report(first);
        reread.borrow_mut().want(on_screen.clone(), false);
        assert!(!reread.borrow().due(first, true, false), "premise: a clean delete waits its second");
        assert_eq!(ticks(&[Some(false), Some(true)], "D:/shoot"), [false, true]);
        assert!(reread.borrow().due(first, true, false), "the return doesn't wait it");
        *reread.borrow_mut() = Reread::default();
        // Nothing scanned yet (a plain launch): nothing to list.
        assert_eq!(ticks(&[Some(false), Some(true)], ""), [false, false]);
        assert!(!reread.borrow().wanted_or_running());
    }

    /// Real bin, generated files, never run by an implementation agent: three photos at once, each back by
    /// name with its side file, in the queue's order, and nothing else in the bin touched.
    /// `cargo test -p falcon-native --bin falcon -- --ignored live_three_deletes_three_recovers --nocapture`
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore]
    fn live_three_deletes_three_recovers() {
        let dir = std::env::temp_dir().join(format!("falcon_queue_live_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin_before = trash::os_limited::list().unwrap().len();
        let queue = FileQueue::new();
        let mut sent = Vec::new();
        for (seq, photo) in [(1, "ANT"), (2, "BEE"), (3, "CAT")] {
            let jpg = dir.join(format!("{photo}.JPG"));
            std::fs::write(&jpg, photo.as_bytes()).unwrap();
            std::fs::write(sidecar_path_for(&jpg, false), b"<x/>").unwrap();
            let shot = Shot { id: 0, name: photo.into(), raw: None, jpg: Some(jpg),
                has_raw: false, has_jpg: true, kind: falcon_decode::SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
            let outcome: Arc<OnceLock<DeleteOutcome>> = Default::default();
            queue.push(FileJob::Delete { seq, dir: dir.display().to_string(), targets: vec![shot], for_good: Default::default(), outcome: outcome.clone() }, None);
            sent.push((seq + 10, photo, outcome));
        }
        for (seq, photo, outcome) in sent.iter().rev() {
            queue.push(FileJob::Recover { seq: *seq, dir: dir.display().to_string(), name: photo.to_string(), plural: false, outcome: outcome.clone(), not_in_bin: Vec::new(), nav_at_press: 0, seats: Vec::new(), waited: a_wait(*seq) }, None);
        }
        until_quiet(&queue);
        for (_, photo, _) in &sent {
            let jpg = dir.join(format!("{photo}.JPG"));
            assert_eq!(std::fs::read(&jpg).unwrap(), photo.as_bytes(), "{photo} is back, with its bytes");
            assert!(sidecar_path_for(&jpg, false).is_file(), "and its side file");
        }
        assert_eq!(trash::os_limited::list().unwrap().len(), bin_before, "nothing of ours stays in the bin, and nothing else moved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
