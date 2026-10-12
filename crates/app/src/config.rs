//! Optional settings preserve existing keys and symlink targets when rewritten.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Value};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("redwax")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("redwax")
}

fn xdg(environment_variable: &str, fallback_directory: &str) -> PathBuf {
    match std::env::var_os(environment_variable) {
        Some(configured_directory) if !configured_directory.is_empty() => PathBuf::from(configured_directory),
        _ => home().join(fallback_directory),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    #[default]
    High,
    Fast,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub theme: String,
    pub icons: String,
    pub art: String,
    pub accent_from_art: bool,
    pub transparent: bool,
    /// The equaliser, the marquee and the toast fade. Off leaves every one of
    /// them in its still form, for a slow terminal or a screen recording.
    pub animations: bool,
    pub search_vim_mode: bool,
    /// Rows the album art is allowed. Lower it to give the queue box more of
    /// the column, raise it for a bigger cover. Clamped 6 to 30.
    pub art_height: u16,
    /// 0 to 100, clamped on load.
    pub volume: u8,
    pub home_rows: usize,
    pub mpv_args: Vec<String>,
    pub ytdl_raw_options: String,
    /// Controls mpv fallback cookies. Resolver authentication follows the selected session.
    pub use_login_cookies: bool,
    pub quality: Quality,
    /// The equaliser, off until asked for. The gains themselves live in
    /// `~/.cache/redwax/eq.json`, because the panel changes them.
    pub eq_enabled: bool,
    /// Look the genre of each track up and load the preset it maps to. A band
    /// the user sets by hand stops it for the rest of the session.
    pub eq_genre_match: bool,
    /// The preset the equaliser starts on. An unknown name reads as `flat`.
    pub eq_preset: String,
    /// Off leaves the download library alone.
    pub downloads_enabled: bool,
    /// How long an unmarked file stays before it moves to `.evicted`.
    pub downloads_grace_days: u32,
    /// Files removed from download marks stay under this library in `.evicted`.
    pub library_dir: PathBuf,
    pub auth_path: PathBuf,
    pub cookies_path: PathBuf,
    pub session_path: PathBuf,
    pub art_cache_dir: PathBuf,
    pub page_cache_dir: PathBuf,
    pub state_path: PathBuf,
    /// The file this config was read from, so a choice can be written back.
    pub source: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let config_directory = config_dir();
        let cache_directory = cache_dir();
        Config {
            theme: "terminal".into(),
            icons: "nerd".into(),
            art: "auto".into(),
            accent_from_art: false,
            transparent: false,
            animations: true,
            search_vim_mode: true,
            art_height: 22,
            volume: 80,
            home_rows: 6,
            mpv_args: Vec::new(),
            ytdl_raw_options: String::new(),
            use_login_cookies: false,
            quality: Quality::High,
            eq_enabled: false,
            eq_genre_match: false,
            eq_preset: "flat".into(),
            downloads_enabled: true,
            downloads_grace_days: 14,
            library_dir: home().join("Music/redwax"),
            auth_path: config_directory.join("browser.json"),
            cookies_path: config_directory.join("cookies.txt"),
            session_path: config_directory.join("session.json"),
            art_cache_dir: cache_directory.join("art"),
            page_cache_dir: cache_directory.join("pages"),
            state_path: cache_directory.join("state.json"),
            source: config_directory.join("config.toml"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    theme: Option<String>,
    icons: Option<String>,
    art: Option<String>,
    accent_from_art: Option<bool>,
    transparent: Option<bool>,
    animations: Option<bool>,
    search_vim_mode: Option<bool>,
    art_height: Option<i64>,
    volume: Option<i64>,
    home_rows: Option<usize>,
    quality: Option<Quality>,
    library_dir: Option<PathBuf>,
    #[serde(default)]
    mpv: RawMpv,
    #[serde(default)]
    downloads: RawDownloads,
    #[serde(default)]
    eq: RawEq,
}

#[derive(Debug, Default, Deserialize)]
struct RawEq {
    enabled: Option<bool>,
    genre_match: Option<bool>,
    preset: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDownloads {
    enabled: Option<bool>,
    grace_days: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMpv {
    args: Option<Vec<String>>,
    ytdl_raw_options: Option<String>,
    use_login_cookies: Option<bool>,
}

impl Config {
    /// Read `path`, or the default location. A missing file gives the defaults.
    /// A file that does not parse is an error, as in Python.
    pub fn load(requested_path: Option<&Path>) -> Result<Config, String> {
        let config_path = requested_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config_dir().join("config.toml"));
        let config_text = match std::fs::read_to_string(&config_path) {
            Ok(config_text) => config_text,
            // A machine with no config file yet still knows where one goes,
            // which is what lets the themes tab write the first choice.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config {
                    source: config_path,
                    ..Config::default()
                });
            }
            Err(error) => return Err(format!("{}: {error}", config_path.display())),
        };
        let parsed_config: RawConfig =
            toml::from_str(&config_text).map_err(|error| format!("{}: {error}", config_path.display()))?;
        let mut config = Config {
            source: config_path.clone(),
            ..Config::default()
        };
        if let Some(theme) = parsed_config.theme {
            config.theme = theme;
        }
        if let Some(icons) = parsed_config.icons {
            config.icons = icons;
        }
        if let Some(art_renderer) = parsed_config.art {
            config.art = art_renderer;
        }
        if let Some(accent_from_art) = parsed_config.accent_from_art {
            config.accent_from_art = accent_from_art;
        }
        if let Some(transparent) = parsed_config.transparent {
            config.transparent = transparent;
        }
        if let Some(animations) = parsed_config.animations {
            config.animations = animations;
        }
        if let Some(search_vim_mode) = parsed_config.search_vim_mode {
            config.search_vim_mode = search_vim_mode;
        }
        if let Some(art_height) = parsed_config.art_height {
            config.art_height = art_height.clamp(6, 30) as u16;
        }
        if let Some(volume) = parsed_config.volume {
            config.volume = volume.clamp(0, 100) as u8;
        }
        if let Some(home_rows) = parsed_config.home_rows {
            config.home_rows = home_rows.max(1);
        }
        if let Some(quality) = parsed_config.quality {
            config.quality = quality;
        }
        if let Some(library_directory) = parsed_config.library_dir {
            config.library_dir = expand_home(&library_directory);
        }
        if let Some(mpv_args) = parsed_config.mpv.args {
            config.mpv_args = mpv_args;
        }
        if let Some(ytdl_raw_options) = parsed_config.mpv.ytdl_raw_options {
            config.ytdl_raw_options = ytdl_raw_options;
        }
        if let Some(use_login_cookies) = parsed_config.mpv.use_login_cookies {
            config.use_login_cookies = use_login_cookies;
        }
        if let Some(eq_enabled) = parsed_config.eq.enabled {
            config.eq_enabled = eq_enabled;
        }
        if let Some(eq_genre_match) = parsed_config.eq.genre_match {
            config.eq_genre_match = eq_genre_match;
        }
        if let Some(eq_preset) = parsed_config.eq.preset {
            config.eq_preset = eq_preset;
        }
        if let Some(downloads_enabled) = parsed_config.downloads.enabled {
            config.downloads_enabled = downloads_enabled;
        }
        if let Some(downloads_grace_days) = parsed_config.downloads.grace_days {
            config.downloads_grace_days = downloads_grace_days;
        }
        Ok(config)
    }

    pub fn download_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.downloads_grace_days) * 24 * 3600)
    }

    pub fn set_search_vim_mode(&mut self, enabled: bool) -> Result<(), String> {
        let mut document = ConfigDocument::open(&self.source)?;
        document.set("search_vim_mode", Value::from(enabled));
        document.write()?;
        self.search_vim_mode = enabled;
        Ok(())
    }

    /// Persist a theme identity, then make it the live configured choice.
    pub fn set_theme_choice(&mut self, choice: &str) -> Result<(), String> {
        write_string(&self.source, "theme", choice)?;
        self.theme = choice.to_string();
        Ok(())
    }

    /// `ytdl_raw_options` with the cookies file added when `use_login_cookies`
    /// is on and the file exists, in mpv's `key=value,key=value` form. Only
    /// mpv's own hook is meant here, never the resolver.
    pub fn resolved_ytdl_raw_options(&self) -> String {
        if !self.use_login_cookies || !self.cookies_path.exists() {
            return self.ytdl_raw_options.clone();
        }
        let cookies = format!("cookies={}", self.cookies_path.display());
        if self.ytdl_raw_options.is_empty() {
            cookies
        } else {
            format!("{},{cookies}", self.ytdl_raw_options)
        }
    }
}

pub(crate) fn write_string(
    config_path: &Path,
    setting_name: &str,
    setting_value: &str,
) -> Result<(), String> {
    let mut document = ConfigDocument::open(config_path)?;
    document.set(setting_name, Value::from(setting_value));
    document.write()
}

/// One validated config document and the physical file an atomic rename must
/// replace. The source path remains unchanged when it is a dotfiles symlink.
struct ConfigDocument {
    source_path: PathBuf,
    destination_path: PathBuf,
    document: DocumentMut,
}

impl ConfigDocument {
    fn open(source_path: &Path) -> Result<ConfigDocument, String> {
        let source_metadata = std::fs::symlink_metadata(source_path);
        let (destination_path, document_text) = match source_metadata {
            Ok(source_metadata) if source_metadata.file_type().is_symlink() => {
                let destination_path = std::fs::canonicalize(source_path)
                    .map_err(|error| format!("could not resolve {}: {error}", source_path.display()))?;
                let document_text = std::fs::read_to_string(&destination_path)
                    .map_err(|error| format!("could not read {}: {error}", source_path.display()))?;
                (destination_path, document_text)
            }
            Ok(_) => {
                let document_text = std::fs::read_to_string(source_path)
                    .map_err(|error| format!("could not read {}: {error}", source_path.display()))?;
                (source_path.to_path_buf(), document_text)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (source_path.to_path_buf(), String::new())
            }
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", source_path.display()));
            }
        };
        toml::from_str::<RawConfig>(&document_text)
            .map_err(|error| format!("could not parse {}: {error}", source_path.display()))?;
        let document = document_text
            .parse::<DocumentMut>()
            .map_err(|error| format!("could not preserve {}: {error}", source_path.display()))?;
        Ok(ConfigDocument {
            source_path: source_path.to_path_buf(),
            destination_path,
            document,
        })
    }

    fn set(&mut self, setting_name: &str, mut setting_value: Value) {
        if let Some(existing_value) = self.document.get(setting_name).and_then(Item::as_value) {
            *setting_value.decor_mut() = existing_value.decor().clone();
        }
        self.document[setting_name] = Item::Value(setting_value);
    }

    fn write(self) -> Result<(), String> {
        let document_text = self.document.to_string();
        toml::from_str::<RawConfig>(&document_text)
            .map_err(|error| format!("could not update {}: {error}", self.source_path.display()))?;
        atomic_replace(&self.destination_path, document_text.as_bytes())
            .map_err(|error| format!("could not write {}: {error}", self.source_path.display()))
    }
}

fn atomic_replace(destination_path: &Path, file_contents: &[u8]) -> std::io::Result<()> {
    let parent_directory = destination_path
        .parent()
        .filter(|parent_directory| !parent_directory.as_os_str().is_empty());
    if let Some(parent_directory) = parent_directory {
        std::fs::create_dir_all(parent_directory)?;
    }
    let destination_directory = parent_directory.unwrap_or(Path::new("."));
    let destination_name = destination_path.file_name().unwrap_or_default().to_string_lossy();
    let mut last_error = None;
    for _attempt in 0..100 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = destination_directory.join(format!(
            ".{destination_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let open_result = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path);
        let mut temporary_file = match open_result {
            Ok(temporary_file) => temporary_file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let write_result = (|| {
            if let Ok(destination_metadata) = std::fs::metadata(destination_path) {
                temporary_file.set_permissions(destination_metadata.permissions())?;
            }
            temporary_file.write_all(file_contents)?;
            temporary_file.sync_all()?;
            std::fs::rename(&temporary_path, destination_path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        return write_result;
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("could not create a temporary file")))
}

fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(relative_path) => home().join(relative_path),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_toml_and_clamps_volume() {
        let dir = std::env::temp_dir().join(format!("redwax-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "volume = 140\n",
                "theme = \"nord\"\n",
                "quality = \"fast\"\n",
                "[mpv]\n",
                "args = [\"--audio-device=pipewire\"]\n",
            ),
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.volume, 100);
        assert_eq!(config.theme, "nord");
        assert_eq!(config.quality, Quality::Fast);
        assert_eq!(config.mpv_args, vec!["--audio-device=pipewire".to_string()]);
        assert!(config.animations);
        assert!(config.search_vim_mode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_editing_defaults_to_vim_and_loads_standard_mode() {
        let dir = std::env::temp_dir().join(format!("redwax-search-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        std::fs::write(&path, "volume = 50\n").unwrap();
        assert!(Config::load(Some(&path)).unwrap().search_vim_mode);

        std::fs::write(&path, "search_vim_mode = false\n").unwrap();
        assert!(!Config::load(Some(&path)).unwrap().search_vim_mode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setting_search_editing_preserves_the_config_document() {
        let dir = std::env::temp_dir().join(format!("redwax-search-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let before = concat!(
            "# My choices\n",
            "volume = 50\n",
            "search_vim_mode = true  # modal editing\n\n",
            "[mpv]\n",
            "args = [\"--quiet\"]\n",
        );
        std::fs::write(&path, before).unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_search_vim_mode(false).unwrap();

        assert!(!config.search_vim_mode);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before.replacen("true", "false", 1)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setting_search_editing_recognises_a_quoted_key() {
        let dir = std::env::temp_dir().join(format!("redwax-search-quoted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "'search_vim_mode' = true # keep this\n").unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_search_vim_mode(false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "'search_vim_mode' = false # keep this\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_search_setting_stays_outside_existing_tables() {
        let dir = std::env::temp_dir().join(format!("redwax-search-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# Player options\n[mpv]\nargs = [\"--quiet\"]\n").unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_search_vim_mode(false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "search_vim_mode = false\n# Player options\n[mpv]\nargs = [\"--quiet\"]\n"
        );
        assert!(!Config::load(Some(&path)).unwrap().search_vim_mode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn array_and_multiline_string_contents_are_not_table_headers() {
        let dir = std::env::temp_dir().join(format!("redwax-search-values-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let before = "matrix = [\n  [1, 2]\n]\nnote = \"\"\"\n[not-a-table]\n\"\"\"\n[mpv]\nargs = []\n";
        std::fs::write(&path, before).unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_search_vim_mode(false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before.replacen("[mpv]", "search_vim_mode = false\n[mpv]", 1)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_search_setting_write_keeps_the_live_value() {
        let dir = std::env::temp_dir().join(format!("redwax-search-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocked_parent = dir.join("not-a-directory");
        std::fs::write(&blocked_parent, "occupied").unwrap();
        let mut config = Config {
            source: blocked_parent.join("config.toml"),
            ..Config::default()
        };

        let error = config.set_search_vim_mode(false).unwrap_err();

        assert!(error.contains("could not inspect"));
        assert!(error.contains("config.toml"));
        assert!(config.search_vim_mode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_setting_name_inside_a_multiline_string_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("redwax-search-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let before = "note = '''\nsearch_vim_mode = true\n'''\n[mpv]\nargs = []\n";
        std::fs::write(&path, before).unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_search_vim_mode(false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before.replacen("[mpv]", "search_vim_mode = false\n[mpv]", 1)
        );
        assert!(!Config::load(Some(&path)).unwrap().search_vim_mode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invalid_live_config_is_not_overwritten_by_settings() {
        let dir = std::env::temp_dir().join(format!("redwax-search-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut config = Config::load(Some(&path)).unwrap();
        std::fs::write(&path, "volume =").unwrap();

        assert!(config.set_search_vim_mode(false).is_err());

        assert!(config.search_vim_mode);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "volume =");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setting_a_theme_preserves_comments_and_escapes_the_value() {
        let dir = std::env::temp_dir().join(format!("redwax-theme-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# My choices\ntheme = \"nord\" # keep this note\n\n[mpv]\nargs = [\"--quiet\"]\n",
        )
        .unwrap();
        let mut config = Config::load(Some(&path)).unwrap();

        config.set_theme_choice("file:quote\"and\\slash").unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# My choices\n"));
        assert!(saved.contains("# keep this note"));
        assert!(saved.contains("[mpv]\nargs = [\"--quiet\"]"));
        assert_eq!(Config::load(Some(&path)).unwrap().theme, "file:quote\"and\\slash");
        assert_eq!(config.theme, "file:quote\"and\\slash");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_theme_write_keeps_the_live_choice() {
        let dir = std::env::temp_dir().join(format!("redwax-theme-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocked_parent = dir.join("not-a-directory");
        std::fs::write(&blocked_parent, "occupied").unwrap();
        let mut config = Config {
            theme: "nord".to_string(),
            source: blocked_parent.join("config.toml"),
            ..Config::default()
        };

        assert!(config.set_theme_choice("builtin:dracula").is_err());

        assert_eq!(config.theme, "nord");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn setting_a_theme_replaces_the_symlink_target_atomically() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("redwax-theme-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("dotfiles.toml");
        let source = dir.join("config.toml");
        std::fs::write(&target, "theme = \"nord\"\nvolume = 23\n").unwrap();
        symlink(&target, &source).unwrap();
        let mut config = Config::load(Some(&source)).unwrap();

        config.set_theme_choice("builtin:dracula").unwrap();

        assert!(
            std::fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Config::load(Some(&target)).unwrap().theme, "builtin:dracula");
        assert_eq!(Config::load(Some(&target)).unwrap().volume, 23);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_downloads_table_is_optional_and_overrides_the_defaults() {
        let dir = std::env::temp_dir().join(format!("redwax-downloads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        std::fs::write(&path, "volume = 50\n").unwrap();
        let plain = Config::load(Some(&path)).unwrap();
        assert!(plain.downloads_enabled);
        assert_eq!(plain.downloads_grace_days, 14);

        std::fs::write(&path, "[downloads]\nenabled = false\ngrace_days = 3\n").unwrap();
        let set = Config::load(Some(&path)).unwrap();
        assert!(!set.downloads_enabled);
        assert_eq!(set.downloads_grace_days, 3);
        assert_eq!(
            set.download_grace(),
            std::time::Duration::from_secs(3 * 24 * 3600)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_eq_table_is_optional_and_overrides_the_defaults() {
        let dir = std::env::temp_dir().join(format!("redwax-eq-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        std::fs::write(&path, "volume = 50\n").unwrap();
        let plain = Config::load(Some(&path)).unwrap();
        assert!(!plain.eq_enabled);
        assert!(!plain.eq_genre_match);
        assert_eq!(plain.eq_preset, "flat");

        std::fs::write(
            &path,
            "[eq]\nenabled = true\ngenre_match = true\npreset = \"bass\"\n",
        )
        .unwrap();
        let set = Config::load(Some(&path)).unwrap();
        assert!(set.eq_enabled);
        assert!(set.eq_genre_match);
        assert_eq!(set.eq_preset, "bass");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_config_gives_defaults() {
        let path = Path::new("/nonexistent/redwax/config.toml");
        let config = Config::load(Some(path)).unwrap();
        // Everything is the default, but the file it would have been read from
        // is remembered, which is where the themes tab writes a first choice.
        assert_eq!(config.source, path);
        assert_eq!(
            Config {
                source: Config::default().source,
                ..config
            },
            Config::default()
        );
    }
}
