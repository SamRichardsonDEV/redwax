//! First-run guidance shares prerequisite detection with doctor.

use std::path::Path;

use crate::config::Config;
use crate::prerequisites::{Prerequisites, ProgramStatus, SessionStatus};

/// A step of the first run: what it is, whether it is satisfied, and what to
/// type when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub ready: bool,
    /// What was found, or what is missing.
    pub detail: String,
    /// The package that provides it, for the one install line. Empty for a
    /// step no package manager can satisfy, which is the session.
    pub package: &'static str,
}

/// The first run, as a list of steps and whether it can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub steps: Vec<Step>,
    /// How this machine installs things, for the one command the block ends on.
    pub manager: PackageManager,
    pub warnings: Vec<String>,
}

impl Welcome {
    pub fn ready(&self) -> bool {
        self.steps.iter().all(|step| step.ready)
    }

    /// The block a first run prints: every step, then the one thing to do next.
    ///
    /// Everything missing that a package manager can supply is one command,
    /// because three missing programs are one apt away and not three.
    pub fn lines(&self) -> Vec<String> {
        let mut instruction_lines: Vec<String> = self
            .steps
            .iter()
            .map(|step| {
                let status_symbol = match step.ready {
                    true => "\u{2713}",
                    false => "\u{2717}",
                };
                format!("  {status_symbol}  {:<10} {}", step.name, step.detail)
            })
            .collect();
        for warning in &self.warnings {
            instruction_lines.push(format!("  warning: {warning}"));
        }

        let missing_packages: Vec<&str> = self
            .steps
            .iter()
            .filter(|step| !step.ready && !step.package.is_empty())
            .map(|step| step.package)
            .collect();
        if !missing_packages.is_empty() {
            instruction_lines.push(String::new());
            match self.manager.install(&missing_packages.join(" ")) {
                // A distribution redwax has no command for still gets the list.
                command if command.is_empty() => {
                    instruction_lines.push(format!("Next, install {}.", missing_packages.join(", ")));
                }
                command => {
                    instruction_lines.push(format!("Next, install {}:", plain_list(&missing_packages)));
                    instruction_lines.push(String::new());
                    instruction_lines.push(format!("  {command}"));
                }
            }
            instruction_lines.push(format!(
                "After installing, run `{} setup` again.",
                crate::cli::program_name()
            ));
            return instruction_lines;
        }

        if self.steps.iter().any(|step| step.name == "solver" && !step.ready) {
            instruction_lines.push(String::new());
            instruction_lines.push("Next, install the native solver from the redwax checkout:".to_owned());
            instruction_lines.push("  scripts/install_resolver.sh".to_owned());
            instruction_lines.push(format!("Then run `{} setup` again.", crate::cli::program_name()));
            return instruction_lines;
        }

        if self.steps.iter().any(|step| !step.ready) {
            instruction_lines.push(String::new());
            instruction_lines.push("Next, sign in:".to_string());
            instruction_lines.push(
                "Open https://music.youtube.com in your browser and sign in to your account.".to_string(),
            );
            instruction_lines.push("Then import that browser session:".to_string());
            instruction_lines.push(String::new());
            instruction_lines.push(format!("  {} login", crate::cli::program_name()));
            instruction_lines.push(format!(
                "Choose a browser with `{} login --browser NAME` if needed.",
                crate::cli::program_name()
            ));
        }
        instruction_lines
    }
}

fn plain_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The steps, in the order a person has to satisfy them. A program that is not
/// there is asked about before the session, because signing in is wasted on a
/// machine that cannot play the result.
pub async fn welcome(config: &Config) -> Welcome {
    let prerequisites = Prerequisites::inspect(config).await;
    Welcome::from_prerequisites(&prerequisites, PackageManager::detect())
}

impl Welcome {
    pub fn from_prerequisites(prerequisites: &Prerequisites, package_manager: PackageManager) -> Welcome {
        let steps = vec![
            program_step(&prerequisites.mpv, "mpv", "mpv"),
            program_step(&prerequisites.ytdlp, "yt-dlp", "yt-dlp"),
            program_step(
                &prerequisites.node,
                "node",
                if package_manager == PackageManager::Brew {
                    "node"
                } else {
                    "nodejs"
                },
            ),
            Step {
                name: "solver",
                ready: prerequisites.solver_ready,
                detail: if prerequisites.solver_ready {
                    "native signature solver installed"
                } else {
                    "native solver library or core is missing"
                }
                .to_owned(),
                package: "",
            },
            Step {
                name: "session",
                ready: prerequisites.session == SessionStatus::Ready,
                detail: match prerequisites.session {
                    SessionStatus::Ready => "saved login found",
                    SessionStatus::Missing => "not signed in",
                    SessionStatus::Empty => "saved login has no cookies",
                    SessionStatus::Invalid => "saved login cannot be read",
                }
                .to_owned(),
                package: "",
            },
        ];
        Welcome {
            steps,
            manager: package_manager,
            warnings: if prerequisites.proof_token_provider_ready {
                Vec::new()
            } else {
                vec!["Proof-token provider is optional and absent. Token-required streams use yt-dlp fallback. Install it with scripts/install_resolver.sh --with-provider.".to_owned()]
            },
        }
    }
}

fn program_step(status: &ProgramStatus, name: &'static str, package: &'static str) -> Step {
    let detail = match status {
        ProgramStatus::Ready { version, .. } => version.clone(),
        ProgramStatus::Missing => "not installed".to_owned(),
        ProgramStatus::Unusable { .. } => "does not answer --version".to_owned(),
        ProgramStatus::Unsupported {
            version, requirement, ..
        } => format!("{version}, {requirement}"),
    };
    Step {
        name,
        ready: status.ready(),
        detail,
        package,
    }
}

/// The package managers redwax knows how to advise on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Dnf,
    Pacman,
    Apt,
    Zypper,
    Apk,
    Brew,
    Unknown,
}

impl PackageManager {
    pub fn detect() -> PackageManager {
        PackageManager::from_os_release(&std::fs::read_to_string("/etc/os-release").unwrap_or_default())
    }

    /// The manager for an `os-release` file. `ID_LIKE` is read as well as `ID`,
    /// so a derivative nobody has heard of still gets the right command from
    /// the family it names.
    pub fn from_os_release(os_release_contents: &str) -> PackageManager {
        let distribution_ids: Vec<String> = os_release_contents
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| *key == "ID" || *key == "ID_LIKE")
            .flat_map(|(_, distribution_names)| {
                distribution_names
                    .trim_matches('"')
                    .split_whitespace()
                    .map(str::to_lowercase)
                    .collect::<Vec<_>>()
            })
            .collect();
        for distribution_id in &distribution_ids {
            match distribution_id.as_str() {
                "fedora" | "rhel" | "centos" => return PackageManager::Dnf,
                "arch" | "archlinux" | "omarchy" | "manjaro" | "endeavouros" => {
                    return PackageManager::Pacman;
                }
                "debian" | "ubuntu" | "pop" | "linuxmint" => return PackageManager::Apt,
                "opensuse" | "suse" | "opensuse-tumbleweed" => return PackageManager::Zypper,
                "alpine" => return PackageManager::Apk,
                _ => {}
            }
        }
        match cfg!(target_os = "macos") {
            true => PackageManager::Brew,
            false => PackageManager::Unknown,
        }
    }

    pub fn install(self, package: &str) -> String {
        match self {
            PackageManager::Dnf => format!("sudo dnf install {package}"),
            PackageManager::Pacman => format!("sudo pacman -S {package}"),
            PackageManager::Apt => format!("sudo apt install {package}"),
            PackageManager::Zypper => format!("sudo zypper install {package}"),
            PackageManager::Apk => format!("sudo apk add {package}"),
            PackageManager::Brew => format!("brew install {package}"),
            PackageManager::Unknown => String::new(),
        }
    }
}

/// Where redwax keeps what it is given, for the line that says so.
pub fn kept_under(config: &Config) -> &Path {
    config.auth_path.parent().unwrap_or(Path::new("~/.config/redwax"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &'static str, ready: bool, package: &'static str) -> Step {
        Step {
            name,
            ready,
            detail: match ready {
                true => "found".to_string(),
                false => "not installed".to_string(),
            },
            package,
        }
    }

    fn welcome(steps: Vec<Step>, manager: PackageManager) -> Welcome {
        Welcome {
            steps,
            manager,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_distribution_is_read_from_its_own_id() {
        for (release, wanted) in [
            ("ID=fedora\nVERSION_ID=44\n", PackageManager::Dnf),
            ("ID=arch\n", PackageManager::Pacman),
            ("ID=ubuntu\nID_LIKE=debian\n", PackageManager::Apt),
            ("ID=\"opensuse-tumbleweed\"\n", PackageManager::Zypper),
            ("ID=alpine\n", PackageManager::Apk),
        ] {
            assert_eq!(PackageManager::from_os_release(release), wanted, "{release:?}");
        }
    }

    /// A derivative nobody has heard of still gets the right command from the
    /// family it names, which is the whole reason `ID_LIKE` is read.
    #[test]
    fn a_derivative_falls_back_to_the_family_it_names() {
        assert_eq!(
            PackageManager::from_os_release("ID=omarchy\nID_LIKE=arch\n"),
            PackageManager::Pacman
        );
        assert_eq!(
            PackageManager::from_os_release("ID=nobody-has-heard-of-this\nID_LIKE=\"rhel fedora\"\n"),
            PackageManager::Dnf
        );
    }

    #[test]
    fn an_unknown_distribution_advises_nothing_rather_than_guessing() {
        let manager = PackageManager::from_os_release("ID=plan9\n");
        if !cfg!(target_os = "macos") {
            assert_eq!(manager, PackageManager::Unknown);
            assert_eq!(manager.install("mpv"), "");
        }
    }

    #[test]
    fn everything_missing_is_one_command_and_not_three() {
        let block = welcome(
            vec![
                step("mpv", false, "mpv"),
                step("yt-dlp", false, "yt-dlp"),
                step("node", false, "nodejs"),
                step("session", false, ""),
            ],
            PackageManager::Dnf,
        )
        .lines()
        .join("\n");
        assert!(
            block.contains("sudo dnf install mpv yt-dlp nodejs"),
            "not one command: {block}"
        );
        assert!(
            block.contains("mpv, yt-dlp and nodejs"),
            "the sentence does not read: {block}"
        );
        assert_eq!(
            block.matches("sudo dnf").count(),
            1,
            "more than one command offered"
        );
    }

    /// Signing in is asked for only once nothing is missing, because a session
    /// is wasted on a machine that cannot play the result.
    #[test]
    fn the_session_is_the_last_thing_asked_for() {
        let programs_missing = welcome(
            vec![step("mpv", false, "mpv"), step("session", false, "")],
            PackageManager::Pacman,
        )
        .lines()
        .join("\n");
        assert!(!programs_missing.contains("redwax login"), "{programs_missing}");

        let only_session = welcome(
            vec![step("mpv", true, "mpv"), step("session", false, "")],
            PackageManager::Pacman,
        )
        .lines()
        .join("\n");
        assert!(only_session.contains("Next, sign in:"), "{only_session}");
        assert!(only_session.contains("redwax login"), "{only_session}");
    }

    #[test]
    fn a_ready_install_is_asked_for_nothing() {
        let block = welcome(
            vec![step("mpv", true, "mpv"), step("session", true, "")],
            PackageManager::Dnf,
        );
        assert!(block.ready());
        let text = block.lines().join("\n");
        assert!(
            !text.contains("Next,"),
            "a ready install was still asked for something"
        );
    }

    /// A distribution with no command still names what is missing, because a
    /// list a reader can act on beats an empty screen.
    #[test]
    fn an_unknown_distribution_still_names_what_is_missing() {
        let block = welcome(
            vec![step("mpv", false, "mpv"), step("node", false, "nodejs")],
            PackageManager::Unknown,
        )
        .lines()
        .join("\n");
        assert!(block.contains("install mpv, nodejs"), "{block}");
    }

    #[test]
    fn program_evidence_is_used_without_parsing_a_doctor_message() {
        let status = ProgramStatus::Ready {
            path: "/synthetic/mpv".into(),
            version: "synthetic version with spaces".to_owned(),
        };
        let step = program_step(&status, "mpv", "mpv");
        assert!(step.ready);
        assert_eq!(step.detail, "synthetic version with spaces");
    }

    #[test]
    fn a_missing_solver_blocks_setup_without_asking_for_another_login() {
        let status = ProgramStatus::Ready {
            path: "/synthetic/program".into(),
            version: "supported".to_owned(),
        };
        let prerequisites = Prerequisites {
            mpv: status.clone(),
            ytdlp: status.clone(),
            node: status,
            solver_ready: false,
            proof_token_provider_ready: false,
            session: SessionStatus::Ready,
        };
        let missing = Welcome::from_prerequisites(&prerequisites, PackageManager::Unknown);
        assert!(!missing.ready());
        let lines = missing.lines().join("\n");
        assert!(lines.contains("scripts/install_resolver.sh"));
        assert!(!lines.contains("Next, sign in:"));

        let complete = Welcome::from_prerequisites(
            &Prerequisites {
                solver_ready: true,
                ..prerequisites
            },
            PackageManager::Unknown,
        );
        assert!(complete.ready());
        assert!(complete.lines().join("\n").contains("optional and absent"));
    }
}
