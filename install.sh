#!/usr/bin/env bash
# Install redwax from a checked Linux release or build from this checkout.
#
#   ./install.sh                install dependencies and prebuilt binaries
#   ./install.sh --release TAG  install a particular release
#   ./install.sh --build        install dependencies and build from source
#   ./install.sh --no-deps      use dependencies already installed
#   ./install.sh --with-provider  also install the optional proof-token provider
#   ./install.sh --timer      also install the nightly doctor timer
#   ./install.sh --native-timer  enable native resolution checks every 15 minutes
#   ./install.sh --uninstall  remove what this script installed
#
set -euo pipefail
# Bash reads scripts as they run. Keep a build safe from edits to this checkout.
if [[ "${REDWAX_INSTALL_SNAPSHOT:-}" != "$0" ]]; then
    original_root="$(cd "$(dirname "$0")" && pwd)"
    snapshot="$(mktemp -d)"
    cp "$0" "$snapshot/install.sh"
    bash -n "$snapshot/install.sh" || { rm -rf "$snapshot"; exit 1; }
    export REDWAX_INSTALL_SNAPSHOT="$snapshot/install.sh"
    export REDWAX_INSTALL_ROOT="$original_root"
    exec bash -c 'snapshot=$1; shift; trap '\''rm -rf "$snapshot"'\'' EXIT; bash "$snapshot/install.sh" "$@"' bash "$snapshot" "$@"
fi
cd "$REDWAX_INSTALL_ROOT"

unit_dir="${REDWAX_UNIT_DIR:-$HOME/.config/systemd/user}"
target="${CARGO_TARGET_DIR:-target}/release"
binaries=(redwax redwax-resolve redwax-play redwax-marks)
doctor_units=(redwax-doctor.service redwax-doctor.timer)
native_units=(redwax-native.service redwax-native.timer)
units=("${doctor_units[@]}" "${native_units[@]}")

with_timer=0
with_native_timer=0
uninstall=0
with_dependencies=1
with_provider=0
build_from_source=0
release=""
while (( $# )); do
    argument="$1"
    shift
    case "$argument" in
        --build) build_from_source=1 ;;
        --release)
            if (( ! $# )) || [[ ! "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
                echo "--release requires a release tag" >&2
                exit 2
            fi
            release="$1"
            shift
            ;;
        --no-deps) with_dependencies=0 ;;
        --with-provider) with_provider=1 ;;
        --timer) with_timer=1 ;;
        --native-timer) with_native_timer=1 ;;
        --uninstall) uninstall=1 ;;
        -h|--help) sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
        *) echo "install.sh: unknown argument $argument" >&2; exit 2 ;;
    esac
done
if (( build_from_source )) && [[ -n "$release" ]]; then
    echo "--release and --build cannot be combined" >&2
    exit 2
fi
if (( with_provider && ! with_dependencies )); then
    echo "--with-provider requires dependency installation. Remove --no-deps." >&2
    exit 2
fi

path_contains() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

install_record="$HOME/.local/share/redwax/install-location"
bin_dir="${REDWAX_BIN_DIR:-}"
if (( uninstall )) && [[ -z "$bin_dir" && -f "$install_record" ]]; then
    bin_dir="$(cat "$install_record")"
fi
if [[ -z "$bin_dir" ]]; then
    for candidate in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
        if path_contains "$candidate"; then
            if [[ -d "$candidate" && -w "$candidate" && -O "$candidate" ]]; then
                bin_dir="$candidate"
                break
            fi
        fi
    done
    bin_dir="${bin_dir:-$HOME/.local/bin}"
fi
needs_shell_path=1
path_contains "$bin_dir" && needs_shell_path=0
cargo_command=(cargo)
scratch=""
trap 'if [[ -n "$scratch" ]]; then rm -rf "$scratch"; fi' EXIT

install_system_dependencies() {
    if [[ "$(uname -s)" != Linux ]]; then
        echo "automatic installation supports Linux. Use --no-deps with a prepared toolchain." >&2
        exit 1
    fi
    missing=0
    required_programs=(mpv python3 curl tar xz sha256sum)
    if (( build_from_source )); then
        required_programs+=(cc make pkg-config cmake)
    fi
    for program in "${required_programs[@]}"; do
        command -v "$program" >/dev/null || missing=1
    done
    if command -v python3 >/dev/null; then
        python3 -c 'import sys, venv, ensurepip; sys.exit(sys.version_info < (3, 10))' 2>/dev/null || missing=1
    fi
    extra_packages=()
    if (( with_provider )); then
        extra_packages+=(npm)
        if ! command -v npm >/dev/null; then
            local managed_bin="$HOME/.local/share/redwax/node/bin"
            if [[ ! -x "$managed_bin/npm" || ! -x "$managed_bin/node" ]] \
                || ! "$managed_bin/node" -e 'if (Number(process.versions.node.split(".")[0]) < 22) process.exit(1)'; then
                missing=1
            fi
        fi
    fi
    if (( ! missing )); then
        return
    fi
    . "${REDWAX_OS_RELEASE:-/etc/os-release}"
    privilege=()
    if (( EUID != 0 )); then
        if ! command -v sudo >/dev/null; then
            echo "install sudo or ask your administrator to install the dependencies" >&2
            exit 1
        fi
        privilege=(sudo)
    fi
    for distribution in "${ID:-}" ${ID_LIKE:-}; do
        case "$distribution" in
            debian|ubuntu)
                build_packages=()
                (( ! build_from_source )) || build_packages+=(build-essential pkg-config cmake)
                "${privilege[@]}" apt-get update
                "${privilege[@]}" apt-get install -y --no-install-recommends \
                    "${build_packages[@]}" curl ca-certificates xz-utils \
                    mpv python3 python3-venv "${extra_packages[@]}"
                return
                ;;
            fedora|rhel|centos)
                build_packages=()
                (( ! build_from_source )) || build_packages+=(gcc gcc-c++ make pkgconf-pkg-config cmake)
                "${privilege[@]}" dnf install -y "${build_packages[@]}" \
                    curl ca-certificates xz mpv python3 "${extra_packages[@]}"
                return
                ;;
            arch)
                build_packages=()
                (( ! build_from_source )) || build_packages+=(base-devel cmake)
                "${privilege[@]}" pacman -S --needed --noconfirm \
                    "${build_packages[@]}" curl ca-certificates xz mpv python "${extra_packages[@]}"
                return
                ;;
        esac
    done
    echo "unsupported distribution. Install mpv, Python 3.10+ with venv, curl and Node.js 22+, then use --no-deps. Source builds also need Rust 1.98+ and C build tools." >&2
    exit 1
}

download_checked() {
    local url="$1" checksum="$2" destination="$3"
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 "$url" -o "$destination"
    printf '%s  %s\n' "$checksum" "$destination" | sha256sum --check
}

install_node() {
    local node_binary="${REDWAX_NODE:-$HOME/.local/share/redwax/node/bin/node}"
    if [[ ! -x "$node_binary" ]]; then
        node_binary=node
    fi
    if command -v "$node_binary" >/dev/null && "$node_binary" -e 'if (Number(process.versions.node.split(".")[0]) < 22) process.exit(1)'; then
        export REDWAX_NODE="$node_binary"
        if [[ "$node_binary" == */* ]]; then
            export PATH="$(dirname "$node_binary"):$PATH"
        fi
        return
    fi
    local architecture checksum
    case "$(uname -m)" in
        x86_64)
            architecture=x64
            checksum=df450af89261115ef9f9e3830c3eeb2cc9213b63c720b1af623cb5dcbe2e02de
            ;;
        aarch64|arm64)
            architecture=arm64
            checksum=a44aeb94849a299b22df10b9e622ec2f605c2183501bc40590705131de7c740f
            ;;
        *) echo "Node bootstrap supports x86_64 and aarch64. Install Node.js 22+ manually." >&2; exit 1 ;;
    esac
    scratch="${scratch:-$(mktemp -d)}"
    download_checked "https://nodejs.org/dist/v22.23.3/node-v22.23.3-linux-$architecture.tar.xz" "$checksum" "$scratch/node.tar.xz"
    local node_home="$HOME/.local/share/redwax/node"
    mkdir -p "$node_home"
    tar -xJf "$scratch/node.tar.xz" --strip-components=1 -C "$node_home"
    export REDWAX_NODE="$node_home/bin/node"
    export PATH="$node_home/bin:$PATH"
    "$REDWAX_NODE" --version
}

install_rust() {
    local version
    version="$(rustc --version 2>/dev/null || true)"
    if command -v cargo >/dev/null && [[ "$version" =~ ^rustc\ ([0-9]+)\.([0-9]+) ]] \
        && (( BASH_REMATCH[1] > 1 || BASH_REMATCH[1] == 1 && BASH_REMATCH[2] >= 98 )); then
        return
    fi
    if [[ -x "$HOME/.cargo/bin/rustup" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    if command -v rustup >/dev/null; then
        rustup toolchain install 1.98.0 --profile minimal
    else
        local architecture checksum
        case "$(uname -m)" in
            x86_64) architecture=x86_64; checksum=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c ;;
            aarch64|arm64) architecture=aarch64; checksum=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c ;;
            *) echo "Rust bootstrap supports x86_64 and aarch64. Install Rust 1.98+ manually." >&2; exit 1 ;;
        esac
        scratch="${scratch:-$(mktemp -d)}"
        download_checked "https://static.rust-lang.org/rustup/archive/1.28.2/$architecture-unknown-linux-gnu/rustup-init" "$checksum" "$scratch/rustup-init"
        chmod +x "$scratch/rustup-init"
        "$scratch/rustup-init" -y --profile minimal --default-toolchain 1.98.0 --no-modify-path
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    cargo_command=(rustup run 1.98.0 cargo)
}

select_release_target() {
    if [[ "$(uname -s)" != Linux ]]; then
        echo "prebuilt installation supports Linux" >&2
        exit 1
    fi
    case "$(uname -m)" in
        x86_64) release_target=x86_64-unknown-linux-musl; elf_machine=62 ;;
        aarch64|arm64) release_target=aarch64-unknown-linux-musl; elf_machine=183 ;;
        *) echo "no prebuilt binary for this architecture. Use --build with a prepared toolchain." >&2; exit 1 ;;
    esac
}

prepare_release() {
    select_release_target
    scratch="${scratch:-$(mktemp -d)}"
    if [[ -z "$release" && -f SHA256SUMS && -d bin ]]; then
        runtime_root="$REDWAX_INSTALL_ROOT"
    else
        local asset="redwax-$release_target.tar.gz"
        if [[ -n "${REDWAX_RELEASE_DIR:-}" ]]; then
            cp "$REDWAX_RELEASE_DIR/$asset" "$scratch/$asset"
            cp "$REDWAX_RELEASE_DIR/$asset.sha256" "$scratch/$asset.sha256"
        else
            local base="https://github.com/SamRichardsonDEV/redwax/releases/latest/download"
            if [[ -n "$release" ]]; then
                base="https://github.com/SamRichardsonDEV/redwax/releases/download/$release"
            fi
            echo "downloading $asset"
            if ! curl --proto '=https' --tlsv1.2 -fsSL --retry 3 "$base/$asset" -o "$scratch/$asset" \
                || ! curl --proto '=https' --tlsv1.2 -fsSL --retry 3 "$base/$asset.sha256" -o "$scratch/$asset.sha256"; then
                echo "release download failed. No source build was started. Use --build to build this checkout." >&2
                exit 1
            fi
        fi
        python3 - "$scratch/$asset" "$scratch/extracted" <<'PY_RELEASE'
import hashlib
from pathlib import Path, PurePosixPath
import re
import tarfile
import sys

archive_path, destination = map(Path, sys.argv[1:])
try:
    checksum = archive_path.with_name(archive_path.name + ".sha256").read_text().strip()
    if not re.fullmatch(r"[a-fA-F0-9]{64}  " + re.escape(archive_path.name), checksum):
        raise ValueError("release checksum file is malformed")
    if hashlib.sha256(archive_path.read_bytes()).hexdigest() != checksum[:64].lower():
        raise ValueError("release archive checksum mismatch")
    with tarfile.open(archive_path, "r:gz") as archive:
        entries = archive.getmembers()
        seen = set()
        for entry in entries:
            path = PurePosixPath(entry.name)
            if not path.parts or path.parts[0] != "redwax" or path.is_absolute() or ".." in path.parts or "\\" in entry.name:
                raise ValueError("release archive contains an unsafe path")
            if str(path) in seen or not (entry.isdir() or entry.isfile()):
                raise ValueError("release archive contains a link, duplicate or special file")
            seen.add(str(path))
        for entry in entries:
            path = destination / entry.name
            if entry.isdir():
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(entry) as source, path.open("xb") as output:
                    while chunk := source.read(1024 * 1024):
                        output.write(chunk)
                path.chmod(0o755 if entry.mode & 0o111 else 0o644)
except (OSError, ValueError, tarfile.TarError) as error:
    sys.exit(f"release validation failed: {error}")
PY_RELEASE
        runtime_root="$scratch/extracted/redwax"
    fi
    python3 - "$runtime_root" "$elf_machine" <<'PY_BINARIES'
import hashlib
from pathlib import Path
import re
import stat
import struct
import sys

root, machine = Path(sys.argv[1]), int(sys.argv[2])
names = ("redwax", "redwax-resolve", "redwax-play", "redwax-marks")
try:
    manifest = root / "SHA256SUMS"
    if not stat.S_ISREG(manifest.lstat().st_mode):
        raise ValueError("binary checksums must be a regular file")
    hashes = {}
    for line in manifest.read_text().splitlines():
        match = re.fullmatch(r"([a-fA-F0-9]{64})  (bin/[a-z-]+)", line)
        if not match or match[2] in hashes:
            raise ValueError("binary checksum manifest is malformed")
        hashes[match[2]] = match[1].lower()
    if set(hashes) != {f"bin/{name}" for name in names}:
        raise ValueError("binary checksum manifest must list all four binaries")
    for name in names:
        path = root / "bin" / name
        if not stat.S_ISREG(path.lstat().st_mode) or not path.stat().st_mode & 0o111 or not path.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"release binary must be a regular executable: {name}")
        contents = path.read_bytes()
        if hashlib.sha256(contents).hexdigest() != hashes[f"bin/{name}"]:
            raise ValueError(f"release binary checksum mismatch: {name}")
        if len(contents) < 64 or contents[:7] != b"\x7fELF\x02\x01\x01" or contents[7] not in (0, 3):
            raise ValueError(f"release binary must be a Linux ELF64 executable: {name}")
        executable_type, executable_machine = struct.unpack_from("<HH", contents, 16)
        if executable_type not in (2, 3) or executable_machine != machine:
            raise ValueError(f"release binary architecture mismatch: {name}")
        header_offset = struct.unpack_from("<Q", contents, 32)[0]
        entry_size, entry_count = struct.unpack_from("<HH", contents, 54)
        if not entry_count or entry_size < 56 or header_offset < 64 or header_offset + entry_size * entry_count > len(contents):
            raise ValueError(f"release binary has invalid ELF program headers: {name}")
        has_load_segment = False
        for index in range(entry_count):
            header = header_offset + index * entry_size
            segment_type = struct.unpack_from("<I", contents, header)[0]
            segment_offset = struct.unpack_from("<Q", contents, header + 8)[0]
            segment_size = struct.unpack_from("<Q", contents, header + 32)[0]
            if segment_offset + segment_size > len(contents):
                raise ValueError(f"release binary has a truncated ELF segment: {name}")
            has_load_segment |= segment_type == 1
            if segment_type == 3:
                raise ValueError(f"release binary requires a dynamic interpreter: {name}")
            if segment_type == 2:
                if segment_size % 16:
                    raise ValueError(f"release binary has an invalid ELF dynamic table: {name}")
                for offset in range(segment_offset, segment_offset + segment_size, 16):
                    tag = struct.unpack_from("<q", contents, offset)[0]
                    if tag == 0:
                        break
                    if tag == 1:
                        raise ValueError(f"release binary requires shared libraries: {name}")
        if not has_load_segment:
            raise ValueError(f"release binary has no ELF load segment: {name}")
    for relative in ("scripts/install_resolver.sh", "scripts/systemd/redwax-doctor.service", "scripts/systemd/redwax-doctor.timer", "scripts/systemd/redwax-native.service", "scripts/systemd/redwax-native.timer"):
        path = root / relative
        if not stat.S_ISREG(path.lstat().st_mode) or not path.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"release runtime file must be regular: {relative}")
except (OSError, ValueError) as error:
    sys.exit(f"release validation failed: {error}")
PY_BINARIES
    target="$runtime_root/bin"
    if ! "$target/redwax" --version; then
        echo "release binary failed its version check: redwax" >&2
        exit 1
    fi
}

configure_shell_path() {
    if (( ! needs_shell_path )); then
        return
    fi
    local path_line startup_file quoted_directory
    printf -v quoted_directory '%q' "$bin_dir"
    path_line="export PATH=$quoted_directory:\"\$PATH\""
    startup_files=()
    local login_shell="${SHELL:-bash}"
    case "${login_shell##*/}" in
        bash|"")
            startup_files+=("$HOME/.bashrc")
            if [[ -f "$HOME/.bash_profile" ]]; then
                startup_files+=("$HOME/.bash_profile")
            elif [[ -f "$HOME/.bash_login" ]]; then
                startup_files+=("$HOME/.bash_login")
            else
                startup_files+=("$HOME/.profile")
            fi
            ;;
        zsh) startup_files+=("${ZDOTDIR:-$HOME}/.zshrc" "${ZDOTDIR:-$HOME}/.zprofile") ;;
        fish)
            startup_files+=("${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/redwax.fish")
            path_line="fish_add_path --prepend $quoted_directory"
            ;;
        *) echo "add $bin_dir to PATH in your shell startup file before running redwax"; return ;;
    esac
    for startup_file in "${startup_files[@]}"; do
        mkdir -p "$(dirname "$startup_file")"
        if [[ ! -f "$startup_file" ]] || ! grep -Fxq "$path_line" "$startup_file"; then
            printf '\n# redwax executable directory\n%s\n' "$path_line" >> "$startup_file"
        fi
    done
    echo "restart your shell once to make the redwax command available"
}

# ------------------------------------------------------------------ uninstall

if (( uninstall )); then
    for timer in redwax-doctor.timer redwax-native.timer; do
        if command -v systemctl > /dev/null && [[ -f "$unit_dir/$timer" ]]; then
            systemctl --user disable --now "$timer" > /dev/null 2>&1 || true
        fi
    done
    for name in "${binaries[@]}"; do
        if [[ -e "$bin_dir/$name" ]]; then
            rm -f "$bin_dir/$name"
            echo "removed $bin_dir/$name"
        fi
    done
    for name in "${units[@]}"; do
        if [[ -e "$unit_dir/$name" ]]; then
            rm -f "$unit_dir/$name"
            echo "removed $unit_dir/$name"
        fi
    done
    command -v systemctl > /dev/null && systemctl --user daemon-reload || true
    rm -f "$install_record"
    echo "install.sh: uninstalled. Configuration and cache files were retained."
    exit 0
fi

# -------------------------------------------------------------------- install

if [[ -d "$bin_dir" && -z "${REDWAX_BIN_DIR:-}" && ( ! -w "$bin_dir" || ! -O "$bin_dir" ) ]]; then
    echo "choose a writable directory you own with REDWAX_BIN_DIR" >&2
    exit 1
fi

runtime_root="$REDWAX_INSTALL_ROOT"
if (( ! build_from_source )); then
    for program in python3 curl; do
        if ! command -v "$program" >/dev/null; then
            echo "install $program first so the release can be downloaded and checked" >&2
            exit 1
        fi
    done
    prepare_release
elif [[ ! -f Cargo.toml ]]; then
    echo "--build needs a source checkout containing Cargo.toml" >&2
    exit 1
fi

if (( with_dependencies )); then
    install_system_dependencies
    install_node
    if (( build_from_source )); then
        install_rust
    fi
    resolver_arguments=()
    if (( with_provider )); then
        resolver_arguments+=(--with-provider)
    fi
    "$runtime_root/scripts/install_resolver.sh" "${resolver_arguments[@]}"
fi

if (( build_from_source )); then
    echo "building redwax"
    "${cargo_command[@]}" build --locked --release --workspace
fi

if (( with_native_timer )); then
    if [[ ! -x "$target/redwax-resolve" ]]; then
        echo "install.sh: native canary executable was not built" >&2
        exit 1
    fi
    echo "checking the candidate native resolver before installation"
    "$target/redwax-resolve" --native-canary
fi

for name in "${binaries[@]}"; do
    built="$target/$name"
    if [[ ! -x "$built" ]]; then
        echo "install.sh: missing built executable $name" >&2
        exit 1
    fi
done
mkdir -p "$bin_dir"
for name in "${binaries[@]}"; do
    install -m 755 "$target/$name" "$bin_dir/$name"
    echo "installed $bin_dir/$name"
done

if (( with_native_timer )); then
    echo "checking the installed native resolver before timer activation"
    "$bin_dir/redwax-resolve" --native-canary
fi

if (( with_timer || with_native_timer )); then
    mkdir -p "$unit_dir"
    selected_units=()
    if (( with_timer )); then
        selected_units+=("${doctor_units[@]}")
    fi
    if (( with_native_timer )); then
        selected_units+=("${native_units[@]}")
    fi
    for name in "${selected_units[@]}"; do
        unit_binary="$bin_dir/redwax"
        if [[ "$name" == redwax-native.* ]]; then
            unit_binary="$bin_dir/redwax-resolve"
        fi
        escaped_binary="${unit_binary//\\/\\\\}"
        escaped_binary="${escaped_binary//\"/\\\"}"
        escaped_binary="${escaped_binary//%/%%}"
        while IFS= read -r unit_line || [[ -n "$unit_line" ]]; do
            if [[ "$unit_line" == ExecStart=@BIN@* ]]; then
                printf 'ExecStart="%s"%s\n' "$escaped_binary" "${unit_line#ExecStart=@BIN@}"
            else
                printf '%s\n' "$unit_line"
            fi
        done < "$runtime_root/scripts/systemd/$name" > "$unit_dir/$name"
        echo "installed $unit_dir/$name"
    done
    if command -v systemctl > /dev/null; then
        systemctl --user daemon-reload
        echo "reloaded the user units"
    fi
    if (( with_timer )); then
        # The nightly doctor includes checks outside native resolution.
        echo "enable it with: systemctl --user enable --now redwax-doctor.timer"
    fi
    if (( with_native_timer )); then
        if command -v systemctl > /dev/null; then
            systemctl --user enable --now redwax-native.timer
            echo "enabled native resolution checks every 15 minutes"
        else
            echo "enable it with: systemctl --user enable --now redwax-native.timer"
        fi
    fi
fi

configure_shell_path
mkdir -p "$(dirname "$install_record")"
printf '%s\n' "$bin_dir" > "$install_record"
chmod 600 "$install_record"
printf '\nInstalled. Sign in, then open the player:\n\n  redwax login\n  redwax\n'
