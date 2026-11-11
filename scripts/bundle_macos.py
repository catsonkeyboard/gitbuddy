#!/usr/bin/env python3
"""Build a local .app bundle; no signing identity or installation required."""
import argparse
import json
import os
import pathlib
import plistlib
import shutil
import stat
import subprocess
import sys
import tomllib
import tempfile
from install_local import digest, executable_target, require


def publish_bundle(staged, destination):
    """Replace only this build output, retaining the old build until success."""
    require(not destination.is_symlink(), "build bundle destination must not be a symlink")
    require(not destination.exists() or destination.is_dir(), "build bundle destination must be a directory")
    backup_directory = None
    backup = None
    if destination.exists():
        # Keep the previous build outside the staging TemporaryDirectory: even
        # failed rollback must not let automatic cleanup discard it.
        backup_directory = pathlib.Path(tempfile.mkdtemp(prefix=".gitbuddy-previous-", dir=destination.parent))
        backup = backup_directory / "previous.app"
        try:
            os.replace(destination, backup)
        except BaseException:
            backup_directory.rmdir()
            raise
    try:
        os.replace(staged, destination)
    except BaseException as error:
        if backup is not None:
            try:
                require(not destination.exists() and not destination.is_symlink(), "another bundle appeared during publication")
                os.replace(backup, destination)
                backup_directory.rmdir()
            except BaseException as rollback_error:
                raise RuntimeError(f"Bundle publication failed; previous build retained at {backup}: {rollback_error}") from error
        raise
    if backup_directory is not None:
        shutil.rmtree(backup_directory)


parser = argparse.ArgumentParser()
parser.add_argument("--debug", action="store_true", help="Package a faster development build")
parser.add_argument("--sign-identity", help="Developer ID Application identity from the keychain")
parser.add_argument("--notary-profile", help="notarytool keychain profile; requires --sign-identity")
parser.add_argument("--archive", action="store_true", help="Create a versioned zip and SHA-256 checksum")
args = parser.parse_args()
if args.notary_profile and not args.sign_identity:
    parser.error("--notary-profile requires --sign-identity")
if sys.platform != "darwin":
    raise SystemExit("This bundler requires macOS.")
root = pathlib.Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
command = ["cargo", "build", "--locked", "--bin", "gitbuddy", "--message-format=json-render-diagnostics"]
if not args.debug:
    command.append("--release")
result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, text=True, check=True)
artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
binary = next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["target"]["name"] == "gitbuddy")
dist = root / "dist"
require(not dist.is_symlink(), "build output directory must not be a symlink")
dist.mkdir(exist_ok=True)
bundle = dist / "GitBuddy.app"
with tempfile.TemporaryDirectory(prefix=".gitbuddy-bundle-", dir=dist) as directory:
    staging = pathlib.Path(directory)
    checked = staging / "artifact"
    shutil.copy2(binary, checked)
    system, arches = executable_target(checked)
    require(system == "macos", "macOS bundles require a native macOS executable")
    require(checked.stat().st_mode & stat.S_IXUSR, "macOS artifact is missing its executable permission")
    # A fresh tree cannot inherit signatures, extended attributes or obsolete
    # resources from a previously signed dist/GitBuddy.app.
    staged_bundle = staging / "GitBuddy.app"
    macos = staged_bundle / "Contents/MacOS"
    macos.mkdir(parents=True)
    os.replace(checked, macos / "gitbuddy")
    resources = staged_bundle / "Contents/Resources"
    resources.mkdir()
    shutil.copy2(root / "scripts/install_local.py", resources / "install_local.py")
    shutil.copy2(root / "docs/INSTALL.md", resources / "INSTALL.md")
    with (staged_bundle / "Contents/Info.plist").open("wb") as file:
        plistlib.dump({
            "CFBundleName": "GitBuddy",
            "CFBundleDisplayName": "GitBuddy",
            "CFBundleExecutable": "gitbuddy",
            "CFBundleIdentifier": "dev.gitbuddy.desktop",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": "1",
            "NSHighResolutionCapable": True,
            "NSSupportsAutomaticGraphicsSwitching": True,
        }, file)
    if args.sign_identity:
        subprocess.run(["codesign", "--force", "--options", "runtime", "--timestamp", "--sign", args.sign_identity, str(staged_bundle)], check=True)
        subprocess.run(["codesign", "--verify", "--strict", "--verbose=2", str(staged_bundle)], check=True)
    package_name = f"GitBuddy-{version}-macos-{'-'.join(arches)}.zip"
    staged_archive = staging / package_name
    if args.archive or args.notary_profile:
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(staged_bundle), str(staged_archive)], check=True)
    if args.notary_profile:
        subprocess.run(["xcrun", "notarytool", "submit", str(staged_archive), "--keychain-profile", args.notary_profile, "--wait"], check=True)
        subprocess.run(["xcrun", "stapler", "staple", str(staged_bundle)], check=True)
        subprocess.run(["xcrun", "stapler", "validate", str(staged_bundle)], check=True)
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(staged_bundle), str(staged_archive)], check=True)
    if args.archive or args.notary_profile:
        staged_checksum = staged_archive.with_suffix(".zip.sha256")
        with staged_archive.open("rb") as file:
            staged_checksum.write_text(digest(file) + "  " + package_name + "\n")
    publish_bundle(staged_bundle, bundle)
    if args.archive or args.notary_profile:
        archive = dist / package_name
        os.replace(staged_archive, archive)
        os.replace(staged_checksum, archive.with_suffix(".zip.sha256"))
        print(archive)
print(bundle)
