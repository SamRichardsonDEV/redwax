//! The terminal's own colours, read from its config file, so redwax follows
//! the rice. Ghostty and foot keep their palettes in the dotfiles, so reading
//! those files means a change there changes redwax too. Ported from the
//! Python `palette.py`, whose tests are the specification (`test_palette.py`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

/// Ghostty stops following `theme` and `config-file` at this depth, so a pair
/// of files that include each other terminates.
const MAX_DEPTH: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// The file it came from, `None` for the built-in fallback.
    pub source: Option<PathBuf>,
    /// `#rrggbb`, lower case, as every colour here.
    pub background: String,
    pub foreground: String,
    /// The 16 ANSI colours in order.
    pub ansi: [String; 16],
    pub selection_background: Option<String>,
}

/// A palette whose colours have all crossed the external input boundary.
/// Theme construction can use it without fallback colours or parse errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPalette {
    source: Option<PathBuf>,
    background: HexColor,
    foreground: HexColor,
    ansi: [HexColor; 16],
    selection_background: Option<HexColor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(u8, u8, u8);

impl HexColor {
    pub fn parse(hex_text: &str) -> Option<HexColor> {
        let hex_digits = hex_text.trim().strip_prefix('#')?;
        if hex_digits.len() != 6 || !hex_digits.chars().all(|character| character.is_ascii_hexdigit()) {
            return None;
        }
        let rgb_number = u32::from_str_radix(hex_digits, 16).ok()?;
        Some(HexColor(
            (rgb_number >> 16) as u8,
            (rgb_number >> 8) as u8,
            rgb_number as u8,
        ))
    }

    pub fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }

    pub fn components(self) -> (u8, u8, u8) {
        (self.0, self.1, self.2)
    }
}

impl Palette {
    /// Validate all colours together. A theme with one malformed colour is
    /// omitted instead of carrying a hidden black fallback into the UI.
    pub fn validated(&self) -> Option<ValidatedPalette> {
        let validated_ansi = self
            .ansi
            .iter()
            .map(|ansi_colour| HexColor::parse(ansi_colour))
            .collect::<Option<Vec<_>>>()?
            .try_into()
            .ok()?;
        Some(ValidatedPalette {
            source: self.source.clone(),
            background: HexColor::parse(&self.background)?,
            foreground: HexColor::parse(&self.foreground)?,
            ansi: validated_ansi,
            selection_background: match self.selection_background.as_deref() {
                Some(colour) => Some(HexColor::parse(colour)?),
                None => None,
            },
        })
    }
}

impl ValidatedPalette {
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn background(&self) -> HexColor {
        self.background
    }

    pub fn foreground(&self) -> HexColor {
        self.foreground
    }

    pub fn ansi(&self, color_index: usize) -> Option<HexColor> {
        self.ansi.get(color_index).copied()
    }

    pub fn selection_background(&self) -> Option<HexColor> {
        self.selection_background
    }
}

/// The palette of the terminal named by `TERM` or `TERM_PROGRAM`, when redwax
/// knows its config format: Ghostty (`$XDG_CONFIG_HOME/ghostty/config`) or
/// foot (`$XDG_CONFIG_HOME/foot/foot.ini`).
pub fn terminal_palette(terminal_name: Option<&str>) -> Option<Palette> {
    let lowercase_terminal_name = terminal_name.unwrap_or_default().to_ascii_lowercase();
    if lowercase_terminal_name.contains("ghostty") {
        return read_ghostty(&config_home().join("ghostty").join("config"));
    }
    if lowercase_terminal_name.contains("foot") {
        return read_foot(&config_home().join("foot").join("foot.ini"));
    }
    None
}

/// Ghostty's `key = value` file with `theme` and `config-file` resolved the
/// way Ghostty does: a theme is a base the file's own keys override, and
/// `config-file` includes are applied after the whole file, so their keys win.
/// Themes are looked up in `$XDG_CONFIG_HOME/ghostty/themes/<name>` then
/// `$GHOSTTY_RESOURCES_DIR/themes/<name>` (default `/usr/share/ghostty`).
/// `dark:X,light:Y` picks the dark one. Include depth is capped at 2.
pub fn read_ghostty(path: &Path) -> Option<Palette> {
    let resolved_values = ghostty_values(path, 0)?;
    build_palette(
        path,
        resolved_values.settings.get("background").map(String::as_str),
        resolved_values.settings.get("foreground").map(String::as_str),
        &resolved_values.ansi_by_index,
        resolved_values
            .settings
            .get("selection-background")
            .map(String::as_str),
    )
}

/// foot's ini `[colors]` section: `regular0..7`, `bright0..7`, `background`,
/// `foreground`, `selection-background`.
pub fn read_foot(path: &Path) -> Option<Palette> {
    let color_settings = ini_section(path, "colors")?;
    let mut ansi_by_index: HashMap<usize, String> = HashMap::new();
    for ansi_index in 0..8 {
        if let Some(color_value) = color_settings.get(&format!("regular{ansi_index}")) {
            ansi_by_index.insert(ansi_index, color_value.clone());
        }
        if let Some(color_value) = color_settings.get(&format!("bright{ansi_index}")) {
            ansi_by_index.insert(ansi_index + 8, color_value.clone());
        }
    }
    build_palette(
        path,
        color_settings.get("background").map(String::as_str),
        color_settings.get("foreground").map(String::as_str),
        &ansi_by_index,
        color_settings.get("selection-background").map(String::as_str),
    )
}

/// `#rrggbb` normalised to lower case with the hash, `None` for anything else.
pub fn hex_color(color_text: &str) -> Option<String> {
    let parsed_color = HexColor::parse(color_text)?;
    let (red, green, blue) = parsed_color.components();
    Some(format!("#{red:02x}{green:02x}{blue:02x}"))
}

/// Ghostty and foot accept six hex digits with or without a leading hash.
fn terminal_hex_color(color_text: &str) -> Option<String> {
    let color_text = color_text.trim();
    if color_text.starts_with('#') {
        hex_color(color_text)
    } else {
        hex_color(&format!("#{color_text}"))
    }
}

/// One Ghostty config file with its theme and its includes folded in. Every
/// `palette = N=#rrggbb` line merges per index, every other key replaces.
#[derive(Default)]
struct GhosttyValues {
    ansi_by_index: HashMap<usize, String>,
    settings: HashMap<String, String>,
}

fn ghostty_values(config_path: &Path, include_depth: u32) -> Option<GhosttyValues> {
    let config_text = std::fs::read_to_string(config_path).ok()?;
    let mut resolved_values = GhosttyValues::default();
    let mut include_paths: Vec<PathBuf> = Vec::new();
    for config_line in config_text.lines() {
        let config_line = config_line.trim();
        if config_line.is_empty() || config_line.starts_with('#') {
            continue;
        }
        let Some((setting_name, setting_value)) = config_line.split_once('=') else {
            continue;
        };
        let (setting_name, setting_value) = (setting_name.trim(), setting_value.trim());
        match setting_name {
            "palette" => {
                if let Some((index_text, color_value)) = setting_value.split_once('=')
                    && let Ok(color_index) = index_text.trim().parse::<usize>()
                {
                    resolved_values
                        .ansi_by_index
                        .insert(color_index, color_value.trim().to_string());
                }
            }
            "theme" if include_depth < MAX_DEPTH => {
                if let Some(base_theme_values) = ghostty_theme_values(setting_value, include_depth) {
                    resolved_values = fold_over(base_theme_values, resolved_values);
                }
            }
            "config-file" if include_depth < MAX_DEPTH => {
                let relative_include = setting_value.trim_matches('"').trim_start_matches('?');
                include_paths.push(
                    config_path
                        .parent()
                        .unwrap_or(Path::new(""))
                        .join(relative_include),
                );
            }
            _ => {
                resolved_values
                    .settings
                    .insert(setting_name.to_string(), setting_value.to_string());
            }
        }
    }
    for include_path in include_paths {
        if let Some(included_values) = ghostty_values(&include_path, include_depth + 1) {
            resolved_values = fold_over(resolved_values, included_values);
        }
    }
    Some(resolved_values)
}

/// `later` wins key by key, and its palette entries merge into the base rather
/// than replacing the whole palette.
fn fold_over(mut base_values: GhosttyValues, override_values: GhosttyValues) -> GhosttyValues {
    base_values.ansi_by_index.extend(override_values.ansi_by_index);
    base_values.settings.extend(override_values.settings);
    base_values
}

fn ghostty_theme_values(theme_setting: &str, include_depth: u32) -> Option<GhosttyValues> {
    let mut theme_name = theme_setting.to_string();
    for theme_variant in theme_name.clone().split(',') {
        // "dark:X,light:Y" picks per system appearance, and redwax is dark.
        if let Some((appearance, variant_name)) = theme_variant.split_once(':')
            && !variant_name.is_empty()
            && appearance.trim() == "dark"
        {
            theme_name = variant_name.trim().to_string();
        }
    }
    let resources_directory = match std::env::var("GHOSTTY_RESOURCES_DIR") {
        Ok(configured_directory) if !configured_directory.is_empty() => PathBuf::from(configured_directory),
        _ => PathBuf::from("/usr/share/ghostty"),
    };
    let candidate_paths = [
        config_home().join("ghostty").join("themes").join(&theme_name),
        resources_directory.join("themes").join(&theme_name),
    ];
    candidate_paths
        .iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| ghostty_values(candidate, include_depth + 1))
}

/// A palette needs a background, a foreground and all 16 ANSI colours, so a
/// half written config reads as no palette at all rather than as a broken one.
fn build_palette(
    source_path: &Path,
    background: Option<&str>,
    foreground: Option<&str>,
    ansi_by_index: &HashMap<usize, String>,
    selection_background: Option<&str>,
) -> Option<Palette> {
    let mut ansi_colors: Vec<String> = Vec::with_capacity(16);
    for ansi_index in 0..16 {
        ansi_colors.push(terminal_hex_color(ansi_by_index.get(&ansi_index)?)?);
    }
    Some(Palette {
        source: Some(source_path.to_path_buf()),
        background: terminal_hex_color(background?)?,
        foreground: terminal_hex_color(foreground?)?,
        ansi: ansi_colors.try_into().ok()?,
        selection_background: selection_background.and_then(terminal_hex_color),
    })
}

/// The named section of an ini file, keys lower cased, as Python's
/// `configparser` reads it. `None` when the file or the section is missing.
fn ini_section(ini_path: &Path, wanted_section: &str) -> Option<HashMap<String, String>> {
    let ini_text = std::fs::read_to_string(ini_path).ok()?;
    let mut current_section = String::new();
    let mut found_wanted_section = false;
    let mut section_values = HashMap::new();
    for ini_line in ini_text.lines() {
        let ini_line = ini_line.trim();
        if ini_line.is_empty() || ini_line.starts_with('#') || ini_line.starts_with(';') {
            continue;
        }
        if let Some(section_name) = ini_line
            .strip_prefix('[')
            .and_then(|section_without_prefix| section_without_prefix.strip_suffix(']'))
        {
            current_section = section_name.trim().to_string();
            found_wanted_section |= current_section == wanted_section;
            continue;
        }
        if current_section != wanted_section {
            continue;
        }
        let separator_index = [ini_line.find('='), ini_line.find(':')]
            .into_iter()
            .flatten()
            .min();
        if let Some(separator_index) = separator_index {
            section_values.insert(
                ini_line[..separator_index].trim().to_ascii_lowercase(),
                ini_line[separator_index + 1..].trim().to_string(),
            );
        }
    }
    found_wanted_section.then_some(section_values)
}

fn config_home() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(configured_home) if !configured_home.is_empty() => PathBuf::from(configured_home),
        _ => home().join(".config"),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
