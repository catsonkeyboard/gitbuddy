import contextlib
import importlib.util
import io
import json
import os
import pathlib
import struct
import sys
import tarfile
import tempfile
import types
import unittest
from unittest import mock
import zipfile

from native_binary_fixtures import elf, macho, pe, universal

SCRIPTS = pathlib.Path(__file__).resolve().parents[1] / "scripts"


def installer_module():
    spec = importlib.util.spec_from_file_location("native_installer_under_test", SCRIPTS / "install_local.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NativeFormatTests(unittest.TestCase):
    def classify(self, image):
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "gitbuddy"
            binary.write_bytes(image)
            return installer_module().executable_target(binary)

    def test_identifies_supported_64_bit_formats_and_universal_slices(self):
        for image, expected in [
            (macho("arm64"), ("macos", ("arm64",))),
            (macho("x86_64"), ("macos", ("x86_64",))),
            (universal(), ("macos", ("arm64", "x86_64"))),
            (universal(fat64=True), ("macos", ("arm64", "x86_64"))),
            (elf("arm64"), ("linux", ("arm64",))),
            (elf("x86_64"), ("linux", ("x86_64",))),
            (pe("arm64"), ("windows", ("arm64",))),
            (pe("x86_64"), ("windows", ("x86_64",))),
        ]:
            with self.subTest(expected=expected):
                self.assertEqual(self.classify(image), expected)

    def test_rejects_truncation_wrong_abi_library_and_forged_universal_architecture(self):
        lying_fat = bytearray(universal())
        struct.pack_into(">I", lying_fat, 8, 0x0100000C)  # First slice is actually x86_64.
        overflowing_fat = bytearray(universal())
        struct.pack_into(">I", overflowing_fat, 16, len(overflowing_fat) + 1)
        lying_subtype = bytearray(universal())
        struct.pack_into(">I", lying_subtype, 12, 0)
        constrained_x86 = bytearray(macho("x86_64"))
        struct.pack_into("<I", constrained_x86, 8, 8)  # x86_64h is not generic x86_64.
        dll = bytearray(pe())
        struct.pack_into("<H", dll, 128 + 22, 0x2022)
        for image in [b"#!/bin/sh\n", macho()[:20], elf()[:65], pe()[:128], bytes(lying_fat), bytes(overflowing_fat), bytes(lying_subtype), bytes(constrained_x86), bytes(dll), macho(platform_id=2), elf(osabi=9)]:
            with self.subTest(magic=image[:8]), self.assertRaises(ValueError):
                self.classify(image)


class PackageTargetTests(unittest.TestCase):
    def run_packager(self, root, image, script="package_release.py", host_arch="x86_64"):
        (root / "scripts").mkdir(exist_ok=True)
        (root / "docs").mkdir(exist_ok=True)
        (root / "Cargo.toml").write_text('[package]\nname="gitbuddy"\nversion="1.2.3"\n')
        (root / "scripts/install_local.py").write_bytes((SCRIPTS / "install_local.py").read_bytes())
        (root / "docs/INSTALL.md").write_text("installer instructions\n")
        binary = root / "target/cross-target/debug/gitbuddy"
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_bytes(image)
        binary.chmod(0o755)
        artifact = {"reason": "compiler-artifact", "executable": str(binary), "target": {"name": "gitbuddy"}}
        def run(command, **kwargs):
            if command[0] == "cargo":
                return types.SimpleNamespace(stdout=json.dumps(artifact) + "\n")
            if command[0] == "ditto":
                bundle, archive = pathlib.Path(command[-2]), pathlib.Path(command[-1])
                with zipfile.ZipFile(archive, "w") as output:
                    for file in bundle.rglob("*"):
                        if file.is_file():
                            output.write(file, bundle.name + "/" + file.relative_to(bundle).as_posix())
                return types.SimpleNamespace(returncode=0)
            self.fail("Unexpected external command: " + repr(command))
        arguments = [script, "--debug"] + (["--archive"] if script == "bundle_macos.py" else [])
        with mock.patch.object(sys, "argv", arguments), mock.patch.object(sys, "platform", "darwin"), mock.patch("platform.system", return_value="Linux"), mock.patch("platform.machine", return_value=host_arch), mock.patch("os.uname", return_value=types.SimpleNamespace(machine=host_arch)), mock.patch("subprocess.run", side_effect=run), mock.patch.dict(sys.modules, {"install_local": installer_module()}):
            with contextlib.redirect_stdout(io.StringIO()):
                exec(compile((SCRIPTS / script).read_text(), str(SCRIPTS / script), "exec"), {"__name__": "__main__", "__file__": str(root / "scripts" / script)})

    def test_release_platform_architecture_and_archive_format_follow_the_actual_artifact(self):
        for image, label, kind in [(elf("arm64"), "linux-arm64", "tar.gz"), (pe("arm64"), "windows-arm64", "zip")]:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                self.run_packager(root, image)
                archive = root / "dist" / ("GitBuddy-1.2.3-" + label + "." + kind)
                self.assertTrue(archive.is_file(), "package was labeled for the builder instead of the artifact")
                if kind == "zip":
                    with zipfile.ZipFile(archive) as package:
                        self.assertEqual(package.read("GitBuddy-1.2.3-" + label + "/gitbuddy.exe"), image)
                        self.assertIn("GitBuddy-1.2.3-" + label + "/install_local.py", package.namelist())
                else:
                    with tarfile.open(archive) as package:
                        self.assertEqual(package.extractfile("GitBuddy-1.2.3-" + label + "/gitbuddy").read(), image)
                        self.assertEqual(package.getmember("GitBuddy-1.2.3-" + label + "/gitbuddy").mode & 0o777, 0o755)

    @unittest.skipIf(os.name == "nt", "macOS bundle staging requires POSIX executable permissions")
    def test_macos_bundle_architecture_comes_from_its_executable_not_uname(self):
        for image, label in [(macho("x86_64"), "x86_64"), (universal(), "arm64-x86_64")]:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                self.run_packager(root, image, "bundle_macos.py", host_arch="arm64")
                archive = root / "dist" / ("GitBuddy-1.2.3-macos-" + label + ".zip")
                self.assertTrue(archive.is_file(), "bundle was mislabeled using the builder architecture")
                with zipfile.ZipFile(archive) as package:
                    self.assertEqual(package.read("GitBuddy.app/Contents/MacOS/gitbuddy"), image)
                    self.assertIn("GitBuddy.app/Contents/Resources/install_local.py", package.namelist())

    def test_macos_bundler_rejects_non_macos_artifact_before_touching_old_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            executable = root / "dist/GitBuddy.app/Contents/MacOS/gitbuddy"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"preserve old bundle")
            signature = root / "dist/GitBuddy.app/Contents/_CodeSignature/CodeResources"
            signature.parent.mkdir()
            signature.write_bytes(b"preserve old signature on refusal")
            with self.assertRaises(ValueError):
                self.run_packager(root, elf(), "bundle_macos.py")
            self.assertEqual(executable.read_bytes(), b"preserve old bundle")
            self.assertEqual(signature.read_bytes(), b"preserve old signature on refusal")

    @unittest.skipIf(os.name == "nt", "macOS bundle staging requires POSIX executable permissions")
    def test_unsigned_rebuild_never_inherits_the_previous_bundle_signature(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            bundle = root / "dist/GitBuddy.app"
            signature = bundle / "Contents/_CodeSignature/CodeResources"
            signature.parent.mkdir(parents=True)
            signature.write_bytes(b"signature for the old build")
            old = bundle / "Contents/MacOS/gitbuddy"
            old.parent.mkdir()
            old.write_bytes(macho("arm64", b"old build"))
            unrelated = root / "dist/Other.app/Contents/_CodeSignature/CodeResources"
            unrelated.parent.mkdir(parents=True)
            unrelated.write_bytes(b"do not touch another bundle")
            image = macho("arm64", b"new unsigned build")
            self.run_packager(root, image, "bundle_macos.py")
            self.assertFalse(signature.exists(), "unsigned rebuild retained an invalid signature for previous bytes")
            self.assertEqual(old.read_bytes(), image)
            self.assertEqual(unrelated.read_bytes(), b"do not touch another bundle")
            with zipfile.ZipFile(root / "dist/GitBuddy-1.2.3-macos-arm64.zip") as archive:
                self.assertFalse(any("_CodeSignature" in path for path in archive.namelist()))
                self.assertEqual(archive.read("GitBuddy.app/Contents/MacOS/gitbuddy"), image)

    @unittest.skipIf(os.name == "nt", "macOS bundle staging requires POSIX executable permissions")
    def test_failed_fresh_bundle_publication_restores_the_previous_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            bundle = root / "dist/GitBuddy.app"
            old = bundle / "Contents/MacOS/gitbuddy"
            old.parent.mkdir(parents=True)
            old.write_bytes(macho("arm64", b"previous signed build"))
            signature = bundle / "Contents/_CodeSignature/CodeResources"
            signature.parent.mkdir()
            signature.write_bytes(b"previous signature")
            archive = root / "dist/GitBuddy-1.2.3-macos-arm64.zip"
            archive.write_bytes(b"previous package")
            actual_replace = os.replace
            def fail_new_bundle(source, destination):
                if pathlib.Path(destination).resolve() == bundle.resolve() and pathlib.Path(source).name == "GitBuddy.app":
                    raise OSError("injected bundle publication failure")
                return actual_replace(source, destination)
            with mock.patch("os.replace", side_effect=fail_new_bundle), self.assertRaises(OSError):
                self.run_packager(root, macho("arm64", b"next build"), "bundle_macos.py")
            self.assertEqual(old.read_bytes(), macho("arm64", b"previous signed build"))
            self.assertEqual(signature.read_bytes(), b"previous signature")
            self.assertEqual(archive.read_bytes(), b"previous package")


if __name__ == "__main__":
    unittest.main()
