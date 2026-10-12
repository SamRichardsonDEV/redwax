#!/usr/bin/env bash
# Install the pinned yt-dlp solver stack without changing a system package.
set -euo pipefail
umask 077

with_provider=0
case "${1:-}" in
    "") ;;
    --with-provider) with_provider=1 ;;
    -h|--help)
        echo "Usage: scripts/install_resolver.sh [--with-provider]"
        echo "Requires Python 3.10+, venv, curl and Node.js 22+. Provider builds also require npm."
        exit 0
        ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
    echo "too many arguments" >&2
    exit 2
fi
node_binary="${REDWAX_NODE:-node}"
for program in python3 curl "$node_binary"; do
    if ! command -v "$program" >/dev/null; then
        echo "install $program before running this script" >&2
        exit 1
    fi
done
"$node_binary" -e 'if (Number(process.versions.node.split(".")[0]) < 22) process.exit(1)' || {
    echo "Node.js 22 or newer is required" >&2
    exit 1
}
python3 -c 'import sys; sys.exit(sys.version_info < (3, 10))' || {
    echo "Python 3.10 or newer is required" >&2
    exit 1
}
if (( with_provider )) && ! command -v npm >/dev/null; then
    echo "install npm before building the proof-token provider" >&2
    exit 1
fi

install_root="$HOME/.local/share/redwax"
python_home="$install_root/yt-dlp"
provider_home="$install_root/bgutil-pot-provider"
if (( with_provider )) && [[ -e "$provider_home" ]]; then
    echo "provider already exists at $provider_home. Keep it or remove it before reinstalling." >&2
    exit 1
fi
mkdir -p "$install_root"
scratch=$(mktemp -d "$install_root/.resolver.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

# These wheel hashes come from the versioned PyPI release metadata.
curl -fsSL --retry 3 \
    https://files.pythonhosted.org/packages/69/b2/8cd1613f56eed7ceb64fbd4df3f1c01246bfb098e6f398228bafda22b80b/yt_dlp-2026.8.19-py3-none-any.whl \
    -o "$scratch/yt_dlp-2026.8.19-py3-none-any.whl"
curl -fsSL --retry 3 \
    https://files.pythonhosted.org/packages/e3/bd/520769863744b669440a924271a6159ddd82ad5ae26b4ac4d4b69e9f8d44/yt_dlp_ejs-0.8.0-py3-none-any.whl \
    -o "$scratch/yt_dlp_ejs-0.8.0-py3-none-any.whl"
echo "1d57897e94c6665a0a6f9bc54b34e584284e32c034ffab3a7df25d8f7b24eedf  $scratch/yt_dlp-2026.8.19-py3-none-any.whl" | sha256sum --check
echo "79300e5fca7f937a1eeede11f0456862c1b41107ce1d726871e0207424f4bdb4  $scratch/yt_dlp_ejs-0.8.0-py3-none-any.whl" | sha256sum --check
python3 -m venv "$python_home"
"$python_home/bin/python" -m pip install \
    "$scratch/yt_dlp-2026.8.19-py3-none-any.whl[default,secretstorage]" \
    "$scratch/yt_dlp_ejs-0.8.0-py3-none-any.whl"
"$python_home/bin/yt-dlp" --version
echo "installed the native solver and managed yt-dlp under $python_home"

if (( with_provider )); then
    provider_revision=37169ee2656e08c5c2e5dc9df4c598c0cb4c88a8
    curl -fsSL --retry 3 \
        "https://codeload.github.com/Brainicism/bgutil-ytdlp-pot-provider/tar.gz/$provider_revision" \
        -o "$scratch/provider.tar.gz"
    echo "aa92bac728aebe2d3dbf2081b27523da95ef0fbc6a97540b44b398ba5383ccc6  $scratch/provider.tar.gz" | sha256sum --check
    mkdir "$scratch/provider"
    tar -xzf "$scratch/provider.tar.gz" --strip-components=1 -C "$scratch/provider"
    (
        cd "$scratch/provider/server"
        npm ci
        ./node_modules/.bin/tsc
        "$node_binary" build/generate_once.js --version
    )
    curl -fsSL --retry 3 \
        https://files.pythonhosted.org/packages/29/38/fd54194c03f6636488343d4c49e96ce2fe8ecd228c0cf7ce4d024ab7d52b/bgutil_ytdlp_pot_provider-2.0.0-py3-none-any.whl \
        -o "$scratch/bgutil_ytdlp_pot_provider-2.0.0-py3-none-any.whl"
    echo "22884c399802771ddfd9275bb05b178d67c89db67a1d5d1e17ed7f681956628e  $scratch/bgutil_ytdlp_pot_provider-2.0.0-py3-none-any.whl" | sha256sum --check
    "$python_home/bin/python" -m pip install "$scratch/bgutil_ytdlp_pot_provider-2.0.0-py3-none-any.whl"
    mv "$scratch/provider" "$provider_home"
    if ! "$node_binary" "$provider_home/server/build/generate_once.js" --version; then
        mv "$provider_home" "$scratch/provider"
        exit 1
    fi
    echo "installed optional bgutil provider 2.0.0 under $provider_home/server"
fi

echo "run redwax setup to check the programs and saved login"
