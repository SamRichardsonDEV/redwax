#!/usr/bin/env bash
# Usage: scripts/rust_lint.sh [--fix]
set -euo pipefail
cd "$(dirname "$0")/.."

fix=0
[[ "${1:-}" == "--fix" ]] && fix=1

status=0

echo "== em dashes"
# Built from its code point, so this line is not itself a hit.
em_dash="$(printf '\u2014')"
if grep -rn --include='*.rs' --include='*.md' --include='*.toml' --include='*.sh' -e "$em_dash" \
    crates scripts docs fixtures ./*.md 2>/dev/null; then
    echo "FAIL: em dashes found"
    status=1
fi

echo "== cargo fmt"
if (( fix )); then
    cargo fmt --all
else
    cargo fmt --all --check || status=1
fi

echo "== cargo clippy"
if (( fix )); then
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged -- -D warnings || status=1
else
    cargo clippy --workspace --all-targets -- -D warnings || status=1
fi

echo "== cargo test"
cargo test --workspace --quiet || status=1

echo "== comment prose"
lint="$HOME/.claude/hooks/prose_lint.py"
if [[ -f "$lint" ]]; then
    tmp="$(mktemp)"
    trap 'rm -f "$tmp"' EXIT
    # Doc comments and line comments only, joined into one document per file.
    for f in $(git ls-files 'crates/**/*.rs' 2>/dev/null || find crates -name '*.rs'); do
        sed -n 's#^[[:space:]]*//[/!]\{0,1\}[[:space:]]\{0,1\}##p' "$f" > "$tmp"
        [[ -s "$tmp" ]] || continue
        if ! python3 "$lint" "$tmp" > /dev/null 2>&1; then
            echo "-- $f"
            python3 "$lint" "$tmp" || true
            status=1
        fi
    done
else
    echo "skipped, prose_lint.py not installed"
fi

if (( status )); then
    echo "LINT FAILED"
else
    echo "LINT OK"
fi
exit $status
