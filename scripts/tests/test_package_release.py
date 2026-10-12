import hashlib
import importlib.util
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "package_release", Path(__file__).resolve().parents[1] / "package_release.py"
)
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)
TARGET = "x86_64-unknown-linux-musl"


def executable(machine=62, extra_segments=()):
    headers = [(1, b"synthetic-static-binary"), *extra_segments]
    contents = bytearray(64 + 56 * len(headers))
    contents[:8] = b"\x7fELF\x02\x01\x01\x00"
    struct.pack_into("<HHI", contents, 16, 2, machine, 1)
    struct.pack_into("<Q", contents, 32, 64)
    struct.pack_into("<HHH", contents, 52, 64, 56, len(headers))
    for index, (segment_type, data) in enumerate(headers):
        offset = len(contents)
        struct.pack_into("<IIQQQQQQ", contents, 64 + index * 56,
                         segment_type, 5, offset, 0, 0, len(data), len(data), 1)
        contents.extend(data)
    return bytes(contents)


class PackageReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.root = self.directory / "source"
        self.root.mkdir()
        self.binaries = self.directory / "binaries"
        self.binaries.mkdir()
        self.output = self.directory / "output"
        for name in PACKAGE.RUNTIME_FILES:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"public runtime file {name}\n")
        for name in PACKAGE.BINARIES:
            self.write_binary(name, executable())

    def write_binary(self, name, data):
        path = self.binaries / name
        path.write_bytes(data)
        path.chmod(0o755)

    def package(self, target=TARGET, output=None):
        return PACKAGE.package_release(self.root, self.binaries, output or self.output, target)

    def assert_rejected(self, message):
        with self.assertRaisesRegex(ValueError, message):
            self.package()
        self.assertFalse(self.output.exists())

    def test_archive_is_deterministic_and_contains_only_runtime_inputs(self):
        for name in ("LICENSE", ".git/config", "crates/ytm/tests/fixtures/private.json"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("private input must not be packaged")
        first = self.package()
        second = self.package(output=self.directory / "second")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        checksum = first.with_name(first.name + ".sha256").read_text()
        self.assertEqual(checksum, f"{hashlib.sha256(first.read_bytes()).hexdigest()}  {first.name}\n")
        with tarfile.open(first) as archive:
            files = {member.name: member for member in archive if member.isfile()}
            expected = {f"redwax/{name}" for name in PACKAGE.RUNTIME_FILES}
            expected.update(f"redwax/bin/{name}" for name in PACKAGE.BINARIES)
            expected.add("redwax/SHA256SUMS")
            self.assertEqual(set(files), expected)
            for member in archive:
                self.assertEqual((member.uid, member.gid, member.mtime), (0, 0, 0))
                self.assertEqual((member.uname, member.gname), ("", ""))
                self.assertFalse(member.issym() or member.islnk())
            manifest = archive.extractfile("redwax/SHA256SUMS").read().decode()
            for line in manifest.splitlines():
                digest, name = line.split("  ")
                data = archive.extractfile(f"redwax/{name}").read()
                self.assertEqual(hashlib.sha256(data).hexdigest(), digest)
                self.assertEqual(files[f"redwax/{name}"].mode, 0o755)

    def test_accepts_aarch64_static_binaries(self):
        for name in PACKAGE.BINARIES:
            self.write_binary(name, executable(machine=183))
        self.assertTrue(self.package("aarch64-unknown-linux-musl").is_file())

    def test_rejects_wrong_architecture(self):
        self.write_binary("redwax", executable(machine=183))
        self.assert_rejected("does not match")

    def test_rejects_dynamic_interpreter(self):
        self.write_binary("redwax", executable(extra_segments=((3, b"/lib/ld.so\0"),)))
        self.assert_rejected("dynamic interpreter")

    def test_rejects_shared_library_requirements(self):
        dynamic = struct.pack("<qQqQ", 1, 1, 0, 0)
        self.write_binary("redwax", executable(extra_segments=((2, dynamic),)))
        self.assert_rejected("shared libraries")

    def test_rejects_truncated_program_headers(self):
        self.write_binary("redwax", executable()[:80])
        self.assert_rejected("invalid ELF program headers")

    def test_rejects_truncated_segments(self):
        self.write_binary("redwax", executable()[:-1])
        self.assert_rejected("truncated ELF segment")

    def test_rejects_non_executable_input(self):
        (self.binaries / "redwax").chmod(0o644)
        self.assert_rejected("not executable")

    def test_rejects_non_elf_input(self):
        self.write_binary("redwax", b"#!/bin/sh\necho not-a-binary\n")
        self.assert_rejected("not a Linux ELF64")

    def test_rejects_symlinked_binary(self):
        binary = self.binaries / "redwax"
        binary.rename(self.binaries / "original")
        binary.symlink_to("original")
        self.assert_rejected("symlink")

    def test_rejects_symlinked_runtime_directory(self):
        original = self.root / "docs"
        original.rename(self.root / "original-docs")
        original.symlink_to("original-docs", target_is_directory=True)
        self.assert_rejected("symlink")

    def test_rejects_input_escaping_source(self):
        file = self.root / "README.md"
        file.unlink()
        file.symlink_to(self.directory / "private")
        (self.directory / "private").write_text("sensitive")
        self.assert_rejected("escapes its source")

    def test_existing_outputs_are_not_overwritten(self):
        archive = self.package()
        original = archive.read_bytes()
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.package()
        self.assertEqual(archive.read_bytes(), original)

    def test_existing_checksum_blocks_archive_creation(self):
        self.output.mkdir()
        checksum = self.output / f"redwax-{TARGET}.tar.gz.sha256"
        checksum.write_text("existing")
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.package()
        self.assertEqual(list(self.output.iterdir()), [checksum])

    def test_rejects_output_directory_symlink(self):
        real = self.directory / "real-output"
        real.mkdir()
        self.output.symlink_to(real, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "output must be a directory"):
            self.package()
        self.assertEqual(list(real.iterdir()), [])

    def test_rejects_unsupported_target(self):
        with self.assertRaisesRegex(ValueError, "unsupported release target"):
            self.package("x86_64-unknown-linux-gnu")
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
