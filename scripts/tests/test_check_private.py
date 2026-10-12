import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]


def load_script(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Could not load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CHECK_PRIVATE = load_script(
    "check_private_under_test", ROOT / "scripts/check_private.py"
)
CHECK_FIXTURES = load_script(
    "check_fixtures_under_test", ROOT / "scripts/check_fixtures.py"
)


def run_git(repo: Path, *args: str, input_data: bytes | None = None) -> bytes:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        input=input_data,
        capture_output=True,
        check=True,
    )
    return result.stdout


class RepositoryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="redwax-private-test-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "repo"
        self.repo.mkdir()
        run_git(self.repo, "init", "--quiet")
        run_git(self.repo, "config", "user.name", "Redwax Test")
        run_git(self.repo, "config", "user.email", "redwax@example.invalid")
        run_git(self.repo, "config", "commit.gpgsign", "false")
        run_git(self.repo, "config", "core.hooksPath", str(self.repo / ".git/hooks"))
        repo_patch = mock.patch.object(CHECK_PRIVATE, "REPO", self.repo)
        repo_patch.start()
        self.addCleanup(repo_patch.stop)

    def write(self, name: str, data: str | bytes) -> Path:
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(data, str):
            path.write_text(data)
        else:
            path.write_bytes(data)
        return path

    def stage(self, name: str, data: str | bytes) -> Path:
        path = self.write(name, data)
        run_git(self.repo, "add", "--", name)
        return path

    def hash_blob(self, data: bytes) -> str:
        return run_git(
            self.repo, "hash-object", "-w", "--stdin", input_data=data
        ).decode("ascii").strip()


class IndexSnapshotTests(RepositoryTest):
    def test_snapshot_uses_staged_blob_during_partial_staging(self) -> None:
        config = self.stage(".gitleaks.toml", "[extend]\nuseDefault = true\n")
        notes = self.stage("notes.txt", "content selected for commit\n")
        unstaged_private_email = "owner" + "@private-domain.dev"
        config.write_text("[allowlist]\npaths = ['.*']\n")
        notes.write_text(f"unstaged edit {unstaged_private_email}\n")
        snapshot = Path(self.temporary.name) / "snapshot"

        findings = CHECK_PRIVATE.inspect_entries(
            CHECK_PRIVATE.index_entries(), [], "index", snapshot
        )

        self.assertEqual(findings, 0)
        self.assertEqual(
            (snapshot / "notes.txt").read_text(), "content selected for commit\n"
        )
        self.assertEqual(
            (snapshot / ".gitleaks.toml").read_text(),
            "[extend]\nuseDefault = true\n",
        )
        self.assertNotIn(unstaged_private_email, (snapshot / "notes.txt").read_text())

    def test_staged_deletion_and_rename_match_index_paths(self) -> None:
        self.stage("deleted.txt", "old content\n")
        self.stage("before.txt", "renamed content\n")
        run_git(self.repo, "commit", "--quiet", "-m", "initial")

        run_git(self.repo, "rm", "--quiet", "--", "deleted.txt")
        run_git(self.repo, "mv", "before.txt", "after.txt")

        paths = {path for _, _, path in CHECK_PRIVATE.index_entries()}
        self.assertEqual(paths, {"after.txt"})

    def test_initial_repository_has_an_index_and_no_history(self) -> None:
        self.stage("first.txt", "first staged content\n")

        self.assertEqual(
            [path for _, _, path in CHECK_PRIVATE.index_entries()], ["first.txt"]
        )
        self.assertEqual(list(CHECK_PRIVATE.history_entries()), [])

    def test_unmerged_index_is_rejected(self) -> None:
        base = self.hash_blob(b"base\n")
        ours = self.hash_blob(b"ours\n")
        theirs = self.hash_blob(b"theirs\n")
        records = (
            f"100644 {base} 1\tconflicted.txt\n"
            f"100644 {ours} 2\tconflicted.txt\n"
            f"100644 {theirs} 3\tconflicted.txt\n"
        ).encode("ascii")
        run_git(self.repo, "update-index", "--index-info", input_data=records)

        with self.assertRaisesRegex(RuntimeError, "Unmerged index"):
            CHECK_PRIVATE.index_entries()


class WorktreeSnapshotTests(RepositoryTest):
    def test_worktree_snapshot_includes_pending_public_files(self) -> None:
        self.stage(".gitignore", "ignored/\n")
        tracked = self.stage("src/tracked.txt", "staged content\n")
        deleted = self.stage("src/deleted.txt", "deleted content\n")
        tracked.write_text("modified content\n")
        deleted.unlink()
        self.write("docs/untracked.txt", "untracked content\n")
        self.write("ignored/private.txt", "ignored content\n")

        entries = {
            path: data
            for _mode, _oid, path, data in CHECK_PRIVATE.worktree_entries()
        }

        self.assertEqual(entries["src/tracked.txt"], b"modified content\n")
        self.assertEqual(entries["docs/untracked.txt"], b"untracked content\n")
        self.assertNotIn("src/deleted.txt", entries)
        self.assertNotIn("ignored/private.txt", entries)

    def test_pending_personal_and_fixture_data_is_rejected_without_disclosure(self) -> None:
        identity = "owned-" + "channel-id-123"
        email = "owner" + "@private-domain.dev"
        self.write("src/settings.txt", "/home/" + "alice/project\n" + identity)
        self.write(
            "fixtures/pending.json",
            json.dumps({"feedback" + "Token": "fixture-value", "email": email}),
        )
        self.write(".env", "PUBLIC_NAME=redwax\n")
        run_git(self.repo, "add", "-f", "--", ".env")
        source = CHECK_PRIVATE.ScanSource(Path(self.temporary.name) / "scan")
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.inspect_worktree_entries(
                CHECK_PRIVATE.worktree_entries(), [identity], source
            )

        output = error.getvalue()
        self.assertGreaterEqual(findings, 4)
        for rule in [
            "personal-home-path",
            "private-account-identifier",
            "personal-email",
            "fixture-account-metadata",
            "private-artifact-path",
        ]:
            self.assertIn(rule, output)
        self.assertNotIn(identity, output)
        self.assertNotIn(email, output)

    def test_untracked_browser_credential_reaches_gitleaks(self) -> None:
        executable = shutil.which("gitleaks")
        if executable is None:
            self.fail("Gitleaks 8.30.1 or newer is required for privacy tests.")
        shutil.copy2(ROOT / ".gitleaks.toml", self.repo / ".gitleaks.toml")
        credential = "SAP" + "ISID=" + "abcdefghijklmnopqrst"
        self.write("crates/pending.rs", credential + "\n")
        source = CHECK_PRIVATE.ScanSource(Path(self.temporary.name) / "scan")
        CHECK_PRIVATE.inspect_worktree_entries(
            CHECK_PRIVATE.worktree_entries(), [], source
        )
        work = Path(self.temporary.name) / "gitleaks"
        work.mkdir()
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.gitleaks(
                executable,
                self.repo / ".gitleaks.toml",
                source.directory,
                work,
                source=source,
                source_scope="worktree",
            )

        self.assertGreater(findings, 0)
        self.assertIn(
            'worktree: "crates/pending.rs":1: redwax-google-cookie [value redacted]',
            error.getvalue(),
        )
        self.assertNotIn(credential, error.getvalue())


class InputInspectionTests(RepositoryTest):
    def test_unscannable_entry_types_and_contents_are_findings(self) -> None:
        link = self.hash_blob(b"target")
        binary = self.hash_blob(b"a\0b")
        oversized = self.hash_blob(b"four")
        entries = [
            ("120000", link, "link"),
            ("160000", "0" * 40, "vendor"),
            ("100644", binary, "binary.dat"),
            ("100644", oversized, "large.txt"),
        ]
        error = io.StringIO()

        with mock.patch.object(CHECK_PRIVATE, "MAX_BLOB", 3), contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.inspect_entries(entries, [], "index")

        self.assertEqual(findings, 4)
        output = error.getvalue()
        self.assertEqual(output.count("symlink-or-submodule-needs-review"), 2)
        self.assertIn("opaque-file-needs-review", output)
        self.assertIn("oversized-file-needs-review", output)

    def test_private_artifact_path_is_a_finding(self) -> None:
        oid = self.hash_blob(b"synthetic fixture\n")
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.inspect_entries(
                [("100644", oid, "capture/cookies.txt")], [], "index"
            )

        self.assertEqual(findings, 1)
        self.assertIn("private-artifact-path", error.getvalue())

    def test_personal_data_checks_cover_distinct_data_classes(self) -> None:
        scenarios = {
            "personal-email": "contact=" + "owner" + "@private-domain.dev",
            "personal-home-path": "cache=" + "/home/" + "alice/config",
            "machine-coupled-path": "python=$HOME/" + "Projects/" + "redwax/.venv/bin/python",
            "account-ip-address": '{"i' + 'p":"8.8.8.8"}',
            "personal-data-field": 'phone_' + 'number="020 7946 0958"',
            "payment-card-number": 'card_' + 'number="4111 1111 1111 1111"',
        }

        for expected_rule, text in scenarios.items():
            with self.subTest(expected_rule):
                rules = {
                    rule
                    for _, rule in CHECK_PRIVATE.personal_findings(
                        text, "src/data.txt", []
                    )
                }
                self.assertIn(expected_rule, rules)

    def test_documented_example_values_are_exempt(self) -> None:
        values = [
            "person@example.com",
            "person@example.org",
            "person@example.net",
            "person@docs." + "example.com",
            "person@documentation.invalid",
            "person@documentation.test",
            "person@documentation.example",
            "/home/user/project",
            '{"ip":"127.0.0.1"}',
            '{"' + 'ip":"192.0.2.1"}',
            '{"' + 'ip":"198.51.100.2"}',
            '{"' + 'ip":"203.0.113.3"}',
            '{"' + 'ip":"2001:db8::1"}',
            'street_address="placeholder"',
            "artist@1200.png",
        ]

        for value in values:
            with self.subTest(value):
                self.assertEqual(
                    CHECK_PRIVATE.personal_findings(value, "src/data.txt", []), []
                )

        historical_path = "$HOME/" + "Projects/" + "redwax/.venv/bin/python"
        self.assertEqual(
            CHECK_PRIVATE.personal_findings(historical_path, "docs/audit.md", []),
            [],
        )

    def test_nested_fixture_response_token_is_rejected(self) -> None:
        key = "addToLibrary" + "FeedbackToken"
        text = json.dumps({"nested": [{key: "fixture-value"}]})

        self.assertEqual(
            CHECK_PRIVATE.personal_findings(text, "fixtures/example.json", []),
            [(1, "fixture-account-metadata")],
        )

    def test_finding_output_never_contains_the_matched_value(self) -> None:
        identifier = "private-" + "channel-42"
        oid = self.hash_blob(f"selected={identifier}\n".encode())
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.inspect_entries(
                [("100644", oid, "settings.txt")], [identifier], "index"
            )

        self.assertEqual(findings, 1)
        self.assertNotIn(identifier, error.getvalue())
        self.assertIn("private-account-identifier [value redacted]", error.getvalue())


class LocalIdentityTests(RepositoryTest):
    def identity_path(self) -> Path:
        return Path(
            os.fsdecode(
                run_git(
                    self.repo,
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "redwax-private-identifiers",
                )
            ).strip()
        )

    def test_missing_identity_file_warns_and_keeps_generic_checks_active(self) -> None:
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            identities = CHECK_PRIVATE.load_identities()

        self.assertEqual(identities, [])
        self.assertIn("not configured", error.getvalue())

    def test_identity_file_accepts_only_private_permissions(self) -> None:
        path = self.identity_path()
        path.write_text("# local values\n\nprivate-handle\n")
        path.chmod(0o600)

        self.assertEqual(CHECK_PRIVATE.load_identities(), ["private-handle"])

        path.chmod(0o640)
        with self.assertRaisesRegex(RuntimeError, "mode 0600"):
            CHECK_PRIVATE.load_identities()

    def test_identity_file_rejects_a_symlink(self) -> None:
        path = self.identity_path()
        target = Path(self.temporary.name) / "identities"
        target.write_text("private-handle\n")
        target.chmod(0o600)
        path.symlink_to(target)

        with self.assertRaisesRegex(RuntimeError, "regular file"):
            CHECK_PRIVATE.load_identities()

    def test_identity_path_must_be_a_regular_file(self) -> None:
        path = self.identity_path()
        path.mkdir(mode=0o700)
        path.chmod(0o600)
        self.addCleanup(path.chmod, 0o700)

        with self.assertRaisesRegex(RuntimeError, "regular file"):
            CHECK_PRIVATE.load_identities()


class LegacyFixtureReporterTests(unittest.TestCase):
    def test_findings_name_categories_without_disclosing_values(self) -> None:
        email = "owner" + "@private-domain.dev"
        cookie = "SAP" + "ISID=" + "abcdefghijklmnopqrst"
        identity = "private-" + "account-identifier"
        findings = CHECK_FIXTURES.shape_findings(
            f"{email}\n{cookie}\n"
        ) + CHECK_FIXTURES.identity_findings(identity, [identity])
        rendered = "\n".join(findings)

        self.assertEqual(
            findings,
            [
                "email [value redacted]",
                "cookie [value redacted]",
                "identity [value redacted]",
            ],
        )
        self.assertNotIn(email, rendered)
        self.assertNotIn(cookie, rendered)
        self.assertNotIn(identity, rendered)


class GitleaksTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="redwax-gitleaks-test-")
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name) / "work"
        self.target = Path(self.temporary.name) / "index"
        self.work.mkdir()
        self.target.mkdir()
        self.config = self.target / ".gitleaks.toml"
        self.config.write_text("[extend]\nuseDefault = true\n")

    def scanner_result(self, returncode: int, records: object | None):
        def run(command, **kwargs):
            if records is not None:
                report = Path(command[command.index("--report-path") + 1])
                report.write_text(json.dumps(records))
            return SimpleNamespace(returncode=returncode, stdout=b"", stderr=b"")

        return run

    def test_index_scanner_uses_private_snapshot_and_strict_options(self) -> None:
        record = {
            "File": str(self.target / "nested/file.txt"),
            "StartLine": 7,
            "RuleID": "synthetic-rule",
        }
        error = io.StringIO()
        scanner = mock.Mock(side_effect=self.scanner_result(1, [record]))

        with mock.patch.object(CHECK_PRIVATE.subprocess, "run", scanner), contextlib.redirect_stderr(error):
            count = CHECK_PRIVATE.gitleaks(
                "/tools/gitleaks", self.config, self.target, self.work
            )

        self.assertEqual(count, 1)
        command = scanner.call_args.args[0]
        self.assertEqual(command[:3], ["/tools/gitleaks", "dir", str(self.target)])
        self.assertIn("--ignore-gitleaks-allow", command)
        self.assertIn("--max-decode-depth=5", command)
        self.assertIn("--max-archive-depth=5", command)
        self.assertIn("--max-target-megabytes=0", command)
        ignore_path = Path(command[command.index("--gitleaks-ignore-path") + 1])
        self.assertEqual(ignore_path, self.work / "empty-ignore")
        self.assertEqual(ignore_path.read_bytes(), b"")
        self.assertEqual(stat.S_IMODE(ignore_path.stat().st_mode), 0o600)
        self.assertEqual(scanner.call_args.kwargs["cwd"], self.work)
        self.assertIn('index: "nested/file.txt":7: synthetic-rule [value redacted]', error.getvalue())

    def test_scanner_process_failure_is_rejected(self) -> None:
        scanner = mock.Mock(side_effect=self.scanner_result(2, []))

        with mock.patch.object(CHECK_PRIVATE.subprocess, "run", scanner):
            with self.assertRaisesRegex(RuntimeError, "Gitleaks failed"):
                CHECK_PRIVATE.gitleaks(
                    "/tools/gitleaks", self.config, self.target, self.work
                )

    def test_missing_scanner_report_is_rejected(self) -> None:
        scanner = mock.Mock(side_effect=self.scanner_result(0, None))

        with mock.patch.object(CHECK_PRIVATE.subprocess, "run", scanner):
            with self.assertRaisesRegex(RuntimeError, "Gitleaks failed"):
                CHECK_PRIVATE.gitleaks(
                    "/tools/gitleaks", self.config, self.target, self.work
                )

    def test_finding_exit_without_records_is_rejected(self) -> None:
        scanner = mock.Mock(side_effect=self.scanner_result(1, []))

        with mock.patch.object(CHECK_PRIVATE.subprocess, "run", scanner):
            with self.assertRaisesRegex(RuntimeError, "without producing findings"):
                CHECK_PRIVATE.gitleaks(
                    "/tools/gitleaks", self.config, self.target, self.work
                )

    def test_malformed_scanner_reports_are_rejected(self) -> None:
        malformed_reports = [
            {"File": "source.txt", "RuleID": "synthetic-rule"},
            [{"File": "source.txt", "RuleID": "synthetic-rule", "StartLine": "1"}],
        ]

        for report in malformed_reports:
            with self.subTest(report):
                scanner = mock.Mock(side_effect=self.scanner_result(0, report))
                with mock.patch.object(CHECK_PRIVATE.subprocess, "run", scanner):
                    with self.assertRaisesRegex(RuntimeError, "invalid report"):
                        CHECK_PRIVATE.gitleaks(
                            "/tools/gitleaks", self.config, self.target, self.work
                        )

    def test_missing_scanner_dependency_blocks_main(self) -> None:
        with mock.patch.object(CHECK_PRIVATE.shutil, "which", return_value=None), mock.patch.object(
            sys, "argv", ["check_private.py"]
        ):
            with self.assertRaisesRegex(RuntimeError, "Gitleaks 8.30.1"):
                CHECK_PRIVATE.main()


class GitleaksIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(
            prefix="redwax-gitleaks-integration-"
        )
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.executable = shutil.which("gitleaks")
        if self.executable is None:
            self.fail(
                "Gitleaks is required for privacy integration tests. "
                "Install version 8.30.1 or newer."
            )

    def test_project_policy_detects_custom_and_default_credentials(self) -> None:
        cookie_value = "".join(chr(ord("a") + offset) for offset in range(20))
        github_suffix = "".join(
            chr(ord("A") + (offset * 7) % 26) for offset in range(36)
        )
        scenarios = [
            (
                "google-cookie",
                "SAP" + "ISID=" + cookie_value,
                "redwax-google-cookie",
            ),
            (
                "netscape-cookie",
                ".example.com\tTRUE\t/\tFALSE\t2147483647\t"
                + "SI"
                + "D\t"
                + cookie_value,
                "redwax-netscape-cookie",
            ),
            (
                "password",
                "pass" + 'word="' + cookie_value + '"',
                "redwax-password-assignment",
            ),
            (
                "tuple-cookie",
                '"cookie"' + ', "session=' + cookie_value + '"',
                "redwax-cookie-header",
            ),
            (
                "github-token",
                "gh" + "p_" + github_suffix,
                "github-pat",
            ),
        ]

        for name, credential, expected_rule in scenarios:
            with self.subTest(name):
                source = self.root / name / "source"
                work = self.root / name / "work"
                source.mkdir(parents=True)
                work.mkdir()
                config = source / ".gitleaks.toml"
                shutil.copy2(ROOT / ".gitleaks.toml", config)
                (source / "input.txt").write_text(credential + "\n")
                error = io.StringIO()

                with contextlib.redirect_stderr(error):
                    findings = CHECK_PRIVATE.gitleaks(
                        self.executable, config, source, work
                    )

                self.assertGreater(findings, 0)
                self.assertIn(expected_rule, error.getvalue())
                self.assertNotIn(credential, error.getvalue())

    def test_adjacent_header_names_are_not_a_cookie_value(self) -> None:
        source = self.root / "header-names" / "source"
        work = self.root / "header-names" / "work"
        source.mkdir(parents=True)
        work.mkdir()
        config = source / ".gitleaks.toml"
        shutil.copy2(ROOT / ".gitleaks.toml", config)
        (source / "input.txt").write_text('"cookie"' + ', "authorization"\n')

        self.assertEqual(
            CHECK_PRIVATE.gitleaks(self.executable, config, source, work),
            0,
        )

    def test_scanner_alias_prevents_svg_path_exclusion(self) -> None:
        work = self.root / "svg-work"
        work.mkdir()
        config = self.root / "svg-policy.toml"
        shutil.copy2(ROOT / ".gitleaks.toml", config)
        source = CHECK_PRIVATE.ScanSource(self.root / "svg-source")
        credential = "pass" + 'word="' + "abcdefghijklmnopqrst" + '"'
        source.add("art/notes.svg", "1" * 40, credential.encode())
        error = io.StringIO()

        with contextlib.redirect_stderr(error):
            findings = CHECK_PRIVATE.gitleaks(
                self.executable,
                config,
                source.directory,
                work,
                source=source,
            )

        output = error.getvalue()
        self.assertGreater(findings, 0)
        self.assertIn('index: "art/notes.svg":1:', output)
        self.assertIn("redwax-password-assignment", output)
        self.assertNotIn(next(iter(source.origins)), output)
        self.assertNotIn(credential, output)

    def test_cookie_vector_exceptions_are_limited_to_their_original_paths(self) -> None:
        work = self.root / "vector-work"
        work.mkdir()
        config = self.root / "vector-policy.toml"
        shutil.copy2(ROOT / ".gitleaks.toml", config)
        source = CHECK_PRIVATE.ScanSource(self.root / "vector-source")
        vectors = [
            "SAPISID" + "HASH 1700000000_" + "fff3f1c7f1891571ba9c0a66cf1432d4b8de3eb2",
            "SAPISID1P" + "HASH 1700000000_" + "59c1bdd52ba359a8150bd6535903a2870cd8a8e4",
            "SAPISID3P" + "HASH 1700000000_" + "5e2448d2438680741ddc5cb5023a91cacd06b7a8",
            "SAPISID" + "HASH 1700000000_" + "6d9ba99351596c63d591fcbe89e83bf065809c0c_u",
            "SAPISID1P" + "HASH 1700000000_" + "01af9442080adb0794424caa3639bcd0d36410e5_u",
            "SAPISID3P" + "HASH 1700000000_" + "dc19465d68772f68d2200bb811adcc0a5cf60162_u",
        ]
        vector_source = "\n".join(vectors).encode()
        source.add(".gitleaks.toml", "2" * 40, config.read_bytes())
        source.add("crates/resolve/src/chain/cookies.rs", "3" * 40, vector_source)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(
                CHECK_PRIVATE.gitleaks(self.executable, config, source.directory, work, source=source),
                0,
            )
        source.add("notes.txt", "4" * 40, vector_source)
        error = io.StringIO()
        with contextlib.redirect_stderr(error):
            self.assertEqual(
                CHECK_PRIVATE.gitleaks(self.executable, config, source.directory, work, source=source),
                len(vectors),
            )
        self.assertIn('"notes.txt"', error.getvalue())
        for vector in vectors:
            self.assertNotIn(vector, error.getvalue())


if __name__ == "__main__":
    unittest.main()
