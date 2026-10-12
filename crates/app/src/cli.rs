use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Run {
        config: Option<PathBuf>,
    },
    Setup {
        config: Option<PathBuf>,
    },
    Doctor {
        config: Option<PathBuf>,
        live: bool,
    },
    Login {
        config: Option<PathBuf>,
        browser: Option<String>,
        profile: Option<String>,
        account: Option<String>,
        list_accounts: bool,
    },
    Logout {
        config: Option<PathBuf>,
    },
    Sync {
        config: Option<PathBuf>,
        dry_run: bool,
    },
}

/// Explicit config flags override the caller's environment default in every command.
pub fn parse(
    arguments: impl IntoIterator<Item = String>,
    mut config_path: Option<PathBuf>,
) -> Result<Command, String> {
    let mut arguments = arguments.into_iter().peekable();
    let mut subcommand = None;
    let mut run_live_checks = false;
    let mut is_dry_run = false;
    let mut profile_name = None;
    let mut browser_name = None;
    let mut account_query = None;
    let mut list_accounts = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" if subcommand.is_none() => return Ok(Command::Version),
            "--config" => config_path = Some(argument_value("--config", &mut arguments)?.into()),
            "setup" | "doctor" | "login" | "sync" | "logout" if subcommand.is_none() => {
                subcommand = Some(argument)
            }
            "--live" if subcommand.as_deref() == Some("doctor") => run_live_checks = true,
            "--dry-run" if subcommand.as_deref() == Some("sync") => is_dry_run = true,
            "--profile" if subcommand.as_deref() == Some("login") => {
                profile_name = Some(argument_value("--profile", &mut arguments)?);
            }
            "--browser" if subcommand.as_deref() == Some("login") => {
                browser_name = Some(argument_value("--browser", &mut arguments)?);
            }
            "--account" if subcommand.as_deref() == Some("login") => {
                account_query = Some(argument_value("--account", &mut arguments)?);
            }
            "--list-accounts" if subcommand.as_deref() == Some("login") => list_accounts = true,
            _ => {
                if let Some(path) = argument.strip_prefix("--config=").filter(|path| !path.is_empty()) {
                    config_path = Some(path.into());
                } else {
                    return Err(format!("unknown argument {argument}"));
                }
            }
        }
    }
    Ok(match subcommand.as_deref() {
        Some("setup") => Command::Setup { config: config_path },
        Some("doctor") => Command::Doctor {
            config: config_path,
            live: run_live_checks,
        },
        Some("login") => Command::Login {
            config: config_path,
            browser: browser_name,
            profile: profile_name,
            account: account_query,
            list_accounts,
        },
        Some("logout") => Command::Logout { config: config_path },
        Some("sync") => Command::Sync {
            config: config_path,
            dry_run: is_dry_run,
        },
        _ => Command::Run { config: config_path },
    })
}

pub fn program_name() -> &'static str {
    "redwax"
}

fn argument_value(flag: &str, arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    arguments
        .next()
        .filter(|candidate_value| !candidate_value.is_empty() && !candidate_value.starts_with("--"))
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_accepts_browser_profile_and_account_discovery_together() {
        assert_eq!(
            parse(
                [
                    "login",
                    "--browser",
                    "chrome",
                    "--profile",
                    "Profile 1",
                    "--list-accounts",
                    "--account",
                    "2"
                ]
                .map(str::to_owned),
                None
            ),
            Ok(Command::Login {
                config: None,
                browser: Some("chrome".into()),
                profile: Some("Profile 1".into()),
                account: Some("2".into()),
                list_accounts: true
            })
        );
        assert!(parse(["setup", "--browser", "chrome"].map(str::to_owned), None).is_err());
        assert!(parse(["login", "--browser"].map(str::to_owned), None).is_err());
    }

    #[test]
    fn every_command_uses_the_same_explicit_config_precedence() {
        for command in [
            None,
            Some("setup"),
            Some("doctor"),
            Some("login"),
            Some("sync"),
            Some("logout"),
        ] {
            let args = command
                .into_iter()
                .chain(["--config", "chosen path.toml"])
                .map(str::to_owned);
            let parsed = parse(args, Some("environment.toml".into())).expect("the explicit path is valid");
            let config = match parsed {
                Command::Run { config }
                | Command::Setup { config }
                | Command::Doctor { config, .. }
                | Command::Login { config, .. }
                | Command::Sync { config, .. }
                | Command::Logout { config } => config,
                _ => panic!("a real command was supplied"),
            };
            assert_eq!(config, Some("chosen path.toml".into()));
        }
    }

    #[test]
    fn missing_values_and_flags_for_the_wrong_command_are_errors() {
        for args in [
            vec!["--config"],
            vec!["doctor", "--dry-run"],
            vec!["sync", "--live"],
            vec!["login", "--profile", "--config", "x"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned), None).is_err());
        }
        assert_eq!(
            parse(["sync".to_owned(), "--help".to_owned()], None),
            Ok(Command::Help)
        );
    }
}
