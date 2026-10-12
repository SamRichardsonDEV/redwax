//! Browser cookies can rotate while the browser stays signed in. Re-importing
//! on authentication failure lets playback and page requests recover from a
//! stale saved session without asking for another password.
//!
//! Remembered account selection belongs to its browser and profile. A refresh
//! preserves it only when importing the same source.

#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use redwax_marks::{FirefoxProfile, ProfileCapability, encode_netscape, read_cookies};
use redwax_ytm::{Account, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser_login::{self, Browser, BrowserSource};
use crate::config::Config;

#[derive(Clone, PartialEq, Eq)]
pub struct RememberedSession {
    pub source: BrowserSource,
    pub account: Option<Account>,
}

impl std::fmt::Debug for RememberedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RememberedSession")
            .field("source", &self.source)
            .field("has_account", &self.account.is_some())
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct SavedSession {
    #[serde(skip_serializing_if = "Option::is_none")]
    browser: Option<String>,
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<Account>,
}

impl RememberedSession {
    pub fn read(session_path: &Path) -> Option<Self> {
        let serialized_session = std::fs::read_to_string(session_path).ok()?;
        let saved: SavedSession = serde_json::from_str(&serialized_session).ok()?;
        let browser = Browser::parse(saved.browser.as_deref().unwrap_or("firefox")).ok()?;
        Some(Self {
            source: BrowserSource {
                browser,
                profile: saved.profile.filter(|profile| !profile.is_empty()),
            },
            account: saved.account,
        })
    }
}

/// Credentials stay unpublished until the caller confirms its account choice.
pub struct PreparedBrowserSession {
    session: Session,
    source: BrowserSource,
    browser_headers: Map<String, Value>,
    cookie_export: Vec<u8>,
    account: Option<Account>,
}

impl std::fmt::Debug for PreparedBrowserSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedBrowserSession")
            .field("session", &self.session)
            .field("source", &self.source)
            .field("has_account", &self.account.is_some())
            .finish()
    }
}

impl PreparedBrowserSession {
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn source(&self) -> &BrowserSource {
        &self.source
    }

    pub fn select_account(&mut self, account: &Account) -> Result<(), String> {
        let selected_session = Session::new(
            self.session.cookie_header().to_owned(),
            redwax_auth::SelectedIdentity::new(account.authuser, account.page_id.clone()),
        )
        .map_err(|error| error.to_string())?;
        account_headers(&mut self.browser_headers, account);
        self.session = selected_session;
        self.account = Some(account.clone());
        Ok(())
    }

    pub fn publish(self, config: &Config) -> Result<Session, String> {
        let browser_export = serde_json::to_vec_pretty(&self.browser_headers)
            .map_err(|_| "could not encode browser credentials".to_owned())?;
        let saved_session = SavedSession {
            browser: Some(self.source.browser.name().to_owned()),
            profile: self.source.profile.as_deref().map(str::to_owned),
            account: self.account,
        };
        let session_export = serde_json::to_vec_pretty(&saved_session)
            .map_err(|_| "could not encode remembered browser session".to_owned())?;
        publish_exports(&[
            (&config.cookies_path, self.cookie_export.as_slice()),
            (&config.auth_path, browser_export.as_slice()),
            (&config.session_path, session_export.as_slice()),
        ])?;
        Ok(self.session)
    }
}

enum RefreshState {
    Idle,
    Running,
    Finished {
        completed_at: Instant,
        operation_result: Result<(), String>,
    },
    SignedOut,
    Closed,
}

/// Page and playback recovery share one import and one throttle decision.
pub struct SessionCoordinator {
    import_session: crate::app::SessionRefresh,
    refresh_state: Mutex<RefreshState>,
    refresh_completed: Condvar,
    throttle_window: Duration,
}

impl SessionCoordinator {
    pub fn new(import_session: crate::app::SessionRefresh, throttle_window: Duration) -> Self {
        Self {
            import_session,
            refresh_state: Mutex::new(RefreshState::Idle),
            refresh_completed: Condvar::new(),
            throttle_window,
        }
    }

    /// Call on the blocking pool. A failed import is shared with its waiters.
    pub fn refresh(&self) -> Result<(), String> {
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| "session refresh state is unavailable".to_owned())?;
        loop {
            match &*refresh_state {
                RefreshState::Running => {
                    refresh_state = self
                        .refresh_completed
                        .wait(refresh_state)
                        .map_err(|_| "session refresh state is unavailable".to_owned())?;
                }
                RefreshState::Finished {
                    completed_at,
                    operation_result,
                } if completed_at.elapsed() < self.throttle_window => return operation_result.clone(),
                RefreshState::SignedOut => {
                    return Err("Signed out. Run `redwax login` and restart to sign in.".to_owned());
                }
                RefreshState::Closed => return Err("session recovery is closed".to_owned()),
                RefreshState::Idle | RefreshState::Finished { .. } => break,
            }
        }
        *refresh_state = RefreshState::Running;
        drop(refresh_state);
        let operation_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.import_session)()))
                .unwrap_or_else(|_| Err("session refresh did not finish".to_owned()));
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| "session refresh state is unavailable".to_owned())?;
        *refresh_state = RefreshState::Finished {
            completed_at: Instant::now(),
            operation_result: operation_result.clone(),
        };
        self.refresh_completed.notify_all();
        operation_result
    }

    /// Logout waits for an import, then blocks automatic imports for this run.
    pub fn logout(&self, remove_session_exports: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| "session refresh state is unavailable".to_owned())?;
        while matches!(*refresh_state, RefreshState::Running) {
            refresh_state = self
                .refresh_completed
                .wait(refresh_state)
                .map_err(|_| "session refresh state is unavailable".to_owned())?;
        }
        if matches!(*refresh_state, RefreshState::Closed) {
            return Err("session recovery is closed".to_owned());
        }
        *refresh_state = RefreshState::Running;
        drop(refresh_state);
        let operation_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(remove_session_exports))
            .unwrap_or_else(|_| Err("session logout did not finish".to_owned()));
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| "session refresh state is unavailable".to_owned())?;
        *refresh_state = RefreshState::SignedOut;
        self.refresh_completed.notify_all();
        operation_result
    }

    pub fn close(&self) -> Result<(), String> {
        let mut refresh_state = self
            .refresh_state
            .lock()
            .map_err(|_| "session refresh state is unavailable".to_owned())?;
        while matches!(*refresh_state, RefreshState::Running) {
            refresh_state = self
                .refresh_completed
                .wait(refresh_state)
                .map_err(|_| "session refresh state is unavailable".to_owned())?;
        }
        *refresh_state = RefreshState::Closed;
        self.refresh_completed.notify_all();
        Ok(())
    }
}

/// Deletes redwax exports only. Firefox and personal playback data stay owned
/// by their existing modules. All removals are attempted before reporting failure.
pub fn remove_exports(config: &Config) -> Result<(), String> {
    let mut failed_paths = Vec::new();
    let mut modified_directories = std::collections::HashSet::new();
    for export_path in [&config.auth_path, &config.cookies_path, &config.session_path] {
        match std::fs::remove_file(export_path) {
            Ok(()) => {
                let parent = export_path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                modified_directories.insert(parent.to_path_buf());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed_paths.push(export_path.display().to_string()),
        }
    }
    for directory in modified_directories {
        if std::fs::File::open(&directory)
            .and_then(|file| file.sync_all())
            .is_err()
        {
            failed_paths.push(directory.display().to_string());
        }
    }
    if failed_paths.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Could not complete credential removal at {}",
            failed_paths.join(", ")
        ))
    }
}

/// Browser discovery and extraction never replace the saved credentials.
pub fn prepare_from_browser(
    config: &Config,
    browser: Option<Browser>,
    profile: Option<&str>,
) -> Result<PreparedBrowserSession, String> {
    let remembered = RememberedSession::read(&config.session_path);
    let saved_headers = read_import_headers(&config.auth_path)?;
    let remembered_source = remembered.as_ref().map(|saved| &saved.source);
    let requested_source = match (browser, profile) {
        (Some(browser), Some(profile)) => Some(BrowserSource {
            browser,
            profile: Some(profile.to_owned()),
        }),
        (Some(browser), None) => remembered_source
            .filter(|source| {
                source.browser == browser
                    && (browser == Browser::Firefox || source.profile.as_deref().is_some())
            })
            .cloned(),
        (None, Some(profile)) => Some(BrowserSource {
            browser: remembered_source.map_or(Browser::Firefox, |source| source.browser),
            profile: Some(profile.to_owned()),
        }),
        (None, None) => remembered_source.cloned(),
    };
    if let Some(source) = requested_source {
        return prepare_browser_source(source, remembered.as_ref(), &saved_headers);
    }
    if browser.is_none() || browser == Some(Browser::Firefox) {
        let firefox_source = BrowserSource {
            browser: Browser::Firefox,
            profile: None,
        };
        match prepare_browser_source(firefox_source, remembered.as_ref(), &saved_headers) {
            Ok(prepared) => return Ok(prepared),
            Err(error) if browser.is_some() => return Err(error),
            Err(_) => {}
        }
    }
    let sources = match browser {
        Some(browser) => browser_candidates(browser, browser_login::detected_sources()),
        None => browser_login::detected_sources(),
    };
    let mut last_error = None;
    for source in sources {
        match prepare_browser_source(source, remembered.as_ref(), &saved_headers) {
            Ok(prepared) => return Ok(prepared),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| "no signed-in browser session was found. Choose a browser and profile".to_owned()))
}

fn browser_candidates(browser: Browser, detected_sources: Vec<BrowserSource>) -> Vec<BrowserSource> {
    let mut sources: Vec<_> = detected_sources
        .into_iter()
        .filter(|source| source.browser == browser && source.profile.as_deref().is_some())
        .collect();
    sources.push(BrowserSource {
        browser,
        profile: None,
    });
    sources
}

fn prepare_browser_source(
    source: BrowserSource,
    remembered: Option<&RememberedSession>,
    saved_headers: &Map<String, Value>,
) -> Result<PreparedBrowserSession, String> {
    let firefox_error = if source.browser == Browser::Firefox {
        match prepare_firefox(&source, remembered, saved_headers) {
            Ok(prepared) => return Ok(prepared),
            Err(error) => Some(error),
        }
    } else {
        None
    };
    let browser_cookies = browser_login::extract(&source).map_err(|error| match firefox_error {
        Some(firefox_error) => format!("{firefox_error}. {error}"),
        None => error,
    })?;
    prepare_cookie_snapshot(source, &browser_cookies, remembered, saved_headers)
}

fn prepare_firefox(
    source: &BrowserSource,
    remembered: Option<&RememberedSession>,
    saved_headers: &Map<String, Value>,
) -> Result<PreparedBrowserSession, String> {
    let firefox_profiles =
        FirefoxProfile::find_all_for(source.profile.as_deref(), ProfileCapability::Cookies).map_err(
            |error| match source.profile.as_deref() {
                Some(profile) => format!("no Firefox profile named {profile}: {error}"),
                None => error.to_string(),
            },
        )?;
    let mut remembered = remembered
        .filter(|saved| saved.source.browser == Browser::Firefox)
        .cloned();
    if let Some(saved) = &mut remembered
        && let Some(profile) = saved.source.profile.as_deref()
        && let Ok(profile) = FirefoxProfile::find_for(Some(profile), ProfileCapability::Cookies)
    {
        saved.source = BrowserSource {
            browser: Browser::Firefox,
            profile: Some(profile.dir.to_string_lossy().into_owned()),
        };
    }
    let mut last_error = None;
    for profile in firefox_profiles {
        let cookies = match read_cookies(&profile) {
            Ok(cookies) => cookies,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let selected_source = BrowserSource {
            browser: Browser::Firefox,
            profile: Some(profile.dir.to_string_lossy().into_owned()),
        };
        match prepare_cookie_snapshot(selected_source, &cookies, remembered.as_ref(), saved_headers) {
            Ok(prepared) => return Ok(prepared),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "Firefox has no YouTube session, sign in there first".to_owned()))
}

fn prepare_cookie_snapshot(
    source: BrowserSource,
    cookies: &[redwax_marks::Cookie],
    remembered: Option<&RememberedSession>,
    saved_headers: &Map<String, Value>,
) -> Result<PreparedBrowserSession, String> {
    let cookie_export = encode_netscape(cookies);
    let cookie_jar = redwax_auth::CookieJar::from_netscape(
        std::str::from_utf8(&cookie_export).expect("Netscape serialization produces UTF-8"),
    );
    let music_url =
        reqwest::Url::parse("https://music.youtube.com/").expect("the Music origin is a fixed HTTPS URL");
    let unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cookie_header = cookie_jar.for_request(&music_url, unix_seconds).header();
    let mut browser_headers = saved_headers.clone();
    set_cookie_header(&mut browser_headers, &cookie_header);
    let account = remembered
        .filter(|saved| saved.source == source)
        .and_then(|saved| saved.account.clone());
    match &account {
        Some(account) => account_headers(&mut browser_headers, account),
        None => clear_account_headers(&mut browser_headers),
    }
    let identity = account.as_ref().map_or_else(
        || redwax_auth::SelectedIdentity::new(0, None),
        |account| redwax_auth::SelectedIdentity::new(account.authuser, account.page_id.clone()),
    );
    let session = Session::new(cookie_header, identity).map_err(|error| match error {
        redwax_auth::Error::MissingCookies | redwax_auth::Error::MissingSigningCookie => {
            format!("{} has no signed-in YouTube session. Sign in at https://music.youtube.com. For an existing login: {}",
                source.browser.name(), browser_login::browser_access_help(source.browser))
        }
        error => error.to_string(),
    })?;
    Ok(PreparedBrowserSession {
        session,
        source,
        browser_headers,
        cookie_export,
        account,
    })
}

fn publish_exports(exports: &[(&Path, &[u8])]) -> Result<(), String> {
    let previous_exports: Vec<_> = exports
        .iter()
        .map(|(path, _)| previous_export(path))
        .collect::<Result<_, _>>()?;
    let staged_exports: Vec<_> = exports
        .iter()
        .map(|(path, contents)| {
            crate::private_file::StagedWrite::new(path, contents)
                .map_err(|error| format!("could not stage credentials: {error}"))
        })
        .collect::<Result<_, _>>()?;
    publish_staged_exports(staged_exports, exports, previous_exports)
}

fn publish_staged_exports(
    staged_exports: Vec<crate::private_file::StagedWrite>,
    exports: &[(&Path, &[u8])],
    previous_exports: Vec<Option<Vec<u8>>>,
) -> Result<(), String> {
    for staged_export in staged_exports {
        if let Err(error) = staged_export.publish() {
            let mut restored = true;
            for ((path, _), previous_contents) in exports.iter().zip(previous_exports) {
                restored &= restore_export(path, previous_contents).is_ok();
            }
            return if restored {
                Err(format!("could not publish credentials: {error}"))
            } else {
                Err("credential exports disagree after a failed write, run `redwax login`".to_owned())
            };
        }
    }
    Ok(())
}

fn previous_export(export_path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(export_path) {
        Ok(previous_contents) => Ok(Some(previous_contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("could not read a previous credential export".to_owned()),
    }
}

fn restore_export(export_path: &Path, previous_contents: Option<Vec<u8>>) -> Result<(), String> {
    match previous_contents {
        Some(previous_contents) => {
            crate::private_file::write(export_path, &previous_contents).map_err(|error| error.to_string())
        }
        None => match std::fs::remove_file(export_path) {
            Ok(()) => {
                let parent = export_path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|error| error.to_string())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

fn account_headers(headers: &mut Map<String, Value>, account: &Account) {
    clear_account_headers(headers);
    headers.insert(
        "x-goog-authuser".to_owned(),
        Value::String(account.authuser.to_string()),
    );
    if let Some(page_id) = &account.page_id {
        headers.insert("x-goog-pageid".to_owned(), Value::String(page_id.clone()));
    }
}

fn clear_account_headers(headers: &mut Map<String, Value>) {
    headers.retain(|name, _| {
        !name.eq_ignore_ascii_case("x-goog-authuser") && !name.eq_ignore_ascii_case("x-goog-pageid")
    });
    headers.insert("x-goog-authuser".to_owned(), Value::String("0".to_owned()));
}

fn set_cookie_header(browser_headers: &mut Map<String, Value>, cookie_header_value: &str) {
    browser_headers.retain(|name, _| !name.eq_ignore_ascii_case("cookie"));
    browser_headers.insert("cookie".to_owned(), Value::String(cookie_header_value.to_owned()));
}

/// Login can replace an invalid export because the browser remains the source of
/// truth. Filesystem failures still need intervention before credentials move.
fn read_import_headers(auth_path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(auth_path) {
        Ok(serialized_headers) => match serde_json::from_str::<Value>(&serialized_headers) {
            Ok(Value::Object(browser_headers)) => Ok(browser_headers),
            Ok(_) | Err(_) => Ok(default_headers()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default_headers()),
        Err(error) => Err(format!("{}: {error}", auth_path.display())),
    }
}

fn default_headers() -> Map<String, Value> {
    let mut headers = Map::new();
    for (name, value) in [
        ("accept", "*/*"),
        ("accept-encoding", "gzip, deflate"),
        ("content-type", "application/json"),
        ("origin", "https://music.youtube.com"),
        ("x-goog-authuser", "0"),
    ] {
        headers.insert(name.to_owned(), Value::String(value.to_owned()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn concurrent_refreshes_share_the_import_and_its_failure() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let coordinator = Arc::new(SessionCoordinator::new(
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
                Err("fixture failure".to_owned())
            }),
            Duration::from_secs(60),
        ));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let coordinator = coordinator.clone();
                std::thread::spawn(move || coordinator.refresh())
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), Err("fixture failure".to_owned()));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        coordinator.close().unwrap();
        assert!(coordinator.refresh().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn logout_publishes_after_an_already_running_import() {
        use std::sync::{Arc, mpsc};
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let order = Arc::new(Mutex::new(Vec::new()));
        let imported = order.clone();
        let coordinator = Arc::new(SessionCoordinator::new(
            Arc::new(move || {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                imported.lock().unwrap().push("import");
                Ok(())
            }),
            Duration::ZERO,
        ));
        let refreshing = coordinator.clone();
        let refresh = std::thread::spawn(move || refreshing.refresh());
        started.recv().unwrap();
        let logout_owner = coordinator.clone();
        let logged_out = order.clone();
        let logout = std::thread::spawn(move || {
            logout_owner.logout(|| {
                logged_out.lock().unwrap().push("logout");
                Ok(())
            })
        });
        release.send(()).unwrap();
        refresh.join().unwrap().unwrap();
        logout.join().unwrap().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["import", "logout"]);
        assert!(coordinator.refresh().is_err());
    }

    #[test]
    fn logout_prevents_refresh_after_the_throttle_window() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let coordinator = SessionCoordinator::new(
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
            Duration::ZERO,
        );
        coordinator.refresh().unwrap();
        coordinator.logout(|| Ok(())).unwrap();
        assert!(coordinator.refresh().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_metadata_publish_restores_all_three_previous_exports() {
        let dir = std::env::temp_dir().join(format!("redwax-export-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let paths = [
            dir.join("cookies.txt"),
            dir.join("browser.json"),
            dir.join("session.json"),
        ];
        let old_contents = [
            b"old cookie".as_slice(),
            b"old auth".as_slice(),
            b"old metadata".as_slice(),
        ];
        let exports: Vec<_> = paths
            .iter()
            .map(|path| (path.as_path(), b"new fixture".as_slice()))
            .collect();
        for (path, contents) in paths.iter().zip(old_contents) {
            std::fs::write(path, contents).unwrap();
        }
        let previous = paths.iter().map(|path| previous_export(path).unwrap()).collect();
        let staged = exports
            .iter()
            .map(|(path, contents)| crate::private_file::StagedWrite::new(path, contents).unwrap())
            .collect();
        let staged_metadata = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("session.json.")
            })
            .unwrap();
        std::fs::remove_file(staged_metadata).unwrap();

        assert!(publish_staged_exports(staged, &exports, previous).is_err());

        for (path, contents) in paths.iter().zip(old_contents) {
            assert_eq!(std::fs::read(path).unwrap(), contents);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_rollback_reports_that_credentials_may_disagree() {
        let dir = std::env::temp_dir().join(format!("redwax-export-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let cookie_path = dir.join("cookies.txt");
        std::fs::write(&cookie_path, b"old fixture").unwrap();
        let staged = crate::private_file::StagedWrite::new(&cookie_path, b"new fixture").unwrap();
        let previous = vec![previous_export(&cookie_path).unwrap()];
        std::fs::remove_file(&cookie_path).unwrap();
        std::fs::create_dir(&cookie_path).unwrap();

        let result =
            publish_staged_exports(vec![staged], &[(cookie_path.as_path(), b"new fixture")], previous);

        assert!(result.unwrap_err().contains("credential exports disagree"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn logout_leaves_personal_playback_files_in_place() {
        let dir = std::env::temp_dir().join(format!("redwax-logout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let config = Config {
            auth_path: dir.join("browser.json"),
            cookies_path: dir.join("cookies.txt"),
            session_path: dir.join("session.json"),
            state_path: dir.join("state.json"),
            ..Config::default()
        };
        for path in [
            &config.auth_path,
            &config.cookies_path,
            &config.session_path,
            &config.state_path,
        ] {
            std::fs::write(path, b"fixture").unwrap();
        }
        remove_exports(&config).unwrap();
        assert!(config.state_path.is_file());
        assert!(!config.auth_path.exists());
        assert!(!config.cookies_path.exists());
        assert!(!config.session_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn test_cookie(name: &str, value: &str, host: &str) -> redwax_marks::Cookie {
        redwax_marks::Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            host: host.to_owned(),
            include_subdomains: host.starts_with('.'),
            path: "/".to_owned(),
            secure: true,
            expiry: 0,
        }
    }

    fn test_account() -> Account {
        Account {
            authuser: 1,
            page_id: Some("channel".to_owned()),
            name: "Test account".to_owned(),
            handle: None,
        }
    }

    #[test]
    fn preparation_and_selection_leave_exports_unchanged_until_publication() {
        let dir = std::env::temp_dir().join(format!("redwax-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let config = Config {
            auth_path: dir.join("browser.json"),
            cookies_path: dir.join("cookies.txt"),
            session_path: dir.join("session.json"),
            ..Config::default()
        };
        for path in [&config.auth_path, &config.cookies_path, &config.session_path] {
            std::fs::write(path, b"old fixture").unwrap();
        }
        let source = BrowserSource {
            browser: Browser::Chrome,
            profile: Some("Profile 2".to_owned()),
        };
        let mut prepared = prepare_cookie_snapshot(
            source.clone(),
            &[test_cookie("SAPISID", "secret", ".youtube.com")],
            None,
            &default_headers(),
        )
        .unwrap();
        prepared.select_account(&test_account()).unwrap();
        for path in [&config.auth_path, &config.cookies_path, &config.session_path] {
            assert_eq!(std::fs::read(path).unwrap(), b"old fixture");
        }
        let session = prepared.publish(&config).unwrap();
        assert_eq!(Session::read(&config.auth_path).unwrap(), Some(session));
        assert_eq!(
            RememberedSession::read(&config.session_path),
            Some(RememberedSession {
                source,
                account: Some(test_account())
            })
        );
        for path in [&config.auth_path, &config.cookies_path, &config.session_path] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imported_music_header_keeps_only_cookies_in_request_scope() {
        let cookies = [
            test_cookie("SAPISID", "secret", ".youtube.com"),
            test_cookie("SID", "private", "www.youtube.com"),
            redwax_marks::Cookie {
                path: "/other".to_owned(),
                ..test_cookie("PREF", "f1", ".youtube.com")
            },
            redwax_marks::Cookie {
                include_subdomains: false,
                ..test_cookie("HSID", "private", ".youtube.com")
            },
            test_cookie("LOGIN_INFO", "private", "music.youtube.com"),
        ];
        let prepared = prepare_cookie_snapshot(
            BrowserSource {
                browser: Browser::Firefox,
                profile: None,
            },
            &cookies,
            None,
            &default_headers(),
        )
        .unwrap();
        assert_eq!(
            prepared.session().cookie_header(),
            "SAPISID=secret; LOGIN_INFO=private"
        );
    }

    #[test]
    fn selected_account_is_preserved_only_for_the_same_browser_and_profile() {
        let source = BrowserSource {
            browser: Browser::Chrome,
            profile: Some("Profile 2".to_owned()),
        };
        let remembered = RememberedSession {
            source: source.clone(),
            account: Some(test_account()),
        };
        let cookies = [test_cookie("SAPISID", "secret", ".youtube.com")];
        let prepared =
            prepare_cookie_snapshot(source, &cookies, Some(&remembered), &default_headers()).unwrap();
        assert_eq!(prepared.session().identity().authuser(), 1);
        for different in [
            BrowserSource {
                browser: Browser::Firefox,
                profile: Some("Profile 2".to_owned()),
            },
            BrowserSource {
                browser: Browser::Chrome,
                profile: Some("Default".to_owned()),
            },
        ] {
            let prepared =
                prepare_cookie_snapshot(different, &cookies, Some(&remembered), &default_headers()).unwrap();
            assert_eq!(prepared.session().identity().authuser(), 0);
            assert_eq!(prepared.session().identity().page_id(), None);
            assert_eq!(prepared.account, None);
        }
    }

    #[test]
    fn cookie_replacement_removes_all_case_variants_of_the_old_header() {
        let mut headers = Map::from_iter([
            ("Cookie".to_owned(), Value::String("OLD=1".to_owned())),
            ("cookie".to_owned(), Value::String("OLD=2".to_owned())),
        ]);
        set_cookie_header(&mut headers, "SAPISID=new");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["cookie"], Value::String("SAPISID=new".to_owned()));
    }

    #[test]
    fn changing_profiles_clears_both_previous_identity_headers() {
        let mut headers = Map::from_iter([
            ("X-Goog-AuthUser".to_owned(), Value::String("2".to_owned())),
            ("x-goog-pageid".to_owned(), Value::String("channel".to_owned())),
            ("cookie".to_owned(), Value::String("SAPISID=secret".to_owned())),
        ]);

        clear_account_headers(&mut headers);

        assert_eq!(headers.get("x-goog-authuser").and_then(Value::as_str), Some("0"));
        assert!(
            !headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("x-goog-pageid"))
        );
        assert!(headers.contains_key("cookie"));
    }

    #[test]
    fn login_replaces_malformed_or_non_object_header_exports() {
        let dir = std::env::temp_dir().join(format!("redwax-invalid-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("browser.json");

        for invalid_export in ["{", "[]", "null"] {
            std::fs::write(&path, invalid_export).unwrap();
            assert_eq!(read_import_headers(&path).unwrap(), default_headers());
        }

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_browser_tries_each_discovered_profile_before_the_default() {
        let discovered = vec![
            BrowserSource {
                browser: Browser::Chrome,
                profile: Some("/chrome/Default".to_owned()),
            },
            BrowserSource {
                browser: Browser::Firefox,
                profile: None,
            },
            BrowserSource {
                browser: Browser::Chrome,
                profile: Some("/chrome/Profile 2".to_owned()),
            },
        ];

        let sources = browser_candidates(Browser::Chrome, discovered);

        assert_eq!(
            sources,
            vec![
                BrowserSource {
                    browser: Browser::Chrome,
                    profile: Some("/chrome/Default".to_owned())
                },
                BrowserSource {
                    browser: Browser::Chrome,
                    profile: Some("/chrome/Profile 2".to_owned())
                },
                BrowserSource {
                    browser: Browser::Chrome,
                    profile: None
                },
            ]
        );
    }
}
