//! Theme sources share a validated sixteen-colour palette.
//! File discovery follows symlinks and skips unreadable or malformed themes.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

use crate::config::Config;
use crate::palette::{Palette, ValidatedPalette};
use crate::theme::{Theme, theme_from_validated_palette};

pub const TERMINAL: &str = "terminal";

pub const OMARCHY: &str = "omarchy";

/// Where Omarchy keeps the palette of the theme it currently has applied. It
/// is a directory Omarchy rewrites on every theme change, so reading it at
/// startup is how redwax follows the system.
const OMARCHY_COLORS: &str = ".config/omarchy/current/theme/colors.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable source-qualified identity. The label can collide across sources.
    pub id: ThemeId,
    pub name: String,
    pub origin: Origin,
    /// `None` for a source that has to be read at the moment it is applied.
    pub palette: Option<ValidatedPalette>,
}

impl Entry {
    pub fn matches_choice(&self, choice: &str) -> bool {
        self.id == ThemeId::from_config(choice)
    }
}

/// A theme identity that survives duplicate labels from different sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThemeId {
    Terminal,
    Omarchy,
    Builtin(String),
    File(String),
}

impl ThemeId {
    /// Read qualified identities and the bare names written by older releases.
    /// A legacy collision keeps the old built-in-first resolution order.
    pub fn from_config(config_value: &str) -> ThemeId {
        if config_value == TERMINAL {
            ThemeId::Terminal
        } else if config_value == OMARCHY {
            ThemeId::Omarchy
        } else if let Some(theme_name) = config_value.strip_prefix("builtin:") {
            ThemeId::Builtin(theme_name.to_string())
        } else if let Some(theme_name) = config_value.strip_prefix("file:") {
            ThemeId::File(theme_name.to_string())
        } else if BUILTIN.iter().any(|(theme_name, _)| *theme_name == config_value) {
            ThemeId::Builtin(config_value.to_string())
        } else {
            ThemeId::File(config_value.to_string())
        }
    }

    pub fn persisted(&self) -> String {
        match self {
            ThemeId::Terminal => TERMINAL.to_string(),
            ThemeId::Omarchy => OMARCHY.to_string(),
            ThemeId::Builtin(name) => format!("builtin:{name}"),
            ThemeId::File(name) => format!("file:{name}"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            ThemeId::Terminal => TERMINAL,
            ThemeId::Omarchy => OMARCHY,
            ThemeId::Builtin(name) | ThemeId::File(name) => name,
        }
    }

    fn live_source(&self) -> Option<LiveThemeSource> {
        match self {
            ThemeId::Terminal => Some(LiveThemeSource::Terminal),
            ThemeId::Omarchy => Some(LiveThemeSource::Omarchy),
            ThemeId::Builtin(_) | ThemeId::File(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Terminal,
    Omarchy,
    Builtin,
    File,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Terminal => "follows the terminal",
            Origin::Omarchy => "follows Omarchy",
            Origin::Builtin => "built in",
            Origin::File => "your file",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThemeFile {
    pub background: String,
    pub foreground: String,
    pub ansi: Vec<String>,
    #[serde(default)]
    pub selection_background: Option<String>,
}

impl ThemeFile {
    /// The palette, or nothing when the file does not carry sixteen colours.
    /// A short list is a typo rather than a theme, and saying so beats
    /// painting half a screen in the wrong colour.
    pub fn palette(self, source_path: Option<PathBuf>) -> Option<Palette> {
        let ansi_colors: [String; 16] = self.ansi.try_into().ok()?;
        Some(Palette {
            source: source_path,
            background: self.background,
            foreground: self.foreground,
            ansi: ansi_colors,
            selection_background: self.selection_background,
        })
    }
}

/// Every theme this machine can offer, in the order the tab lists them: what
/// follows something first, then what redwax ships, then what the user wrote.
pub fn catalogue(config: &Config) -> Vec<Entry> {
    let mut entries = vec![Entry {
        id: ThemeId::Terminal,
        name: TERMINAL.to_string(),
        origin: Origin::Terminal,
        palette: None,
    }];
    if omarchy_palette().is_some() {
        entries.push(Entry {
            id: ThemeId::Omarchy,
            name: OMARCHY.to_string(),
            origin: Origin::Omarchy,
            palette: None,
        });
    }
    for (theme_name, palette_factory) in BUILTIN {
        entries.push(Entry {
            id: ThemeId::Builtin((*theme_name).to_string()),
            name: (*theme_name).to_string(),
            origin: Origin::Builtin,
            palette: palette_factory().validated(),
        });
    }
    entries.extend(from_directory(&themes_dir(config)));
    entries
}

pub fn themes_dir(config: &Config) -> PathBuf {
    config.auth_path.parent().unwrap_or(Path::new(".")).join("themes")
}

/// Every readable theme file in one directory, by name, sorted so the tab does
/// not reorder itself between runs. An unreadable or malformed file is skipped
/// rather than fatal: one bad file must not cost a user the other nine.
fn from_directory(themes_directory: &Path) -> Vec<Entry> {
    let Ok(directory_entries) = std::fs::read_dir(themes_directory) else {
        return Vec::new();
    };
    let mut found_themes: Vec<Entry> = directory_entries
        .filter_map(Result::ok)
        .map(|directory_entry| directory_entry.path())
        .filter(|theme_path| {
            theme_path
                .extension()
                .is_some_and(|extension| extension == "toml")
        })
        .filter_map(|theme_path| {
            let theme_name = theme_path.file_stem()?.to_str()?.to_string();
            let validated_palette = read_file(&theme_path)?;
            Some(Entry {
                id: ThemeId::File(theme_name.clone()),
                name: theme_name,
                origin: Origin::File,
                palette: Some(validated_palette),
            })
        })
        .collect();
    found_themes.sort_by(|left, right| left.name.cmp(&right.name));
    found_themes
}

pub fn read_file(path: &Path) -> Option<ValidatedPalette> {
    let theme_file_text = std::fs::read_to_string(path).ok()?;
    let theme_file: ThemeFile = toml::from_str(&theme_file_text).ok()?;
    theme_file.palette(Some(path.to_path_buf()))?.validated()
}

/// The palette of the theme Omarchy currently has applied, if this is a
/// machine running Omarchy and the theme carries one.
pub fn omarchy_palette() -> Option<ValidatedPalette> {
    let home_directory = std::env::var_os("HOME")?;
    read_file(&Path::new(&home_directory).join(OMARCHY_COLORS))
}

/// The theme a name asks for, ready to paint. `terminal` is the caller's
/// business, because only the caller knows what the terminal answered, so it
/// comes back as `None` here.
pub fn resolve(theme_name: &str, config: &Config) -> Option<Theme> {
    resolve_id(&ThemeId::from_config(theme_name), config)
}

pub fn resolve_id(theme_id: &ThemeId, config: &Config) -> Option<Theme> {
    let validated_palette = match theme_id {
        ThemeId::Terminal => return None,
        ThemeId::Omarchy => omarchy_palette()?,
        ThemeId::Builtin(theme_name) => BUILTIN
            .iter()
            .find(|(builtin_name, _)| *builtin_name == theme_name.as_str())
            .and_then(|(_, palette_factory)| palette_factory().validated())?,
        ThemeId::File(_theme_name) => {
            from_directory(&themes_dir(config))
                .into_iter()
                .find(|entry| entry.id == *theme_id)?
                .palette?
        }
    };
    Some(theme_from_validated_palette(&validated_palette, theme_id.name()))
}

fn palette_of(
    background: &str,
    foreground: &str,
    ansi_colors: [&str; 16],
    selection_background: &str,
) -> Palette {
    Palette {
        source: None,
        background: background.to_string(),
        foreground: foreground.to_string(),
        ansi: ansi_colors.map(String::from),
        selection_background: Some(selection_background.to_string()),
    }
}

/// How a shipped theme names its colours. A function rather than a value,
/// because a `Palette` owns its strings and a const cannot.
pub type PaletteFn = fn() -> Palette;

/// The themes redwax ships. Every one is a palette in wide use, so a terminal
/// redwax cannot read still lands on something a user recognises.
pub const BUILTIN: &[(&str, PaletteFn)] = &[
    ("tokyo-night", || {
        palette_of(
            "#1a1b26",
            "#c0caf5",
            [
                "#16161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
                "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
            ],
            "#292e42",
        )
    }),
    ("catppuccin-mocha", || {
        palette_of(
            "#1e1e2e",
            "#cdd6f4",
            [
                "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
                "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
            ],
            "#313244",
        )
    }),
    ("gruvbox-dark", || {
        palette_of(
            "#282828",
            "#ebdbb2",
            [
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
                "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
            ],
            "#3c3836",
        )
    }),
    ("nord", || {
        palette_of(
            "#2e3440",
            "#d8dee9",
            [
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
                "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
            ],
            "#434c5e",
        )
    }),
    ("dracula", || {
        palette_of(
            "#282a36",
            "#f8f8f2",
            [
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
                "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
            ],
            "#44475a",
        )
    }),
    ("rose-pine", || {
        palette_of(
            "#191724",
            "#e0def4",
            [
                "#26233a", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4",
                "#6e6a86", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4",
            ],
            "#403d52",
        )
    }),
    ("everforest-dark", || {
        palette_of(
            "#2d353b",
            "#d3c6aa",
            [
                "#343f44", "#e67e80", "#a7c080", "#dbbc7f", "#7fbbb3", "#d699b6", "#83c092", "#d3c6aa",
                "#475258", "#e67e80", "#a7c080", "#dbbc7f", "#7fbbb3", "#d699b6", "#83c092", "#d3c6aa",
            ],
            "#3d484d",
        )
    }),
    ("kanagawa", || {
        palette_of(
            "#1f1f28",
            "#dcd7ba",
            [
                "#16161d", "#c34043", "#76946a", "#c0a36e", "#7e9cd8", "#957fb8", "#6a9589", "#c8c093",
                "#727169", "#e82424", "#98bb6c", "#e6c384", "#7fb4ca", "#938aa9", "#7aa89f", "#dcd7ba",
            ],
            "#2d4f67",
        )
    }),
    ("catppuccin-latte", || {
        palette_of(
            "#eff1f5",
            "#4c4f69",
            [
                "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be",
                "#6c6f85", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#bcc0cc",
            ],
            "#ccd0da",
        )
    }),
    ("solarized-dark", || {
        palette_of(
            "#002b36",
            "#839496",
            [
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
                "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
            ],
            "#073642",
        )
    }),
];

/// The themes tab: what this machine can offer and which one the cursor is on.
///
/// Picking a theme is a thing you do by looking, so moving the cursor paints
/// the whole app in that theme at once and leaving the tab without choosing
/// puts back the one that was on. The pane holds what to put back, which is
/// the only state a preview needs.
pub struct ThemesPane {
    theme_entries: Vec<Entry>,
    selected_theme_index: usize,
    /// The theme identity that was in the config when the tab was opened.
    initial_selection: ThemeId,
}

impl ThemesPane {
    /// The catalogue as it is right now. Read at open rather than at startup,
    /// so a theme dropped into the directory shows up without a relaunch.
    pub fn open(config: &Config) -> ThemesPane {
        let theme_entries = catalogue(config);
        let selected_theme_index = theme_entries
            .iter()
            .position(|entry| entry.matches_choice(&config.theme))
            .unwrap_or(0);
        ThemesPane {
            theme_entries,
            selected_theme_index,
            initial_selection: ThemeId::from_config(&config.theme),
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.theme_entries
    }

    pub fn cursor(&self) -> usize {
        self.selected_theme_index
    }

    /// The identity under the cursor, which is what a preview paints and what
    /// enter writes down.
    pub fn selected(&self) -> Option<&ThemeId> {
        self.theme_entries
            .get(self.selected_theme_index)
            .map(|theme_entry| &theme_entry.id)
    }

    /// The theme that was on before the tab was opened, for leaving without
    /// choosing.
    pub fn previous(&self) -> &ThemeId {
        &self.initial_selection
    }

    /// This choice is the one being kept, so leaving no longer puts anything
    /// back.
    pub fn keep(&mut self) {
        if let Some(selected_theme) = self.selected().cloned() {
            self.initial_selection = selected_theme;
        }
    }

    pub fn move_cursor(&mut self, row_delta: isize) {
        if self.theme_entries.is_empty() {
            return;
        }
        let last_index = self.theme_entries.len() - 1;
        self.selected_theme_index = self
            .selected_theme_index
            .saturating_add_signed(row_delta)
            .min(last_index);
    }

    pub fn cursor_to(&mut self, theme_index: usize) {
        if theme_index < self.theme_entries.len() {
            self.selected_theme_index = theme_index;
        }
    }

    pub fn cursor_to_edge(&mut self, last_edge: bool) {
        self.selected_theme_index = match last_edge {
            true => self.theme_entries.len().saturating_sub(1),
            false => 0,
        };
    }
}

/// Record a qualified theme choice through the config document owner.
pub fn write_choice(config_path: &Path, theme_id: &ThemeId) -> Result<(), String> {
    crate::config::write_string(config_path, "theme", &theme_id.persisted())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveThemeSource {
    Terminal,
    Omarchy,
}

/// Capability for one live-source read. Activating any later choice makes old
/// tokens stale, including work that was already running in another task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveThemeRequest {
    source_generation: u64,
    pub source: LiveThemeSource,
}

/// Owns the selected identity and the generation rule for live theme input.
pub struct ThemeController {
    selected_theme: ThemeId,
    source_generation: u64,
}

impl ThemeController {
    pub fn new(configured_choice: &str) -> ThemeController {
        ThemeController {
            selected_theme: ThemeId::from_config(configured_choice),
            source_generation: 0,
        }
    }

    pub fn selected(&self) -> &ThemeId {
        &self.selected_theme
    }

    /// Persist first, then activate the choice and issue any live-source work.
    pub fn commit(
        &mut self,
        config: &mut Config,
        theme_id: ThemeId,
    ) -> Result<Option<LiveThemeRequest>, String> {
        config.set_theme_choice(&theme_id.persisted())?;
        Ok(self.activate(theme_id))
    }

    /// Activate a persisted choice. The caller starts a poll only when this
    /// returns a request.
    pub fn activate(&mut self, theme_id: ThemeId) -> Option<LiveThemeRequest> {
        self.source_generation = self.source_generation.wrapping_add(1);
        self.selected_theme = theme_id;
        self.selected_theme.live_source().map(|source| LiveThemeRequest {
            source_generation: self.source_generation,
            source,
        })
    }

    /// Convert live input only when it belongs to the current source request.
    pub fn accept_live(&self, request: LiveThemeRequest, palette: &Palette) -> Option<Theme> {
        let validated_palette = palette.validated()?;
        self.accept_validated_live(request, &validated_palette)
    }

    pub fn accept_validated_live(
        &self,
        request: LiveThemeRequest,
        palette: &ValidatedPalette,
    ) -> Option<Theme> {
        if request.source_generation != self.source_generation
            || Some(request.source) != self.selected_theme.live_source()
        {
            return None;
        }
        Some(theme_from_validated_palette(palette, self.selected_theme.name()))
    }

    pub fn resolve(&self, config: &Config) -> Option<Theme> {
        resolve_id(&self.selected_theme, config)
    }

    /// Resolve a static source or the last accepted value of a live source.
    pub fn resolve_or_cached(&self, config: &Config) -> Theme {
        self.resolve(config)
            .or_else(|| read_saved_theme(&theme_cache_path(config), &self.selected_theme))
            .unwrap_or_else(crate::theme::redwax_theme)
    }

    /// Apply session presentation after source selection. Cover accents never
    /// change the persisted source identity.
    pub fn present(&self, mut source_theme: Theme, transparent: bool, accent: Option<Color>) -> Theme {
        if let Some(cover_accent) = accent {
            source_theme = source_theme.with_accent(cover_accent, "art");
        }
        if transparent {
            source_theme = source_theme.transparent();
        }
        source_theme
    }

    /// Accept current live input and cache the base theme for the next launch.
    pub fn accept_live_and_cache(
        &self,
        request: LiveThemeRequest,
        palette: &Palette,
        config: &Config,
    ) -> Option<Theme> {
        let validated_palette = palette.validated()?;
        self.accept_validated_live_and_cache(request, &validated_palette, config)
    }

    pub fn accept_validated_live_and_cache(
        &self,
        request: LiveThemeRequest,
        palette: &ValidatedPalette,
        config: &Config,
    ) -> Option<Theme> {
        let accepted_theme = self.accept_validated_live(request, palette)?;
        write_saved_theme(&theme_cache_path(config), &self.selected_theme, &accepted_theme);
        Some(accepted_theme)
    }
}

pub fn theme_cache_path(config: &Config) -> PathBuf {
    config
        .state_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("theme.json")
}

fn read_saved_theme(cache_path: &Path, selected_theme: &ThemeId) -> Option<Theme> {
    let cache_json = std::fs::read_to_string(cache_path).ok()?;
    let cached_theme: serde_json::Value = serde_json::from_str(&cache_json).ok()?;
    match cached_theme.get("source").and_then(serde_json::Value::as_str) {
        Some(source) if source != selected_theme.persisted() => return None,
        None if selected_theme != &ThemeId::Terminal => return None,
        Some(_) | None => {}
    }
    let cached_color = |field_name: &str| -> Option<Color> {
        match cached_theme.get(field_name)?.as_str()? {
            "reset" => Some(Color::Reset),
            hex => crate::theme::parse_hex(hex),
        }
    };
    Some(Theme {
        name: cached_theme.get("name")?.as_str()?.to_string(),
        accent: cached_color("accent")?,
        background: cached_color("background")?,
        surface: cached_color("surface")?,
        panel: cached_color("panel")?,
        foreground: cached_color("foreground")?,
        muted: cached_color("muted")?,
        dim: cached_color("dim")?,
        gutter: cached_color("gutter")?,
        success: cached_color("success")?,
        warning: cached_color("warning")?,
        error: cached_color("error")?,
        transparent: false,
    })
}

fn write_saved_theme(cache_path: &Path, selected_theme: &ThemeId, theme: &Theme) {
    let color_to_hex = |colour: Color| match colour {
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        _ => "reset".to_string(),
    };
    let cached_theme = serde_json::json!({
        "source": selected_theme.persisted(),
        "name": theme.name,
        "accent": color_to_hex(theme.accent),
        "background": color_to_hex(theme.background),
        "surface": color_to_hex(theme.surface),
        "panel": color_to_hex(theme.panel),
        "foreground": color_to_hex(theme.foreground),
        "muted": color_to_hex(theme.muted),
        "dim": color_to_hex(theme.dim),
        "gutter": color_to_hex(theme.gutter),
        "success": color_to_hex(theme.success),
        "warning": color_to_hex(theme.warning),
        "error": color_to_hex(theme.error),
    });
    if let Some(cache_directory) = cache_path.parent() {
        let _ = std::fs::create_dir_all(cache_directory);
    }
    let _ = std::fs::write(cache_path, cached_theme.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_in(dir: &Path) -> Config {
        Config {
            auth_path: dir.join("browser.json"),
            ..Config::default()
        }
    }

    #[test]
    fn every_shipped_theme_is_sixteen_colours_and_parses() {
        assert!(BUILTIN.len() >= 8, "not enough to be worth a tab");
        for (name, palette) in BUILTIN {
            let palette = palette();
            assert_eq!(palette.ansi.len(), 16, "{name}");
            for colour in std::iter::once(&palette.background)
                .chain(std::iter::once(&palette.foreground))
                .chain(palette.ansi.iter())
            {
                assert!(
                    crate::theme::parse_hex(colour).is_some(),
                    "{name} has a colour that is not a hex value: {colour}"
                );
            }
            // The mapping is what the rest of the app paints from, so a theme
            // that cannot go through it is not a theme.
            let theme = crate::theme::theme_from_palette(&palette, name);
            assert_eq!(&theme.name, name);
        }
    }

    #[test]
    fn a_theme_file_is_read_from_the_themes_directory() {
        let dir = std::env::temp_dir().join(format!("redwax-themes-{}", std::process::id()));
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("a temp directory");
        let ansi = (0..16).map(|_| "\"#112233\"").collect::<Vec<_>>().join(", ");
        std::fs::write(
            themes.join("mine.toml"),
            format!("background = \"#000000\"\nforeground = \"#ffffff\"\nansi = [{ansi}]\n"),
        )
        .expect("a theme file");

        let config = config_in(&dir);
        let found = catalogue(&config);
        let mine = found
            .iter()
            .find(|entry| entry.name == "mine")
            .expect("the file was not listed");
        assert_eq!(mine.origin, Origin::File);

        let theme = resolve("mine", &config).expect("the file did not resolve");
        assert_eq!(theme.name, "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One bad file must not cost a user the other nine, and a theme that is
    /// short of colours is a typo rather than something to paint with.
    #[test]
    fn a_malformed_file_is_skipped_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("redwax-bad-themes-{}", std::process::id()));
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("a temp directory");
        std::fs::write(themes.join("broken.toml"), "this is not toml {{{").expect("a file");
        std::fs::write(
            themes.join("short.toml"),
            "background = \"#000000\"\nforeground = \"#ffffff\"\nansi = [\"#111111\"]\n",
        )
        .expect("a file");

        let config = config_in(&dir);
        let found = catalogue(&config);
        let named = |wanted: &str| found.iter().any(|entry| entry.name == wanted);
        assert!(!named("broken"), "a broken file was offered");
        assert!(!named("short"), "a theme missing colours was offered");
        assert!(named(TERMINAL), "the terminal theme went missing");
        assert!(named("nord"), "the shipped themes went missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_theme_with_one_invalid_colour_is_skipped() {
        let dir = std::env::temp_dir().join(format!("redwax-colour-themes-{}", std::process::id()));
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("a temp directory");
        let mut ansi = (0..16).map(|_| "\"#112233\"").collect::<Vec<_>>();
        ansi[9] = "\"blue\"";
        std::fs::write(
            themes.join("invalid-colour.toml"),
            format!(
                "background = \"#000000\"\nforeground = \"#ffffff\"\nansi = [{}]\n",
                ansi.join(", ")
            ),
        )
        .expect("a theme file");

        assert!(
            catalogue(&config_in(&dir))
                .iter()
                .all(|entry| entry.name != "invalid-colour")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualified_identity_selects_a_file_that_shadows_a_builtin() {
        let dir = std::env::temp_dir().join(format!("redwax-shadow-theme-{}", std::process::id()));
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("a temp directory");
        let ansi = (0..16).map(|_| "\"#112233\"").collect::<Vec<_>>().join(", ");
        std::fs::write(
            themes.join("nord.toml"),
            format!("background = \"#010203\"\nforeground = \"#ffffff\"\nansi = [{ansi}]\n"),
        )
        .expect("a theme file");
        let config = config_in(&dir);

        let legacy = resolve("nord", &config).expect("the legacy built-in");
        let file = resolve("file:nord", &config).expect("the qualified file");

        assert_ne!(legacy.background, file.background);
        assert!(catalogue(&config).iter().any(|entry| {
            entry.id == ThemeId::File("nord".to_string()) && entry.matches_choice("file:nord")
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_live_palette_cannot_replace_a_static_choice() {
        let mut controller = ThemeController::new(TERMINAL);
        let request = controller.activate(ThemeId::Terminal).expect("terminal is live");
        assert!(
            controller
                .activate(ThemeId::Builtin("nord".to_string()))
                .is_none()
        );

        assert!(
            controller
                .accept_live(request, &crate::theme::tokyo_night())
                .is_none()
        );
    }

    #[test]
    fn cached_live_theme_is_scoped_to_its_source_identity() {
        let dir = std::env::temp_dir().join(format!("redwax-theme-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.json");
        let theme = crate::theme::redwax_theme();
        write_saved_theme(&path, &ThemeId::Terminal, &theme);

        assert_eq!(read_saved_theme(&path, &ThemeId::Terminal), Some(theme));
        assert!(read_saved_theme(&path, &ThemeId::Omarchy).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_terminal_theme_is_left_to_the_caller() {
        let dir = std::env::temp_dir().join("redwax-no-themes");
        assert!(
            resolve(TERMINAL, &config_in(&dir)).is_none(),
            "only the caller knows what the terminal answered"
        );
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing_rather_than_a_wrong_theme() {
        let dir = std::env::temp_dir().join("redwax-no-themes");
        assert!(resolve("no-such-theme", &config_in(&dir)).is_none());
    }
}
