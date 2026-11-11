"""Small structured native-image fixtures. They are parsed, never executed."""
import hashlib
import io
import pathlib
import plistlib
import struct
import tarfile
import zipfile


def macho(arch="arm64", payload=b"fixture", platform_id=1):
    cpu = {"arm64": 0x0100000C, "x86_64": 0x01000007}[arch]
    command = struct.pack("<6I", 0x32, 24, platform_id, 0x000C0000, 0x000C0000, 0)
    return struct.pack("<8I", 0xFEEDFACF, cpu, 3 if arch == "x86_64" else 0, 2, 1, len(command), 0, 0) + command + payload


def universal(payload=b"fixture", fat64=False):
    images = [(0x01000007, macho("x86_64", payload)), (0x0100000C, macho("arm64", payload))]
    table_size = 8 + len(images) * (32 if fat64 else 20)
    offset = (table_size + 7) & ~7
    table = bytearray(struct.pack(">II", 0xCAFEBABF if fat64 else 0xCAFEBABE, len(images)))
    slices = []
    for cpu, image in images:
        subtype = 3 if cpu == 0x01000007 else 0
        if fat64:
            table.extend(struct.pack(">IIQQII", cpu, subtype, offset, len(image), 3, 0))
        else:
            table.extend(struct.pack(">5I", cpu, subtype, offset, len(image), 3))
        slices.append((offset, image))
        offset = (offset + len(image) + 7) & ~7
    output = bytearray(offset)
    output[:len(table)] = table
    for offset, image in slices:
        output[offset:offset + len(image)] = image
    return bytes(output)


def elf(arch="x86_64", payload=b"fixture", osabi=0):
    machine = {"x86_64": 62, "arm64": 183}[arch]
    ident = b"\x7fELF\x02\x01\x01" + bytes([osabi]) + bytes(8)
    size = 64 + 56 + len(payload)
    header = struct.pack("<16sHHIQQQIHHHHHH", ident, 3, machine, 1, 0x400078, 64, 0, 0, 64, 56, 1, 0, 0, 0)
    segment = struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0x400000, size, size, 0x1000)
    return header + segment + payload


def pe(arch="x86_64", payload=b"fixture"):
    machine = {"x86_64": 0x8664, "arm64": 0xAA64}[arch]
    dos = bytearray(128)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, len(dos))
    coff = b"PE\0\0" + struct.pack("<HHIIIHH", machine, 1, 0, 0, 0, 240, 0x22)
    optional = bytearray(240)
    struct.pack_into("<H", optional, 0, 0x20B)
    struct.pack_into("<I", optional, 16, 0x1000)
    struct.pack_into("<Q", optional, 24, 0x140000000)
    struct.pack_into("<II", optional, 32, 0x1000, 0x200)
    struct.pack_into("<II", optional, 56, 0x2000, 0x200)
    struct.pack_into("<H", optional, 68, 2)
    struct.pack_into("<I", optional, 108, 16)
    section = struct.pack("<8sIIIIIIHHI", b".text", 512, 0x1000, 512, 512, 0, 0, 0, 0, 0x60000020)
    return (bytes(dos) + coff + optional + section).ljust(512, b"\0") + payload.ljust(512, b"\0")


def binary(system, arch, payload=b"fixture"):
    return {"macos": macho, "linux": elf, "windows": pe}[system](arch, payload)


def package(root, system, arch, payload=b"fixture", kind="zip", plain_macos=False, traversal=False):
    image = binary(system, arch, payload)
    app = system == "macos" and not plain_macos
    folder = "GitBuddy.app" if app else "GitBuddy"
    executable = "Contents/MacOS/gitbuddy" if app else ("gitbuddy.exe" if system == "windows" else "gitbuddy")
    entries = [(folder + "/" + executable, image, 0o755)]
    if app:
        entries.append((folder + "/Contents/Info.plist", plistlib.dumps({
            "CFBundleIdentifier": "dev.gitbuddy.desktop", "CFBundleExecutable": "gitbuddy", "CFBundlePackageType": "APPL",
        }), 0o644))
    entries.append((folder + "/README.md", b"GitBuddy fixture", 0o644))
    if traversal:
        entries.append(("../escaped", b"outside", 0o644))
    archive = pathlib.Path(root) / (hashlib.sha256(image).hexdigest() + (".zip" if kind == "zip" else ".tar.gz"))
    if kind == "zip":
        with zipfile.ZipFile(archive, "w") as output:
            for name, data, mode in entries:
                info = zipfile.ZipInfo(name)
                info.create_system = 3
                info.external_attr = (0o100000 | mode) << 16
                output.writestr(info, data)
    else:
        with tarfile.open(archive, "w:gz") as output:
            for name, data, mode in entries:
                info = tarfile.TarInfo(name)
                info.mode = mode
                info.size = len(data)
                output.addfile(info, io.BytesIO(data))
    return archive, hashlib.sha256(archive.read_bytes()).hexdigest(), image
