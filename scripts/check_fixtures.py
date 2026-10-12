#!/usr/bin/env python3
"""Refuse a fixture that still names the account it was captured from.

The fixtures are real YouTube Music responses, so they arrive carrying whoever
captured them: a handle, a channel id, and the ids of that account's own
playlists. None of it is a credential and none of it is what the parser tests
are about, but this repository is public and that data belongs to a person.

Two checks, because they answer different questions:

Shape. Emails and cookie values are never legitimate in a response we keep,
whoever captured it, so they are refused with no argument needed.

Identity. An artist handle is public and an account handle is not, and nothing
in the file tells them apart. So the identity to look for is passed in by
whoever knows it, which is `capture_fixtures.py` reading the live session, or
a person running this by hand before a commit.

    ./scripts/check_fixtures.py
    ./scripts/check_fixtures.py --handle example-user --channel UCabc... --playlist PLdef...

Exit status is 1 when anything is found, so this works as a gate.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

from check_private import personal_findings

REPO = pathlib.Path(__file__).resolve().parent.parent

EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")

# The names Google gives its session cookies, followed by a value. A fixture is
# a response body and never carries one, so a hit is a capture bug.
COOKIE_RE = re.compile(
    r"(?:SAPISID|APISID|HSID|SSID|SID|__Secure-[13]PSID(?:TS|CC)?|__Secure-[13]PAPISID)"
    r"\s*[=:]\s*\"?[A-Za-z0-9_/+.-]{20,}"
)

# Thumbnail urls end in a size suffix like `=w60-h60` or `...@1200.png`, which
# looks like a handle and is not one.
IMAGE_SUFFIX_RE = re.compile(r"@\d+(?:\.[a-z]{3,4})?$")


def shape_findings(text: str) -> list[str]:
    """What is never legitimate in a kept response, whoever captured it."""
    found = []
    for email in sorted(set(EMAIL_RE.findall(text))):
        if IMAGE_SUFFIX_RE.search(email):
            continue
        found.append("email [value redacted]")
    for cookie in sorted(set(COOKIE_RE.findall(text))):
        found.append("cookie [value redacted]")
    return found


def identity_findings(text: str, wanted: list[str]) -> list[str]:
    """The capturing account's own identifiers, which only the caller knows."""
    return ["identity [value redacted]" for value in wanted if value and value.lower() in text.lower()]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("directory", nargs="?", default=str(REPO / "fixtures"))
    parser.add_argument("--handle", action="append", default=[], help="account handle, without the @")
    parser.add_argument("--channel", action="append", default=[], help="account channel id")
    parser.add_argument("--playlist", action="append", default=[], help="a playlist id the account owns")
    args = parser.parse_args()

    wanted = args.handle + args.channel + args.playlist
    fixtures = sorted(pathlib.Path(args.directory).glob("*.json"))
    if not fixtures:
        print(f"no fixtures under {args.directory}", file=sys.stderr)
        return 1

    bad = 0
    for path in fixtures:
        text = path.read_text()
        try:
            found = shape_findings(text) + identity_findings(text, wanted)
            found.extend(rule + " [value redacted]" for _, rule in personal_findings(text, f"fixtures/{path.name}", wanted))
        except ValueError:
            found = ["invalid fixture JSON"]
        if found:
            bad += 1
            print(f"{path.name}:")
            for line in found:
                print(f"  {line}")
    if bad:
        print(f"\n{bad} fixture(s) still carry something private.", file=sys.stderr)
        return 1
    checked = "shape" if not wanted else f"shape and {len(wanted)} identifier(s)"
    print(f"{len(fixtures)} fixtures checked for {checked}, nothing found")
    return 0


if __name__ == "__main__":
    sys.exit(main())
