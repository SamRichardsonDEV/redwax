//! Offline browser cookie extraction through yt-dlp.

use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redwax_marks::{Cookie, read_netscape};

const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(20);
const OFFLINE_INFO: &[u8] = br#"{
  "id": "redwax-login",
  "title": "redwax login",
  "url": "https://music.youtube.com/",
  "ext": "none"
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Firefox,
    Chrome,
    Chromium,
    Brave,
    Edge,
    Opera,
    Vivaldi,
    Whale,
    Safari,
}

impl Browser {
    pub const ALL: [Browser; 9] = [
        Browser::Firefox,
        Browser::Chrome,
        Browser::Chromium,
        Browser::Brave,
        Browser::Edge,
        Browser::Opera,
        Browser::Vivaldi,
        Browser::Whale,
        Browser::Safari,
    ];

    pub fn parse(name: &str) -> Result<Browser, String> {
        let normalized_name = name.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|browser| browser.name() == normalized_name)
            .ok_or_else(|| format!("unsupported browser {name}"))
    }

    pub fn name(self) -> &'static str {
        match self {
            Browser::Firefox => "firefox",
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Brave => "brave",
            Browser::Edge => "edge",
            Browser::Opera => "opera",
            Browser::Vivaldi => "vivaldi",
            Browser::Whale => "whale",
            Browser::Safari => "safari",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserSource {
    pub browser: Browser,
    pub profile: Option<String>,
}

impl std::fmt::Debug for BrowserSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserSource")
            .field("browser", &self.browser)
            .field("has_profile", &self.profile.is_some())
            .finish()
    }
}

impl BrowserSource {
    fn yt_dlp_specification(&self) -> String {
        match &self.profile {
            Some(profile) => format!("{}:{profile}", self.browser.name()),
            None => self.browser.name().to_owned(),
        }
    }
}

/// Browser installations worth trying automatically. Explicit selection does
/// not depend on these hints because yt-dlp owns the platform-specific lookup.
pub fn detected_sources() -> Vec<BrowserSource> {
    let Some(base) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    let config = base.config_dir();
    let home = base.home_dir();
    let application_support = home.join("Library/Application Support");
    let mut candidates = vec![
        (Browser::Firefox, home.join(".var/app/org.mozilla.firefox")),
        (
            Browser::Firefox,
            home.join("snap/firefox/common/.mozilla/firefox"),
        ),
        (Browser::Chrome, config.join("google-chrome")),
        (Browser::Chromium, config.join("chromium")),
        (Browser::Brave, config.join("BraveSoftware/Brave-Browser")),
        (Browser::Edge, config.join("microsoft-edge")),
        (Browser::Opera, config.join("opera")),
        (Browser::Vivaldi, config.join("vivaldi")),
        (Browser::Whale, config.join("naver-whale")),
        (
            Browser::Chrome,
            home.join(".var/app/com.google.Chrome/config/google-chrome"),
        ),
        (
            Browser::Chromium,
            home.join(".var/app/org.chromium.Chromium/config/chromium"),
        ),
        (
            Browser::Brave,
            home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
        ),
        (
            Browser::Edge,
            home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"),
        ),
        (Browser::Opera, home.join(".var/app/com.opera.Opera/config/opera")),
        (
            Browser::Vivaldi,
            home.join(".var/app/com.vivaldi.Vivaldi/config/vivaldi"),
        ),
        (Browser::Chromium, home.join("snap/chromium/common/chromium")),
        (
            Browser::Brave,
            home.join("snap/brave/common/.config/BraveSoftware/Brave-Browser"),
        ),
        (Browser::Vivaldi, home.join("snap/vivaldi/common/.config/vivaldi")),
        (Browser::Chrome, application_support.join("Google/Chrome")),
        (Browser::Chromium, application_support.join("Chromium")),
        (
            Browser::Brave,
            application_support.join("BraveSoftware/Brave-Browser"),
        ),
        (Browser::Edge, application_support.join("Microsoft Edge")),
        (
            Browser::Opera,
            application_support.join("com.operasoftware.Opera"),
        ),
        (Browser::Vivaldi, application_support.join("Vivaldi")),
        (Browser::Whale, application_support.join("Naver/Whale")),
        (
            Browser::Safari,
            home.join("Library/Cookies/Cookies.binarycookies"),
        ),
        (
            Browser::Safari,
            home.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies"),
        ),
    ];
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        candidates.extend([
            (Browser::Chrome, local_app_data.join("Google/Chrome/User Data")),
            (Browser::Chromium, local_app_data.join("Chromium/User Data")),
            (
                Browser::Brave,
                local_app_data.join("BraveSoftware/Brave-Browser/User Data"),
            ),
            (Browser::Edge, local_app_data.join("Microsoft/Edge/User Data")),
            (Browser::Vivaldi, local_app_data.join("Vivaldi/User Data")),
            (Browser::Whale, local_app_data.join("Naver/Naver Whale/User Data")),
        ]);
    }
    if let Some(roaming_app_data) = std::env::var_os("APPDATA").map(PathBuf::from) {
        candidates.push((
            Browser::Opera,
            roaming_app_data.join("Opera Software/Opera Stable"),
        ));
    }
    let mut detected = detected_sources_from_candidates(candidates);
    if let Some(preferred) = preferred_browser() {
        detected.sort_by_key(|source| source.browser != preferred);
    }
    detected
}

fn detected_sources_from_candidates(candidates: Vec<(Browser, PathBuf)>) -> Vec<BrowserSource> {
    let mut sources = Vec::new();
    for (browser, browser_root) in candidates {
        if !browser_root.exists() {
            continue;
        }
        let profiles = match browser {
            Browser::Firefox | Browser::Safari => Vec::new(),
            _ => chromium_cookie_profiles(&browser_root),
        };
        if profiles.is_empty() {
            append_unique_source(
                &mut sources,
                BrowserSource {
                    browser,
                    profile: None,
                },
            );
        } else {
            for profile in profiles {
                append_unique_source(
                    &mut sources,
                    BrowserSource {
                        browser,
                        profile: Some(profile.to_string_lossy().into_owned()),
                    },
                );
            }
        }
    }
    sources
}

fn chromium_cookie_profiles(browser_root: &Path) -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    if has_chromium_cookie_database(browser_root) {
        profiles.push(browser_root.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(browser_root) else {
        return profiles;
    };
    for entry in entries.filter_map(Result::ok) {
        let profile = entry.path();
        let Some(name) = profile.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if (name == "Default" || name.starts_with("Profile"))
            && profile.is_dir()
            && has_chromium_cookie_database(&profile)
        {
            profiles.push(profile);
        }
    }
    profiles.sort_by(|left, right| {
        let left_is_default = left.file_name().is_some_and(|name| name == "Default");
        let right_is_default = right.file_name().is_some_and(|name| name == "Default");
        right_is_default.cmp(&left_is_default).then(left.cmp(right))
    });
    profiles
}

fn has_chromium_cookie_database(profile: &Path) -> bool {
    profile.join("Cookies").is_file() || profile.join("Network/Cookies").is_file()
}

fn append_unique_source(sources: &mut Vec<BrowserSource>, source: BrowserSource) {
    if !sources.contains(&source) {
        sources.push(source);
    }
}

fn preferred_browser() -> Option<Browser> {
    let configured = std::env::var_os("BROWSER")?;
    let executable = Path::new(configured.to_str()?.split(':').next()?)
        .file_name()?
        .to_str()?;
    match executable {
        "firefox" => Some(Browser::Firefox),
        "google-chrome" | "google-chrome-stable" | "chrome" => Some(Browser::Chrome),
        "chromium" | "chromium-browser" => Some(Browser::Chromium),
        "brave" | "brave-browser" => Some(Browser::Brave),
        "microsoft-edge" | "microsoft-edge-stable" => Some(Browser::Edge),
        "opera" => Some(Browser::Opera),
        "vivaldi" | "vivaldi-stable" => Some(Browser::Vivaldi),
        "whale" | "naver-whale" => Some(Browser::Whale),
        "safari" => Some(Browser::Safari),
        _ => None,
    }
}

/// Extract cookies without asking yt-dlp to resolve or download a URL.
pub fn extract(source: &BrowserSource) -> Result<Vec<Cookie>, String> {
    let temporary_directory = PrivateDirectory::new()?;
    let info_path = temporary_directory.path.join("offline-info.json");
    let cookies_path = temporary_directory.path.join("cookies.txt");
    crate::private_file::write(&info_path, OFFLINE_INFO).map_err(|error| error.to_string())?;
    crate::private_file::write(&cookies_path, b"# Netscape HTTP Cookie File\n")
        .map_err(|error| error.to_string())?;

    let yt_dlp = redwax_resolve::ResolverDiagnostics::inspect(&crate::config::cache_dir()).ytdlp;
    let mut child = Command::new(yt_dlp)
        .args(command_arguments(source, &info_path, &cookies_path))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start yt-dlp: {error}"))?;
    let deadline = Instant::now() + EXTRACTION_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} cookie extraction timed out", source.browser.name()));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for yt-dlp: {error}"));
            }
        }
    };
    if !status.success() {
        return Err(format!(
            "could not read {} cookies. {}",
            source.browser.name(),
            browser_access_help(source.browser),
        ));
    }
    read_netscape(&cookies_path).map_err(|error| error.to_string())
}

fn command_arguments(source: &BrowserSource, info_path: &Path, cookies_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--ignore-config"),
        OsString::from("--quiet"),
        OsString::from("--no-warnings"),
        OsString::from("--skip-download"),
        OsString::from("--load-info-json"),
        info_path.as_os_str().to_owned(),
        OsString::from("--cookies-from-browser"),
        OsString::from(source.yt_dlp_specification()),
        OsString::from("--cookies"),
        cookies_path.as_os_str().to_owned(),
    ]
}

pub(crate) fn browser_access_help(browser: Browser) -> &'static str {
    match browser {
        Browser::Safari => "Grant the terminal Full Disk Access or select another profile.",
        Browser::Firefox => "Check that the profile exists and the browser data is readable.",
        _ => "Unlock the browser keyring or select another profile.",
    }
}

struct PrivateDirectory {
    path: PathBuf,
}

impl PrivateDirectory {
    fn new() -> Result<PrivateDirectory, String> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("redwax-browser-login-{}-{unique}", std::process::id(),));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("could not create private cookie directory: {error}"))?;
        Ok(PrivateDirectory { path })
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_names_are_exact_yt_dlp_names() {
        for browser in Browser::ALL {
            assert_eq!(Browser::parse(browser.name()), Ok(browser));
        }
        assert!(Browser::parse("unknown").is_err());
    }

    #[test]
    fn profile_is_one_argv_value_not_shell_text() {
        let source = BrowserSource {
            browser: Browser::Chrome,
            profile: Some("Profile 2; echo private".to_owned()),
        };
        assert_eq!(source.yt_dlp_specification(), "chrome:Profile 2; echo private");
        assert!(!format!("{source:?}").contains("Profile 2"));
    }

    #[test]
    fn extraction_arguments_are_offline_and_ignore_user_configuration() {
        let source = BrowserSource {
            browser: Browser::Chrome,
            profile: Some("Profile 2".to_owned()),
        };
        let arguments = command_arguments(
            &source,
            Path::new("/private/info.json"),
            Path::new("/private/cookies.txt"),
        );

        assert!(arguments.contains(&OsString::from("--ignore-config")));
        assert!(arguments.contains(&OsString::from("--skip-download")));
        assert!(arguments.contains(&OsString::from("--load-info-json")));
        assert!(
            !arguments
                .iter()
                .any(|argument| { argument.to_string_lossy() == "https://music.youtube.com/" })
        );
        assert!(serde_json::from_slice::<serde_json::Value>(OFFLINE_INFO).is_ok());
    }

    #[test]
    fn discovery_expands_chromium_profiles_and_keeps_sandbox_firefox() {
        let temporary = PrivateDirectory::new().unwrap();
        let chrome_root = temporary.path.join("chrome");
        let default = chrome_root.join("Default");
        let older_signed_in = chrome_root.join("Profile 2");
        let firefox_sandbox = temporary.path.join("flatpak-firefox");
        std::fs::create_dir_all(default.join("Network")).unwrap();
        std::fs::create_dir_all(&older_signed_in).unwrap();
        std::fs::create_dir_all(&firefox_sandbox).unwrap();
        std::fs::write(default.join("Network/Cookies"), []).unwrap();
        std::fs::write(older_signed_in.join("Cookies"), []).unwrap();

        let sources = detected_sources_from_candidates(vec![
            (Browser::Chrome, chrome_root),
            (Browser::Firefox, firefox_sandbox),
        ]);

        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0].browser, Browser::Chrome);
        assert_eq!(sources[0].profile.as_deref(), default.to_str());
        assert_eq!(sources[1].profile.as_deref(), older_signed_in.to_str());
        assert_eq!(
            sources[2],
            BrowserSource {
                browser: Browser::Firefox,
                profile: None
            }
        );
    }
}
