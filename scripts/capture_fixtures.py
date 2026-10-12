#!/usr/bin/env python3
"""Capture the InnerTube responses the parser tests are written against.

Run with an installed ytmusicapi package:

    python3 scripts/capture_fixtures.py

For a source checkout, pass its package root explicitly:

    python3 scripts/capture_fixtures.py --ytmusicapi-source /path/to/ytmusicapi

Every response goes through `scrub`, so no account field, tracking id or
stream `ip` parameter reaches `fixtures/`. Raw captures are kept under
`fixtures/raw/`, which is gitignored, because a scrub bug is easier to see
against the original than against a second capture an hour later.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tempfile

from check_fixtures import identity_findings, shape_findings
from check_private import personal_findings
import urllib.parse
from datetime import date
from pathlib import Path
from typing import Any, Callable

REPO = Path(__file__).resolve().parent.parent
AUTH = os.path.expanduser("~/.config/redwax/browser.json")

SEARCH_QUERY = "oasis wonderwall"
ALBUM_HINT = "morning glory"
ARTIST_HINT = "oasis"
HIGH_TIER_VIDEO = "1-Fxb9jsOuI"
NO_HIGH_TIER_VIDEO = "Bq56cLjoIgo"

SIZE_BUDGET = 400_000
HISTORY_ITEMS = 10

# Deleted everywhere, recursively. Tracking ids, session ids and the fields
# that identify this account or this playback session.
DROP_KEYS = {
    "responseContext",
    "trackingParams",
    "clickTrackingParams",
    "serviceTrackingParams",
    "frameworkUpdates",
    "loggingContext",
    "visitorData",
    "datasyncId",
    "delegatedSessionId",
    "feedbackToken",
    "feedbackTokens",
    "addToLibraryFeedbackToken",
    "formEntityKey",
    "formItemEntityKey",
    "collectionKey",
    "colorSamplePaletteEntityKey",
    "themeKey",
    "key",
    "playerConfig",
    "attestation",
    "adPlacements",
    "adBreakHeartbeatParams",
    "playbackTracking",
    "cards",
    "storyboards",
    "heartbeatToken",
    "drmSessionId",
}

SYNTHETIC_VALUES = {"token": "fixture"}

FAKE_NAME = "Test Account"
FAKE_HANDLE = "@testaccount"
FAKE_EMAIL = "fixture-account"
FAKE_AVATAR = "https://example.invalid/avatar.jpg"

# Lists that grow without bound, trimmed when a file is over budget.
TRIMMABLE_LIST_KEYS = ("contents", "items", "continuationItems", "results", "sectionListContinuation")

EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
HANDLE_RE = re.compile(r"^@[A-Za-z0-9._-]{3,}$")


def load_ytmusicapi(source_dir: str | None) -> None:
    if source_dir:
        source_path = Path(source_dir).expanduser()
        if not source_path.is_dir():
            raise RuntimeError(f"ytmusicapi source is not a directory: {source_path}")
        sys.path.insert(0, str(source_path))
    try:
        from ytmusicapi import YTMusic as api
        from ytmusicapi.continuations import get_continuation_params as continuation_params
        from ytmusicapi.parsers.search import get_search_params as search_params
    except ImportError as error:
        raise RuntimeError(
            "ytmusicapi is unavailable. Install it or pass --ytmusicapi-source DIR."
        ) from error

    globals()["YTMusic"] = api
    globals()["get_continuation_params"] = continuation_params
    globals()["get_search_params"] = search_params


# ---------------------------------------------------------------- walking


def walk(node: Any, visit: Callable[[str | None, Any], None], key: str | None = None) -> None:
    visit(key, node)
    if isinstance(node, dict):
        for k, v in node.items():
            walk(v, visit, k)
    elif isinstance(node, list):
        for v in node:
            walk(v, visit, key)


def drop_keys(node: Any) -> Any:
    if isinstance(node, dict):
        return {
            k: SYNTHETIC_VALUES[k] if k in SYNTHETIC_VALUES else drop_keys(v)
            for k, v in node.items()
            if k not in DROP_KEYS
        }
    if isinstance(node, list):
        return [drop_keys(v) for v in node]
    return node


def find_values(node: Any, wanted: str, prefix: str = "") -> list[str]:
    """Every string value of key `wanted` that starts with `prefix`, in document order."""
    found: list[str] = []

    def visit(key: str | None, value: Any) -> None:
        if key == wanted and isinstance(value, str) and value.startswith(prefix):
            found.append(value)

    walk(node, visit)
    return found


def texts(node: Any) -> str:
    """Every `text` leaf under a node, joined, for matching an item by its title."""
    parts: list[str] = []

    def visit(key: str | None, value: Any) -> None:
        if key == "text" and isinstance(value, str):
            parts.append(value)

    walk(node, visit)
    return " ".join(parts).lower()


def containers_with(node: Any, wanted: str, prefix: str) -> list[tuple[str, Any]]:
    """Pairs of id and the smallest dict holding it, so the id can be matched by title."""
    found: list[tuple[str, Any]] = []

    def descend(value: Any) -> None:
        if isinstance(value, dict):
            for k, v in value.items():
                if k == wanted and isinstance(v, str) and v.startswith(prefix):
                    found.append((v, value))
                descend(v)
        elif isinstance(value, list):
            for v in value:
                descend(v)

    descend(node)
    return found


def pick_id(response: Any, key: str, prefix: str, hint: str) -> str | None:
    """The id whose surrounding item mentions `hint`, else the first one seen."""
    candidates = containers_with(response, key, prefix)
    for value, container in candidates:
        if hint in texts(container):
            return value
    for value, container in candidates:
        parent_text = texts(response)
        if hint in parent_text and value:
            return value
    return candidates[0][0] if candidates else None


# ---------------------------------------------------------------- scrubbing


def strip_ip(url: str) -> str:
    """Drop the `ip` parameter from a stream URL, leaving every other field byte for byte."""
    head, sep, query = url.partition("?")
    if not sep:
        return url
    kept = [p for p in query.split("&") if not p.startswith("ip=")]
    return head + "?" + "&".join(kept)


def strip_ip_from_cipher(cipher: str) -> str:
    """Same, for a `signatureCipher` value, whose `url` field is percent encoded."""
    parts = []
    for part in cipher.split("&"):
        name, sep, value = part.partition("=")
        if name == "url" and sep:
            value = urllib.parse.quote(strip_ip(urllib.parse.unquote(value)), safe="")
        parts.append(name + sep + value)
    return "&".join(parts)


def scrub_stream_urls(node: Any, key: str | None = None) -> Any:
    if isinstance(node, dict):
        return {k: scrub_stream_urls(v, k) for k, v in node.items()}
    if isinstance(node, list):
        return [scrub_stream_urls(v, key) for v in node]
    if isinstance(node, str):
        if key == "signatureCipher":
            return strip_ip_from_cipher(node)
        if node.startswith("http") and "ip=" in node:
            return strip_ip(node)
    return node


def fake_text(node: Any, value: str) -> Any:
    """A text node of the same shape as `node`, holding `value`."""
    if isinstance(node, dict) and "runs" in node:
        return {"runs": [{"text": value}]}
    if isinstance(node, dict) and "simpleText" in node:
        return {"simpleText": value}
    if isinstance(node, list):
        return [fake_text(v, value) for v in node]
    return value


def fake_photo(node: Any) -> Any:
    """The same thumbnail node with every URL replaced, so sizes still parse."""
    if isinstance(node, dict):
        return {k: (FAKE_AVATAR if k == "url" else fake_photo(v)) for k, v in node.items()}
    if isinstance(node, list):
        return [fake_photo(v) for v in node]
    return node


# `title` is deliberately absent: every menu link has one, and none is the account.
NAME_KEYS = ("accountName", "accountDisplayName")
HANDLE_KEYS = ("channelHandle", "handle")
EMAIL_KEYS = ("email", "accountByline")
PHOTO_KEYS = ("accountPhoto", "avatar", "thumbnail", "thumbnails")

# Page ids and datasync ids are long digit runs, and one of them survives inside
# the Google account link.
LONG_DIGITS_RE = re.compile(r"\d{15,}")
FAKE_DIGITS = "000000000000000000000"


def scrub_account(node: Any, key: str | None = None) -> Any:
    """Replace the identity in an account menu with fixed fake values.

    Covers the signed in identity and any brand channel entry, because both
    carry a name, a handle and an avatar in the same shape.
    """
    if isinstance(node, dict):
        out: dict[str, Any] = {}
        for k, v in node.items():
            if k in NAME_KEYS and not isinstance(v, (list, str)):
                out[k] = fake_text(v, FAKE_NAME)
            elif k in HANDLE_KEYS:
                out[k] = fake_text(v, FAKE_HANDLE)
            elif k in EMAIL_KEYS:
                out[k] = fake_text(v, FAKE_EMAIL)
            elif k in PHOTO_KEYS:
                out[k] = fake_photo(v)
            else:
                out[k] = scrub_account(v, k)
        return out
    if isinstance(node, list):
        return [scrub_account(v, key) for v in node]
    if isinstance(node, str):
        if EMAIL_RE.fullmatch(node):
            return FAKE_EMAIL
        if HANDLE_RE.match(node):
            return FAKE_HANDLE
        if "ggpht" in node or "googleusercontent" in node:
            return FAKE_AVATAR
        return LONG_DIGITS_RE.sub(FAKE_DIGITS, node)
    return node


def trim_history(response: Any) -> Any:
    """Keep the first period only, and its first ten items.

    History is one `musicShelfRenderer` per period, newest first. One period
    specifies the page and keeps a long listening record out of the repository.
    """

    def trim_section_list(section_list: Any) -> Any:
        contents = section_list.get("contents")
        if not isinstance(contents, list):
            return section_list
        shelves = [x for x in contents if isinstance(x, dict) and "musicShelfRenderer" in x]
        if not shelves:
            return section_list
        first = dict(shelves[0])
        inner = dict(first["musicShelfRenderer"])
        inner["contents"] = inner.get("contents", [])[:HISTORY_ITEMS]
        first["musicShelfRenderer"] = inner
        trimmed = dict(section_list)
        trimmed["contents"] = [first]
        return trimmed

    def descend(value: Any) -> Any:
        if isinstance(value, dict):
            return {
                k: descend(trim_section_list(v) if k == "sectionListRenderer" and isinstance(v, dict) else v)
                for k, v in value.items()
            }
        if isinstance(value, list):
            return [descend(v) for v in value]
        return value

    return descend(response)


def dumps(doc: Any) -> str:
    """The one serialiser, so the size check measures the bytes that get written.

    `indent=0` keeps one value per line, which makes the scrub grep and a git
    diff useful, and costs a byte a line next to the compact form. Escaping to
    ASCII keeps a song title with an em dash in it out of the repository wide
    em dash grep in `scripts/rust_lint.sh`.
    """
    return json.dumps(doc, indent=0, ensure_ascii=True) + "\n"


def size_of(doc: Any) -> int:
    return len(dumps(doc).encode())


def trim_to_budget(doc: Any, budget: int = SIZE_BUDGET) -> Any:
    """Shorten continuation lists until the document fits.

    Item shape is what the parser tests need, not item count, so trimming the
    long lists costs the tests nothing.
    """
    for cap in (200, 150, 100, 75, 50, 40, 30, 25, 20, 15, 12, 10, 8, 6, 5, 4, 3, 2):
        if size_of(doc) <= budget:
            return doc
        doc = cap_lists(doc, cap)
    return doc


def cap_lists(node: Any, cap: int, key: str | None = None) -> Any:
    if isinstance(node, dict):
        return {k: cap_lists(v, cap, k) for k, v in node.items()}
    if isinstance(node, list):
        trimmed = capped(node, cap) if key in TRIMMABLE_LIST_KEYS and len(node) > cap else node
        return [cap_lists(v, cap, key) for v in trimmed]
    return node


def capped(entries: list[Any], cap: int) -> list[Any]:
    """`entries` shortened to `cap`, with any trailing continuation entry kept.

    A shelf names its own continuation in the last entry of its `contents`, so
    a plain slice leaves a fixture whose page cannot be followed.
    """
    kept = entries[:cap]
    last = entries[-1]
    if isinstance(last, dict) and "continuationItemRenderer" in last:
        kept.append(last)
    return kept


def scrub(name: str, response: Any) -> Any:
    doc = synthetic_capabilities(scrub_stream_urls(drop_keys(response)))
    if name == "account_menu":
        doc = scrub_account(doc)
    if name.startswith("lyrics"):
        doc = synthetic_lyrics(doc)
    if name == "history":
        doc = trim_history(doc)
    return trim_to_budget(doc)


def synthetic_capabilities(node: Any) -> Any:
    replacements: dict[tuple[str, str], str] = {}
    counts: dict[str, int] = {}
    fields = {"continuation": "continuation", "playlistSetVideoId": "entry", "setVideoId": "entry"}

    def replace(value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {name: replace(child, name) for name, child in value.items()}
        if isinstance(value, list):
            return [replace(child, key) for child in value]
        if key in fields and isinstance(value, str) and value:
            kind = fields[key]
            identity = (kind, value)
            if identity not in replacements:
                counts[kind] = counts.get(kind, 0) + 1
                replacements[identity] = f"fixture-{kind}-{counts[kind]:04d}"
            return replacements[identity]
        return value

    return replace(node)


def synthetic_lyrics(node: Any) -> Any:
    line_number = 0

    def replace(value: Any, key: str | None = None) -> Any:
        nonlocal line_number
        if isinstance(value, dict):
            return {name: replace(child, name) for name, child in value.items()}
        if isinstance(value, list):
            return [replace(child, key) for child in value]
        if isinstance(value, str):
            if key == "lyricLine":
                line_number += 1
                return " " if not value.strip() else f"Synthetic lyric line {line_number}"
            if key == "text":
                return "Synthetic fixture lyrics\nA second sample line" if "\n" in value else "Synthetic fixture source"
            if key in ("sourceMessage", "source", "lyricsSource"):
                return "Synthetic fixture source"
        return value

    return replace(node)


def private_write(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".capture-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as destination:
            destination.write(contents)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def capture_identities(captures: dict[str, Any]) -> dict[str, str]:
    if any(name not in captures for name in ("account_menu", "library_playlists", "_library_identity")):
        raise ValueError("identity metadata is incomplete, recapture before publishing fixtures")
    replacements: dict[str, str] = {}

    def remember(value: Any, replacement: str) -> None:
        if isinstance(value, str) and len(value) >= 3:
            replacements[value] = replacement

    def account_value(key: str | None, value: Any) -> None:
        if key in NAME_KEYS + HANDLE_KEYS + EMAIL_KEYS:
            if isinstance(value, str):
                remember(value, FAKE_NAME)
            elif isinstance(value, dict):
                for text in find_values(value, "text") + find_values(value, "simpleText"):
                    remember(text, FAKE_NAME)
        if key in ("pageId", "channelId", "browseId", "datasyncId", "delegatedSessionId"):
            remember(value, "UCfixtureaccount" if isinstance(value, str) and value.startswith("UC") else FAKE_DIGITS)
        if isinstance(value, str):
            for email in EMAIL_RE.findall(value):
                remember(email, "fixture-account")
            for digits in LONG_DIGITS_RE.findall(value):
                remember(digits, FAKE_DIGITS)
            if value.startswith("http") and ("ggpht" in value or "googleusercontent" in value):
                remember(value, FAKE_AVATAR)

    walk(captures["account_menu"], account_value)
    if not replacements:
        raise ValueError("account menu contains no identity, refusing an unverified capture")
    playlist_ids = set(find_values(captures["_library_identity"], "playlistId"))
    playlist_ids.update(find_values(captures["library_playlists"], "playlistId"))
    playlist_ids.update(value[2:] for value in find_values(captures["library_playlists"], "browseId", "VL"))
    for index, playlist_id in enumerate(sorted(playlist_ids)):
        if playlist_id not in ("LM", "SE"):
            remember(playlist_id, f"PLfixture{index:04d}")
    return replacements


def scrub_identities(node: Any, replacements: dict[str, str]) -> Any:
    if isinstance(node, dict):
        return {scrub_identities(key, replacements): scrub_identities(value, replacements) for key, value in node.items()}
    if isinstance(node, list):
        return [scrub_identities(value, replacements) for value in node]
    if isinstance(node, str):
        variants = dict(replacements)
        for value, replacement in replacements.items():
            variants[urllib.parse.quote(value, safe="")] = urllib.parse.quote(replacement, safe="")
            variants[urllib.parse.quote_plus(value)] = urllib.parse.quote_plus(replacement)
        pattern = re.compile("|".join(re.escape(value) for value in sorted(variants, key=len, reverse=True)), re.IGNORECASE)
        normalized = {value.lower(): replacement for value, replacement in variants.items()}
        return pattern.sub(lambda match: normalized[match.group().lower()], node)
    return node


# ---------------------------------------------------------------- capture


class Capture:
    def __init__(self, yt: YTMusic | None, raw_dir: Path) -> None:
        self.yt = yt
        self.raw_dir = raw_dir
        self.raw: dict[str, Any] = {}
        self.notes: dict[str, str] = {}

    def load_raw(self) -> None:
        for path in sorted(self.raw_dir.glob("*.json")):
            self.raw[path.stem] = json.loads(path.read_text())

    def send(self, endpoint: str, body: dict[str, Any], params: str = "") -> Any:
        # _send_request folds the client context into the body it is given, so
        # each call gets its own dict.
        return self.yt._send_request(endpoint, dict(body), params)

    def record(self, name: str, response: Any, note: str) -> Any:
        self.raw[name] = response
        self.notes[name] = note
        self.raw_dir.mkdir(parents=True, exist_ok=True)
        self.raw_dir.chmod(0o700)
        private_write(self.raw_dir / f"{name}.json", json.dumps(response, indent=1))
        return response

    def run(self) -> None:
        self.record("account_menu", self.send("account/account_menu", {}), "account/account_menu")
        self.record("_library_identity", self.yt.get_library_playlists(limit=None), "complete library identity inventory")
        home = self.record("home", self.send("browse", {"browseId": "FEmusic_home"}), "browse FEmusic_home")
        self.capture_home_continuation(home)
        search = self.record(
            "search", self.send("search", {"query": SEARCH_QUERY}), f"search {SEARCH_QUERY!r}"
        )
        songs_params = get_search_params("songs", None, False)
        self.record(
            "search_songs",
            self.send("search", {"query": SEARCH_QUERY, "params": songs_params}),
            f"search {SEARCH_QUERY!r} params {songs_params}",
        )
        self.capture_album(search)
        self.capture_artist(search)
        self.capture_playlist(home)
        for name, browse_id in (
            ("library_playlists", "FEmusic_liked_playlists"),
            ("library_albums", "FEmusic_liked_albums"),
            ("library_artists", "FEmusic_library_corpus_track_artists"),
            ("history", "FEmusic_history"),
        ):
            self.record(name, self.send("browse", {"browseId": browse_id}), f"browse {browse_id}")
        self.capture_watch()
        for name, video_id in (
            ("player_web_remix_high", HIGH_TIER_VIDEO),
            ("player_web_remix_no_high", NO_HIGH_TIER_VIDEO),
        ):
            self.record(
                name,
                self.send("player", {"videoId": video_id}),
                f"player videoId {video_id}, WEB_REMIX context",
            )

    def capture_home_continuation(self, home: Any) -> None:
        section = home.get("contents", {})
        tokens = find_values(section, "continuation")
        params = None
        try:
            section_list = home["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"][
                "content"
            ]["sectionListRenderer"]
            params = get_continuation_params(section_list)
        except (KeyError, IndexError, TypeError):
            if tokens:
                params = "&ctoken=" + tokens[0] + "&continuation=" + tokens[0]
        if not params:
            print("home has no continuation, skipping home_continuation", file=sys.stderr)
            return
        self.record(
            "home_continuation",
            self.send("browse", {"browseId": "FEmusic_home"}, params),
            "browse FEmusic_home with the first ctoken",
        )

    def capture_album(self, search: Any) -> None:
        browse_id = pick_id(search, "browseId", "MPRE", ALBUM_HINT)
        if not browse_id:
            print("no MPRE browse id in search, skipping album", file=sys.stderr)
            return
        self.record("album", self.send("browse", {"browseId": browse_id}), f"browse {browse_id}")

    def capture_artist(self, search: Any) -> None:
        browse_id = pick_id(search, "browseId", "UC", ARTIST_HINT)
        if not browse_id:
            print("no UC browse id in search, skipping artist", file=sys.stderr)
            return
        self.record("artist", self.send("browse", {"browseId": browse_id}), f"browse {browse_id}")

    def capture_playlist(self, home: Any) -> None:
        seen: list[str] = []
        for playlist_id in find_values(home, "playlistId"):
            if playlist_id.startswith(("PL", "RDCLAK")) and playlist_id not in seen:
                seen.append(playlist_id)
        for playlist_id in seen[:5]:
            body = {"browseId": "VL" + playlist_id}
            try:
                response = self.send("browse", body)
            except Exception as exc:  # a private or region locked playlist, try the next
                print(f"playlist {playlist_id} failed: {exc}", file=sys.stderr)
                continue
            if not response.get("contents"):
                continue
            token = self.playlist_continuation_token(response)
            note = f"browse VL{playlist_id}"
            if token:
                more = self.send("browse", {"continuation": token})
                response["continuationResponses"] = [more]
                note += ", first continuation folded in under continuationResponses"
            self.record("playlist", response, note)
            return
        print("no usable public playlist in home, skipping playlist", file=sys.stderr)

    @staticmethod
    def playlist_continuation_token(response: Any) -> str | None:
        tokens = find_values(response, "token")
        return tokens[0] if tokens else None

    def capture_watch(self) -> None:
        radio_body = {
            "enablePersistentPlaylistPanel": True,
            "isAudioOnly": True,
            "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
            "videoId": HIGH_TIER_VIDEO,
            "playlistId": "RDAMVM" + HIGH_TIER_VIDEO,
            "params": "wAEB",
        }
        self.record("watch_playlist", self.send("next", radio_body), "next, radio for " + HIGH_TIER_VIDEO)
        plain_body = {
            "enablePersistentPlaylistPanel": True,
            "isAudioOnly": True,
            "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
            "videoId": HIGH_TIER_VIDEO,
            "watchEndpointMusicSupportedConfigs": {
                "watchEndpointMusicConfig": {
                    "hasPersistentPlaylistPanel": True,
                    "musicVideoType": "MUSIC_VIDEO_TYPE_ATV",
                }
            },
            "playlistId": "RDAMVM" + HIGH_TIER_VIDEO,
        }
        plain = self.record(
            "watch_playlist_plain", self.send("next", plain_body), "next, no radio, " + HIGH_TIER_VIDEO
        )
        lyrics_ids = find_values(plain, "browseId", "MPLYt")
        if not lyrics_ids:
            print("no MPLYt browse id in the watch response, skipping lyrics", file=sys.stderr)
            return
        self.record("lyrics", self.send("browse", {"browseId": lyrics_ids[0]}), f"browse {lyrics_ids[0]}")
        self.capture_timed_lyrics(lyrics_ids[0])

    def capture_timed_lyrics(self, browse_id: str) -> None:
        """The same lyrics page as the Android music app sees it, with line timings.

        Signed in, this request answers HTTP 400, so it goes out through a fresh
        unauthenticated client rather than the one the other captures share.
        """
        anonymous = YTMusic()
        with anonymous.as_mobile():
            response = anonymous._send_request("browse", {"browseId": browse_id})
        self.record(
            "lyrics_timed", response, f"browse {browse_id}, ANDROID_MUSIC context, signed out"
        )


# ---------------------------------------------------------------- output


def write_fixtures(captures: Capture, out_dir: Path) -> list[tuple[str, int, str]]:
    replacements = capture_identities(captures.raw)
    prepared = []
    for name, response in captures.raw.items():
        if name.startswith("_"):
            continue
        doc = scrub_identities(scrub(name, response), replacements)
        text = dumps(doc)
        findings = shape_findings(text) + identity_findings(text.lower(), [value.lower() for value in replacements])
        findings.extend(personal_findings(text, f"fixtures/{name}.json", list(replacements)))
        if findings:
            raise ValueError(f"{name}: fixture privacy verification failed, values redacted")
        prepared.append((name, text, doc))
    out_dir.mkdir(parents=True, exist_ok=True)
    rows = []
    for name, text, doc in prepared:
        path = out_dir / f"{name}.json"
        private_write(path, text)
        top = ", ".join(sorted(doc.keys())) if isinstance(doc, dict) else type(doc).__name__
        rows.append((path.name, len(text.encode()), top))
    return rows


def print_table(rows: list[tuple[str, int, str]]) -> None:
    width = max((len(r[0]) for r in rows), default=4)
    print(f"{'file'.ljust(width)}  {'bytes':>8}  top level keys")
    for name, size, top in sorted(rows):
        flag = "" if size <= SIZE_BUDGET else "  OVER BUDGET"
        print(f"{name.ljust(width)}  {size:>8}  {top}{flag}")
    print(f"\n{len(rows)} fixtures, captured {date.today().isoformat()}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default=str(REPO / "fixtures"), help="fixture directory")
    parser.add_argument("--raw-only", action="store_true", help="capture and keep raw, write nothing")
    parser.add_argument(
        "--from-raw", action="store_true", help="rescrub the last raw capture, make no request"
    )
    parser.add_argument(
        "--ytmusicapi-source",
        default=os.environ.get("REDWAX_YTMUSICAPI_SOURCE"),
        metavar="DIR",
        help="source checkout containing the ytmusicapi package",
    )
    args = parser.parse_args()

    out_dir = Path(args.out)
    if args.from_raw:
        captures = Capture(None, out_dir / "raw")
        captures.load_raw()
        print_table(write_fixtures(captures, out_dir))
        return 0

    try:
        load_ytmusicapi(args.ytmusicapi_source)
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 1

    if not os.path.exists(AUTH):
        print("no saved browser session, run redwax login first", file=sys.stderr)
        return 1

    yt = YTMusic(AUTH)
    captures = Capture(yt, out_dir / "raw")
    captures.run()
    if args.raw_only:
        print(f"raw only, {len(captures.raw)} captures under {out_dir / 'raw'}")
        return 0
    print_table(write_fixtures(captures, out_dir))
    return 0


if __name__ == "__main__":
    sys.exit(main())
