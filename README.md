# redwax

A terminal YouTube Music player in Rust, with browser login, playlists,
search, a queue, album art, synced lyrics and an eighteen-band equaliser.
Playback uses mpv. Premium accounts can request higher-quality audio when
YouTube makes it available.

## Requirements

Linux is the supported platform. Other operating systems are not tested.
The installer supports apt, dnf and pacman distributions. It uses sudo when
system packages are missing.

Before running the installer, you need curl and Python 3.10 or newer.
You need Git if you clone the repository, and a YouTube Music account signed in
through a supported browser. The default installer downloads a prebuilt redwax
and sets up Node.js, mpv, yt-dlp and the native signature solver.
Rust and C build tools are needed only when you build from source.

Firefox, Chrome, Chromium, Brave, Edge, Opera, Vivaldi and Whale login use
yt-dlp. Encrypted browser cookies can require an unlocked desktop keyring.
Download-mark sync currently reads Firefox storage only.

Album art uses the terminal image protocol when available and half blocks
otherwise. Set `icons = "text"` if you do not use a Nerd Font.

## Install

```sh
git clone https://github.com/SamRichardsonDEV/redwax
cd redwax
./install.sh
```

If no binary release has been published yet, use `./install.sh --build`.

The installer verifies the release checksum and puts `redwax` on your PATH.
It installs playback dependencies when they are missing.
If it adds a PATH entry to your shell configuration,
open a new terminal after installation. Launch with:

```sh
redwax
```

The managed resolver lives under `~/.local/share/redwax/yt-dlp`. redwax finds
it automatically. Its primary packages are pinned and checksum verified.
The installer does not need a YouTube account to build or install the app.

The installer supports prebuilt Linux x86_64 and ARM64 releases.
A download or checksum failure stops installation. To install a specific release, use
`./install.sh --release v1.0.0` after that version is published.
`./install.sh --no-deps` skips dependency provisioning.

You can also download an archive and its `.sha256` file from
[Releases](https://github.com/SamRichardsonDEV/redwax/releases).
For an x86_64 archive, check and install it with:

```sh
sha256sum --check redwax-x86_64-unknown-linux-musl.tar.gz.sha256
tar -xzf redwax-x86_64-unknown-linux-musl.tar.gz
cd redwax
./install.sh
```

For ARM64, use the `aarch64` archive instead. An extracted archive installs its
included binaries after checking their hashes. This path needs no Git or Rust.

### Build from source

Use this path to install your local changes:

```sh
./install.sh --build
```

The installer sets up the build tools and runs a locked release build.
The first build can take several minutes. Later builds reuse Cargo output.
For a prepared toolchain, use `./install.sh --build --no-deps`.
Manual builds need Rust 1.98 or newer and Node.js 22 or newer.
After each source change, run the same install command to update `redwax`.

`./install.sh --uninstall` removes the installed binaries and timer units,
while retaining configuration and caches.

The proof-token provider is optional:

```sh
./install.sh --with-provider
```

The pinned provider currently has unresolved npm dependency advisories,
including a critical advisory. It is excluded from the default install.
Playback can use the remaining resolver sources when the provider is absent.
See [native checks](docs/native-canary.md) for checking signed-in quality and
latency independently of fallback playback.

## First run

Open [YouTube Music](https://music.youtube.com) in your browser and sign in.
Then run:

```sh
redwax login
redwax
```

`redwax setup` checks local dependencies and saved login files. Missing requirements
make it exit with status 1. Login confirms the session with YouTube.
On first startup, redwax can also import a browser session automatically.

Choose a browser, profile or account explicitly when needed:

```sh
redwax login --browser chrome
redwax login --browser chrome --profile 'Profile 1'
redwax login --list-accounts
redwax login --account 2
```

Inside redwax, press `tab` to focus the account box at the top right, then
`enter` to open the account picker. Choose with arrows or `j` and `k`, then
press `enter` to switch. Press `escape` to cancel. Playback continues while
you switch accounts.

`redwax login` follows the account selected in your browser. Changing account
on the website alone does not switch a running redwax. Account listing leaves
your saved login unchanged. `--account` selects an identity explicitly.

redwax remembers that selection for later refreshes. Open Home and Library,
search for a song and play it. After quitting and relaunching, the saved queue
and position return paused.

## Keys

The Keys panel keeps the same two-column layout across tabs. Press `?` for
detailed help. Mouse clicks, scrolling and dragging the seek and volume bars
are supported. Right-click a row to queue it, or middle-click to play it next.

| Key | Action |
| --- | --- |
| `space` | Play or pause |
| `n` / `p` | Next or previous track |
| `hjkl` / arrows | Move the selection or switch tabs |
| `gg` / `G` | First or last row in the section |
| `{` / `}` | Previous or next section |
| `enter` | Play or open the selected row |
| `1` to `7` | Home, Search, Library, Queue, Lyrics, Themes, Settings |
| `/` | Find on the page or edit the search query |
| `q` | Open Queue on the playing track |
| `b` | Open the highlighted track album |
| `K` / `J` in Queue | Move the selected entry up or down |
| `r` | Start radio from a track |
| `+` | Like or unlike the highlighted track |
| `s` / `R` | Shuffle or cycle repeat mode |
| `-` / `=` / `m` | Lower volume, raise volume or mute |
| `,` / `.` | Jump back or forward five seconds |
| `P` | Add to a playlist |
| `e` | Open the equaliser |
| `ctrl+r` | Reload the tab or retry failed playback |
| `escape` | Back or close the current panel |
| `tab` | Focus the account box or return to the page |
| `Q` | Quit |

See [the usage guide](docs/usage.md) for the full bindings, playlist edits,
search editing, diagnostics and environment variables.

## Configuration and themes

The optional config file is `~/.config/redwax/config.toml`:

```toml
theme = "terminal"
icons = "text"
art = "auto"
quality = "high"
volume = 80
transparent = true
search_vim_mode = true
```

Press `6` to preview and choose themes. redwax includes ten themes, can read
custom palettes, and can follow Ghostty, foot or Omarchy colours.
See [themes](docs/themes.md) for custom files and dotfile links.

## Troubleshooting

```sh
redwax doctor
redwax doctor --live
```

The first command checks local setup. `--live` also reaches YouTube. Review
diagnostic output before sharing it, since local paths can contain your username.
The app records bounded [playback timings](docs/usage.md#playback-timings)
without song titles, account headers or stream URLs.

YouTube can change its responses or playback requirements. Successful setup
does not establish that every track or future YouTube change will work.

## Source layout

| Crate | Responsibility |
| --- | --- |
| `auth` | Cookie scope, account selection, request signing and private files |
| `ytm` | YouTube Music requests and response parsing |
| `resolve` | Local files, native stream resolution and yt-dlp fallback |
| `player` | mpv playback and queue control over IPC |
| `marks` | Firefox download marks |
| `app` | Terminal interface and application state |

To check a local build:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests use offline fixtures by default. Live checks are ignored unless requested.
The installation tests need mpv, compatible yt-dlp and Node.js 22 or newer.

External pull requests are not accepted. redwax is not affiliated with YouTube
or Google.
