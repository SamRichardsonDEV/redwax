use std::path::{Path, PathBuf};
use std::time::Duration;

use redwax_ytm::Session;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramStatus {
    Missing,
    Ready {
        path: PathBuf,
        version: String,
    },
    Unusable {
        path: PathBuf,
    },
    Unsupported {
        path: PathBuf,
        version: String,
        requirement: &'static str,
    },
}

impl ProgramStatus {
    pub fn ready(&self) -> bool {
        matches!(self, ProgramStatus::Ready { .. })
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            ProgramStatus::Ready { version, .. } => Some(version),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Ready,
    Missing,
    Empty,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prerequisites {
    pub mpv: ProgramStatus,
    pub ytdlp: ProgramStatus,
    pub node: ProgramStatus,
    pub solver_ready: bool,
    pub proof_token_provider_ready: bool,
    pub session: SessionStatus,
}

impl Prerequisites {
    /// Onboarding needs playback prerequisites without maintenance or live probes.
    pub async fn inspect(config: &Config) -> Self {
        let resolver_diagnostics = redwax_resolve::ResolverDiagnostics::inspect(&crate::config::cache_dir());
        let solver_ready = resolver_diagnostics.solver_ready();
        let proof_token_provider_ready = resolver_diagnostics.proof_token_provider.is_some();
        let node_path = resolver_diagnostics.node;
        let ytdlp_path = resolver_diagnostics.ytdlp;
        let (mpv_status, ytdlp_status, node_status) = tokio::join!(
            inspect_program(on_path("mpv")),
            inspect_ytdlp(discovered_program(ytdlp_path)),
            inspect_node(discovered_program(node_path)),
        );
        Self {
            mpv: mpv_status,
            ytdlp: ytdlp_status,
            node: node_status,
            solver_ready,
            proof_token_provider_ready,
            session: session_status(&config.auth_path),
        }
    }
}

pub fn session_status(auth_path: &Path) -> SessionStatus {
    match Session::read(auth_path) {
        Ok(Some(_)) => SessionStatus::Ready,
        Ok(None) if !auth_path.exists() => SessionStatus::Missing,
        Ok(None) => SessionStatus::Empty,
        Err(_) => SessionStatus::Invalid,
    }
}

pub fn on_path(program_name: &str) -> Option<PathBuf> {
    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .map(|directory| directory.join(program_name))
        .find(|path| path.is_file())
}

fn discovered_program(path: PathBuf) -> Option<PathBuf> {
    if path.components().count() == 1 {
        path.to_str().and_then(on_path)
    } else if path.is_file() {
        Some(path)
    } else {
        None
    }
}

async fn inspect_program(path: Option<PathBuf>) -> ProgramStatus {
    let Some(path) = path else {
        return ProgramStatus::Missing;
    };
    let version_output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new(&path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let Ok(Ok(version_output)) = version_output else {
        return ProgramStatus::Unusable { path };
    };
    if !version_output.status.success() {
        return ProgramStatus::Unusable { path };
    }
    let version = String::from_utf8_lossy(&version_output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if version.is_empty() {
        ProgramStatus::Unusable { path }
    } else {
        ProgramStatus::Ready { path, version }
    }
}

async fn inspect_node(path: Option<PathBuf>) -> ProgramStatus {
    let status = inspect_program(path).await;
    match status {
        ProgramStatus::Ready { path, version } if !supported_node(&version) => ProgramStatus::Unsupported {
            path,
            version,
            requirement: "Node.js 22 or newer is required",
        },
        status => status,
    }
}

fn supported_node(version: &str) -> bool {
    version
        .strip_prefix('v')
        .unwrap_or(version)
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 22)
}

async fn inspect_ytdlp(path: Option<PathBuf>) -> ProgramStatus {
    let status = inspect_program(path).await;
    let ProgramStatus::Ready { path, version } = status else {
        return status;
    };
    let help = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new(&path)
            .args(["--ignore-config", "--help"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if let Ok(Ok(help)) = help
        && help.status.success()
        && supported_ytdlp(&String::from_utf8_lossy(&help.stdout))
    {
        return ProgramStatus::Ready { path, version };
    }
    ProgramStatus::Unsupported {
        path,
        version,
        requirement: "upgrade yt-dlp for browser login and JavaScript support",
    }
}

fn supported_ytdlp(help: &str) -> bool {
    [
        "--cookies-from-browser",
        "--load-info-json",
        "--js-runtimes",
        "--remote-components",
    ]
    .iter()
    .all(|option| help.contains(option))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_must_support_the_youtube_solver() {
        for version in ["v18.20.0", "v20.18.1", "unrecognized"] {
            assert!(!supported_node(version));
        }
        for version in ["v22.0.0", "v24.1.0", "26.8.1"] {
            assert!(supported_node(version));
        }
    }

    #[test]
    fn ytdlp_needs_browser_import_and_javascript_options() {
        let options = [
            "--cookies-from-browser",
            "--load-info-json",
            "--js-runtimes",
            "--remote-components",
        ];
        assert!(supported_ytdlp(&options.join("\n")));
        for missing in options {
            assert!(!supported_ytdlp(
                &options
                    .into_iter()
                    .filter(|option| *option != missing)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    #[tokio::test]
    async fn a_missing_program_is_absence_and_a_failed_probe_is_unusable() {
        assert_eq!(inspect_program(None).await, ProgramStatus::Missing);
        let path = PathBuf::from("/nonexistent/redwax-synthetic-program");
        assert_eq!(
            inspect_program(Some(path.clone())).await,
            ProgramStatus::Unusable { path }
        );
    }
}
