# AGENTS.md

Instructions for coding agents working on redwax, or setting redwax up on
someone's machine.

redwax is a terminal YouTube Music player in Rust. Six crates: `ytm` talks to
YouTube, `resolve` turns a video id into a stream URL, `player` drives mpv over
its IPC socket, `marks` reads the browser's stored library, `app` is the
interface. `auth` owns cookie scope, account selection, and request signing.

---

## Task: link a user's dotfiles to redwax themes

This is the most common setup request. Read all of it before running anything.

### What you need to know first

A redwax theme is a TOML file holding a sixteen colour palette. redwax lists
`~/.config/redwax/themes/` and reads every `.toml` in it. Nothing treats a
symlink specially, so a link is all that is needed. There is no import step
and no registration.

A file that is missing, unreadable, or short of sixteen colours is skipped
rather than fatal. One broken theme never costs the user the others.

### Steps

1. Find where the user keeps their theme files. Ask if it is not obvious.
   Common locations are `~/dotfiles/`, `~/.dotfiles/` and `~/src/dotfiles/`.

2. Check what format those files are in. redwax reads its own TOML shape,
   shown below. If the user's files are Alacritty, kitty, Ghostty or foot
   configs, convert them: every one of those formats is the same sixteen
   colours under different key names.

3. Create the themes directory if it is absent:

   ```sh
   mkdir -p ~/.config/redwax/themes
   ```

4. Link one theme:

   ```sh
   ln -s ~/dotfiles/redwax/themes/mine.toml ~/.config/redwax/themes/mine.toml
   ```

   If every theme the user has lives in one directory, link the directory
   instead. Do this only when `~/.config/redwax/themes` does not already
   exist, because linking over a real directory loses what is in it:

   ```sh
   ln -s ~/dotfiles/redwax/themes ~/.config/redwax/themes
   ```

5. Verify. The theme must appear in the list:

   ```sh
   redwax doctor | grep config
   ```

   Then open redwax and press `6`. A linked theme shows with the origin
   "your file". If it is absent, the file did not parse. Go to step 7.

6. Set it as the default. Either press `enter` on it in the themes tab, which
   writes the config line, or edit `~/.config/redwax/config.toml`:

   ```toml
   theme = "mine"
   ```

   The name is the filename without `.toml`.

7. If a theme does not appear, check it in this order:

   - The file ends in `.toml`.
   - `ansi` holds exactly sixteen entries. Fifteen or seventeen is skipped.
   - Every colour is `#rrggbb`. Three digit hex is not read.
   - The symlink resolves: `readlink -f ~/.config/redwax/themes/mine.toml`.

### The file format

```toml
background = "#101014"
foreground = "#e6e6ea"
selection_background = "#22222c"   # optional
ansi = [
  # black, red, green, yellow, blue, magenta, cyan, white
  "#101014", "#e06c75", "#98c379", "#e5c07b",
  "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
  # the same eight, bright
  "#4b5263", "#e06c75", "#98c379", "#e5c07b",
  "#61afef", "#c678dd", "#56b6c2", "#e6e6ea",
]
```

Only `background`, `foreground` and `ansi` are required. Everything redwax
paints is worked out from those.

### When not to link anything

Three cases need no theme file at all. Check these before writing one.

- The user wants redwax to match their terminal. Set `theme = "terminal"`.
  redwax reads the terminal's own config directly. Ghostty and foot are
  supported.
- The user runs Omarchy. Set `theme = "omarchy"`. redwax reads
  `~/.config/omarchy/current/theme/colors.toml`, so redwax follows the system
  theme whenever it changes.
- The user names a theme redwax already ships: `tokyo-night`,
  `catppuccin-mocha`, `catppuccin-latte`, `gruvbox-dark`, `nord`, `dracula`,
  `rose-pine`, `everforest-dark`, `kanagawa`, `solarized-dark`.

---

## Repository rules

1. Never commit a cookie, a token, or an account's own identifiers. Fixtures
   are real YouTube responses and arrive carrying whoever captured them. Run
   the guard before committing any:

   ```sh
   ./scripts/check_fixtures.py --handle THEIRS --channel UCtheirs
   ```

2. Comments say why, not what. A comment that paraphrases the line under it is
   a defect. Name the constraint, not the behaviour.

3. No em dashes, no semicolons and no contractions in comment or documentation
   prose. This is house style and it is checked.

4. A new type earns its place by hiding a rule, not by holding fields. A
   struct that hides nothing is interface cost with no benefit behind it.

5. Never add AI attribution trailers to commits, pull requests or code.
