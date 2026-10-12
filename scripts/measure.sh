#!/usr/bin/env bash
# Measure launch, stream resolution and playback latency.
#
#   scripts/measure.sh launch [binary]      launch to first frame, 5 runs
#   scripts/measure.sh resolve <id> [args]  redwax-resolve, 3 runs, per source
#   scripts/measure.sh play <id>...         redwax-play, time to audible
#
# Every mode ends with one line starting RESULT, so a gate can grep it.
set -uo pipefail
cd "$(dirname "$0")/.."

target="${CARGO_TARGET_DIR:-target}"
mode="${1:-}"
shift || true

die() {
    echo "$1" >&2
    echo "RESULT $2"
    exit "${3:-1}"
}

# ------------------------------------------------------------------ launch

measure_launch() {
    local binary
    binary="${1:-$target/debug/redwax}"

    if [[ ! -x "$binary" ]]; then
        echo "not measurable yet: $binary is not built"
        echo "RESULT launch not measurable yet, $binary is not built"
        exit 0
    fi

    local output status
    output="$(python3 scripts/measure_launch.py --runs 5 "$binary")"
    status=$?
    echo "$output"
    if (( status == 3 )); then
        echo "not measurable yet: $(echo "$output" | sed -n 's/^UNMEASURABLE //p')"
        echo "RESULT launch not measurable yet, $binary $(echo "$output" | sed -n 's/^UNMEASURABLE //p')"
        exit 0
    fi
    (( status == 0 )) || die "measure_launch.py failed" "launch failed" 1
    echo "RESULT launch median $(echo "$output" | sed -n 's/^MEDIAN //p') ms, gate 50 ms ($binary)"
}

# ----------------------------------------------------------------- resolve

measure_resolve() {
    local binary="$target/debug/redwax-resolve"
    [[ -x "$binary" ]] || die "redwax-resolve not built" "resolve not built" 1
    [[ $# -ge 1 ]] || die "usage: measure.sh resolve <video_id> [args...]" "resolve needs a video id" 1

    local all winners
    all="$(mktemp)"
    winners="$(mktemp)"
    trap 'rm -f "$all" "$winners"' RETURN

    local run
    for run in 1 2 3; do
        "$binary" "$@" > "$all.run" 2>/dev/null
        cat "$all.run" >> "$all"
        tail -n 1 "$all.run" >> "$winners"
    done
    rm -f "$all.run"

    if ! grep -qE '^[a-z-]+ +[A-Za-z0-9-]+ +[0-9]+ ' "$all"; then
        die "redwax-resolve printed no attempt line" \
            "resolve produced no measurable output" 1
    fi

    # Contract: <source> <tier|none> <elapsed_ms> <sanitized evidence>.
    awk '$3 ~ /^[0-9]+$/ { print $1, $3 }' "$all" \
        | sort -k1,1 -k2,2n \
        | awk '{ n[$1]++; v[$1 SUBSEP n[$1]] = $2 }
               END { for (s in n) printf "%-14s runs %d  min %s ms  median %s ms\n",
                         s, n[s], v[s SUBSEP 1], v[s SUBSEP int((n[s] + 1) / 2)] }' \
        | sort

    local tier
    tier="$(awk '{ print $2 }' "$winners" | sort | uniq -c | sort -rn | head -n 1 | awk '{ print $2 }')"
    local best
    best="$(awk '$3 ~ /^[0-9]+$/ { print $3 }' "$winners" | sort -n | head -n 1)"
    echo "RESULT resolve $1 winning tier ${tier:-none}, best ${best:-unknown} ms, gate 800 ms"
}

# -------------------------------------------------------------------- play

measure_play() {
    local binary="$target/debug/redwax-play"
    [[ -x "$binary" ]] || die "redwax-play not built" "play not built" 1
    [[ $# -ge 1 ]] || die "usage: measure.sh play <video_id>..." "play needs a video id" 1

    local out
    out="$(mktemp)"
    trap 'rm -f "$out"' RETURN
    "$binary" --ao null --prefetch --skip-after 8 "$@" > "$out" 2>/dev/null

    if ! grep -qE '(^| )audible( |$)' "$out"; then
        die "redwax-play printed no audible line" \
            "play produced no measurable output" 1
    fi
    grep -E '(^| )audible( |$)' "$out"

    # A leading timestamp is taken as seconds when it has a decimal point and
    # as milliseconds otherwise, because the play contract does not fix a unit.
    awk '
        { unit = ($1 ~ /\./) ? 1000 : 1
          stamp = ($1 ~ /^[0-9]+(\.[0-9]+)?$/) ? $1 * unit : "" }
        /(^| )next( |$)/   { pending = stamp }
        /(^| )audible( |$)/ {
            if (pending != "" && stamp != "") printf "next to audible %.0f ms\n", stamp - pending
            pending = "" }
    ' "$out"

    local count
    count="$(grep -cE '(^| )audible( |$)' "$out")"
    echo "RESULT play $count audible of $# tracks, gate 400 ms prefetched"
}

case "$mode" in
    launch)   measure_launch "$@" ;;
    resolve)  measure_resolve "$@" ;;
    play)     measure_play "$@" ;;
    *)        die "usage: measure.sh launch|resolve|play" "unknown mode ${mode:-none}" 1 ;;
esac
