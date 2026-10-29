"""Send the actual app a system log-out quit request, and an ordinary one, in an isolated profile.

1.0.16 (Mac log-out fix). Each case launches its own copy of the packaged Falcon.app with its own
HOME (so its own `~/Library/Application Support/Falcon`), synthetic photos whose folder already
carries review data, and diagnostic logging on. The normal app writes `falcon.log` in the folder
that contains Falcon.app, so the per-case copy also gives each case a fresh log of its own (no older
or rotated log to confuse; review R1). Once the folder is open, a small Swift sender delivers one
`quit` Apple event to that process: with `kAEQuitReason = 'rlgo'` (log out) for the system case,
without a reason for the ordinary case (what Dock → Quit sends). The runner never logs out.

System case: the save runs inside `applicationShouldTerminate:`, Falcon answers Now, AppKit ends the
process (exit 0) with no panic in the native teardown, and the review data on disk was written during
that save (read back here, from a separate process). Ordinary case: the old route — Cancel, the loop
ends, the same save runs after it and the process exits cleanly.

1.1.0 (issue #11 batch B1): the final saves write only what changed since the window last loaded or
saved it. So before the quit each case moves to the second photo (`FALCON_DEBUG_STEP=1`, the arrow-tap
path, at about 6 s), which changes the folder's remembered photo (`resume_shot`). Nothing writes that
before the final save but the ~15 s backstop, which is counted in ticks from boot and cannot come
first; so the review data must be written during the save, holding the new photo. The settings may
have nothing new: they pass if written during the save or if the log says they were unchanged.

If the check cannot run as designed (the folder never opens, the sender fails or is refused
permission), it fails as SETUP FAILED, which does not claim a Falcon regression (review R3).
"""
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import time
import zlib

SENDER = r'''
// Sends one quit Apple event to a process, optionally with a kAEQuitReason attribute.
// Usage: send-quit <pid> <four-character reason | none>
import Foundation

func fourCC(_ s: String) -> FourCharCode {
    precondition(s.utf8.count == 4, "four characters")
    return s.utf8.reduce(0) { ($0 << 8) | FourCharCode($1) }
}

let args = CommandLine.arguments
guard args.count == 3, let pid = pid_t(args[1]) else {
    print("usage: send-quit <pid> <reason|none>")
    exit(64)
}
let event = NSAppleEventDescriptor(
    eventClass: fourCC("aevt"), eventID: fourCC("quit"),
    targetDescriptor: NSAppleEventDescriptor(processIdentifier: pid),
    returnID: -1, transactionID: 0)  // kAutoGenerateReturnID, kAnyTransactionID
if args[2] != "none" {
    event.setAttribute(NSAppleEventDescriptor(enumCode: fourCC(args[2])), forKeyword: fourCC("why?"))
}
do {
    _ = try event.sendEvent(options: [.noReply], timeout: 10)
    print("sent quit to \(pid) reason=\(args[2])")
} catch {
    print("send failed: \(error)")
    exit(2)
}
'''

SEEDED = {"test-02.png": {"rating": 4}, "test-05.png": {"flag": True}, "test-07.png": {"reject": True}}


def png(width, height, rows):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"".join(b"\0" + r for r in rows))) + chunk(b"IEND", b""))


def stamp(log, needle):
    """The millisecond stamp of the first log line containing `needle`, or None."""
    m = re.search(r"\[(\d+)\] [^\n]*" + re.escape(needle), log)
    return int(m.group(1)) if m else None


def case_log(case):
    """This case's log: the one `falcon*.log` beside its Falcon.app copy, as (name, text)."""
    logs = sorted(case.glob("falcon*.log"))
    if len(logs) != 1:
        return (", ".join(p.name for p in logs) or None), ""
    return logs[0].name, logs[0].read_text(errors="replace")


def run_case(app, executable_name, sender, out, name, reason):
    case = out / name
    home, photos = case / "home", case / "photos"
    config = home / "Library/Application Support/Falcon"
    config.mkdir(parents=True)
    photos.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(
        diagnostic_logging=True, onboarding_shown=True, assoc_prompt_shown=True, language="en")))
    for index in range(8):
        rows = [bytes([(index * 37 + y // 5) % 256, (80 + y // 3) % 256, (170 + index * 12) % 256]) * 640
                for y in range(480)]
        (photos / f"test-{index + 1:02}.png").write_bytes(png(640, 480, rows))
    review = photos / "falcon_review_data.json"
    review.write_text(json.dumps({"version": 1, "photos": SEEDED}))
    # This case's own app copy: its log lands in `case`, the folder that contains Falcon.app.
    bundle = case / app.name
    subprocess.run(["ditto", str(app), str(bundle)], check=True)
    executable = bundle / "Contents/MacOS" / executable_name

    result = dict(case=name, reason=reason, setup_error=None, folder_opened=False, stepped=False, sender=None,
                  exit_code=None, timed_out=False, killed=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("FALCON_", "SLINT_"))}
    env["HOME"] = str(home)
    env["FALCON_DEBUG_STEP"] = "1"  # 1.1.0: one arrow-tap step, so the final save has a change to write
    proc = None
    # Review R3: everything from the launch on is inside this try, so whatever fails, the Falcon this
    # case started is stopped and reaped before anything deletes its photos.
    try:
        with (case / "stdout.txt").open("w") as stdout, (case / "stderr.txt").open("w") as stderr:
            proc = subprocess.Popen([str(executable), str(photos)], env=env, stdout=stdout, stderr=stderr)
            deadline = time.monotonic() + 40
            while time.monotonic() < deadline and proc.poll() is None:
                if "scan: 8 shots" in case_log(case)[1]:
                    result["folder_opened"] = True
                    break
                time.sleep(0.5)
            if not result["folder_opened"]:
                result["setup_error"] = "the folder never opened (no `scan: 8 shots` line in this case's log)"
            else:
                while time.monotonic() < deadline and proc.poll() is None:
                    if "dbg: FALCON_DEBUG_STEP 1/1" in case_log(case)[1]:
                        result["stepped"] = True
                        break
                    time.sleep(0.25)
                if not result.get("stepped"):
                    result["setup_error"] = "the photo step never ran (no `FALCON_DEBUG_STEP 1/1` line)"
            if result["setup_error"] is None:
                time.sleep(1)  # the next settled tick records the new photo; the backstop comes seconds later
                try:
                    sent = subprocess.run([str(sender), str(proc.pid), reason],
                                          capture_output=True, text=True, timeout=30)
                    result["sender"] = dict(exit=sent.returncode, stdout=sent.stdout.strip(),
                                            stderr=sent.stderr.strip())
                    if sent.returncode != 0:
                        result["setup_error"] = "the Apple-event sender failed (for example, permission refused)"
                except (subprocess.TimeoutExpired, OSError) as e:
                    result["sender"] = dict(exit=None, error=f"{type(e).__name__}: {e}",
                                            stdout=str(getattr(e, "stdout", "") or "").strip(),
                                            stderr=str(getattr(e, "stderr", "") or "").strip())
                    result["setup_error"] = "the Apple-event sender did not finish"
                if result["setup_error"] is None:
                    try:
                        result["exit_code"] = proc.wait(timeout=60)
                    except subprocess.TimeoutExpired:
                        result["timed_out"] = True
    except Exception as e:  # reported below; the finally still stops this case's Falcon
        result["setup_error"] = result["setup_error"] or f"{type(e).__name__}: {e}"
    finally:
        if proc is not None:
            if proc.poll() is None:
                proc.kill()
                result["killed"] = True
            code = proc.wait()
            if result["exit_code"] is None and not result["killed"]:
                result["exit_code"] = code

    log_name, log = case_log(case)
    begin, complete = stamp(log, "shutdown: save sequence begin"), stamp(log, "shutdown: save sequence complete")

    def written_in_save(path):
        if not path.is_file():
            return False
        mtime = int(path.stat().st_mtime * 1000)
        return begin is not None and complete is not None and begin - 5 <= mtime <= complete + 50

    settings = json.loads((config / "settings.json").read_text())
    saved_review = json.loads(review.read_text()) if review.is_file() else {}
    result.update(
        log_file=log_name,
        terminate_lines=[l.split("] ", 1)[-1] for l in log.splitlines() if "terminate:" in l],
        shutdown_lines=[l.split("] ", 1)[-1] for l in log.splitlines() if "shutdown:" in l or "clean shutdown" in l],
        review_lines=[l.split("] ", 1)[-1] for l in log.splitlines() if "review data:" in l],
        panic="PANIC" in log,
        settings_written_in_save=written_in_save(config / "settings.json"),
        review_written_in_save=written_in_save(review),
        last_dir_is_photos=Path(settings.get("last_dir", "")).resolve() == photos.resolve(),
        seeded_kept=all(saved_review.get("photos", {}).get(k) == v for k, v in SEEDED.items()),
        resume_saved=saved_review.get("resume_shot"),
    )
    return result, log


def main(executable, output):
    executable = Path(executable).resolve()
    app = executable.parents[2]  # …/Falcon.app/Contents/MacOS/<executable>
    if app.suffix != ".app" or executable.parent != app / "Contents/MacOS":
        raise RuntimeError(f"expected the executable inside a .app bundle: {executable}")
    out = Path(output).resolve()
    if out.exists():
        raise RuntimeError("use a fresh output directory; a stale result must not pass")
    out.mkdir(parents=True)
    (out / "send-quit.swift").write_text(SENDER)
    sender = out / "send-quit"
    subprocess.run(["xcrun", "swiftc", "-O", str(out / "send-quit.swift"), "-o", str(sender)], check=True)

    results = []
    try:
        for name, reason in [("system-log-out", "rlgo"), ("ordinary-quit", "none")]:
            result, log = run_case(app, executable.name, sender, out, name, reason)
            (out / f"{name}.log").write_text(log)
            results.append(result)
    finally:
        (out / "report.json").write_text(json.dumps(results, indent=2) + "\n")
        for case in out.iterdir():
            if case.is_dir():  # synthetic photos and the app copy only; logs and settings stay
                shutil.rmtree(case / "photos", ignore_errors=True)
                shutil.rmtree(case / app.name, ignore_errors=True)
    print(json.dumps(results, indent=2))

    setup = [r for r in results if r["setup_error"]]
    if setup:  # the check could not run as designed; this does not show a Falcon regression
        raise RuntimeError("SETUP FAILED (not a Falcon result): "
                           + "; ".join(f"{r['case']}: {r['setup_error']}" for r in setup))
    system, ordinary = results
    for r in results:
        assert r["log_file"] == "falcon.log", f"{r['case']}: expected one fresh falcon.log: {r}"
        assert not r["timed_out"] and r["exit_code"] == 0 and not r["panic"], f"{r['case']}: exit: {r}"
        # 1.1.0 (B1): the settings are written only if they changed; the review data has the step's change.
        settings_ok = r["settings_written_in_save"] or any(
            "settings unchanged since the last save" in l for l in r["shutdown_lines"])
        assert settings_ok and r["review_written_in_save"], f"{r['case']}: not saved: {r}"
        assert r["last_dir_is_photos"] and r["seeded_kept"], f"{r['case']}: saved state: {r}"
        assert r["resume_saved"] == "test-02", f"{r['case']}: the stepped-to photo was not saved: {r}"
    assert any(l.startswith("terminate: system reason='rlgo' (log out)") for l in system["terminate_lines"]), system
    assert "terminate: reply=now" in " ".join(system["terminate_lines"]), system
    assert any("save sequence begin route=system" in l for l in system["shutdown_lines"]), system
    assert not any("route=after-loop" in l or l == "clean shutdown" for l in system["shutdown_lines"]), system
    assert any("ordinary quit" in l for l in ordinary["terminate_lines"]), ordinary
    assert any("save sequence begin route=after-loop" in l for l in ordinary["shutdown_lines"]), ordinary
    assert "clean shutdown" in ordinary["shutdown_lines"], ordinary
    print("System log-out: saved, then NSTerminateNow, clean exit. Ordinary quit: unchanged route.")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
