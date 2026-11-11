#!/usr/bin/env python3
"""Run on the target machine; emit machine-readable validation evidence.

GUI startup is never recorded as GUI acceptance. This script uses an isolated
GitBuddy config directory and never changes Git remotes or pushes anything.
"""
import argparse
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--gui", action="store_true", help="Launch the debug build with isolated settings; acceptance stays pending")
parser.add_argument("--package", action="store_true", help="Also run native packaging")
parser.add_argument("--lfs-program", type=pathlib.Path, help="Official git-lfs executable for migration/extension tests")
parser.add_argument("--report", type=pathlib.Path, help="Report path; defaults to target/native-validation-<OS>.json")
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parent.parent
report_path = args.report or root / "target" / f"native-validation-{platform.system().lower()}.json"
report_path.parent.mkdir(parents=True, exist_ok=True)
report = {"platform": platform.platform(), "machine": platform.machine(), "checks": [], "gui": "not_run", "accepted": False}
environment = os.environ.copy()
lfs = args.lfs_program or environment.get("GITBUDDY_TEST_GIT_LFS") or environment.get("GITBUDDY_TEST_LFS_PROGRAM") or shutil.which("git-lfs")
if lfs is None:
    downloaded = root / "target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs"
    if downloaded.is_file():
        lfs = downloaded
if lfs is None or not pathlib.Path(lfs).is_file():
    parser.error("Install official git-lfs or supply --lfs-program; migration tests require it")
lfs = str(pathlib.Path(lfs).resolve())
environment["GITBUDDY_TEST_GIT_LFS"] = lfs
environment["GITBUDDY_TEST_LFS_PROGRAM"] = lfs
report["lfs_program"] = lfs

def save():
    temp = report_path.with_suffix(".json.new")
    temp.write_text(json.dumps(report, indent=2), encoding="utf-8")
    os.replace(temp, report_path)

for command in [
    [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-p", "test_*.py"],
    ["cargo", "fmt", "--all", "--check"],
    ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
    ["cargo", "test", "--locked", "--no-fail-fast"],
    ["cargo", "build", "--locked", "--bin", "gitbuddy"],
]:
    started = time.monotonic()
    try:
        result = subprocess.run(command, cwd=root, env=environment, check=False)
        code = result.returncode
    except OSError as error:
        code = 127
        report["error"] = str(error)
    report["checks"].append({"command": command, "exit_code": code, "seconds": time.monotonic() - started})
    save()
    if code:
        print(f"Validation failed; report: {report_path}", file=sys.stderr)
        raise SystemExit(code)

if args.package:
    script = "bundle_macos.py" if sys.platform == "darwin" else "package_release.py"
    command = [sys.executable, str(root / "scripts" / script), "--debug"]
    if sys.platform == "darwin":
        command.append("--archive")
    result = subprocess.run(command, cwd=root, check=False)
    report["checks"].append({"command": command, "exit_code": result.returncode})
    save()
    if result.returncode:
        raise SystemExit(result.returncode)

if args.gui:
    config = root / "target" / f"native-qa-config-{platform.system().lower()}"
    config.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["GITBUDDY_CONFIG_DIR"] = str(config)
    binary = root / "target" / "debug" / ("gitbuddy.exe" if os.name == "nt" else "gitbuddy")
    process = subprocess.Popen([str(binary)], cwd=root, env=env)
    report["gui"] = "launched_manual_acceptance_pending"
    report["gui_pid"] = process.pid
else:
    report["gui"] = "manual_acceptance_pending"
report["checks_passed"] = True
save()
print(report_path)
