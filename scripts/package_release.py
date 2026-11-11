#!/usr/bin/env python3
"""Build the native executable and package it for manual installation.

Run on the destination platform. macOS signing uses bundle_macos.py instead.
"""
import argparse
import json
import os
import pathlib
import shutil
import stat
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile
from install_local import digest, executable_target, require

parser = argparse.ArgumentParser()
parser.add_argument("--debug", action="store_true")
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
cmd = ["cargo", "build", "--locked", "--bin", "gitbuddy", "--message-format=json-render-diagnostics"]
if not args.debug:
    cmd.append("--release")
result = subprocess.run(cmd, cwd=root, text=True, stdout=subprocess.PIPE, check=True)
artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
binary = pathlib.Path(next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["target"]["name"] == "gitbuddy"))
with tempfile.TemporaryDirectory() as temp:
    folder = pathlib.Path(temp) / "installation"
    folder.mkdir()
    staged_binary = folder / "artifact"
    shutil.copy2(binary, staged_binary)
    system, arches = executable_target(staged_binary)
    require(system != "macos", "Use bundle_macos.py to package a macOS application bundle")
    require(system == "windows" or os.name == "nt" or staged_binary.stat().st_mode & stat.S_IXUSR, "native artifact is missing its executable permission")
    staged_binary.rename(folder / ("gitbuddy.exe" if system == "windows" else "gitbuddy"))
    name = f"GitBuddy-{version}-{system}-{'-'.join(arches)}"
    named_folder = pathlib.Path(temp) / name
    folder.rename(named_folder)
    folder = named_folder
    dest = root / "dist"
    dest.mkdir(exist_ok=True)
    for file in ["README.md", "LICENSE"]:
        if (root / file).is_file():
            shutil.copy2(root / file, folder / file)
    shutil.copy2(root / "scripts/install_local.py", folder / "install_local.py")
    shutil.copy2(root / "docs/INSTALL.md", folder / "INSTALL.md")
    if system == "windows":
        archive = dest / (name + ".zip")
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            for file in folder.iterdir():
                z.write(file, name + "/" + file.name)
    else:
        archive = dest / (name + ".tar.gz")
        def unix_entry(info):
            if info.name == name + "/gitbuddy":
                info.mode = 0o755
            return info
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(folder, arcname=name, filter=unix_entry)
with archive.open("rb") as file:
    archive.with_suffix(archive.suffix + ".sha256").write_text(digest(file) + "  " + archive.name + "\n")
print(archive)
