//! The session refresh, against a Firefox profile built here rather than the
//! one on the machine.
//!
//! The first two names are the Python `test_auth.py` cases they port. The
//! third is the account switch `redwax login --account` performs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use redwax::browser_login::{Browser, BrowserSource};
use redwax::config::Config;
use redwax::session::{RememberedSession, prepare_from_browser};
use redwax_ytm::Account;
use rusqlite::Connection;
use serde_json::Value;

/// `FirefoxProfile::find` reads the environment, which is process wide.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

const PROFILE_NAME: &str = "testprofile";

struct EnvironmentGuard {
    _lock: MutexGuard<'static, ()>,
    previous_config_home: Option<OsString>,
    previous_ytdlp: Option<OsString>,
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_config_home {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.previous_ytdlp {
                Some(value) => std::env::set_var("REDWAX_YTDLP", value),
                None => std::env::remove_var("REDWAX_YTDLP"),
            }
        }
    }
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "redwax-session-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the temp dir is writable");
        Sandbox { root }
    }

    fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    fn profile_dir(&self) -> PathBuf {
        self.config_home().join("mozilla/firefox").join(PROFILE_NAME)
    }

    /// A profile Firefox would recognise: a `profiles.ini` pointing at it, the
    /// IndexedDB directory `find` requires, and a cookie store.
    fn with_firefox_profile(&self, cookies: &[(&str, &str)]) -> &Sandbox {
        let firefox = self.config_home().join("mozilla/firefox");
        std::fs::create_dir_all(&firefox).expect("the temp dir is writable");
        std::fs::write(
            firefox.join("profiles.ini"),
            format!("[Profile0]\nName={PROFILE_NAME}\nIsRelative=1\nPath={PROFILE_NAME}\nDefault=1\n"),
        )
        .expect("the temp dir is writable");
        std::fs::create_dir_all(
            self.profile_dir()
                .join("storage/default/https+++music.youtube.com/idb"),
        )
        .expect("the temp dir is writable");
        write_cookie_store(&self.profile_dir().join("cookies.sqlite"), cookies);
        self
    }

    fn config(&self) -> Config {
        Config {
            auth_path: self.root.join("browser.json"),
            cookies_path: self.root.join("cookies.txt"),
            session_path: self.root.join("session.json"),
            ..Config::default()
        }
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, body).expect("the temp dir is writable");
        path
    }

    fn hold(&self) -> EnvironmentGuard {
        let held = ENVIRONMENT.lock().unwrap_or_else(|held| held.into_inner());
        let guard = EnvironmentGuard {
            _lock: held,
            previous_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            previous_ytdlp: std::env::var_os("REDWAX_YTDLP"),
        };
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", self.config_home());
            std::env::set_var("REDWAX_YTDLP", self.root.join("nonexistent-ytdlp"));
        }
        guard
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The three columns `read_cookies` selects, plus the two it filters on.
fn write_cookie_store(path: &Path, cookies: &[(&str, &str)]) {
    let db = Connection::open(path).expect("sqlite writes to a temp file");
    db.execute_batch(
        "create table moz_cookies (name text, value text, host text, path text, \
         isSecure integer, expiry integer);",
    )
    .expect("the table is created once");
    for (name, value) in cookies {
        db.execute(
            "insert into moz_cookies values (?1, ?2, '.youtube.com', '/', 1, 1800000000)",
            (name, value),
        )
        .expect("the row is inserted");
    }
}

fn headers(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("the header file is written");
    serde_json::from_str(&text).expect("the header file is JSON")
}

fn save_remembered(config: &Config, source: &BrowserSource, account: Option<&Account>) {
    let saved = serde_json::json!({
        "browser": source.browser.name(), "profile": source.profile.as_deref(), "account": account,
    });
    std::fs::write(&config.session_path, serde_json::to_vec(&saved).unwrap()).unwrap();
}

fn brand() -> Account {
    Account {
        authuser: 1,
        page_id: Some("1051934".to_string()),
        name: "Brand".to_string(),
        handle: Some("@brand".to_string()),
    }
}

fn plain() -> Account {
    Account {
        authuser: 0,
        page_id: None,
        name: "Sam".to_string(),
        handle: Some("@sam".to_string()),
    }
}

#[test]
fn test_refresh_reimports_from_the_remembered_browser_and_reselects_the_identity() {
    let sandbox = Sandbox::new("refresh");
    sandbox.with_firefox_profile(&[("SAPISID", "s"), ("__Secure-3PAPISID", "fresh")]);
    let config = sandbox.config();
    sandbox.write(
        "browser.json",
        "{\"cookie\": \"STALE=1\", \"x-goog-authuser\": \"0\"}",
    );
    let _held = sandbox.hold();
    save_remembered(
        &config,
        &BrowserSource {
            browser: Browser::Firefox,
            profile: Some(PROFILE_NAME.to_owned()),
        },
        Some(&brand()),
    );
    let session = prepare_from_browser(&config, None, None)
        .expect("the profile is found")
        .publish(&config)
        .expect("the exports are written");

    assert!(
        session.cookie_header().contains("fresh"),
        "{}",
        session.cookie_header()
    );
    assert!(
        !session.cookie_header().contains("STALE"),
        "{}",
        session.cookie_header()
    );
    assert_eq!(session.identity().authuser(), 1);
    assert_eq!(session.identity().page_id(), Some("1051934"));

    let saved = headers(&config.auth_path);
    assert_eq!(saved["x-goog-authuser"], Value::from("1"));
    assert_eq!(saved["x-goog-pageid"], Value::from("1051934"));
    assert!(
        saved["cookie"]
            .as_str()
            .expect("a cookie header")
            .contains("fresh"),
        "{saved}"
    );

    let netscape = std::fs::read_to_string(&config.cookies_path).expect("cookies.txt is written");
    assert!(netscape.contains("SAPISID"), "{netscape}");
    assert!(netscape.contains("__Secure-3PAPISID"), "{netscape}");
}

#[test]
fn test_refresh_without_a_remembered_session_says_so() {
    let sandbox = Sandbox::new("no-profile");
    std::fs::create_dir_all(sandbox.config_home()).expect("the temp dir is writable");
    let config = sandbox.config();
    let _held = sandbox.hold();

    let error = prepare_from_browser(&config, Some(Browser::Firefox), Some("nonexistent-profile"))
        .expect_err("there is no profile to read");

    assert!(error.contains("profile"), "{error}");
    assert!(!config.cookies_path.exists());
}

#[test]
fn test_select_account_rewrites_the_saved_headers() {
    let sandbox = Sandbox::new("select");
    let config = sandbox.config();
    sandbox.with_firefox_profile(&[("SAPISID", "s")]);
    let _held = sandbox.hold();
    let mut prepared = prepare_from_browser(&config, Some(Browser::Firefox), Some(PROFILE_NAME)).unwrap();
    prepared
        .select_account(&brand())
        .expect("the identity is selected before publication");
    assert!(!config.auth_path.exists());
    prepared.publish(&config).expect("the header file is rewritten");
    let saved = headers(&config.auth_path);
    assert_eq!(saved["x-goog-authuser"], Value::from("1"));
    assert_eq!(saved["x-goog-pageid"], Value::from("1051934"));
    assert_eq!(saved["cookie"], Value::from("SAPISID=s"));

    // The page id is removed for an identity that has none, because an empty
    // header is not the same as no header.
    let mut prepared = prepare_from_browser(&config, Some(Browser::Firefox), Some(PROFILE_NAME)).unwrap();
    prepared
        .select_account(&plain())
        .expect("the primary identity is selected");
    prepared.publish(&config).expect("the header file is rewritten");
    let saved = headers(&config.auth_path);
    assert_eq!(saved["x-goog-authuser"], Value::from("0"));
    assert!(saved.get("x-goog-pageid").is_none(), "{saved}");
}

#[test]
fn a_remembered_session_keeps_the_python_shape() {
    let sandbox = Sandbox::new("remember");
    let config = sandbox.config();
    sandbox.with_firefox_profile(&[("SAPISID", "s")]);
    let _held = sandbox.hold();
    let mut prepared = prepare_from_browser(&config, Some(Browser::Firefox), Some(PROFILE_NAME)).unwrap();
    prepared.select_account(&brand()).unwrap();
    prepared.publish(&config).unwrap();

    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&config.session_path).expect("the file is written"))
            .expect("the file is JSON");
    assert_eq!(saved["browser"], Value::from("firefox"));
    assert_eq!(
        saved["profile"],
        Value::from(sandbox.profile_dir().to_string_lossy().into_owned())
    );
    assert_eq!(saved["account"]["authuser"], Value::from(1));
    assert_eq!(saved["account"]["page_id"], Value::from("1051934"));
    assert_eq!(saved["account"]["handle"], Value::from("@brand"));
    assert_eq!(
        RememberedSession::read(&config.session_path).and_then(|saved| saved.account),
        Some(brand())
    );
}

#[test]
fn choosing_firefox_ignores_a_remembered_chrome_profile_and_account() {
    let sandbox = Sandbox::new("browser-change");
    sandbox.with_firefox_profile(&[("SAPISID", "s")]);
    let config = sandbox.config();
    let _held = sandbox.hold();
    save_remembered(
        &config,
        &BrowserSource {
            browser: Browser::Chrome,
            profile: Some("Profile 2".to_owned()),
        },
        Some(&brand()),
    );

    let prepared = prepare_from_browser(&config, Some(Browser::Firefox), None).unwrap();

    assert_eq!(prepared.source().browser, Browser::Firefox);
    assert_eq!(
        prepared.source().profile.as_deref(),
        sandbox.profile_dir().to_str()
    );
    assert_eq!(prepared.session().identity().authuser(), 0);
    assert_eq!(prepared.session().identity().page_id(), None);
    assert!(!config.auth_path.exists());
    assert!(!config.cookies_path.exists());
    assert_eq!(
        RememberedSession::read(&config.session_path)
            .unwrap()
            .source
            .browser,
        Browser::Chrome
    );
}
