"""Run the packaged public host in a real native event loop before publishing a tester pack."""
import json
import os
import plistlib
from pathlib import Path
import shutil
import subprocess
import sys


def validate_report(report, expected_build="1.0.8-mac-full04"):
    if not (report.get("ready") is True and report.get("welcome") is False
            and report.get("association") is False and report.get("photo_width", 0) > 0
            and report.get("full_toolbar") is True and report.get("toolbar_roundtrip") is True
            # A toolbar taller than the title bar is drawn but cut off; "drew" is not "visible".
            and report.get("toolbar_clipped") is False
            and report.get("toolbar_mouse_down_can_move_window") is False
            and isinstance(report.get("titlebar_height"), (int, float)) and report["titlebar_height"] > 0
            and report.get("build") == expected_build):
        raise RuntimeError(f"native host smoke failed: {report}")


def main(executable, output):
    executable = Path(executable).resolve()
    info = plistlib.loads((executable.parent.parent / "Info.plist").read_bytes())
    expected_build = info.get("FalconBuildLabel", info.get("FalconSourceVersion"))
    if not expected_build:
        raise RuntimeError("packaged app has no expected build identity")
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    report_path = output / "report.json"
    if report_path.exists():
        raise RuntimeError("use a fresh output directory; a stale report must not pass")
    env = dict(os.environ, FALCON_MAC_PROBE_SMOKE_OUT=str(report_path))
    try:
        with (output / "stdout.txt").open("w") as stdout, (output / "stderr.txt").open("w") as stderr:
            proc = subprocess.run([str(Path(executable).resolve())], env=env, stdout=stdout,
                                  stderr=stderr, timeout=45)
        if proc.returncode:
            raise RuntimeError(f"app exit status {proc.returncode}")
        validate_report(json.loads(report_path.read_text()), expected_build)
    finally:
        # This run uses only the generated diagnostic grid/profile, never user photo data.
        mode = info.get("FalconExperimentMode")
        profile = Path.home() / (f"Library/Application Support/Falcon Mac Full 04/{mode}/ci-smoke"
                                 if mode else "Library/Application Support/Falcon/ci-smoke")
        for log in profile.glob("falcon*.log"):
            shutil.copyfile(log, output / log.name)
    print("Packaged native host ready; photo present; no onboarding/association dialog. Visual Gate A still required.")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
