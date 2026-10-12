//! First-run commands must stay useful without an account or existing config.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct FreshInstall {
    root: PathBuf,
}

impl FreshInstall {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "redwax-first-run-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        for directory in ["home", "config", "cache", "bin"] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        let install = Self { root };
        install.program("mpv", "printf 'mpv 0.41\\n'");
        install.program("node", "printf 'v22.0.0\\n'");
        install.program("yt-dlp", "case \"$*\" in *--version*) printf '2026.08.19\\n' ;; *--help*) printf '%s\\n' --cookies-from-browser --load-info-json --js-runtimes --remote-components ;; *) exit 1 ;; esac");
        std::fs::write(install.root.join("library.js"), "const lib = {};").unwrap();
        std::fs::write(install.root.join("core.js"), "function jsc() { return {}; }").unwrap();
        install
    }

    fn program(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_redwax"))
            .args(arguments)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("PATH", self.root.join("bin"))
            .env("REDWAX_NODE", self.root.join("bin/node"))
            .env("REDWAX_YTDLP", self.root.join("bin/yt-dlp"))
            .env("REDWAX_SOLVER_LIBRARY", self.root.join("library.js"))
            .env("REDWAX_SOLVER_CORE", self.root.join("core.js"))
            .env_remove("REDWAX_CONFIG")
            .env_remove("BROWSER")
            .output()
            .unwrap()
    }

    fn auth_path(&self) -> PathBuf {
        self.root.join("config/redwax/browser.json")
    }
}

impl Drop for FreshInstall {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn fresh_setup_explains_browser_login_without_creating_credentials() {
    let install = FreshInstall::new();
    let output = install.run(&["setup"]);
    assert_eq!(output.status.code(), Some(1));
    let message = text(&output);
    assert!(message.contains("not signed in"), "{message}");
    assert!(message.contains("--browser NAME"), "{message}");
    assert!(!install.auth_path().exists());
}

#[test]
fn unsupported_dependencies_are_rejected_before_import() {
    let install = FreshInstall::new();
    install.program("node", "printf 'v20.18.0\\n'");
    let output = install.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    let message = text(&output);
    assert!(message.contains("22 or newer"), "{message}");
    assert!(!message.contains("Automatic browser login"), "{message}");
    assert!(!install.auth_path().exists());

    install.program("node", "printf 'v22.0.0\\n'");
    install.program("yt-dlp", "printf 'old yt-dlp\\n'");
    let output = install.run(&["setup"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("upgrade yt-dlp"));
}

#[test]
fn startup_discovers_other_browsers_and_recovers_invalid_saved_login() {
    let install = FreshInstall::new();
    std::fs::create_dir(install.root.join("config/google-chrome")).unwrap();
    let output = install.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("could not read chrome cookies"));
    std::fs::create_dir_all(install.auth_path().parent().unwrap()).unwrap();
    std::fs::write(install.auth_path(), "invalid saved credentials").unwrap();
    let output = install.run(&[]);
    let message = text(&output);
    assert!(message.contains("Trying your browser session"), "{message}");
    assert!(message.contains("could not read chrome cookies"), "{message}");
    assert_eq!(
        std::fs::read_to_string(install.auth_path()).unwrap(),
        "invalid saved credentials"
    );
}

#[test]
fn every_supported_browser_reports_import_failure_without_claiming_login() {
    let install = FreshInstall::new();
    for browser in redwax::browser_login::Browser::ALL {
        let output = install.run(&[
            "login",
            "--browser",
            browser.name(),
            "--profile",
            "Missing Profile",
        ]);
        assert_eq!(output.status.code(), Some(1));
        let message = text(&output);
        assert!(message.contains(browser.name()), "{message}");
        assert!(!message.contains("Signed in as"), "{message}");
        assert!(!Path::new(&install.auth_path()).exists());
    }
    let output = install.run(&["login", "--browser", "unsupported"]);
    assert_eq!(output.status.code(), Some(2));
}
