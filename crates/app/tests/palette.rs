//! The nine `test_palette.py` cases, name for name.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use redwax::palette::{Palette, read_foot, read_ghostty};
use redwax::theme::{redwax_theme, theme_from_palette, tokyo_night};

const GHOSTTY: &str = "\
# comment
font-family = Cascadia Mono NF
background = 1a1b26
foreground = c0caf5
selection-background = 292e42
palette = 0=#16161e
palette = 1=#f7768e
palette = 2=#9ece6a
palette = 3=#e0af68
palette = 4=#7aa2f7
palette = 5=#bb9af7
palette = 6=#7dcfff
palette = 7=#a9b1d6
palette = 8=#414868
palette = 9=#f7768e
palette = 10=#9ece6a
palette = 11=#e0af68
palette = 12=#7aa2f7
palette = 13=#bb9af7
palette = 14=#7dcfff
palette = 15=#c0caf5
";

/// One directory per test, so the cases never see each other's files.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("redwax-palette-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test owns this directory");
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("the test owns this file");
}

#[test]
fn test_ghostty_config_becomes_the_rice_palette() {
    let dir = temp_dir("rice");
    let path = dir.join("config");
    write(&path, GHOSTTY);
    let expected = Palette {
        source: Some(path.clone()),
        background: "#1a1b26".into(),
        foreground: "#c0caf5".into(),
        ansi: tokyo_night().ansi,
        selection_background: Some("#292e42".into()),
    };
    assert_eq!(read_ghostty(&path), Some(expected));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_ghostty_theme_file_is_a_base_the_config_overrides() {
    let dir = temp_dir("theme");
    std::fs::create_dir_all(dir.join("themes")).expect("the test owns this directory");
    write(&dir.join("themes").join("Night"), GHOSTTY);
    // No other test in this binary reads the environment, so setting it is safe.
    unsafe { std::env::set_var("GHOSTTY_RESOURCES_DIR", &dir) };
    let path = dir.join("config");
    write(&path, "theme = Night\npalette = 4=#ff0000\n");
    let palette = read_ghostty(&path).expect("the theme supplies every colour");
    assert_eq!(palette.background, "#1a1b26");
    assert_eq!(palette.ansi[4], "#ff0000");
    unsafe { std::env::remove_var("GHOSTTY_RESOURCES_DIR") };
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_ghostty_config_file_include_wins_over_the_files_own_keys() {
    let dir = temp_dir("include");
    write(
        &dir.join("theme.conf"),
        "background = 282828\npalette = 4=#7daea3\n",
    );
    let path = dir.join("config");
    write(&path, &format!("config-file = ?theme.conf\n{GHOSTTY}"));
    let palette = read_ghostty(&path).expect("the file supplies every colour");
    assert_eq!(
        (palette.background.as_str(), palette.ansi[4].as_str()),
        ("#282828", "#7daea3")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_foot_ini_is_read_too() {
    let dir = temp_dir("foot");
    let path = dir.join("foot.ini");
    let ansi = tokyo_night().ansi;
    let mut lines = vec![
        "[colors]".to_string(),
        "background=1a1b26".to_string(),
        "foreground=c0caf5".to_string(),
        "selection-background=292e42".to_string(),
    ];
    for index in 0..8 {
        lines.push(format!("regular{index}={}", &ansi[index][1..]));
        lines.push(format!("bright{index}={}", &ansi[index + 8][1..]));
    }
    write(&path, &(lines.join("\n") + "\n"));
    assert_eq!(read_foot(&path).map(|p| p.ansi), Some(ansi));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_incomplete_config_gives_nothing() {
    let dir = temp_dir("incomplete");
    let path = dir.join("config");
    write(&path, "background = 1a1b26\n");
    assert_eq!(read_ghostty(&path), None);
    assert_eq!(read_foot(&dir.join("missing.ini")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_redwax_palette_colors_still_require_the_hash() {
    let mut palette = tokyo_night();
    palette.background = "1a1b26".into();
    assert!(palette.validated().is_none());
}

#[test]
fn test_theme_maps_surfaces_from_the_palette() {
    let theme = theme_from_palette(&tokyo_night(), "redwax");
    assert_eq!(
        (theme.background, theme.surface, theme.panel, theme.accent),
        (
            Color::Rgb(0x1a, 0x1b, 0x26),
            Color::Rgb(0x16, 0x16, 0x1e),
            Color::Rgb(0x29, 0x2e, 0x42),
            Color::Rgb(0x7a, 0xa2, 0xf7),
        )
    );
    assert_eq!(theme.gutter, Color::Rgb(0x41, 0x48, 0x68));
    assert_eq!(theme.dim, Color::Rgb(0xa9, 0xb1, 0xd6));
    assert_eq!(theme, redwax_theme());
}

#[test]
fn test_light_black_palette_still_gets_a_darker_surface() {
    let mut ansi = tokyo_night().ansi;
    ansi[0] = "#45475a".into();
    ansi[8] = "#585b70".into();
    let catppuccin_like = Palette {
        source: Some(PathBuf::from("x")),
        background: "#1e1e2e".into(),
        foreground: "#cdd6f4".into(),
        ansi,
        selection_background: None,
    };
    let theme = theme_from_palette(&catppuccin_like, "terminal");
    assert_ne!(theme.surface, Color::Rgb(0x45, 0x47, 0x5a));
    assert_ne!(theme.panel, theme.background);
    // The only case that darkens and lightens, pinned to what Python prints.
    assert_eq!(
        (theme.surface, theme.panel, theme.gutter, theme.muted),
        (
            Color::Rgb(0x12, 0x11, 0x21),
            Color::Rgb(0x2f, 0x2e, 0x3f),
            Color::Rgb(0x58, 0x5b, 0x70),
            Color::Rgb(0x6f, 0x73, 0x8a),
        )
    );
}

#[test]
fn test_oled_black_background_keeps_ansi_black_as_the_raised_surface() {
    let night = tokyo_night();
    let oled = Palette {
        source: Some(PathBuf::from("x")),
        background: "#000000".into(),
        foreground: night.foreground.clone(),
        ansi: night.ansi.clone(),
        selection_background: Some("#292e42".into()),
    };
    let theme = theme_from_palette(&oled, "terminal");
    assert_eq!(
        (theme.background, theme.surface, theme.panel),
        (
            Color::Rgb(0x00, 0x00, 0x00),
            Color::Rgb(0x16, 0x16, 0x1e),
            Color::Rgb(0x29, 0x2e, 0x42),
        )
    );
}

#[test]
fn test_with_accent_only_swaps_the_accent() {
    let theme =
        theme_from_palette(&tokyo_night(), "terminal").with_accent(Color::Rgb(0xff, 0x00, 0x00), "art");
    assert_eq!(theme.name, "art");
    assert_eq!(theme.accent, Color::Rgb(0xff, 0x00, 0x00));
    assert_eq!(theme.background, Color::Rgb(0x1a, 0x1b, 0x26));
}

/// Prints the palette this machine's Ghostty config gives, so a change in the
/// dotfiles can be seen. Run with
/// `cargo test -p redwax --test palette -- --ignored --nocapture`.
#[test]
#[ignore = "live check, reads the real Ghostty config"]
fn live_terminal_palette() {
    let palette =
        redwax::palette::terminal_palette(Some("xterm-ghostty")).expect("this machine has a Ghostty config");
    println!("source {:?}", palette.source);
    println!("background {}", palette.background);
    println!("foreground {}", palette.foreground);
    println!("selection {:?}", palette.selection_background);
    for (index, color) in palette.ansi.iter().enumerate() {
        println!("ansi {index:>2} {color}");
    }
    println!("{:?}", theme_from_palette(&palette, "terminal"));
}
