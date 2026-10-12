"""Installer regressions use a temporary checkout and mocked privileged commands."""

import hashlib
import io
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="redwax-installer-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / "home with spaces"
        self.checkout = self.root / "checkout"
        self.programs = self.root / "programs"
        for directory in [self.home, self.checkout / "scripts", self.programs]:
            directory.mkdir(parents=True)
        shutil.copy2(ROOT / "install.sh", self.checkout / "install.sh")
        (self.checkout / "Cargo.toml").write_text("[workspace]\n")
        shutil.copytree(ROOT / "scripts/systemd", self.checkout / "scripts/systemd")
        self.log = self.root / "commands.log"
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("REDWAX_", "CARGO_", "XDG_", "RUSTUP_"))
        }
        self.environment.update(
            HOME=str(self.home), PATH=f"{self.programs}:/usr/bin:/bin", SHELL="/bin/bash",
            CARGO_TARGET_DIR=str(self.root / "build"), REDWAX_TEST_LOG=str(self.log),
            REDWAX_OS_RELEASE=str(self.root / "os-release"),
        )
        (self.root / "os-release").write_text("ID=ubuntu\n")
        self.program("rustc", "echo 'rustc 1.98.0 (mock)'")
        self.program("node", "echo v22.0.0")
        self.program("python3", "exit 1")
        self.program("sudo", 'printf "sudo %s\\n" "$*" >> "$REDWAX_TEST_LOG"\n"$@"')
        for name in ["apt-get", "dnf", "pacman", "systemctl"]:
            self.program(name, f'printf "{name} %s\\n" "$*" >> "$REDWAX_TEST_LOG"')
        self.program("cargo", '''printf 'cargo %s\\n' "$*" >> "$REDWAX_TEST_LOG"
mkdir -p "$CARGO_TARGET_DIR/release"
for name in redwax redwax-resolve redwax-play redwax-marks; do
    printf '#!/bin/sh\\nprintf "binary %%s\\n" "$*" >> "$REDWAX_TEST_LOG"\\n' > "$CARGO_TARGET_DIR/release/$name"
    chmod +x "$CARGO_TARGET_DIR/release/$name"
done''')
        self.write(self.checkout / "scripts/install_resolver.sh", '#!/bin/sh\nprintf "resolver %s\\n" "$*" >> "$REDWAX_TEST_LOG"\n')

    def write(self, path, body):
        path.write_text(body)
        path.chmod(0o755)

    def program(self, name, body):
        self.write(self.programs / name, f"#!/bin/sh\n{body}\n")

    def run_installer(self, *arguments, status=0, binary=False):
        if not binary and "--uninstall" not in arguments:
            arguments = ("--build", *arguments)
        result = subprocess.run(
            ["/bin/bash", str(self.checkout / "install.sh"), *arguments],
            cwd=self.root, env=self.environment, capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, status, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def commands(self):
        return self.log.read_text() if self.log.exists() else ""

    def test_prepared_install_uses_one_public_name_and_persists_shell_path(self):
        output = self.run_installer("--no-deps")
        binary = self.home / ".local/bin/redwax"
        self.assertTrue(binary.is_file())
        self.assertFalse((binary.parent / "redwax-rs").exists())
        self.assertIn("cargo build --locked --release --workspace", self.commands())
        self.assertNotIn("resolver", self.commands())
        self.assertIn("redwax login", output)
        self.assertIn("restart your shell", output)
        self.run_installer("--no-deps")
        self.assertEqual((self.home / ".bashrc").read_text().count("# redwax executable directory"), 1)
        environment = dict(self.environment)
        environment["HOME"] = str(self.home)
        result = subprocess.run(
            ["/bin/bash", "-c", '. "$HOME/.profile"; command -v redwax'],
            env=environment, text=True, capture_output=True, check=True,
        )
        self.assertEqual(result.stdout.strip(), str(binary))

    def test_distribution_dependencies_and_resolver_are_installed(self):
        for distribution, expected in [("ubuntu", "apt-get install"), ("fedora", "dnf install"), ("arch", "pacman -S")]:
            with self.subTest(distribution=distribution):
                (self.root / "os-release").write_text(f"ID={distribution}\n")
                self.log.write_text("")
                self.run_installer()
                self.assertIn(expected, self.commands())
                self.assertIn("mpv", self.commands())
                self.assertNotIn("nodejs", self.commands())
                self.assertNotIn("python3-pip", self.commands())
                self.assertNotIn("python-pip", self.commands())
                self.assertIn("resolver ", self.commands())

    def test_uninstall_uses_recorded_destination_after_path_changes(self):
        cargo_bin = self.home / ".cargo/bin"
        cargo_bin.mkdir(parents=True)
        self.environment["PATH"] = f"{self.programs}:{cargo_bin}:/usr/bin:/bin"
        self.run_installer("--no-deps")
        self.assertTrue((cargo_bin / "redwax").exists())
        config = self.home / ".config/redwax/config.toml"
        config.parent.mkdir(parents=True)
        config.write_text('theme = "terminal"\n')
        self.environment["PATH"] = f"{self.programs}:/usr/bin:/bin"
        self.run_installer("--uninstall")
        self.assertFalse((cargo_bin / "redwax").exists())
        self.assertTrue(config.exists())

    def test_native_timer_checks_candidate_and_installed_binary(self):
        self.run_installer("--no-deps", "--native-timer")
        self.assertEqual(self.commands().count("binary --native-canary"), 2)
        self.assertIn("systemctl --user enable --now redwax-native.timer", self.commands())
        service = (self.home / ".config/systemd/user/redwax-native.service").read_text()
        self.assertIn(f'ExecStart="{self.home}/.local/bin/redwax-resolve" --native-canary', service)

    def test_other_supported_shells_receive_their_startup_file(self):
        for shell, startup in [("zsh", ".zshrc"), ("fish", ".config/fish/conf.d/redwax.fish")]:
            with self.subTest(shell=shell):
                self.environment["SHELL"] = f"/bin/{shell}"
                self.run_installer("--no-deps")
                self.assertIn("redwax executable directory", (self.home / startup).read_text())

    def test_provider_cannot_be_silently_skipped(self):
        output = self.run_installer("--no-deps", "--with-provider", status=2)
        self.assertIn("Remove --no-deps", output)
        self.assertNotIn("cargo build", self.commands())

    def isolate_base_programs(self):
        for name in ["dirname", "cp", "bash", "uname", "mkdir", "cat", "chmod", "install", "mktemp", "rm", "grep", "sed", "tar", "xz", "sha256sum", "cc", "make", "pkg-config", "cmake", "curl"]:
            path = shutil.which(name)
            if path:
                (self.programs / name).symlink_to(path)
            else:
                self.program(name, "exit 0")
        self.program("python3", "exit 0")
        self.program("mpv", "echo mpv")
        self.environment["PATH"] = str(self.programs)

    def test_provider_provisions_npm_when_existing_node_is_supported(self):
        self.isolate_base_programs()
        self.run_installer("--with-provider")
        self.assertIn(" npm", self.commands())
        self.assertIn("resolver --with-provider", self.commands())

    def test_managed_npm_does_not_need_another_system_install(self):
        self.isolate_base_programs()
        managed_bin = self.home / ".local/share/redwax/node/bin"
        managed_bin.mkdir(parents=True)
        self.write(managed_bin / "node", "#!/bin/sh\necho v22.0.0\n")
        self.write(managed_bin / "npm", "#!/bin/sh\nexit 0\n")
        self.run_installer("--with-provider")
        self.assertNotIn("apt-get", self.commands())
        self.assertNotIn("sudo", self.commands())
        self.assertIn("resolver --with-provider", self.commands())

    def test_existing_managed_node_is_visible_to_provider_tooling(self):
        managed_bin = self.home / ".local/share/redwax/node/bin"
        managed_bin.mkdir(parents=True)
        self.write(managed_bin / "node", "#!/bin/sh\necho v22.0.0\n")
        self.write(self.checkout / "scripts/install_resolver.sh", '#!/bin/sh\nprintf "selected-node %s\\n" "$(command -v node)" >> "$REDWAX_TEST_LOG"\n')
        self.program("node", "exit 1")
        self.run_installer()
        self.assertIn(f"selected-node {managed_bin}/node", self.commands())

    def test_existing_rustup_installs_required_toolchain_without_bootstrap(self):
        self.program("rustc", "echo 'rustc 1.97.0 (mock)'")
        self.program("rustup", 'printf "rustup %s\\n" "$*" >> "$REDWAX_TEST_LOG"\nif [ "$1" = run ]; then shift 2; "$@"; fi')
        self.run_installer()
        self.assertIn("rustup toolchain install 1.98.0 --profile minimal", self.commands())
        self.assertIn("rustup run 1.98.0 cargo build --locked --release --workspace", self.commands())

    def test_failed_bootstrap_checksum_stops_before_build(self):
        self.program("node", "exit 1")
        self.program("curl", 'printf "curl %s\\n" "$*" >> "$REDWAX_TEST_LOG"\nwhile [ "$1" != -o ]; do shift; done\nprintf bad > "$2"')
        self.program("sha256sum", 'cat >/dev/null\necho "checksum rejected" >&2\nexit 1')
        self.run_installer(status=1)
        self.assertIn("https://nodejs.org/dist/v22.23.3/", self.commands())
        self.assertNotIn("cargo build", self.commands())

    def release_fixture(self, target="x86_64-unknown-linux-musl", extra=None, broken_binary=None, dynamic_binary=None):
        python = self.programs / "python3"
        python.unlink()
        python.symlink_to(shutil.which("python3"))
        self.release_dir = self.root / "releases"
        self.release_dir.mkdir(exist_ok=True)
        self.environment["REDWAX_RELEASE_DIR"] = str(self.release_dir)
        archive_path = self.release_dir / f"redwax-{target}.tar.gz"
        def executable(exit_code=0):
            code = b"\xb8\x3c\x00\x00\x00\xbf" + struct.pack("<I", exit_code) + b"\x0f\x05"
            size = 64 + 56 + len(code)
            header = struct.pack("<16sHHIQQQIHHHHHH", b"\x7fELF\x02\x01\x01" + b"\x00" * 9,
                                 2, 62, 1, 0x400078, 64, 0, 0, 64, 56, 1, 0, 0, 0)
            segment = struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0x400000, size, size, 4096)
            return header + segment + code
        files = {}
        hashes = []
        for name in ["redwax", "redwax-resolve", "redwax-play", "redwax-marks"]:
            contents = Path("/usr/bin/true").read_bytes() if name == dynamic_binary else executable(1 if name == broken_binary else 0)
            files[f"redwax/bin/{name}"] = (contents, 0o755)
            hashes.append(f"{hashlib.sha256(contents).hexdigest()}  bin/{name}\n")
        files["redwax/SHA256SUMS"] = ("".join(hashes).encode(), 0o644)
        for path in (self.checkout / "scripts").rglob("*"):
            if path.is_file():
                files[f"redwax/{path.relative_to(self.checkout)}"] = (path.read_bytes(), path.stat().st_mode & 0o777)
        with tarfile.open(archive_path, "w:gz") as archive:
            for name, (contents, mode) in files.items():
                entry = tarfile.TarInfo(name)
                entry.size, entry.mode = len(contents), mode
                archive.addfile(entry, io.BytesIO(contents))
            if extra:
                archive.addfile(extra)
        self.write_release_checksum(archive_path)
        return archive_path

    def write_release_checksum(self, archive_path):
        archive_path.with_name(archive_path.name + ".sha256").write_text(
            f"{hashlib.sha256(archive_path.read_bytes()).hexdigest()}  {archive_path.name}\n"
        )

    def test_prebuilt_install_never_builds_or_installs_build_dependencies(self):
        self.release_fixture()
        self.program("mpv", "exit 0")
        self.program("rustc", 'echo "UNEXPECTED rustc" >> "$REDWAX_TEST_LOG"; exit 1')
        self.program("rustup", 'echo "UNEXPECTED rustup" >> "$REDWAX_TEST_LOG"; exit 1')
        self.program("cc", 'echo "UNEXPECTED cc" >> "$REDWAX_TEST_LOG"; exit 1')
        self.run_installer(binary=True)
        for name in ["redwax", "redwax-resolve", "redwax-play", "redwax-marks"]:
            self.assertTrue((self.home / ".local/bin" / name).is_file())
        self.assertNotIn("cargo", self.commands())
        self.assertNotIn("UNEXPECTED", self.commands())
        self.assertIn("resolver ", self.commands())

    def test_prebuilt_runtime_package_lists_exclude_compilers(self):
        self.release_fixture()
        real_python = str(self.programs / "python3") + "-real"
        (self.programs / "python3").rename(real_python)
        self.program("python3", f'if [ "$1" = -c ]; then exit 1; fi\nexec "{real_python}" "$@"')
        for distribution in ["ubuntu", "fedora", "arch"]:
            with self.subTest(distribution=distribution):
                (self.root / "os-release").write_text(f"ID={distribution}\n")
                self.log.write_text("")
                self.run_installer(binary=True)
                for build_package in ["build-essential", "gcc", "make", "pkg-config", "cmake", "base-devel"]:
                    self.assertNotIn(build_package, self.commands())
                self.assertIn("mpv", self.commands())

    def test_bad_release_checksum_preserves_existing_install_and_dependencies(self):
        archive = self.release_fixture()
        archive.write_bytes(archive.read_bytes() + b"bad")
        bin_dir = self.home / ".local/bin"
        bin_dir.mkdir(parents=True)
        existing = bin_dir / "redwax"
        existing.write_text("previous binary")
        output = self.run_installer(binary=True, status=1)
        self.assertIn("checksum mismatch", output)
        self.assertEqual(existing.read_text(), "previous binary")
        self.assertEqual(self.commands(), "")
        self.assertFalse((self.home / ".profile").exists())

    def test_unsafe_archives_are_rejected_before_extraction_or_installation(self):
        for kind in ["traversal", "symlink", "hardlink", "fifo"]:
            with self.subTest(kind=kind):
                entry = tarfile.TarInfo("redwax/../../escape" if kind == "traversal" else "redwax/unsafe")
                if kind == "symlink":
                    entry.type, entry.linkname = tarfile.SYMTYPE, "/tmp/redwax"
                elif kind == "hardlink":
                    entry.type, entry.linkname = tarfile.LNKTYPE, "redwax/bin/redwax"
                elif kind == "fifo":
                    entry.type = tarfile.FIFOTYPE
                self.release_fixture(extra=entry)
                output = self.run_installer("--no-deps", binary=True, status=1)
                self.assertIn("release validation failed", output)
                self.assertFalse((self.home / ".local/bin").exists())
                self.assertFalse((self.root / "escape").exists())

    def test_failed_player_version_check_leaves_no_partial_install(self):
        self.release_fixture(broken_binary="redwax")
        output = self.run_installer("--no-deps", binary=True, status=1)
        self.assertIn("failed its version check: redwax", output)
        self.assertFalse((self.home / ".local/bin").exists())
        self.assertEqual(self.commands(), "")

    def test_missing_release_never_falls_back_to_a_build(self):
        self.program("curl", 'printf "curl %s\\n" "$*" >> "$REDWAX_TEST_LOG"; exit 22')
        output = self.run_installer(binary=True, status=1)
        self.assertIn("No source build was started", output)
        self.assertIn("releases/latest/download/redwax-x86_64-unknown-linux-musl.tar.gz", self.commands())
        self.assertNotIn("cargo", self.commands())
        self.assertNotIn("apt-get", self.commands())
        self.assertFalse((self.home / ".local/bin").exists())

    def test_release_pin_and_arm_target_choose_exact_asset(self):
        self.program("uname", 'if [ "$1" = -s ]; then echo Linux; else echo aarch64; fi')
        self.program("curl", 'printf "curl %s\\n" "$*" >> "$REDWAX_TEST_LOG"; exit 22')
        self.run_installer("--release", "v1.0.0", "--no-deps", binary=True, status=1)
        self.assertIn("releases/download/v1.0.0/redwax-aarch64-unknown-linux-musl.tar.gz", self.commands())
        self.assertNotIn("cargo", self.commands())

    def test_architecture_mismatch_is_rejected_before_installation(self):
        self.release_fixture(target="aarch64-unknown-linux-musl")
        self.program("uname", 'if [ "$1" = -s ]; then echo Linux; else echo aarch64; fi')
        output = self.run_installer("--no-deps", binary=True, status=1)
        self.assertIn("architecture mismatch", output)
        self.assertFalse((self.home / ".local/bin").exists())

    def test_extracted_archive_checks_binary_manifest_without_download(self):
        archive = self.release_fixture()
        with tarfile.open(archive, "r:gz") as bundle:
            bundle.extractall(self.root / "unpacked", filter="data")
        shutil.copy2(ROOT / "install.sh", self.root / "unpacked/redwax/install.sh")
        self.checkout = self.root / "unpacked/redwax"
        self.program("curl", 'echo "UNEXPECTED curl" >> "$REDWAX_TEST_LOG"; exit 1')
        self.run_installer("--no-deps", binary=True)
        self.assertTrue((self.home / ".local/bin/redwax").is_file())
        (self.checkout / "bin/redwax-marks").write_bytes(b"tampered")
        output = self.run_installer("--no-deps", binary=True, status=1)
        self.assertIn("checksum mismatch: redwax-marks", output)
        self.assertNotIn("UNEXPECTED", self.commands())

    def test_invalid_release_tags_and_conflicting_modes_fail_early(self):
        for arguments in [("--release",), ("--release", "../v1"), ("--release", "v1", "--build")]:
            with self.subTest(arguments=arguments):
                self.run_installer(*arguments, binary=True, status=2)
                self.assertEqual(self.commands(), "")

    def test_script_edits_during_build_cannot_break_the_running_install(self):
        marker, resume = self.root / "building", self.root / "resume"
        cargo = (self.programs / "cargo").read_text()
        self.write(self.programs / "cargo", cargo.replace(
            "mkdir -p", f'touch "{marker}"\nwhile [ ! -e "{resume}" ]; do /bin/sleep 0.01; done\nmkdir -p', 1,
        ))
        process = subprocess.Popen(
            ["/bin/bash", str(self.checkout / "install.sh"), "--build", "--no-deps"],
            cwd=self.root, env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 5
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(marker.exists(), "installer did not reach the build")
        (self.checkout / "install.sh").write_text('#!/bin/bash\necho "broken replacement\n')
        resume.touch()
        stdout, stderr = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 0, stdout + stderr)
        self.assertTrue((self.home / ".local/bin/redwax").exists())

    def test_dynamic_last_helper_is_rejected_before_installation(self):
        self.release_fixture(dynamic_binary="redwax-marks")
        output = self.run_installer("--no-deps", binary=True, status=1)
        self.assertIn("dynamic interpreter: redwax-marks", output)
        self.assertFalse((self.home / ".local/bin").exists())
        self.assertEqual(self.commands(), "")

    def test_malformed_checksum_cannot_name_another_file(self):
        archive = self.release_fixture()
        archive.with_name(archive.name + ".sha256").write_text(
            f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  ../../other-file\n"
        )
        output = self.run_installer("--no-deps", binary=True, status=1)
        self.assertIn("checksum file is malformed", output)
        self.assertFalse((self.home / ".local/bin").exists())
