#!/usr/bin/env python3
"""Build a local .app bundle; no signing identity or installation required."""
import argparse
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--debug", action="store_true", help="Package a faster development build")
args = parser.parse_args()
if sys.platform != "darwin":
    raise SystemExit("This bundler requires macOS.")
root = pathlib.Path(__file__).resolve().parent.parent
command = ["cargo", "build", "--locked", "--bin", "gitbuddy", "--message-format=json-render-diagnostics"]
if not args.debug:
    command.append("--release")
result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, text=True, check=True)
artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
binary = next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["target"]["name"] == "gitbuddy")
bundle = root / "dist" / "GitBuddy.app"
macos = bundle / "Contents" / "MacOS"
macos.mkdir(parents=True, exist_ok=True)
staged = macos / "gitbuddy.new"
shutil.copy2(binary, staged)
os.replace(staged, macos / "gitbuddy")
with (bundle / "Contents" / "Info.plist").open("wb") as file:
    plistlib.dump({
        "CFBundleName": "GitBuddy",
        "CFBundleDisplayName": "GitBuddy",
        "CFBundleExecutable": "gitbuddy",
        "CFBundleIdentifier": "dev.gitbuddy.desktop",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "0.1.0",
        "CFBundleVersion": "1",
        "NSHighResolutionCapable": True,
        "NSSupportsAutomaticGraphicsSwitching": True,
    }, file)
print(bundle)
