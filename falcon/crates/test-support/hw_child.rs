//! Test-only, included with `#[path]` like `fixture_paths.rs` (no package, no dependency): **THE
//! TIME LIMIT FOR TESTS THAT CALL THE GRAPHICS CARD OR THE HARDWARE DECODER.**
//!
//! Round 2b follow-up (owner, 27 October: "add the time limit"), widened on 28 October to every such
//! test (round 2b review P2, accepted). A row that reaches a driver runs in a child copy of its own
//! test binary, and the child is ended if it has not finished within [`ROW_LIMIT`]. On 27 October a
//! graphics-driver call spun for over an hour, and the tests that started after it stalled with it,
//! so the run never finished: the stuck call held up more than its own row, which is why a limit
//! inside the same process would not be enough. In its own process a hung call holds nothing the
//! other tests use, and ending the process ends the hang: the row fails, naming the limit, and the
//! run goes on. Pure rows (parsers, struct layouts, shader text, CPU mirrors) are not wrapped: they
//! never enter a driver.
//!
//! A row's first line is `if !hw_child::run_here(module_path!(), "<the row's fn name>") { return; }`.
//! A wrong name fails the row loudly (the child runs no row by that name), never passes it.
//!
//! Known limits, accepted in round 2b: there is no job object, so a child outlives a parent killed
//! from outside; and a child stuck in a kernel-mode driver call cannot be reaped (`ended: false`; the
//! row still fails, and says so).
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// Every wrapped row normally finishes in seconds, child start included; the slowest file,
/// `falcon-hwdec/tests/hw_photo.rs`, takes about 90 s for all its rows together.
pub const ROW_LIMIT: Duration = Duration::from_secs(120);
/// Set in the child, so a row runs its body there instead of starting another child.
pub const CHILD: &str = "FALCON_LIVE_GPU_CHILD";

#[derive(Debug, PartialEq)]
pub enum RowEnd {
    Passed,
    /// It failed, or it exited without running this row (a misspelt name runs no row).
    Failed,
    /// Not finished at the limit; `ended` says whether its process could then be ended.
    TimedOut { ended: bool },
}

/// True in the child copy: run the row's body here.
pub fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// libtest's name for `row` in the module whose `module_path!()` is `module`: the crate's own segment
/// dropped, so an integration test's top-level row is its bare name ("rotation" → "row") and a unit
/// test's keeps its modules ("falcon_gpu::heic::tests" → "heic::tests::row").
pub fn test_path(module: &str, row: &str) -> String {
    module.split_once("::").map_or_else(|| row.to_string(), |(_, m)| format!("{m}::{row}"))
}

/// Runs the row libtest calls `path` alone in a child copy of this test binary, and ends the child at
/// `limit`. Returns how it ended, its stdout (without libtest's tally lines, so a run's totals never
/// count the child twice), its stderr, and how long the parent waited. The output goes to files
/// rather than pipes, so nothing the child leaves behind can keep the parent reading.
pub fn run_row_in_child(path: &str, limit: Duration, env: &[(&str, &str)]) -> (RowEnd, String, String, Duration) {
    let base = std::env::temp_dir().join(format!("falcon_child_row_{}_{}", std::process::id(), path.replace("::", "-")));
    let (out_file, err_file) = (base.with_extension("out"), base.with_extension("err"));
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("the test binary"));
    cmd.args([path, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&out_file).expect("the child's stdout file"))
        .stderr(std::fs::File::create(&err_file).expect("the child's stderr file"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().expect("start the child");
    // `Ok` with the child's exit status, or `Err(ended)` at the limit.
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            break Ok(status);
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            // A child stuck in a kernel-mode driver call cannot exit until that call returns, so
            // the parent never waits on it without a limit: reaped if it goes within the grace
            // period, otherwise left (the row still fails, and says so).
            let grace = Instant::now();
            let ended = loop {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break true;
                }
                if grace.elapsed() >= Duration::from_secs(10) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            break Err(ended);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let waited = start.elapsed();
    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
    let (out, err) = (read(&out_file), read(&err_file));
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&err_file);
    let ran = out.contains(&format!("test {path} ...")) && out.contains("test result: ok. 1 passed");
    let end = match status {
        Err(ended) => RowEnd::TimedOut { ended },
        Ok(s) if s.success() && ran => RowEnd::Passed,
        Ok(_) => RowEnd::Failed,
    };
    let out = out.lines().filter(|l| !l.starts_with("test result:")).map(|l| format!("{l}\n")).collect();
    (end, out, err, waited)
}

/// The parent's half of a wrapped row: runs `row` (in the module `module`, the caller's
/// `module_path!()`) in a child under [`ROW_LIMIT`], fails the row if the child failed or hung, and
/// otherwise passes the child's stderr on (its skip lines) and returns it.
pub fn run_in_child(module: &str, row: &str) -> String {
    let (end, out, err, waited) = run_row_in_child(&test_path(module, row), ROW_LIMIT, &[]);
    match end {
        RowEnd::Passed => eprint!("{err}"),
        RowEnd::Failed => panic!("{row} failed in its own process (or ran no row by that name):\n{out}{err}"),
        RowEnd::TimedOut { ended } => panic!(
            "{row} did not finish within {ROW_LIMIT:?} (waited {waited:?}), and {}: most likely a \
             graphics-driver hang, as on 27 October; the other tests were not held up.\n{out}{err}",
            if ended { "its process was ended" } else { "its process could not be ended (stuck in the driver), so it was left" }
        ),
    }
    err
}

/// A wrapped row's first line. In the child it says "run the body here"; in the parent it runs the
/// row in a child ([`run_in_child`]) and says "done".
pub fn run_here(module: &str, row: &str) -> bool {
    if in_child() {
        return true;
    }
    run_in_child(module, row);
    false
}
