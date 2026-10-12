#!/usr/bin/env python3
"""Package reviewed Linux binaries and runtime files with deterministic metadata."""

import argparse
import gzip
import hashlib
import io
import os
from pathlib import Path
import stat
import struct
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BINARIES = ("redwax", "redwax-resolve", "redwax-play", "redwax-marks")
TARGETS = {
    "x86_64-unknown-linux-musl": (62, True),
    "aarch64-unknown-linux-musl": (183, True),
}
RUNTIME_FILES = (
    "install.sh",
    "scripts/install_resolver.sh",
    "scripts/systemd/redwax-doctor.service",
    "scripts/systemd/redwax-doctor.timer",
    "scripts/systemd/redwax-native.service",
    "scripts/systemd/redwax-native.timer",
    "README.md",
    "docs/usage.md",
    "docs/themes.md",
    "docs/native-canary.md",
)


def read_regular(path, boundary):
    if not path.resolve().is_relative_to(boundary):
        raise ValueError(f"release input escapes its source directory: {path.name}")
    for component in (path, *path.parents):
        if component == boundary:
            break
        if component.is_symlink():
            raise ValueError(f"release input must not contain a symlink: {path.name}")
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"release input must be a regular file: {path.name}")
    return path.read_bytes()


def verify_executable(path, contents, target):
    if not path.stat().st_mode & 0o111:
        raise ValueError(f"release binary is not executable: {path.name}")
    machine, requires_static = TARGETS[target]
    if len(contents) < 64 or contents[:7] != b"\x7fELF\x02\x01\x01" or contents[7] not in (0, 3):
        raise ValueError(f"release binary is not a Linux ELF64 executable: {path.name}")
    executable_type, executable_machine = struct.unpack_from("<HH", contents, 16)
    if executable_type not in (2, 3) or executable_machine != machine:
        raise ValueError(f"release binary does not match {target}: {path.name}")
    header_offset = struct.unpack_from("<Q", contents, 32)[0]
    entry_size, entry_count = struct.unpack_from("<HH", contents, 54)
    if not entry_count or entry_size < 56 or header_offset < 64 or header_offset + entry_size * entry_count > len(contents):
        raise ValueError(f"release binary has invalid ELF program headers: {path.name}")
    has_load_segment = False
    for index in range(entry_count):
        header = header_offset + index * entry_size
        segment_type = struct.unpack_from("<I", contents, header)[0]
        segment_offset, segment_size = struct.unpack_from("<Q", contents, header + 8)[0], struct.unpack_from("<Q", contents, header + 32)[0]
        if segment_offset + segment_size > len(contents):
            raise ValueError(f"release binary has a truncated ELF segment: {path.name}")
        has_load_segment |= segment_type == 1
        if requires_static and segment_type == 3:
            raise ValueError(f"musl release binary has a dynamic interpreter: {path.name}")
        if requires_static and segment_type == 2:
            if segment_size % 16:
                raise ValueError(f"release binary has an invalid ELF dynamic table: {path.name}")
            for offset in range(segment_offset, segment_offset + segment_size, 16):
                tag = struct.unpack_from("<q", contents, offset)[0]
                if tag == 0:
                    break
                if tag == 1:
                    raise ValueError(f"musl release binary requires shared libraries: {path.name}")
    if not has_load_segment:
        raise ValueError(f"release binary has no ELF load segment: {path.name}")


def release_inputs(root, binary_directory, target):
    inputs = {}
    checksums = []
    for name in BINARIES:
        path = binary_directory / name
        contents = read_regular(path, binary_directory)
        verify_executable(path, contents, target)
        inputs[f"bin/{name}"] = (contents, 0o755)
        checksums.append(f"{hashlib.sha256(contents).hexdigest()}  bin/{name}\n")
    for name in RUNTIME_FILES:
        contents = read_regular(root / name, root)
        inputs[name] = (contents, 0o755 if name.endswith(".sh") else 0o644)
    inputs["SHA256SUMS"] = ("".join(checksums).encode(), 0o644)
    return inputs


def write_archive(destination, inputs):
    directories = {"redwax"}
    for name in inputs:
        directories.update(str(parent) for parent in (Path("redwax") / name).parents if str(parent) != ".")
    with gzip.GzipFile(filename="", mode="wb", fileobj=destination, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            entries = {name: (b"", 0o755, True) for name in directories}
            entries.update({f"redwax/{name}": (*value, False) for name, value in inputs.items()})
            for name, (contents, mode, is_directory) in sorted(entries.items()):
                entry = tarfile.TarInfo(name)
                entry.mode = mode
                entry.type = tarfile.DIRTYPE if is_directory else tarfile.REGTYPE
                entry.size = 0 if is_directory else len(contents)
                archive.addfile(entry, None if is_directory else io.BytesIO(contents))


def package_release(root, binary_directory, output_directory, target):
    if target not in TARGETS:
        raise ValueError("unsupported release target")
    root = Path(root).resolve(strict=True)
    binary_directory = Path(binary_directory).resolve(strict=True)
    output_directory = Path(output_directory)
    if not root.is_dir() or not binary_directory.is_dir():
        raise ValueError("release source and binary inputs must be directories")
    if output_directory.is_symlink() or (output_directory.exists() and not output_directory.is_dir()):
        raise ValueError("release output must be a directory")
    archive_path = output_directory / f"redwax-{target}.tar.gz"
    checksum_path = archive_path.with_name(archive_path.name + ".sha256")
    if any(os.path.lexists(path) for path in (archive_path, checksum_path)):
        raise ValueError("release output already exists, choose an empty output directory")
    inputs = release_inputs(root, binary_directory, target)
    output_directory.mkdir(parents=True, exist_ok=True)
    created = []
    try:
        with tempfile.TemporaryDirectory(prefix=".redwax-package-", dir=output_directory) as temporary:
            staged_archive = Path(temporary) / archive_path.name
            with staged_archive.open("wb") as destination:
                write_archive(destination, inputs)
            digest = hashlib.sha256(staged_archive.read_bytes()).hexdigest()
            staged_checksum = Path(temporary) / checksum_path.name
            staged_checksum.write_text(f"{digest}  {archive_path.name}\n")
            for source, destination in ((staged_archive, archive_path), (staged_checksum, checksum_path)):
                os.link(source, destination)
                created.append(destination)
    except Exception:
        for destination in created:
            destination.unlink()
        raise
    return archive_path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--binaries-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    options = parser.parse_args()
    try:
        archive = package_release(ROOT, options.binaries_dir, options.output_dir, options.target)
    except (OSError, ValueError) as error:
        parser.exit(1, f"release packaging failed: {error}\n")
    print(archive)


if __name__ == "__main__":
    main()
