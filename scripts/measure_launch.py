#!/usr/bin/env python3
"""Time a terminal program from spawn to its first drawn frame.

Plain Python 3 and no dependencies, because `scripts/measure.sh` runs this on
whatever interpreter the machine has.

The clock starts before the spawn and stops when the program has entered the
alternate screen and written 200 bytes more. Entering alone is too early: a
terminal user sees nothing until the frame follows.
"""

from __future__ import annotations

import argparse
import fcntl
import os
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time

ALT_SCREEN = b"\x1b[?1049h"
SETTLE_BYTES = 200
READ_CHUNK = 65536
QUIT_GRACE = 2.0


def set_window_size(fd: int, rows: int, cols: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def measure_once(command: list[str], rows: int, cols: int, timeout: float) -> tuple[float | None, str]:
    """Milliseconds to the first frame, or None and the reason there was none."""
    master, slave = os.openpty()
    set_window_size(slave, rows, cols)
    env = dict(os.environ, TERM="xterm-256color", COLUMNS=str(cols), LINES=str(rows))

    started = time.perf_counter()
    try:
        proc = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
            start_new_session=True,
        )
    except OSError as exc:
        os.close(master)
        os.close(slave)
        return None, str(exc)
    os.close(slave)

    elapsed_ms: float | None = None
    reason = "no alternate screen before the timeout"
    seen = b""
    deadline = started + timeout
    while time.perf_counter() < deadline:
        ready, _, _ = select.select([master], [], [], 0.05)
        if not ready:
            if proc.poll() is not None:
                reason = f"exited with code {proc.returncode} before drawing"
                break
            continue
        try:
            chunk = os.read(master, READ_CHUNK)
        except OSError:
            chunk = b""
        if not chunk:
            code = proc.poll()
            reason = f"exited with code {code} before drawing" if code is not None else "closed its terminal"
            break
        seen += chunk
        start_of_frame = seen.find(ALT_SCREEN)
        if start_of_frame >= 0 and len(seen) - start_of_frame - len(ALT_SCREEN) >= SETTLE_BYTES:
            elapsed_ms = (time.perf_counter() - started) * 1000
            break

    quit_and_reap(proc, master)
    return elapsed_ms, reason


def quit_and_reap(proc: subprocess.Popen[bytes], master: int) -> None:
    try:
        os.write(master, b"q")
    except OSError:
        pass
    try:
        proc.wait(timeout=QUIT_GRACE)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=QUIT_GRACE)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
    os.close(master)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()

    if not os.path.exists(args.command[0]) and "/" in args.command[0]:
        print(f"UNMEASURABLE {args.command[0]} is not built")
        return 3

    times: list[float] = []
    for run in range(1, args.runs + 1):
        elapsed, reason = measure_once(args.command, args.rows, args.cols, args.timeout)
        if elapsed is None:
            print(f"UNMEASURABLE {reason}")
            return 3
        times.append(elapsed)
        print(f"run {run} {elapsed:.1f} ms")

    print(f"MEDIAN {statistics.median(times):.1f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
