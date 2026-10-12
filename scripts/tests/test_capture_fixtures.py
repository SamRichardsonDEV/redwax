import json
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import capture_fixtures as capture


class CapturePrivacyTests(unittest.TestCase):
    def responses(self):
        return {
            "account_menu": {"header": {
                "accountName": {"runs": [{"text": "Fixture Person"}]},
                "channelHandle": {"simpleText": "@fixture-person"},
                "channelId": "UCfixturepersonal",
            }},
            "library_playlists": {"playlistId": "PLfixturepersonal"},
            "_library_identity": [{"playlistId": "PLfixturepersonal"}, {"playlistId": "PLfixturelaterpage"}],
            "home": {"items": ["FIXTURE PERSON", "UCfixturepersonal", "PLfixturepersonal", "PLfixturelaterpage", "@fixture-person"],
                     "link": "https://example.invalid/Fixture%20Person", "token": "opaque"},
        }

    def test_all_responses_lose_menu_identity_and_library_playlist_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = pathlib.Path(directory)
            captures = capture.Capture(None, destination / "raw")
            captures.raw = self.responses()
            capture.write_fixtures(captures, destination)
            text = (destination / "home.json").read_text().lower()
            for private_value in ["fixture person", "fixture%20person", "ucfixturepersonal", "plfixturepersonal", "plfixturelaterpage", "@fixture-person"]:
                self.assertNotIn(private_value, text)
            self.assertIn("fixture", text)
            self.assertFalse((destination / "_library_identity.json").exists())

    def test_missing_identity_refuses_to_replace_existing_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = pathlib.Path(directory)
            (destination / "home.json").write_text("original")
            captures = capture.Capture(None, destination / "raw")
            captures.raw = {"home": {"text": "new"}}
            with self.assertRaises(ValueError):
                capture.write_fixtures(captures, destination)
            self.assertEqual((destination / "home.json").read_text(), "original")

    def test_failed_verification_does_not_publish_any_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = pathlib.Path(directory)
            captures = capture.Capture(None, destination / "raw")
            captures.raw = self.responses()
            captures.raw["unknown"] = {"text": "other@example.invalid"}
            with self.assertRaises(ValueError):
                capture.write_fixtures(captures, destination)
            self.assertEqual(list(destination.glob("*.json")), [])

    def test_generic_privacy_guard_blocks_unknown_personal_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = pathlib.Path(directory)
            captures = capture.Capture(None, destination / "raw")
            captures.raw = self.responses()
            captures.raw["unknown"] = {"phone_" + "number": "123456789"}
            with self.assertRaises(ValueError):
                capture.write_fixtures(captures, destination)
            self.assertEqual(list(destination.glob("*.json")), [])

    def test_raw_capture_replaces_public_file_with_private_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            raw = pathlib.Path(directory) / "raw"
            raw.mkdir()
            path = raw / "home.json"
            path.write_text("old")
            path.chmod(0o644)
            capture.Capture(None, raw).record("home", {"text": "value"}, "fixture")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(raw.stat().st_mode & 0o777, 0o700)
            self.assertEqual(json.loads(path.read_text()), {"text": "value"})

    def test_synthetic_lyrics_keep_timing_and_blank_lines(self):
        response = {"timedLyricsData": [
            {"lyricLine": " ", "cueRange": {"startTimeMilliseconds": "0", "endTimeMilliseconds": "100"}},
            {"lyricLine": "Private copyrighted text", "cueRange": {"startTimeMilliseconds": "100", "endTimeMilliseconds": "200"}},
        ], "sourceMessage": "Original provider"}
        replaced = capture.synthetic_lyrics(response)
        self.assertEqual(replaced["timedLyricsData"][0], response["timedLyricsData"][0])
        self.assertEqual(replaced["timedLyricsData"][1]["cueRange"], response["timedLyricsData"][1]["cueRange"])
        self.assertEqual(replaced["timedLyricsData"][1]["lyricLine"], "Synthetic lyric line 2")
        self.assertNotIn("Original provider", json.dumps(replaced))

    def test_unrelated_encoded_cipher_keeps_its_encoding(self):
        cipher = "s=fixture&url=https%3A%2F%2Fexample.invalid%2F%3Fn%3Dfixture%26itag%3D141"
        self.assertEqual(capture.scrub_identities(cipher, {"Fixture Person": "Test Account"}), cipher)

    def test_capabilities_are_synthetic_and_preserve_distinct_edit_authorities(self):
        response = {"contents": [
            {"continuation": "private-page", "playlistSetVideoId": "private-entry-one"},
            {"continuation": "private-page", "setVideoId": "private-entry-one"},
            {"playlistSetVideoId": "private-entry-two"},
        ]}
        scrubbed = capture.scrub("playlist", response)
        items = scrubbed["contents"]
        self.assertEqual(items[0]["continuation"], "fixture-continuation-0001")
        self.assertEqual(items[0]["continuation"], items[1]["continuation"])
        self.assertEqual(items[0]["playlistSetVideoId"], items[1]["setVideoId"])
        self.assertNotEqual(items[0]["playlistSetVideoId"], items[2]["playlistSetVideoId"])
        self.assertNotIn("private-", json.dumps(scrubbed))

    def test_guard_rejects_real_capabilities_and_accepts_synthetic_replacements(self):
        response = {"nested": [{"continuation": "private-page", "playlistSetVideoId": "private-entry"}]}
        self.assertIn((1, "fixture-account-capability"), capture.personal_findings(json.dumps(response), "fixtures/example.json", []))
        self.assertEqual(capture.personal_findings(json.dumps(capture.scrub("playlist", response)), "fixtures/example.json", []), [])

    def test_continuation_container_keeps_shape_and_replaces_its_nested_token(self):
        response = {"continuation": {"reloadContinuationData": {"continuation": "private-page"}}}
        scrubbed = capture.scrub("library", response)
        self.assertEqual(scrubbed, {"continuation": {"reloadContinuationData": {"continuation": "fixture-continuation-0001"}}})
        self.assertEqual(capture.personal_findings(json.dumps(scrubbed), "fixtures/example.json", []), [])
