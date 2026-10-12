//! `redwax-resolve <video_id> [--quality high|fast] [--only signed-in|anonymous|yt-dlp] [--cookies PATH]`
//!
//! `--trace` adds a `trace <phase> <ms>` line for every phase that can cost a
//! round trip or a subprocess, in the order they ran.
//!
//! Prints one line per attempt: `<source> <tier|none> <elapsed_ms> <evidence>`,
//! then the winner. Evidence contains fixed status and policy facts, never a
//! stream URL. Exit 0 when a stream was found. `scripts/measure.sh` reads the
//! elapsed column, so the format is part of the contract.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use redwax_auth::{BrowserSession, SelectedIdentity};
use redwax_resolve::{Attempt, AttemptOutcome, Quality, Resolver, ResolverOptions, Source, Stream};
use sha1::{Digest, Sha1};

const USAGE: &str = "usage: redwax-resolve <video_id> [--quality high|fast] \
[--only signed-in|anonymous|yt-dlp] [--cookies PATH] [--authuser N] [--page-id ID] \
[--cache-dir DIR] [--dump-json FILE] [--trace] [--show-target]\n       redwax-resolve --health [<video_id>]\n       \
redwax-resolve --native-canary [--cold-budget-ms N] [--warm-budget-ms N] [--deadline-ms N]";

struct ParsedArguments {
    video_id: Option<String>,
    quality: Quality,
    selected_source: Option<Source>,
    cookies_path: Option<PathBuf>,
    authuser: Option<u32>,
    page_id: Option<String>,
    cache_dir: Option<PathBuf>,
    dump_json: Option<PathBuf>,
    health: bool,
    trace: bool,
    show_target: bool,
    native_canary: bool,
    cold_budget_ms: u64,
    warm_budget_ms: u64,
    deadline_ms: u64,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_owned()))
}

fn parse_args() -> std::result::Result<ParsedArguments, String> {
    parse_argument_values(std::env::args().skip(1))
}

fn parse_argument_values(
    raw_arguments: impl Iterator<Item = String>,
) -> std::result::Result<ParsedArguments, String> {
    let mut parsed_arguments = ParsedArguments {
        video_id: None,
        quality: Quality::High,
        selected_source: None,
        cookies_path: None,
        authuser: None,
        page_id: None,
        cache_dir: None,
        dump_json: None,
        health: false,
        trace: false,
        show_target: false,
        native_canary: false,
        cold_budget_ms: 3000,
        warm_budget_ms: 1000,
        deadline_ms: 6000,
    };
    let mut raw_arguments = raw_arguments;
    let mut canary_budget_set = false;
    while let Some(flag) = raw_arguments.next() {
        let mut next_value = || {
            raw_arguments
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--health" => parsed_arguments.health = true,
            "--trace" => parsed_arguments.trace = true,
            "--show-target" => parsed_arguments.show_target = true,
            "--native-canary" => parsed_arguments.native_canary = true,
            "--cold-budget-ms" => {
                parsed_arguments.cold_budget_ms = parse_budget(&next_value()?)?;
                canary_budget_set = true;
            }
            "--warm-budget-ms" => {
                parsed_arguments.warm_budget_ms = parse_budget(&next_value()?)?;
                canary_budget_set = true;
            }
            "--deadline-ms" => {
                parsed_arguments.deadline_ms = parse_budget(&next_value()?)?;
                canary_budget_set = true;
            }
            "--quality" => {
                parsed_arguments.quality = match next_value()?.as_str() {
                    "high" => Quality::High,
                    "fast" => Quality::Fast,
                    other => return Err(format!("unknown quality {other}")),
                }
            }
            "--only" => {
                let source_label = next_value()?;
                parsed_arguments.selected_source = Some(
                    Source::from_label(&source_label)
                        .ok_or_else(|| format!("unknown step {source_label}"))?,
                );
            }
            "--cookies" => parsed_arguments.cookies_path = Some(PathBuf::from(next_value()?)),
            "--authuser" => {
                parsed_arguments.authuser = Some(
                    next_value()?
                        .parse()
                        .map_err(|_| "authuser is a number".to_owned())?,
                )
            }
            "--page-id" => parsed_arguments.page_id = Some(next_value()?),
            "--cache-dir" => parsed_arguments.cache_dir = Some(PathBuf::from(next_value()?)),
            "--dump-json" => parsed_arguments.dump_json = Some(PathBuf::from(next_value()?)),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other => parsed_arguments.video_id = Some(other.to_owned()),
        }
    }
    if canary_budget_set && !parsed_arguments.native_canary {
        return Err("canary budgets require --native-canary".to_owned());
    }
    if parsed_arguments.native_canary
        && (parsed_arguments.health
            || parsed_arguments.trace
            || parsed_arguments.show_target
            || parsed_arguments.dump_json.is_some()
            || parsed_arguments.selected_source.is_some()
            || parsed_arguments.video_id.is_some()
            || parsed_arguments.quality != Quality::High)
    {
        return Err("native canary cannot be combined with other probe modes".to_owned());
    }
    Ok(parsed_arguments)
}

fn parse_budget(budget_text: &str) -> std::result::Result<u64, String> {
    budget_text
        .parse::<u64>()
        .ok()
        .filter(|budget_ms| (1..=60_000).contains(budget_ms))
        .ok_or_else(|| "canary budgets must be between 1 and 60000 milliseconds".to_owned())
}

fn executable_fingerprint() -> Option<String> {
    let executable_path = std::env::current_exe().ok()?;
    let executable_contents = std::fs::read(executable_path).ok()?;
    Some(hex::encode(Sha1::digest(executable_contents)))
}

fn native_verdict(
    resolution_result: &redwax_resolve::Result<Stream>,
    attempt_reports: &[Attempt],
    expect_high_quality: bool,
) -> &'static str {
    let Ok(stream) = resolution_result else {
        return "native-failed";
    };
    if stream.source != Source::NativeSignedIn
        || attempt_reports.len() != 1
        || attempt_reports[0].source != Source::NativeSignedIn
        || attempt_reports[0].tier != Some(stream.tier)
    {
        return "unexpected-source";
    }
    if !matches!(
        attempt_reports[0].outcome,
        AttemptOutcome::Success {
            status: Some(200 | 206)
        }
    ) {
        return "cdn-unverified";
    }
    if stream.tier.is_high() != expect_high_quality {
        return "tier-mismatch";
    }
    "ok"
}

async fn native_canary(resolver: &Resolver, parsed_arguments: &ParsedArguments) -> bool {
    // Fixed samples make a quality downgrade fail rather than hide an identity failure.
    let probe_definitions = [
        ("cold", "high", "1-Fxb9jsOuI", parsed_arguments.cold_budget_ms),
        ("warm", "standard", "Bq56cLjoIgo", parsed_arguments.warm_budget_ms),
        ("warm", "high", "1-Fxb9jsOuI", parsed_arguments.warm_budget_ms),
    ];
    let mut passed = true;
    for (probe_index, (cache_phase, expected_tier, video_id, latency_budget_ms)) in
        probe_definitions.into_iter().enumerate()
    {
        let probe_started_at = Instant::now();
        let timed_outcome = tokio::time::timeout(
            Duration::from_millis(latency_budget_ms),
            resolver.resolve_traced(video_id, Some(Source::NativeSignedIn)),
        )
        .await;
        let elapsed_time = probe_started_at.elapsed();
        let verdict_status = match timed_outcome {
            Err(_) => "latency-budget",
            Ok(_) if elapsed_time > Duration::from_millis(latency_budget_ms) => "latency-budget",
            Ok((resolution_result, attempt_reports)) => {
                native_verdict(&resolution_result, &attempt_reports, expected_tier == "high")
            }
        };
        passed &= verdict_status == "ok";
        // The canary emits only fixed policy labels so account data cannot escape.
        println!(
            "{}",
            serde_json::json!({
                "probe": probe_index + 1, "phase": cache_phase, "expected_tier": expected_tier,
                "status": verdict_status, "elapsed_ms": elapsed_time.as_millis(),
                "budget_ms": latency_budget_ms,
            })
        );
    }
    passed
}

fn options(parsed_arguments: &ParsedArguments) -> std::result::Result<ResolverOptions, String> {
    let saved_session = BrowserSession::read(&home().join(".config/redwax/browser.json"))
        .map_err(|error| format!("saved browser session: {error}"))?;
    let authuser = parsed_arguments.authuser.or_else(|| {
        saved_session
            .as_ref()
            .map(|session| session.identity().authuser())
    });
    let page_id = parsed_arguments.page_id.clone().or_else(|| {
        saved_session
            .as_ref()
            .and_then(|session| session.identity().page_id().map(str::to_owned))
    });
    let cookies_path = parsed_arguments
        .cookies_path
        .clone()
        .unwrap_or_else(|| home().join(".config/redwax/cookies.txt"));
    Ok(ResolverOptions {
        cookies_path: cookies_path.exists().then_some(cookies_path),
        identity: Some(SelectedIdentity::new(authuser.unwrap_or(0), page_id)),
        quality: parsed_arguments.quality,
        library_dir: None,
        ytdlp_args: Vec::new(),
        parallel: 3,
        cache_dir: parsed_arguments
            .cache_dir
            .clone()
            .unwrap_or_else(|| home().join(".cache/redwax")),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let parsed_arguments = match parse_args() {
        Ok(parsed_arguments) => parsed_arguments,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let resolver = match options(&parsed_arguments) {
        Ok(resolver_options) => Resolver::new(resolver_options),
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    if parsed_arguments.trace {
        resolver.enable_trace();
    }

    if parsed_arguments.native_canary {
        let canary_started_at = Instant::now();
        let timed_outcome = tokio::time::timeout(
            Duration::from_millis(parsed_arguments.deadline_ms),
            native_canary(&resolver, &parsed_arguments),
        )
        .await;
        let verdict_status = match timed_outcome {
            Err(_) => "deadline",
            Ok(true)
                if canary_started_at.elapsed() <= Duration::from_millis(parsed_arguments.deadline_ms) =>
            {
                "ok"
            }
            Ok(_) => "failed",
        };
        drop(resolver);
        println!(
            "{}",
            serde_json::json!({
                "canary": "native", "status": verdict_status,
                "elapsed_ms": canary_started_at.elapsed().as_millis(),
                "deadline_ms": parsed_arguments.deadline_ms,
                "executable_sha1": executable_fingerprint(), "version": env!("CARGO_PKG_VERSION"),
            })
        );
        return if verdict_status == "ok" {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    if parsed_arguments.health {
        let video_id = parsed_arguments.video_id.as_deref().unwrap_or("1-Fxb9jsOuI");
        for step in resolver.health_check(video_id).await {
            println!(
                "{} {} {} {}",
                step.step.label(),
                if step.ok { "ok" } else { "fail" },
                step.elapsed.as_millis(),
                step.detail
            );
        }
        return ExitCode::SUCCESS;
    }

    let Some(video_id) = parsed_arguments.video_id.clone() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    if let Some(dump_path) = &parsed_arguments.dump_json {
        return match resolver.dump_player_response(&video_id, dump_path).await {
            Ok(()) => {
                println!("dump {}", dump_path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("dump failed: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let resolution_started_at = Instant::now();
    let (resolution_result, attempt_reports) = resolver
        .resolve_traced(&video_id, parsed_arguments.selected_source)
        .await;
    let total_elapsed = resolution_started_at.elapsed();
    for attempt_report in &attempt_reports {
        println!(
            "{} {} {} {}",
            attempt_report.source.label(),
            attempt_report
                .tier
                .map(|tier| tier.itag().to_string())
                .unwrap_or_else(|| "none".to_owned()),
            attempt_report.elapsed.as_millis(),
            attempt_report.evidence
        );
    }
    for (phase, elapsed) in resolver.take_trace() {
        println!("trace {phase} {}", elapsed.as_millis());
    }
    match resolution_result {
        Ok(stream) => {
            if parsed_arguments.show_target {
                eprintln!("Warning: target output contains private stream credentials. Do not share it.");
                println!("TARGET {}", stream.location.as_mpv_target());
            }
            println!(
                "RESULT {} {} {}",
                stream.tier.itag(),
                stream.source.label(),
                total_elapsed.as_millis()
            );
            let _ = resolver.flush();
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("RESULT none none {} {error}", total_elapsed.as_millis());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redwax_resolve::{Error, Failure, FailureKind, Location, Tier};
    use std::time::SystemTime;

    #[test]
    fn stream_target_requires_explicit_opt_in() {
        let arguments = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert!(
            !parse_argument_values(arguments(&["fixture"]))
                .unwrap()
                .show_target
        );
        assert!(
            parse_argument_values(arguments(&["fixture", "--show-target"]))
                .unwrap()
                .show_target
        );
        assert!(parse_argument_values(arguments(&["--native-canary", "--show-target"])).is_err());
    }

    fn native_sample(tier: Tier) -> (redwax_resolve::Result<Stream>, Vec<Attempt>) {
        (
            Ok(Stream {
                video_id: "sample".to_owned(),
                location: Location::Url("https://example.invalid/stream".to_owned()),
                tier,
                source: Source::NativeSignedIn,
                expires_at: SystemTime::now(),
            }),
            vec![Attempt {
                source: Source::NativeSignedIn,
                tier: Some(tier),
                elapsed: Duration::ZERO,
                outcome: AttemptOutcome::Success { status: Some(206) },
                evidence: format!("status=206 itag={}", tier.itag()),
            }],
        )
    }

    #[test]
    fn canary_requires_the_expected_quality_family() {
        for tier in Tier::PREFERRED {
            let (result, attempts) = native_sample(tier);
            assert_eq!(native_verdict(&result, &attempts, tier.is_high()), "ok");
            assert_eq!(
                native_verdict(&result, &attempts, !tier.is_high()),
                "tier-mismatch"
            );
        }
    }

    #[test]
    fn fallback_and_cache_cannot_pass_a_native_canary() {
        for source in [
            Source::Cache,
            Source::LocalFile,
            Source::NativeAnonymous,
            Source::YtDlp,
        ] {
            let (mut result, mut attempts) = native_sample(Tier::Opus283);
            result.as_mut().unwrap().source = source;
            assert_eq!(native_verdict(&result, &attempts, true), "unexpected-source");
            result.as_mut().unwrap().source = Source::NativeSignedIn;
            attempts[0].source = source;
            assert_eq!(native_verdict(&result, &attempts, true), "unexpected-source");
        }
    }

    #[test]
    fn canary_requires_one_matching_cdn_verified_attempt() {
        let (result, mut attempts) = native_sample(Tier::Opus283);
        assert_eq!(native_verdict(&result, &[], true), "unexpected-source");
        attempts.push(attempts[0].clone());
        assert_eq!(native_verdict(&result, &attempts, true), "unexpected-source");
        attempts.pop();
        attempts[0].tier = Some(Tier::Opus160);
        assert_eq!(native_verdict(&result, &attempts, true), "unexpected-source");
        attempts[0].tier = Some(Tier::Opus283);
        for outcome in [
            AttemptOutcome::Failed(FailureKind::CdnRefused),
            AttemptOutcome::Missing,
            AttemptOutcome::Success { status: Some(2060) },
            AttemptOutcome::Success { status: None },
        ] {
            attempts[0].outcome = outcome;
            assert_eq!(native_verdict(&result, &attempts, true), "cdn-unverified");
        }
        attempts[0].outcome = AttemptOutcome::Success { status: Some(200) };
        assert_eq!(native_verdict(&result, &attempts, true), "ok");
    }

    #[test]
    fn resolver_errors_are_reduced_to_a_fixed_status() {
        let result = Err(Error::Failed(Failure {
            kind: FailureKind::UnknownResponse,
            evidence: "fixture: failed".to_owned(),
        }));
        assert_eq!(native_verdict(&result, &[], true), "native-failed");
    }

    #[test]
    fn canary_budgets_reject_zero_extreme_and_invalid_limits() {
        assert_eq!(parse_budget("1").unwrap(), 1);
        assert_eq!(parse_budget("60000").unwrap(), 60_000);
        for invalid in ["0", "60001", "-1", "not-a-number", "18446744073709551616"] {
            assert!(parse_budget(invalid).is_err());
        }
    }
}
