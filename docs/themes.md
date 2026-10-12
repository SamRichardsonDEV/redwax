# Themes

redwax paints from a sixteen colour palette, which is the format the rest of
the terminal world already publishes themes in. Everything else, the accent,
the seams, the muted text, is worked out from those sixteen.

There are four places a palette can come from.

## Follow the terminal

```toml
theme = "terminal"
```

The default. redwax reads the palette out of your terminal's own config, so it
matches the window it is running in. Ghostty and foot are read directly.

## Follow Omarchy

```toml
theme = "omarchy"
```

Reads `~/.config/omarchy/current/theme/colors.toml`, which is the theme
Omarchy currently has applied. Change the system theme and redwax follows it
within a few seconds. The option only appears in the themes tab on a machine
that has that file.

## A theme redwax ships

```toml
theme = "gruvbox-dark"
```

Ten are built in: `tokyo-night`, `catppuccin-mocha`, `catppuccin-latte`,
`gruvbox-dark`, `nord`, `dracula`, `rose-pine`, `everforest-dark`, `kanagawa`,
`solarized-dark`.

## A theme of your own

Drop a file in `~/.config/redwax/themes/`:

```toml
# ~/.config/redwax/themes/mine.toml
background = "#101014"
foreground = "#e6e6ea"
selection_background = "#22222c"
ansi = [
  "#101014", "#e06c75", "#98c379", "#e5c07b",
  "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
  "#4b5263", "#e06c75", "#98c379", "#e5c07b",
  "#61afef", "#c678dd", "#56b6c2", "#e6e6ea",
]
```

Then `theme = "mine"`, or pick it in the themes tab.

The themes tab writes a source-qualified choice such as `file:mine` or
`builtin:nord`. Older bare names continue to work. A bare name that exists as
both a built-in theme and a file selects the built-in theme, so use the
`file:` prefix to select the file in that case.

### Keeping themes in your dotfiles

Nothing here treats a symlink specially, which is the point. redwax lists the
`themes/` directory and reads what it finds, so link yours in:

```sh
mkdir -p ~/.config/redwax/themes
ln -s ~/dotfiles/redwax/themes/mine.toml ~/.config/redwax/themes/mine.toml
```

If the themes directory does not exist, link the whole directory instead:

```sh
ln -s ~/dotfiles/redwax/themes ~/.config/redwax/themes
```

If you are pointing a coding agent at this, [AGENTS.md](../AGENTS.md) has the
same procedure written as steps.

A file that is missing, unreadable or short of sixteen colours is skipped
rather than fatal, so one bad theme never costs you the others.

## The themes tab

Press `6`. Moving the cursor paints the whole app in the theme it is on, so
the preview is the app itself rather than a swatch in a box. `enter` keeps the
choice and writes it into your config file, leaving every other line alone.
Leaving the tab without choosing puts back the one you had.
