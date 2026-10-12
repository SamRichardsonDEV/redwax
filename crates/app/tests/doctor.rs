//! The offline doctor against a throwaway XDG root, driven through
//! `redwax::doctor::run` rather than the binary so the whole file is fast.
//!
//! The two cases are the ones the nightly timer distinguishes: a machine with
//! no session, which must fail, and one with every offline dependency in place,
//! which must not.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use redwax::doctor::{Options, Report, Status};

/// One test's own config and cache roots, removed when the test ends.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "redwax-doctor-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/redwax")).expect("the temp dir is writable");
        std::fs::create_dir_all(root.join("cache/redwax")).expect("the temp dir is writable");
        Sandbox { root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config/redwax")
    }

    fn cache_dir(&self) -> PathBuf {
        self.root.join("cache/redwax")
    }

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the temp dir is writable");
        }
        std::fs::write(&path, body).expect("the temp dir is writable");
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const CONFIG: &str = "theme = \"terminal\"\nicons = \"nerd\"\nart = \"halfcell\"\n\
transparent = true\nquality = \"high\"\nvolume = 80\n";

/// ytmusicapi's header file, with a cookie no service would accept.
const BROWSER_JSON: &str = "{\"cookie\": \"SAPISID=not-a-real-value\", \
\"x-goog-authuser\": \"1\", \"x-goog-pageid\": \"UCtest\"}";

const COOKIES: &str = "# Netscape HTTP Cookie File\n\
.youtube.com\tTRUE\t/\tTRUE\t1800000000\tSAPISID\tnot-a-real-value\n\
.youtube.com\tTRUE\t/\tTRUE\t1800000000\t__Secure-3PAPISID\tnot-a-real-value\n\
.youtube.com\tTRUE\t/\tTRUE\t1800000000\t__Secure-3PSID\tnot-a-real-value\n\
.youtube.com\tTRUE\t/\tTRUE\t1800000000\tLOGIN_INFO\tnot-a-real-value\n";

/// `config_dir` and `cache_dir` read the environment, and the environment is
/// process wide, so one case at a time owns it.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

fn doctor_in(sandbox: &Sandbox, config: &Path) -> Report {
    let _held: MutexGuard<()> = ENVIRONMENT.lock().unwrap_or_else(|held| held.into_inner());
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", sandbox.root.join("config"));
        std::env::set_var("XDG_CACHE_HOME", sandbox.root.join("cache"));
    }
    let options = Options {
        config_path: Some(config.to_path_buf()),
        live: false,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current thread runtime always builds")
        .block_on(redwax::doctor::run(&options))
}

fn status_of(report: &Report, name: &str) -> Status {
    report
        .get(name)
        .unwrap_or_else(|| panic!("the report has no {name} line: {:?}", report.lines()))
        .status
}

#[test]
fn the_offline_doctor_names_a_missing_session_and_a_config_that_parses() {
    let sandbox = Sandbox::new("bare");
    let config = sandbox.write("config/redwax/config.toml", CONFIG);
    let report = doctor_in(&sandbox, &config);

    assert_eq!(status_of(&report, "config"), Status::Ok);
    assert_eq!(status_of(&report, "session"), Status::Fail);
    assert_eq!(status_of(&report, "cookies"), Status::Fail);
    assert!(report.failed(), "exit code 1: {:?}", report.lines());

    // The config line carries the values a user changes, not just the path.
    let detail = &report.get("config").expect("the config line").detail;
    for shown in ["theme=terminal", "icons=nerd", "art=halfcell", "quality=high"] {
        assert!(detail.contains(shown), "{shown} is missing from {detail}");
    }
    assert!(detail.contains("transparent=true"), "{detail}");
    assert!(detail.contains("library_dir="), "{detail}");
}

#[test]
fn the_offline_doctor_passes_when_every_dependency_is_in_place() {
    let sandbox = Sandbox::new("full");
    let config = sandbox.write("config/redwax/config.toml", CONFIG);
    sandbox.write("config/redwax/browser.json", BROWSER_JSON);
    sandbox.write("config/redwax/cookies.txt", COOKIES);
    sandbox.write("cache/redwax/innertube.json", "{}");
    sandbox.write("cache/redwax/player/aaaaaaaa.pre.json", "{}");
    sandbox.write("cache/redwax/pages/home.json", "{}");
    sandbox.write(
        "cache/redwax/solver/lib.json",
        "{\"data\": {\"code\": \"const library = {};\"}}",
    );
    sandbox.write("cache/redwax/solver/core.js", "// core\n");
    sandbox.write(
        "cache/redwax/state.json",
        "{\"queue\":[],\"index\":null,\"position\":0.0,\"volume\":80,\
\"repeat\":\"off\",\"shuffle\":false}",
    );

    let report = doctor_in(&sandbox, &config);

    assert_eq!(status_of(&report, "config"), Status::Ok);
    assert_eq!(status_of(&report, "session"), Status::Ok);
    assert_eq!(status_of(&report, "cookies"), Status::Ok);
    assert_eq!(status_of(&report, "page cache"), Status::Ok);
    assert_eq!(status_of(&report, "state"), Status::Ok);
    assert!(!report.failed(), "exit code 0: {:?}", report.lines());

    // The cookie value never leaves the file.
    let joined = report.lines().join("\n");
    assert!(!joined.contains("not-a-real-value"), "{joined}");

    let session = &report.get("session").expect("the session line").detail;
    assert!(session.contains("authuser 1"), "{session}");
    assert!(session.contains("page id set"), "{session}");

    assert_eq!(sandbox.config_dir().join("config.toml"), config);
    assert!(sandbox.cache_dir().join("state.json").is_file());
}
