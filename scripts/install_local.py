#!/usr/bin/env python3
"""Install a local GitBuddy archive after SHA-256 verification; keep old data.

Quit GitBuddy first. Existing installations are renamed to a recoverable backup,
never removed. Unknown destination directories are refused.
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import platform
import plistlib
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import uuid
import zipfile

MAX_ARCHIVE = 1024 * 1024 * 1024
MAX_EXTRACTED = 4 * MAX_ARCHIVE
MAX_FILES = 10000

def require(condition, message):
    if not condition:
        raise ValueError(message)

def host_target():
    system = {"darwin": "macos", "win32": "windows", "linux": "linux"}.get(sys.platform)
    arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x86_64", "amd64": "x86_64", "x64": "x86_64"}.get(platform.machine().lower())
    require(system is not None and arch is not None, "unsupported installation platform or architecture")
    return system, arch

def executable_target(path):
    """Read supported native executable metadata without executing the image.

    Kept in this standalone installer so packagers and shipped installers use
    the same checks. This verifies format/platform/architecture, not signatures
    or the presence of every library required at runtime.
    """
    with pathlib.Path(path).open("rb") as file:
        info = os.fstat(file.fileno())
        require(stat.S_ISREG(info.st_mode), "executable must be a regular file")
        size = info.st_size
        def read(offset, length):
            require(offset >= 0 and length >= 0 and offset + length <= size, "truncated native executable")
            file.seek(offset)
            data = file.read(length)
            require(len(data) == length, "native executable changed while reading")
            return data
        def thin_macho(offset, length):
            require(length >= 32, "truncated Mach-O header")
            header = read(offset, 32)
            endian = {b"\xcf\xfa\xed\xfe": "<", b"\xfe\xed\xfa\xcf": ">"}.get(header[:4])
            require(endian is not None, "universal slice is not a supported 64-bit Mach-O executable")
            cpu, subtype, kind, count, command_bytes, _, _ = struct.unpack(endian + "7I", header[4:])
            arch = {0x01000007: "x86_64", 0x0100000C: "arm64"}.get(cpu)
            require(arch is not None and kind == 2, "unsupported Mach-O executable architecture or file type")
            require(subtype & 0x00FFFFFF == (3 if arch == "x86_64" else 0), "unsupported constrained Mach-O CPU subtype")
            require(0 < count <= 4096 and command_bytes >= count * 8 and 32 + command_bytes <= length, "invalid Mach-O load commands")
            cursor = 32
            macos = False
            for _ in range(count):
                command, command_size = struct.unpack(endian + "II", read(offset + cursor, 8))
                require(command_size >= 8 and command_size % 8 == 0 and cursor + command_size <= 32 + command_bytes, "invalid Mach-O load command size")
                if command == 0x32:  # LC_BUILD_VERSION; PLATFORM_MACOS == 1.
                    require(command_size >= 24, "truncated Mach-O platform command")
                    target_platform, _, _, tools = struct.unpack(endian + "4I", read(offset + cursor + 8, 16))
                    require(target_platform == 1 and 24 + tools * 8 <= command_size, "Mach-O image targets a non-macOS platform")
                    macos = True
                elif command in (0x24, 0x25, 0x2F, 0x30):
                    require(command == 0x24 and command_size == 16, "Mach-O image targets a non-macOS platform")
                    macos = True
                cursor += command_size
            require(cursor == 32 + command_bytes and macos, "Mach-O executable has no valid macOS platform metadata")
            return arch, subtype
        magic = read(0, 4)
        if magic in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
            return "macos", (thin_macho(0, size)[0],)
        fat = {
            b"\xca\xfe\xba\xbe": (">", False), b"\xbe\xba\xfe\xca": ("<", False),
            b"\xca\xfe\xba\xbf": (">", True), b"\xbf\xba\xfe\xca": ("<", True),
        }.get(magic)
        if fat:
            endian, wide = fat
            count = struct.unpack(endian + "I", read(4, 4))[0]
            record_size = 32 if wide else 20
            table_end = 8 + count * record_size
            require(0 < count <= 16 and table_end <= size, "invalid universal Mach-O architecture table")
            arches, ranges = set(), []
            for index in range(count):
                record = struct.unpack(endian + ("IIQQII" if wide else "5I"), read(8 + index * record_size, record_size))
                cpu, subtype, offset, length, alignment = record[:5]
                expected = {0x01000007: "x86_64", 0x0100000C: "arm64"}.get(cpu)
                require(expected is not None and expected not in arches, "unsupported or duplicate universal architecture")
                require(alignment <= 31 and offset >= table_end and length >= 32 and offset + length <= size and offset % (1 << alignment) == 0, "invalid universal Mach-O slice bounds")
                require(not wide or record[5] == 0, "invalid universal Mach-O reserved field")
                require(all(offset + length <= start or offset >= end for start, end in ranges), "overlapping universal Mach-O slices")
                require(thin_macho(offset, length) == (expected, subtype), "universal architecture does not match its actual slice")
                arches.add(expected)
                ranges.append((offset, offset + length))
            return "macos", tuple(sorted(arches))
        if magic == b"\x7fELF":
            header = read(0, 64)
            require(header[4:7] == b"\x02\x01\x01" and header[7] in (0, 3), "unsupported ELF class, byte order or operating-system ABI")
            kind, machine, version, entry, program_offset = struct.unpack_from("<HHIQQ", header, 16)
            header_size, program_size, count = struct.unpack_from("<HHH", header, 52)
            arch = {62: "x86_64", 183: "arm64"}.get(machine)
            require(arch is not None and kind in (2, 3) and version == 1 and entry != 0, "unsupported ELF executable architecture or type")
            require(header_size == 64 and program_size == 56 and 0 < count <= 4096 and program_offset >= 64 and program_offset + count * program_size <= size, "invalid ELF program headers")
            executable_entry = False
            for index in range(count):
                kind, flags, offset, address, _, file_size, memory_size, _ = struct.unpack("<II6Q", read(program_offset + index * program_size, program_size))
                require(offset + file_size <= size, "ELF segment exceeds executable bounds")
                if kind == 1:
                    require(memory_size >= file_size, "invalid ELF load segment")
                    executable_entry |= bool(flags & 1 and file_size and address <= entry < address + memory_size)
            require(executable_entry, "ELF image has no executable entry point")
            return "linux", (arch,)
        if magic[:2] == b"MZ":
            pe_offset = struct.unpack("<I", read(0x3C, 4))[0]
            require(pe_offset >= 64, "invalid PE header offset")
            header = read(pe_offset, 24)
            require(header[:4] == b"PE\0\0", "invalid PE signature")
            machine, sections = struct.unpack_from("<HH", header, 4)
            optional_size, flags = struct.unpack_from("<HH", header, 20)
            arch = {0x8664: "x86_64", 0xAA64: "arm64"}.get(machine)
            require(arch is not None and flags & 2 and not flags & 0x2000, "unsupported PE executable architecture or DLL")
            require(optional_size >= 112 and 0 < sections <= 96, "invalid PE optional header or sections")
            optional = read(pe_offset + 24, optional_size)
            require(struct.unpack_from("<H", optional)[0] == 0x20B and struct.unpack_from("<H", optional, 68)[0] in (2, 3), "PE image is not a 64-bit Windows application")
            directory_count = struct.unpack_from("<I", optional, 108)[0]
            require(directory_count <= 16 and 112 + directory_count * 8 <= optional_size, "invalid PE data directories")
            entry = struct.unpack_from("<I", optional, 16)[0]
            section_offset = pe_offset + 24 + optional_size
            require(section_offset + sections * 40 <= size, "truncated PE section table")
            executable_entry = False
            for index in range(sections):
                section = read(section_offset + index * 40, 40)
                virtual_size, address, raw_size, raw_offset = struct.unpack_from("<4I", section, 8)
                characteristics = struct.unpack_from("<I", section, 36)[0]
                require(raw_offset + raw_size <= size, "PE section exceeds executable bounds")
                executable_entry |= bool(characteristics & 0x20000000 and raw_size and address <= entry < address + max(virtual_size, raw_size))
            require(entry != 0 and executable_entry, "PE image has no executable entry point")
            return "windows", (arch,)
        raise ValueError("unsupported native executable format")

def validate_installation(source, destination):
    is_app = source.suffix == ".app"
    if is_app:
        binary = source / "Contents/MacOS/gitbuddy"
        info_path = source / "Contents/Info.plist"
        require(info_path.is_file() and not info_path.is_symlink() and info_path.stat().st_size < 65536, "missing or oversized application bundle metadata")
        info = plistlib.loads(info_path.read_bytes())
        require(isinstance(info, dict) and info.get("CFBundleIdentifier") == "dev.gitbuddy.desktop" and info.get("CFBundleExecutable") == "gitbuddy", "invalid GitBuddy application bundle metadata")
    else:
        candidates = [path for path in (source / "gitbuddy", source / "gitbuddy.exe") if path.is_file()]
        require(len(candidates) == 1, "package must contain one GitBuddy executable")
        binary = candidates[0]
    system, arches = executable_target(binary)
    expected_system, expected_arch = host_target()
    require(system == expected_system, f"package platform {system} does not match this host ({expected_system})")
    require(expected_arch in arches, f"package architecture {','.join(arches)} does not match this host ({expected_arch})")
    require(is_app == (system == "macos"), "macOS installation requires a GitBuddy.app bundle")
    require((destination.suffix == ".app") == is_app, "a macOS .app destination must receive an application bundle")
    if not is_app:
        require(binary.name == ("gitbuddy.exe" if system == "windows" else "gitbuddy"), "executable name does not match its native platform")
    if system != "windows" and os.name != "nt":
        require(binary.stat().st_mode & stat.S_IXUSR, "native executable is missing its executable permission")

def digest(file):
    result = hashlib.sha256()
    file.seek(0)
    for data in iter(lambda: file.read(65536), b""):
        result.update(data)
    return result.hexdigest()

def snapshot_archive(path):
    # Hash and extraction must consume this same private copy. Holding the
    # original fd alone would still permit in-place writes after verification.
    snapshot = tempfile.TemporaryFile()
    try:
        flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0)
        with os.fdopen(os.open(path, flags), "rb") as source:
            info = os.fstat(source.fileno())
            require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_ARCHIVE, "archive is missing or exceeds 1 GiB")
            count = 0
            for block in iter(lambda: source.read(65536), b""):
                count += len(block)
                require(count <= MAX_ARCHIVE, "archive exceeds 1 GiB while reading")
                snapshot.write(block)
        snapshot.flush()
        snapshot.seek(0)
        return snapshot
    except BaseException:
        snapshot.close()
        raise

def sync_directory(path):
    if os.name != "nt":
        fd = os.open(path, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)

def known_app(path):
    marker = path / "gitbuddy-install.json"
    if marker.is_file() and not marker.is_symlink() and marker.stat().st_size < 65536:
        try:
            if json.loads(marker.read_text(encoding="utf-8")).get("application") == "GitBuddy":
                return True
        except (ValueError, OSError):
            pass
    info = path / "Contents" / "Info.plist"
    if info.is_file() and not info.is_symlink() and info.stat().st_size < 65536:
        try:
            return plistlib.loads(info.read_bytes()).get("CFBundleIdentifier") == "dev.gitbuddy.desktop"
        except (ValueError, OSError):
            pass
    return False

def extract(archive, staging):
    count = 0
    total = 0
    seen = set()

    def path_for(name):
        nonlocal count
        count += 1
        require(count <= MAX_FILES, "archive has too many entries")
        raw = pathlib.PurePosixPath(name)
        require(not raw.is_absolute() and ".." not in raw.parts and "\\" not in name and ":" not in name, "unsafe archive path")
        if raw.parts and raw.parts[0] == "__MACOSX":
            return None
        require(raw.parts and raw.parts != (".",), "empty archive path")
        target = staging.joinpath(*raw.parts)
        require(target.resolve().is_relative_to(staging.resolve()), "unsafe archive path")
        if os.name == "nt":
            require(not target.is_reserved(), "reserved archive path")
        key = os.path.normcase(str(target))
        require(key not in seen, "duplicate archive path")
        seen.add(key)
        return target

    def copy(source, target, expected, mode):
        nonlocal total
        require(expected >= 0 and total + expected <= MAX_EXTRACTED, "archive expands beyond 4 GiB")
        target.parent.mkdir(parents=True, exist_ok=True)
        require(not target.exists(), "duplicate archive path")
        written = 0
        with target.open("xb") as file:
            for block in iter(lambda: source.read(65536), b""):
                written += len(block)
                require(written <= expected and total + written <= MAX_EXTRACTED, "archive content size differs")
                file.write(block)
            file.flush()
            os.fsync(file.fileno())
        require(written == expected, "archive content is truncated")
        total += written
        if os.name != "nt":
            target.chmod(mode & 0o777 or 0o644)

    if zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as package:
            require(len(package.infolist()) <= MAX_FILES, "archive has too many entries")
            for entry in package.infolist():
                target = path_for(entry.filename)
                if target is None:
                    continue
                mode = entry.external_attr >> 16
                require(not stat.S_ISLNK(mode), "archive links are not supported")
                require(stat.S_IFMT(mode) in (0, stat.S_IFREG, stat.S_IFDIR), "archive special files are not supported")
                if entry.is_dir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    with package.open(entry) as source:
                        copy(source, target, entry.file_size, mode)
    else:
        archive.seek(0)
        with tarfile.open(fileobj=archive, mode="r:*") as package:
            for entry in package:
                target = path_for(entry.name)
                if target is None:
                    continue
                require(entry.isdir() or entry.isfile(), "archive links or special files are not supported")
                if entry.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    source = package.extractfile(entry)
                    require(source is not None, "missing archive content")
                    with source:
                        copy(source, target, entry.size, entry.mode)
    roots = list(staging.iterdir())
    require(len(roots) == 1 and roots[0].is_dir(), "archive must contain one GitBuddy installation directory")
    source = roots[0]
    if source.suffix == ".app":
        require(known_app(source) and (source / "Contents/MacOS/gitbuddy").is_file(), "archive is not a GitBuddy application")
    else:
        require((source / "gitbuddy").is_file() or (source / "gitbuddy.exe").is_file(), "archive has no GitBuddy executable")
    return source

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", type=pathlib.Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--destination", type=pathlib.Path, required=True)
    parser.add_argument("--require-signature", action="store_true", help="Require macOS code-signature and Gatekeeper validation")
    args = parser.parse_args()
    archive = args.archive.expanduser()
    require(len(args.sha256) == 64 and all(c in "0123456789abcdefABCDEF" for c in args.sha256), "invalid SHA-256 value")
    with snapshot_archive(archive) as snapshot:
        require(digest(snapshot) == args.sha256.lower(), "archive SHA-256 mismatch")
        snapshot.seek(0)
        install(args, snapshot)

def install(args, archive):
    requested = args.destination.expanduser()
    require(requested.is_absolute() and requested.name not in ("", ".", ".."), "choose an absolute destination")
    requested.parent.mkdir(parents=True, exist_ok=True)
    destination = requested.parent.resolve() / requested.name
    require(not destination.is_symlink(), "destination must not be a symlink")
    existing = destination.stat() if destination.exists() else None
    if existing:
        require(destination.is_dir() and known_app(destination), "destination is not a recognized GitBuddy installation")
    backup = None
    with tempfile.TemporaryDirectory(prefix=".gitbuddy-install-", dir=destination.parent) as directory:
        source = extract(archive, pathlib.Path(directory))
        validate_installation(source, destination)
        if args.require_signature:
            require(sys.platform == "darwin" and source.suffix == ".app", "signature verification requires a macOS app bundle")
            subprocess.run(["codesign", "--verify", "--deep", "--strict", str(source)], check=True)
            subprocess.run(["spctl", "--assess", "--type", "execute", str(source)], check=True)
        if source.suffix != ".app":
            marker = source / "gitbuddy-install.json"
            require(not marker.exists(), "archive unexpectedly includes an installation marker")
            with marker.open("w", encoding="utf-8") as file:
                json.dump({"application": "GitBuddy", "archive_sha256": args.sha256.lower()}, file)
                file.flush()
                os.fsync(file.fileno())
        sync_directory(source)
        if existing:
            actual = destination.stat()
            require(not destination.is_symlink() and (actual.st_dev, actual.st_ino) == (existing.st_dev, existing.st_ino), "destination changed during preparation")
            stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
            backup = destination.with_name(destination.name + ".backup-" + stamp + "-" + uuid.uuid4().hex[:8])
            os.rename(destination, backup)
            sync_directory(destination.parent)
        else:
            require(not destination.exists(), "destination appeared during preparation")
        try:
            os.rename(source, destination)
            sync_directory(destination.parent)
        except OSError:
            if backup is not None and not destination.exists():
                os.rename(backup, destination)
                sync_directory(destination.parent)
            raise
    print(destination)
    if backup:
        print("Previous installation retained at", backup)

if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, tarfile.TarError, zipfile.BadZipFile, subprocess.CalledProcessError) as error:
        print("Cannot install:", error, file=sys.stderr)
        raise SystemExit(1)
