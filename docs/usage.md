# Usage

Every key, every subcommand, every flag and every environment variable.
`README.md` is the short version. Commands below use the installed `redwax` command.

## Keys

The Keys panel shows the same two-column hints on every tab. Some actions
need a selected track, queue entry or editable playlist. Backspace only edits text in fields.
Space plays, pauses or resumes the saved queue.

The focused list handles keys first. For example, `j` moves its cursor and
`l` switches tabs when the list has no binding for it.

### Playback

| key | action |
|---|---|
| `space` | play or pause |
| `n` | next track |
| `p` | previous track |
| `,` | jump back 5 seconds |
| `.` | jump forward 5 seconds |
| `<` | jump back 30 seconds |
| `>` | jump forward 30 seconds |
| `-` | volume down 5 |
| `=` | volume up 5 |
| `m` | mute |
| `s` | shuffle on or off |
| `R` | repeat off, all, one |

After a relaunch, the saved queue and track position stay paused. Press
`space` to resume from that position. Enter on the saved track also resumes
there. Enter on another queue track starts it from the beginning.

### The queue and the track

| key | action |
|---|---|
| `d` | remove the highlighted entry from the queue |
| `c` | clear the queue |
| `K` | move the highlighted queue entry up |
| `J` | move the highlighted queue entry down |
| `q` | the queue tab, with the cursor on the track that plays |
| `r` | radio seeded from the highlighted track |
| `+` | like or unlike the highlighted track |
| `P` | add the playing song to a playlist, with the highlighted item a `tab` away |
| `ctrl+p` | add the highlighted item to a playlist |
| `delete` | remove the highlighted song from the open playlist or queue |
| `D` | remove the highlighted track from the open playlist, or the track that plays from a playlist you pick |

Right-click a row to add it to the queue. Middle-click a row to play it next.

When redwax restores a song, it opens the queue with that song highlighted.
Press `enter` to play it. In any tab, press `P` to save the playing song or
`ctrl+p` to save the highlighted item. The picker also lets `tab` switch
between these choices. Choose an existing playlist or name a new one.
Delete removes a song from the current queue or playlist and updates the list.
In Liked songs, Delete unlikes the song.

Clear queue opens a confirmation that names how many upcoming tracks are
affected and whether an automatic producer will stop. The safe choice, Keep
queue, has focus first. Use left and right to choose, Enter to apply the chosen
answer, or Escape to cancel.

On the queue tab, `K` and `J` move the highlighted entry by its queue identity.
Two copies of the same track remain separate entries. If mpv changes the queue
after the row was selected, redwax rejects the stale move rather than moving a
different entry.

Radio loads recommendations in batches. The queue shows the tracks already
loaded, not the station's eventual length. When five tracks remain, redwax
fetches the next batch in the background and appends new tracks without
restarting playback. The queue says "continues automatically" while radio is
active. Clearing the queue, playing another page or adding a manual queue
item ends radio. Skipping within the radio queue keeps it active.

A failed fetch leaves the loaded tracks available and retries after 30 seconds.
If YouTube repeats a batch, redwax filters duplicates and delays the next
request. When no continuation remains, it requests another batch from the
original seed. YouTube availability and a valid session are still required.

### Moving about

| key | action |
|---|---|
| `j`, `down` | cursor down |
| `k`, `up` | cursor up |
| `g g`, `home` | first row |
| `G` (Shift+G), `end` | last row |
| `page down` | page down |
| `page up` | page up |
| `enter` | open the highlighted item |
| `{`, `}` | previous or next section |
| `h`, `left` | previous tab |
| `l`, `right` | next tab |
| `1` to `7` | home, search, library, queue, lyrics, themes, settings |
| `/` | find on this page, or focus the remote search box |
| `ctrl+n`, `ctrl+p` while finding | next or previous match |
| `f` | filter the current page by result kind |
| `b` | open the highlighted track album |
| `escape` | back |
| `tab` | focus the account box or return to the page |
| `ctrl+r` | reload the tab or retry failed playback |
| `e` | show or hide the equaliser panel |
| `?` | show or hide searchable help |
| `Q` | quit |

The result-kind filter is a view of the current page. It does not change the
source order or playback order. Press `f` to cycle through the kinds present
on the page and return to all results. Clearing the filter restores the prior
selection where the item is still present.

On-page find reports the selected match and total matches. While its bar is
open, `ctrl+n` and `ctrl+p` wrap through the visible result projection. Plain
letters, including `n` and `N`, extend the query. Outside that bar, `n` keeps
its playback meaning and skips to the next track.

Press `ctrl+r` to retry playback after an automatic restart fails. The same
key reloads Home or Library. Sign out is in the account picker and opens a
confirmation with the safe choice selected first.

### Accounts

The account box at the top right shows the current identity. Press `tab`, then
`enter` to open the account picker. Use arrows or `j` and `k` to choose an
identity, then `enter` to switch. Escape closes the picker. You can also reach
the account box with left or right at the ends of the tab bar.

On startup, Checking login means YouTube has not confirmed the saved session yet.
Login unconfirmed means that the identity check did not complete. A confirmed
account name stays visible through temporary errors. Sign in appears after
logout or a rejected session.

The picker reads the logins in your remembered browser profile. Sign in to
YouTube Music in that browser, then choose Refresh browser logins if needed.
Account changes reload Home and Library without restarting playback. A failed
switch keeps the current login. Changing account on the website alone does
not switch a running redwax.

## Subcommands and flags

### `redwax`

```
redwax [--config PATH] [--version]
```

- `--config PATH`, `--config=PATH`: read this config file instead of
  `~/.config/redwax/config.toml`.
- `--version`, `-V`: print the version and exit.
- `--help`, `-h`: print the usage and exit.

### `redwax setup`

```
redwax setup [--config PATH]
```

Checks local playback dependencies and saved login files. Missing dependencies
come before login instructions. Exit 1 means setup is incomplete. Setup makes
no network request and cannot prove that a saved session is still accepted.

On a first launch, redwax tries an existing browser session when the playback
dependencies are ready. A failed import leaves the terminal in its normal
screen and prints browser selection or installation instructions.

### `redwax doctor`

```
redwax doctor [--live] [--config PATH]
```

One line per check, `ok`, `warn` or `fail` first, then the check name, then
the detail. Exit 1 when any check failed. Without `--live` it touches no
network.

Offline it checks the config file and the values that matter, the session file
and its age, the cookie names, mpv, yt-dlp, node, the two solver scripts, the
InnerTube and player caches, the page cache, the state file, the terminal and
the art protocol, and the session bus.

`--live` checks your identity, the home feed,
one search, one album from that search and one playlist from home, each with a
minimum item count that fails when it is not met. Then one resolution of
`1-Fxb9jsOuI` through every step of the chain, and one of `Bq56cLjoIgo`, which
has no high encode and must come back at 251.

For Firefox logins, it also reads the download marks and checks how many are
missing from `library_dir`. Other login browsers skip that optional check.

A live check that comes back refused says the session has probably rotated.
Run `redwax login` and try again.

Nightly at 04:00 through `scripts/systemd/redwax-doctor.timer`, installed by
`./install.sh --timer` and enabled by you.

### `redwax login`

```
redwax login [--browser NAME] [--profile NAME] [--account QUERY] [--list-accounts] [--config PATH]
```

Open https://music.youtube.com in your browser and sign in before importing.
Login reads the browser session, writes private `cookies.txt` and `browser.json`
exports, and confirms the selected identity with YouTube. The browser stays
open. A password is not required.

`--browser` accepts `firefox`, `chrome`, `chromium`, `brave`, `edge`, `opera`,
`vivaldi`, `whale` or `safari`. Safari requires macOS. Chromium browsers can require an
unlocked desktop keyring. Browser protection or file permissions can prevent
an import. Login reports an actionable error instead of claiming success.

An explicit browser wins over the saved choice. Without a browser flag, login
uses the saved browser and profile, or discovers an installed browser with a
signed-in YouTube session. Native Firefox profiles are checked in Firefox's
preferred order. yt-dlp handles other browsers and sandboxed Firefox stores.
Discovery checks each cookie-bearing Chromium profile, including Flatpak and
Snap locations, so a signed-out default profile does not hide another login.
The selected browser and profile are remembered in `session.json`.

`--profile` narrows the browser lookup. For Firefox, use a `profiles.ini` name
or configured directory. For Chromium browsers, use a profile such as
`Default` or `Profile 1`. Quote profile names with spaces.

`--list-accounts` prints the identities YouTube offers for the imported login
without changing saved credentials. Without `--account`, login follows the
account selected in the browser.
`--account` selects a 1-based number from that listing, a handle with or without
`@`, or an exact name. The selection survives later session refreshes.

```
redwax login --browser chrome --profile 'Profile 1'
redwax login --list-accounts
redwax login --account 2
```

Exit 1 means import or identity confirmation failed. Missing or invalid option
values exit with status 2. A successful login says whose library will open.

### `redwax logout`

```
redwax logout [--config PATH]
```

Removes `browser.json`, `cookies.txt`, and `session.json`. This removes the
remembered browser, profile and account identity. Playback state and caches stay
in place.

A separate running redwax process keeps the session it has already loaded. Use
the in-app Sign out action to invalidate credentials in that process and block
automatic import until you choose an account again. Restart a running redwax
after the CLI command.
The standalone command cannot mark an existing process as signed out. If that
process later receives a rejected read, session recovery can import from
the remembered browser again and recreate the exports. `ctrl+r` starts a page read, but it
imports only when YouTube rejects the session already loaded by that process.

### Session refresh

Google can rotate a browser session while redwax keeps an older exported copy.
A rejected session starts one shared import from the remembered browser and
profile. This includes a signed-out response sent with HTTP 200. Account and
page reads retry once after a successful import.

If the import fails or YouTube rejects the refreshed login, redwax signs out,
removes its saved credentials and opens the account picker. Sign in to YouTube
Music in your browser, then refresh the browser logins and choose an account.
You can also run `redwax login` and restart redwax. An ordinary page or playback
network failure does not start recovery or sign you out.

Redwax does not automatically replay an ambiguous playlist or library mutation.
Sign out blocks further automatic imports until you choose an account again.
Playback and personal files remain in place.

### `redwax sync`

```
redwax sync [--dry-run] [--config PATH]
```

This command requires a Firefox login. Other browsers support normal library
access and playback, but automatic download-mark sync stays off for them.

Reads the download marks the web app stores in the Firefox profile, expands
marked playlists and albums to their tracks, downloads what `library_dir` does
not hold at the best tier, and moves aside files that lost their mark once the
grace period has passed. `--dry-run` prints the plan and changes nothing.

The same pass runs inside the app 20 s after the first frame and every 15
minutes after that. `[downloads] enabled = false` turns it off. Nothing is
deleted: an unmarked file moves to `.evicted` under the library.

### `redwax-resolve`

```
redwax-resolve <video_id> [--quality high|fast] [--only signed-in|anonymous|yt-dlp]
               [--cookies PATH] [--authuser N] [--page-id ID] [--cache-dir DIR]
               [--dump-json FILE] [--trace] [--show-target]
redwax-resolve --health [<video_id>]
redwax-resolve --native-canary
```

One line per attempt, `<source> <tier> <elapsed_ms> <detail>`, then a `RESULT`
line. `--only` runs one step instead of the chain. `--trace` adds a
`trace <phase> <ms>` line per phase. `--dump-json` writes the scrubbed player
response shape with scalar values redacted and private file permissions.
`--show-target` explicitly prints the stream URL. That URL can contain sensitive
query values, so do not share its output.

Playback tries signed-in native audio, standard native audio, then yt-dlp.
`high` allows one second for signed-in audio. `fast` allows 250 milliseconds.
The full playback attempt has a 12-second deadline. A native outage puts
signed-in playback in a short cooldown while standard native audio stays
available. The player bar shows the actual tier.

Signed-in repair runs in the background after standard native audio succeeds.
Repair is limited to one task at a time and one start every 30 seconds.

`--native-canary` checks signed-in resolution and latency without cache or
fallback success masking a failure. See [Native canary](native-canary.md) for
the recurring monitor and automatic installation gate.

### `redwax-play`

```
redwax-play [--ao null] [--prefetch] [--skip-after SECS] <video_id>...
```

Plays the ids in order and prints one timestamped line per event. `--prefetch`
resolves the ids after the first while the first plays, so the `audible` line
after `next` measures the mpv floor. `--ao null` decodes without sound, which
is what a measurement wants.

### Scripts

```
./install.sh [--release TAG] [--no-deps] [--with-provider] [--timer] [--native-timer]
./install.sh --build [--no-deps] [--with-provider] [--timer] [--native-timer]
./install.sh --uninstall
scripts/install_resolver.sh [--with-provider]
scripts/measure.sh launch [binary] | resolve <id> | play <id>...
scripts/rust_lint.sh [--fix]
scripts/fixtures.sh [--check] [--from-raw]
```

The default install uses a checksum-verified prebuilt Linux release. `--release`
selects a tag instead of the latest release. `--build` compiles the current
checkout and installs it, including your local changes. Only source builds need
Rust and C build tools. `--no-deps` leaves dependency provisioning to you.

## The mouse

Mouse reporting is on for as long as redwax holds the terminal. Every target is
a region the last frame drew, so nothing answers a click that is not painted.

| where | left | right | middle |
|---|---|---|---|
| a track row | play it, and the shelf it sits in | add it to the queue | play it next |
| an album, playlist or artist row | open it | queue all its tracks | play them next |
| a header tab | switch to it | | |
| a hint in the bottom bar | do what its key does | | |
| a crumb | go back to that page | | |
| the seek bar | seek to that fraction | | |
| the volume bar | set that level | | |
| the volume icon | mute or unmute | | |
| shuffle or repeat | toggle or cycle it | | |
| an up next line | jump to that track | | |
| a timed lyric line | play from where it starts | | |
| the search box | focus it | | |
| an equaliser column | select the band and set its gain from the height | | |

A click moves the cursor onto the row first, so the highlight follows the
pointer. The wheel moves the cursor three rows in the list under the pointer,
and scrolls the lyrics on the lyrics tab.

## Playlist edits

`P` opens a picker over the right of the screen listing the playlists your
library holds. It adds the track that plays. When the highlighted row is
something else, the picker names it under the track and `tab` swaps it in: a
track, or every track of an album or playlist. With nothing playing, the
highlighted row is the only choice. `j` and `k` move, `enter` adds and
`escape` cancels. `Ctrl+P` opens the picker directly on the highlighted item.
Generated mixes and collection entries are not in the list because the edit
endpoint does not answer for them.

Saving a song already in the destination reports that it is already there.
An album or playlist can contain songs already saved in the destination.
If YouTube refuses that batch, redwax checks the destination and retries
only the missing songs. Ordinary saves need no extra playlist lookup.

The last row, `+ New playlist`, asks for a name. `enter` makes a private
playlist holding what the picker was adding, and `escape` goes back to the
list. The library is fetched again after, so the next picker lists it.

`D` on the page of an open playlist takes the highlighted track out of it. The
row leaves the list at once and goes back if YouTube refuses. Anywhere else,
`D` opens the same picker to take the track that plays out of the playlist you
choose, with the highlighted track a `tab` away. The chosen playlist is read
first, and every row of that track leaves it.

YouTube names one row of a playlist by a set video id it serves only for a
playlist the account may edit, so a playlist saved from somebody else answers
with a toast and no edit.

`L` likes or unlikes the track that plays, and `+` the highlighted track. Each
key names one track, so neither has to guess.

## The equaliser panel

`e` opens it down the right of the screen with a rule between it and the page,
and `e` or `escape` closes it. It takes only the rows it fills, so the page
shows through under it. It draws one column per band of ffmpeg's
`superequalizer`, eighteen of them, with the centre of each under it: 65, 92,
131, 185, 262, 370, 523, 740 Hz, then 1k, 1k4, 2k, 2k9, 4k1, 5k9, 8k3, 11k8,
16k7 and 20k. The column the keys are on is in the accent and the rest are in
the gutter. A rule across the panel a quarter of the way up marks the gain that
changes nothing.

| key | action |
|---|---|
| `h` `l`, `left` `right` | previous, next band |
| `H` `L` | jump five bands |
| `k` `j`, `up` `down` | raise, lower the band by 0.1 |
| `0` | flatten the band |
| `r` | flatten every band |
| `p` | next preset |
| `g` | genre matching on or off |
| `space` | equaliser on or off |
| `e`, `escape` | close the panel |

A gain is the filter's own multiplier and not a decibel figure: 1.00 is flat,
0 is silence in that band and 20 is the ceiling. The presets are `flat`,
`bass`, `treble`, `vocal`, `loudness` and `warm`, and each stays between 0.5
and 2.5 because the top of the range is violent. A gain the keys do not match
to a preset reads as `custom`.

Every change sends the graph to mpv over the socket it already holds, so a band
is heard while the panel is open. A flat equaliser sends no filter at all,
which leaves mpv running the plain chain. A band raised while the equaliser is
off switches it on, because a band that moves and is not heard reads as a
broken panel. The settings are written to `~/.cache/redwax/eq.json` and read
back on the next run, ahead of the `eq` table in the config.

The panel also answers the pointer: a click on a column selects that band and
sets its gain from the height the pointer landed at, and the wheel over a
column raises and lowers it.

## Genre matching

`g` in the panel, or `eq.genre_match` in the config. YouTube Music serves no
genre on any response redwax reads, so the answer comes from the iTunes Search
API, which needs no key and no account. The query carries the artist and the
title and nothing else.

A track change starts the lookup in the background, the way the durations fill
does. Nothing waits for it and nothing is said when there is no answer. What
comes back is written to `~/.cache/redwax/genres.json` under the video id, so a
track costs one request ever. The panel shows the genre and the preset it chose.

| genre | preset |
|---|---|
| electronic, dance, house, techno, bass, dubstep | `bass` |
| rock, metal, punk, alternative | `loudness` |
| hip hop, rap, r&b, soul | `bass` |
| classical, jazz, blues, folk, acoustic | `warm` |
| pop, soundtrack, country | `vocal` |

The match is on a substring, without case and with punctuation read as a space,
so `Hip-Hop/Rap` and `Alternative Rock` both land. A genre in no row leaves the
equaliser where it is. A band you set yourself stops the matching for the rest
of the session, so the app never fights you, and the panel says so while it is
on.

## The lyrics tab

`5` opens it, for the track that plays. The first row is a progress rule with
the elapsed time at its left and the total at its right, and its head marks
where in the track the words are.

When YouTube has the line timings, every line takes a row of its own. The line
the track is on is bold in the accent with a rule at its left edge, the two
lines either side of it are in the foreground, and the rest are muted. The
panel scrolls itself so that line rests a third of the way down, and an
instrumental gap shows as the note YouTube sends. A click on a line plays from
where it starts.

`j`, `k`, `g g`, `G`, Page Up, Page Down and the wheel scroll the panel. The panel
stops following the track for five seconds after that, then picks it up again.

A track YouTube has only the plain words for keeps the same rule over the same
text, with no line lit, because there is nothing to light it against.

## The track grid

A shelf that holds tracks carries the column names over it, on the same
boundaries and the same hairline seams as the rows under it: `TITLE`, `ARTIST`,
`ALBUM` and `TIME` from 72 cells of text, `TITLE`, `ARTIST` and `TIME` from 40.
A narrower pane shows one column and names nothing. The cursor steps over the
row, as it does over a shelf title.

## The big title

`art_height` sets the rows the cover takes, 22 by default and clamped to 6
and 30. The queue box takes the column under it, so a smaller cover shows
more of the queue.

The sidebar shows the art, the title, the artist and the album, then the keys
in a box, then the queue in a box on the bottom edge.

## The keys box

Every hint is in a box in the sidebar, above up next, listed down two columns
with the key in the accent and what it does beside it. A click on a hint does
what its key does. The box takes the rows it needs and up next keeps what is
left, so a short terminal shows fewer tracks under it. A box with too few rows
for every hint drops the last rows, and `?` still lists them all.

A terminal too narrow for the sidebar has no box, and the hints go back to a
single row along the bottom of the screen.

## Animation

Set `animations = false` in the config for a still screen. This disables the
playing indicator, scrolling track titles and fading notifications.

## Playback timings

Normal launches write playback timings to
`~/.cache/redwax/playback-timings.jsonl`. Quit and restart after an update to
capture timings from the new binary. Use the app normally until a slow
selection occurs. The log records input, stream resolution and mpv loading
so that a delay can be traced before changing playback code.

Each JSON line has a session, component, request, track reference, phase and
elapsed time in milliseconds. Resolver decisions can add an `evidence` field.
It contains fixed response-shape facts and never raw response values. Request
numbers belong to one component. Track references are hashed per session. The
log contains no song titles, stream URLs, cookies or account headers.
Foreground requests and background prefetches have separate roles.

| phase | what the time measures |
|---|---|
| `play.request`, `jump.request` | start of a selected playback request |
| `resolve.cache_hit`, `resolve.cache_miss` | whether the player already has a usable stream |
| `semaphore.wait`, `inflight.join_wait` | waiting for resolver capacity or an existing lookup |
| resolver source and network phases | stream lookup, token, player request, script and solver work |
| `native.response`, `native.format`, `native.challenge` | player response, format and solver input shape |
| `cdn.result`, `step.result` | CDN status and the fixed resolver outcome category |
| `token.binding`, `token.binding_page` | where the token binding was found, never its value |
| `session.refresh` | refreshing the browser session before a retry |
| `queue.wait` | waiting to change the mpv queue |
| `queue.commands_done` | sending the queue and playback commands to mpv |
| `file_loaded` | elapsed time from selection until mpv loads the selected entry |
| `playing_approx` | elapsed time from selection until mpv reports unpaused playback progress |
| `frame.slow`, `input.handler_slow`, `event.handler_slow`, `event.drain_slow` | UI work that takes at least 20 milliseconds |
| `tick.late` | a UI tick that runs at least 100 milliseconds late |

`playing_approx` is a player signal, not a measurement of sound reaching the
speakers. Resolve totals contain their nested phases, so do not add every
duration together. Cancelled requests retain completed phases and a player
cancellation marker. They can lack a resolver total.

Disk writes run asynchronously. If the bounded logging queue fills, playback
continues and the next record reports how many records were dropped. The
log rotates at 2 MiB and keeps one previous file, `playback-timings.jsonl.1`.
Set `REDWAX_PLAYBACK_TIMINGS=0` to disable this log.

## Environment variables

| variable | read by | what it does |
|---|---|---|
| `REDWAX_CONFIG` | `redwax` | config file path, same as `--config` |
| `REDWAX_NO_RESOLVER` | `redwax` | start with no resolver, so mpv resolves every entry itself |
| `REDWAX_PLAYBACK_TIMINGS` | `redwax` | set to `0` to disable the bounded playback timing log |
| `REDWAX_NODE` | `resolve` | the Node.js binary, before PATH |
| `REDWAX_YTDLP` | `resolve`, `redwax-play` | the yt-dlp binary, before PATH |
| `REDWAX_YTDLP_ARGS` | `redwax-play` | extra arguments for its yt-dlp call, space separated |
| `REDWAX_POT_PROVIDER` | `resolve` | the bgutil token provider server directory |
| `REDWAX_SOLVER_LIBRARY` | `resolve` | override the signature solver library |
| `REDWAX_SOLVER_CORE` | `resolve` | override the signature solver core |
| `XDG_CONFIG_HOME` | `marks` | where the Firefox profile is looked for first |
| `REDWAX_BIN_DIR` | `install.sh` | override the installed binary directory |
| `REDWAX_UNIT_DIR` | `install.sh` | where the units go, default `~/.config/systemd/user` |
| `CARGO_TARGET_DIR` | the build and the scripts | the cargo target directory |
| `HOME` | everything | the root of every default path |
| `XDG_CONFIG_HOME` | `redwax` | the config root, default `~/.config` |
| `XDG_CACHE_HOME` | `redwax` | the cache root, default `~/.cache` |
| `XDG_RUNTIME_DIR` | `player` | where the mpv IPC socket goes |
| `GHOSTTY_RESOURCES_DIR` | `redwax` | where the Ghostty theme files live |
| `TMUX` | `redwax` | its presence selects the tmux art path |
| `TERM`, `TERM_PROGRAM` | `redwax` | which art protocol the terminal gets |

## Files

| path | what it holds |
|---|---|
| `~/.config/redwax/config.toml` | the application config |
| `~/.config/redwax/browser.json` | the session headers written by `redwax login` |
| `~/.config/redwax/cookies.txt` | the same session in Netscape form, for yt-dlp |
| `~/.config/redwax/session.json` | the browser, profile and identity a refresh reuses |
| `~/.cache/redwax/pages/` | the page cache the first frame draws |
| `~/.cache/redwax/durations.json` | track lengths YouTube left off a row |
| `~/.cache/redwax/eq.json` | the equaliser gains and whether it is on |
| `~/.cache/redwax/genres.json` | the genre found for a track, for the preset it picks |
| `~/.cache/redwax/art/` | album art |
| `~/.cache/redwax/state.json` | the queue, position, volume, repeat and shuffle |
| `~/.cache/redwax/streams.json` | resolved stream URLs, until they expire |
| `~/.cache/redwax/playback-timings.jsonl` | playback latency phases with hashed track references |
| `~/.cache/redwax/innertube.json` | the client version, player id and visitor id |
| `~/.cache/redwax/solver/` | the yt-dlp solver library and core script |
| `~/.cache/redwax/player/` | preprocessed player scripts, one per version |
| `~/.cache/redwax/dumps/` | redacted response shapes no parser could read |
| `~/Music/redwax/` | the download library, `library_dir` in the config |
| `~/Music/redwax/.evicted/` | files whose mark went away, kept rather than deleted |
