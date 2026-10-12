#!/usr/bin/env python3
"""Scan staged or pending source and audit history without printing sensitive values."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from urllib.parse import unquote

REPO = Path(__file__).resolve().parent.parent
MIN_GITLEAKS = (8, 30, 1)
MAX_BLOB = 8 * 1024 * 1024
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9.-]+\.[A-Za-z]{2,})")
HOME_PATH = re.compile(r"(?:/home/|/Users/|[A-Za-z]:\\Users\\)([A-Za-z0-9_.-]+)")
MACHINE_PATH = re.compile(
    r"(?:~|\$HOME|%h)?/?Projects/"
    r"redwax(?:-rs)?(?:/|\b)"
    r"|(?:~|\$HOME)?/?\.local/share/fnm/"
    r"node-versions/v\d"
)
IP_FIELD = re.compile(r'''(?i)(?:[?&]ip=|["'](?:ip|ip_address)["']\s*:\s*["'])([0-9a-f:.]+)''')
PERSONAL_FIELD = re.compile(
    r'''(?i)["']?(?:ssn|social_security_number|phone_number|date_of_birth|birth_date|street_address)["']?\s*[:=]\s*["']([^"'\n]{5,})["']'''
)
CARD_FIELD = re.compile(r'''(?i)(?:card_number|credit_card|pan)["']?\s*[:=]\s*["']([\d -]{13,25})["']''')
FIXTURE_PRIVATE_FIELDS = {
    "visitorData", "datasyncId", "delegatedSessionId", "trackingParams",
    "clickTrackingParams", "serviceTrackingParams", "feedbackToken",
    "feedbackTokens", "addToLibraryFeedbackToken", "loggingContext",
    "responseContext", "attestation", "heartbeatToken", "drmSessionId",
    "formEntityKey", "formItemEntityKey", "collectionKey",
    "colorSamplePaletteEntityKey", "themeKey",
}
FIXTURE_CAPABILITY_FIELDS = {
    "continuation": re.compile(r"fixture-continuation-\d{4,}"),
    "playlistSetVideoId": re.compile(r"fixture-entry-\d{4,}"),
    "setVideoId": re.compile(r"fixture-entry-\d{4,}"),
}
EXAMPLE_DOMAINS = {"example.com", "example.org", "example.net"}
DOCUMENTATION_NETWORKS = tuple(
    ipaddress.ip_network(network)
    for network in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24", "2001:db8::/32")
)


class ScanSource:
    """Keep scanner path exclusions from hiding tracked content."""

    def __init__(self, directory: Path):
        self.directory = directory
        self.directory.mkdir(mode=0o700)
        self.origins: dict[str, tuple[str, str]] = {}

    def add(self, path: str, oid: str, data: bytes) -> None:
        alias = hashlib.sha256(os.fsencode(path)).hexdigest() + "-" + oid + ".txt"
        self.origins[alias] = (path, oid)
        (self.directory / alias).write_bytes(data)

    def origin(self, filename: str) -> tuple[str, str]:
        alias = Path(filename).name
        if alias not in self.origins:
            raise RuntimeError("Gitleaks reported an unknown scan input.")
        return self.origins[alias]


def git(*args: str) -> bytes:
    result = subprocess.run(["git", *args], cwd=REPO, capture_output=True)
    if result.returncode:
        raise RuntimeError("Git could not read the scan input. Resolve index or repository errors first.")
    return result.stdout


def index_entries() -> list[tuple[str, str, str]]:
    entries = []
    for record in git("ls-files", "--stage", "-z").split(b"\0"):
        if not record:
            continue
        metadata, name = record.split(b"\t", 1)
        mode, oid, stage = metadata.decode("ascii").split()
        if stage != "0":
            raise RuntimeError("Unmerged index entries cannot be scanned.")
        entries.append((mode, oid, os.fsdecode(name)))
    return entries


def history_entries():
    seen = set()
    for commit in git("rev-list", "--all").splitlines():
        for record in git("ls-tree", "-rz", "--full-tree", commit.decode("ascii")).split(b"\0"):
            if not record:
                continue
            metadata, name = record.split(b"\t", 1)
            mode, _kind, oid = metadata.decode("ascii").split()
            path = os.fsdecode(name)
            entry = (mode, oid, path)
            if entry not in seen:
                seen.add(entry)
                yield entry


def worktree_entries() -> list[tuple[str, str, str, bytes]]:
    entries = []
    records = git("ls-files", "--cached", "--others", "--exclude-standard", "-z")
    for record in records.split(b"\0"):
        if not record:
            continue
        path = os.fsdecode(record)
        source_path = REPO / path
        try:
            status = source_path.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISREG(status.st_mode):
            mode = "100755" if status.st_mode & stat.S_IXUSR else "100644"
            data = source_path.read_bytes()
            process = subprocess.run(
                ["git", "hash-object", "--stdin"],
                cwd=REPO,
                input=data,
                capture_output=True,
            )
            if process.returncode:
                raise RuntimeError("Git could not hash a worktree scan input.")
            oid = process.stdout.decode("ascii").strip()
        else:
            mode = "120000" if stat.S_ISLNK(status.st_mode) else "unsupported"
            oid = "worktree"
            data = b""
        entries.append((mode, oid, path, data))
    return entries


def private_path(path: str) -> bool:
    parts = PurePosixPath(path).parts
    name = parts[-1].lower()
    if name.startswith(".env") and name not in {".env.example", ".env.sample", ".env.template"}:
        return True
    if name in {
        "browser.json", "session.json", "cookies.txt", "cookies.json", "cookies.sqlite",
        "credentials", "credentials.json", "auth.json", "id_rsa", "id_ed25519",
        ".netrc", ".npmrc", ".pypirc", "redwax-private-identifiers",
    } or name.endswith((".pem", ".key", ".p12", ".pfx", ".sqlite", ".db", ".har", ".log")):
        return True
    return "raw" in parts and "fixtures" in parts


def valid_card(value: str) -> bool:
    digits = [int(c) for c in value if c.isdigit()]
    if not 13 <= len(digits) <= 19:
        return False
    return sum((n * 2 - 9 if n * 2 > 9 else n * 2) if i % 2 else n
               for i, n in enumerate(reversed(digits))) % 10 == 0


def personal_findings(text: str, path: str, identities: list[str]) -> list[tuple[int, str]]:
    found = set()
    decoded = unquote(text)
    for match in EMAIL.finditer(decoded):
        domain = match.group(1).lower()
        if (
            any(domain == example or domain.endswith("." + example) for example in EXAMPLE_DOMAINS)
            or domain.endswith((".invalid", ".test", ".example"))
            or re.fullmatch(r"\d+\.(?:png|jpg|jpeg|webp)", domain)
        ):
            continue
        found.add((decoded.count("\n", 0, match.start()) + 1, "personal-email"))
    for match in HOME_PATH.finditer(decoded):
        if match.group(1).lower() not in {"user", "test", "example", "username"}:
            found.add((decoded.count("\n", 0, match.start()) + 1, "personal-home-path"))
    if not path.endswith(".md"):
        for match in MACHINE_PATH.finditer(decoded):
            found.add((decoded.count("\n", 0, match.start()) + 1, "machine-coupled-path"))
    for match in IP_FIELD.finditer(decoded):
        try:
            address = ipaddress.ip_address(match.group(1))
        except ValueError:
            continue
        documented = any(
            address.version == network.version and address in network
            for network in DOCUMENTATION_NETWORKS
        )
        if not address.is_loopback and not address.is_unspecified and not documented:
            found.add((decoded.count("\n", 0, match.start()) + 1, "account-ip-address"))
    for match in PERSONAL_FIELD.finditer(decoded):
        if match.group(1).lower() not in {"example", "redacted", "placeholder"}:
            found.add((decoded.count("\n", 0, match.start()) + 1, "personal-data-field"))
    for match in CARD_FIELD.finditer(decoded):
        if valid_card(match.group(1)):
            found.add((decoded.count("\n", 0, match.start()) + 1, "payment-card-number"))
    for identity in identities:
        for match in re.finditer(re.escape(identity), decoded, re.IGNORECASE):
            found.add((decoded.count("\n", 0, match.start()) + 1, "private-account-identifier"))
    if path.startswith("fixtures/") and path.endswith(".json"):
        node = json.loads(text)
        pending = [node]
        while pending:
            value = pending.pop()
            if isinstance(value, dict):
                if any(key in FIXTURE_PRIVATE_FIELDS and item not in (None, "", [], {})
                       for key, item in value.items()):
                    found.add((1, "fixture-account-metadata"))
                for key, pattern in FIXTURE_CAPABILITY_FIELDS.items():
                    if key in value and value[key] not in (None, ""):
                        if key == "continuation" and isinstance(value[key], dict):
                            continue
                        if not isinstance(value[key], str) or not pattern.fullmatch(value[key]):
                            found.add((1, "fixture-account-capability"))
                pending.extend(value.values())
            elif isinstance(value, list):
                pending.extend(value)
    return sorted(found)


def load_identities() -> list[str]:
    common = Path(os.fsdecode(git("rev-parse", "--path-format=absolute", "--git-common-dir")).strip())
    path = common / "redwax-private-identifiers"
    if not os.path.lexists(path):
        print("Private account identifiers are not configured. Add one identifier per line to the Git common directory file redwax-private-identifiers with mode 0600.", file=sys.stderr)
        return []
    if path.is_symlink() or not path.is_file() or path.stat().st_mode & 0o777 != 0o600:
        raise RuntimeError("The private identifier file must be a regular file with mode 0600.")
    values = [line.strip() for line in path.read_text().splitlines() if line.strip() and not line.startswith("#")]
    if not values or any(len(value) < 3 for value in values):
        raise RuntimeError("The private identifier file needs identifiers of at least three characters.")
    return values


def report(path: str, line: int, rule: str, scope: str) -> None:
    print(f"{scope}: {json.dumps(path, ensure_ascii=True)}:{line}: {rule} [value redacted]", file=sys.stderr)


def inspect_regular_data(
    path: str, oid: str, data: bytes, identities: list[str], scope: str,
    snapshot: Path | None = None, source: ScanSource | None = None,
) -> int:
    findings = 0
    parts = PurePosixPath(path).parts
    if len(data) > MAX_BLOB:
        report(path, 1, "oversized-file-needs-review", scope)
        return findings + 1
    try:
        text = data.decode("utf-8")
        if "\0" in text:
            raise UnicodeError()
    except UnicodeError:
        report(path, 1, "opaque-file-needs-review", scope)
        return findings + 1
    for line, rule in personal_findings(text, path, identities):
        report(path, line, rule, scope)
        findings += 1
    if source is not None:
        source.add(path, oid, data)
    if snapshot is not None:
        destination = snapshot.joinpath(*parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    return findings


def inspect_entries(
    entries, identities: list[str], scope: str,
    snapshot: Path | None = None, source: ScanSource | None = None,
) -> int:
    findings = 0
    for mode, oid, path in entries:
        parts = PurePosixPath(path).parts
        if not parts or path.startswith("/") or any(part in {"..", ".git"} for part in parts):
            raise RuntimeError("Unsafe repository path cannot be scanned.")
        if mode not in {"100644", "100755"}:
            report(path, 1, "symlink-or-submodule-needs-review", scope)
            findings += 1
            continue
        if private_path(path):
            report(path, 1, "private-artifact-path", scope)
            findings += 1
        size = int(git("cat-file", "-s", oid))
        if size > MAX_BLOB:
            report(path, 1, "oversized-file-needs-review", scope)
            findings += 1
            continue
        data = git("cat-file", "blob", oid)
        findings += inspect_regular_data(
            path, oid, data, identities, scope, snapshot, source
        )
    return findings


def inspect_worktree_entries(
    entries: list[tuple[str, str, str, bytes]], identities: list[str],
    source: ScanSource,
) -> int:
    findings = 0
    for mode, oid, path, data in entries:
        parts = PurePosixPath(path).parts
        if not parts or path.startswith("/") or any(part in {"..", ".git"} for part in parts):
            raise RuntimeError("Unsafe repository path cannot be scanned.")
        if mode not in {"100644", "100755"}:
            report(path, 1, "symlink-or-special-file-needs-review", "worktree")
            findings += 1
            continue
        if private_path(path):
            report(path, 1, "private-artifact-path", "worktree")
            findings += 1
        findings += inspect_regular_data(
            path, oid, data, identities, "worktree", source=source
        )
    return findings


def gitleaks(
    executable: str, config: Path, target: Path, work: Path,
    history: bool = False, source: ScanSource | None = None,
    source_scope: str = "index",
) -> int:
    report_file = work / ("history.json" if history else "index.json")
    ignore_file = work / "empty-ignore"
    ignore_file.touch(mode=0o600)
    command = [
        executable, "dir", str(target),
        "--config", str(config), "--redact=100", "--no-banner", "--no-color",
        "--ignore-gitleaks-allow", "--gitleaks-ignore-path", str(ignore_file),
        "--max-decode-depth=5", "--max-archive-depth=5", "--max-target-megabytes=0",
        "--report-format=json", "--report-path", str(report_file),
    ]
    result = subprocess.run(command, cwd=work, capture_output=True)
    if result.returncode not in (0, 1) or not report_file.exists():
        raise RuntimeError("Gitleaks failed. Check the installed version and scanner configuration.")
    records = json.loads(report_file.read_text())
    if not isinstance(records, list) or any(
        not isinstance(record, dict)
        or not isinstance(record.get("File"), str)
        or not isinstance(record.get("RuleID"), str)
        or not isinstance(record.get("StartLine", 1), int)
        or not isinstance(record.get("Commit", ""), str)
        for record in records
    ):
        raise RuntimeError("Gitleaks produced an invalid report.")
    if result.returncode == 1 and not records:
        raise RuntimeError("Gitleaks failed without producing findings.")
    for record in records:
        path = record["File"]
        if source is not None:
            path, oid = source.origin(path)
            scope = f"history blob {oid[:12]}" if history else source_scope
        else:
            path = os.path.relpath(path, target) if os.path.isabs(path) else path
            scope = "history" if history else "index"
        report(path, record.get("StartLine", 1), record["RuleID"], scope)
    return len(records)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", action="store_true", help="also inspect all reachable local Git history")
    parser.add_argument(
        "--worktree",
        action="store_true",
        help="inspect tracked and untracked public files instead of the index",
    )
    args = parser.parse_args()
    executable = shutil.which("gitleaks")
    if executable is None:
        raise RuntimeError("Gitleaks 8.30.1 or newer is required. Install gitleaks on PATH and rerun this command.")
    executable = str(Path(executable).resolve())
    version = subprocess.run([executable, "version"], capture_output=True, text=True, check=True)
    match = re.search(r"(\d+)\.(\d+)\.(\d+)", version.stdout)
    if not match or tuple(map(int, match.groups())) < MIN_GITLEAKS:
        raise RuntimeError("Gitleaks 8.30.1 or newer is required.")
    if args.history and git("rev-parse", "--is-shallow-repository").strip() == b"true":
        raise RuntimeError("A history audit requires a complete clone.")
    identities = load_identities()
    with tempfile.TemporaryDirectory(prefix="redwax-private-") as temporary:
        work = Path(temporary)
        if args.worktree:
            source = ScanSource(work / "scan-worktree")
            entries = worktree_entries()
            count = inspect_worktree_entries(entries, identities, source)
            config = REPO / ".gitleaks.toml"
            if not config.is_file():
                raise RuntimeError("The worktree needs .gitleaks.toml for a privacy scan.")
        else:
            snapshot = work / "index"
            snapshot.mkdir(mode=0o700)
            source = ScanSource(work / "scan-index")
            entries = index_entries()
            count = inspect_entries(entries, identities, "index", snapshot, source)
            config = snapshot / ".gitleaks.toml"
            if not config.is_file():
                raise RuntimeError("Stage .gitleaks.toml before committing with the privacy guard.")
        source_scope = "worktree" if args.worktree else "index"
        count += gitleaks(
            executable, config, source.directory, work,
            source=source, source_scope=source_scope,
        )
        if args.history:
            history_source = ScanSource(work / "scan-history")
            count += inspect_entries(history_entries(), identities, "history", source=history_source)
            count += gitleaks(
                executable, config, history_source.directory, work,
                history=True, source=history_source,
            )
        if args.worktree:
            if worktree_entries() != entries:
                raise RuntimeError("The worktree changed during the scan. Retry with stable files.")
        elif index_entries() != entries:
            raise RuntimeError("The index changed during the scan. Retry with a stable index.")
    if count:
        print(f"Blocked: {count} sensitive-data or unscannable-input finding(s).", file=sys.stderr)
        return 1
    print("Privacy guard found no matching sensitive-data patterns in the scanned input.")
    return 0


if __name__ == "__main__":
    os.umask(0o077)
    try:
        sys.exit(main())
    except RuntimeError as error:
        print(f"Privacy guard could not complete. Commit blocked. {error}", file=sys.stderr)
        sys.exit(2)
    except (OSError, ValueError, subprocess.SubprocessError):
        print("Privacy guard could not complete. Commit blocked. Check setup and index inputs.", file=sys.stderr)
        sys.exit(2)
