#!/usr/bin/env bash
# Capture the parser fixtures, then refuse to leave a secret in them.
# Usage: scripts/fixtures.sh [--check | --from-raw] [--source-dir DIR]
#
#   --check      run the scrub check only, make no request
#   --from-raw   rescrub the last raw capture under fixtures/raw
#   --source-dir load ytmusicapi from a source checkout
#
# Override Python with REDWAX_PYTHON. REDWAX_YTMUSICAPI_SOURCE supplies the
# same package root as --source-dir.
set -euo pipefail
cd "$(dirname "$0")/.."

python_command="${REDWAX_PYTHON:-python3}"
source_dir="${REDWAX_YTMUSICAPI_SOURCE:-}"
mode="capture"
while (( $# )); do
    case "$1" in
        --check | --from-raw)
            mode="$1"
            shift
            ;;
        --source-dir)
            if (( $# < 2 )); then
                echo "--source-dir needs a directory" >&2
                exit 2
            fi
            source_dir="$2"
            shift 2
            ;;
        --help)
            echo "usage: scripts/fixtures.sh [--check | --from-raw] [--source-dir DIR]"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [[ "$mode" != "--check" ]]; then
    if ! command -v "$python_command" >/dev/null 2>&1; then
        echo "Python command not found: $python_command. Set REDWAX_PYTHON." >&2
        exit 1
    fi
    args=()
    [[ "$mode" == "--from-raw" ]] && args=(--from-raw)
    [[ -n "$source_dir" ]] && args+=(--ytmusicapi-source "$source_dir")
    "$python_command" scripts/capture_fixtures.py "${args[@]}"
    echo
fi

"$python_command" scripts/check_fixtures.py
