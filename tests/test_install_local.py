import contextlib
import importlib.util
import io
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from native_binary_fixtures import binary, package

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "scripts" / "install_local.py"
HOST_OS = {"darwin": "macos", "win32": "windows", "linux": "linux"}[sys.platform]
HOST_ARCH = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"


def module():
    spec = importlib.util.spec_from_file_location("installer_under_test", SCRIPT)
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    return installer


def executable(target, system=HOST_OS):
    return target / ("Contents/MacOS/gitbuddy" if system == "macos" else ("gitbuddy.exe" if system == "windows" else "gitbuddy"))


class InstallationTests(unittest.TestCase):
    def package(self, root, data=b"version-one", traversal=False, **kwargs):
        archive, digest, _ = package(root, kwargs.pop("system", HOST_OS), kwargs.pop("arch", HOST_ARCH), data, traversal=traversal, **kwargs)
        return archive, digest

    def target(self, root):
        return root / ("GitBuddy.app" if HOST_OS == "macos" else "Installed GitBuddy")

    def install(self, archive, digest, destination):
        return subprocess.run([sys.executable, str(SCRIPT), "--archive", str(archive), "--sha256", digest, "--destination", str(destination)], text=True, capture_output=True)

    def test_update_preserves_the_previous_installation_as_a_backup(self):
        for kind in ("zip", "tar"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                archive, digest = self.package(root, kind=kind)
                target = self.target(root)
                result = self.install(archive, digest, target)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(executable(target).read_bytes(), binary(HOST_OS, HOST_ARCH, b"version-one"))
                archive, digest = self.package(root, b"version-two", kind=kind)
                result = self.install(archive, digest, target)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(executable(target).read_bytes(), binary(HOST_OS, HOST_ARCH, b"version-two"))
                backups = list(root.glob(target.name + ".backup-*"))
                self.assertEqual(len(backups), 1)
                self.assertEqual(executable(backups[0]).read_bytes(), binary(HOST_OS, HOST_ARCH, b"version-one"))

    def test_invalid_package_or_destination_leaves_existing_data_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            target = root / "Existing directory"
            target.mkdir()
            (target / "sentinel").write_text("keep")
            archive, digest = self.package(root)
            result = self.install(archive, "0" * 64, target)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("SHA-256", result.stderr)
            self.assertEqual((target / "sentinel").read_text(), "keep")
            result = self.install(archive, digest, target)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not a recognized GitBuddy installation", result.stderr)
            self.assertEqual((target / "sentinel").read_text(), "keep")
            archive, digest = self.package(root, b"bad", traversal=True)
            result = self.install(archive, digest, root / "new")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("archive path", result.stderr)
            self.assertFalse((root / "escaped").exists())
            self.assertFalse((root / "new").exists())

    def test_replacing_archive_after_its_digest_cannot_install_unverified_bytes(self):
        for kind in ("zip", "tar"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                approved, expected = self.package(root, b"approved bytes", kind=kind)
                replacement, _ = self.package(root, b"unverified replacement", kind=kind)
                target = self.target(root)
                installer = module()
                real_digest = installer.digest
                def replace_after_hash(source):
                    result = real_digest(source)
                    replacement.replace(approved)
                    return result
                args = [str(SCRIPT), "--archive", str(approved), "--sha256", expected, "--destination", str(target)]
                with mock.patch.object(sys, "argv", args), mock.patch.object(installer, "digest", replace_after_hash), contextlib.redirect_stdout(io.StringIO()):
                    installer.main()
                self.assertEqual(executable(target).read_bytes(), binary(HOST_OS, HOST_ARCH, b"approved bytes"))

    def test_wrong_platform_or_architecture_cannot_displace_the_existing_installation(self):
        for case in ("platform", "architecture", "plain-macos"):
            if case == "plain-macos" and HOST_OS != "macos":
                continue
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                target = self.target(root)
                approved, digest = self.package(root)
                self.assertEqual(self.install(approved, digest, target).returncode, 0)
                before = executable(target).read_bytes()
                options = {"system": "linux" if HOST_OS != "linux" else "windows"} if case == "platform" else ({"arch": "x86_64" if HOST_ARCH == "arm64" else "arm64"} if case == "architecture" else {"plain_macos": True})
                wrong, digest = self.package(root, b"wrong package", **options)
                result = self.install(wrong, digest, target)
                self.assertNotEqual(result.returncode, 0, "wrong platform/architecture was installed")
                self.assertEqual(executable(target).read_bytes(), before)
                self.assertEqual(list(root.glob(target.name + ".backup-*")), [])

    def test_real_native_format_packages_install_for_each_supported_host(self):
        installer = module()
        for system, arch, kind in [("macos", "arm64", "zip"), ("macos", "x86_64", "tar"), ("linux", "x86_64", "tar"), ("linux", "arm64", "zip"), ("windows", "x86_64", "zip"), ("windows", "arm64", "tar")]:
            with self.subTest(system=system, arch=arch, kind=kind), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                archive, digest, image = package(root, system, arch, kind=kind)
                target = root / ("GitBuddy.app" if system == "macos" else "GitBuddy")
                args = [str(SCRIPT), "--archive", str(archive), "--sha256", digest, "--destination", str(target)]
                with mock.patch.object(sys, "argv", args), mock.patch.object(installer, "host_target", return_value=(system, arch)), contextlib.redirect_stdout(io.StringIO()):
                    installer.main()
                self.assertEqual(executable(target, system).read_bytes(), image)
                if system != "macos":
                    self.assertEqual(json.loads((target / "gitbuddy-install.json").read_text())["archive_sha256"], digest)

    def test_copied_installer_runs_without_any_repository_helpers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            standalone = root / "install_local.py"
            shutil.copy2(SCRIPT, standalone)
            archive, digest = self.package(root)
            target = self.target(root)
            result = subprocess.run([sys.executable, str(standalone), "--archive", str(archive), "--sha256", digest, "--destination", str(target)], cwd=root, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(executable(target).read_bytes(), binary(HOST_OS, HOST_ARCH, b"version-one"))

    def test_archive_snapshot_reserves_binary_mode_for_windows_crt(self):
        installer = module()
        with tempfile.TemporaryDirectory() as directory:
            archive = pathlib.Path(directory) / "bytes.zip"
            content = b"PK\0\r\n\x1a\xffbinary bytes"
            archive.write_bytes(content)
            actual_open = os.open
            binary_flag = getattr(os, "O_BINARY", 1 << 29)
            opened = []
            def open_file(path, flags, *args, **kwargs):
                if os.fspath(path) == os.fspath(archive):
                    opened.append(flags)
                return actual_open(path, flags if os.name == "nt" else flags & ~binary_flag, *args, **kwargs)
            with mock.patch.object(os, "O_BINARY", binary_flag, create=True), mock.patch.object(os, "open", side_effect=open_file):
                with installer.snapshot_archive(archive) as snapshot:
                    self.assertEqual(snapshot.read(), content)
            self.assertTrue(opened[0] & binary_flag, "archive descriptor was opened with CRT text conversion enabled")


if __name__ == "__main__":
    unittest.main()
