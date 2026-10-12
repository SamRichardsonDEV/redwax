//! Tests that talk to YouTube. Run with `cargo test -p redwax-resolve -- --ignored`.
//!
//! They need the session `redwax login` writes, so a failure here is as often a
//! rotated cookie as a broken step. Each one names the track it uses.

use std::path::PathBuf;

use redwax_auth::BrowserSession;

use crate::chain::solver::{NodeSolver, SolveRequest};
use crate::{Location, Quality, Resolver, ResolverOptions, Source, Tier};

const HIGH_TIER_TRACK: &str = "1-Fxb9jsOuI";
const NO_HIGH_TIER_TRACK: &str = "Bq56cLjoIgo";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("a live test runs as a user with a home"))
}

fn resolver() -> Resolver {
    let session = BrowserSession::read(&home().join(".config/redwax/browser.json"))
        .expect("the saved browser session is readable");
    Resolver::new(ResolverOptions {
        cookies_path: Some(home().join(".config/redwax/cookies.txt")),
        identity: session.as_ref().map(|session| session.identity().clone()),
        quality: Quality::High,
        library_dir: None,
        ytdlp_args: Vec::new(),
        parallel: 3,
        cache_dir: home().join(".cache/redwax"),
    })
}

/// One runtime keeps the resident solver and warm timing under the same owner.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("a test can start a runtime"))
}

fn tier_from(resolver: &Resolver, video_id: &str, step: Source) -> Tier {
    let (result, attempts) = futures_block_on(resolver, video_id, step);
    let stream = result.unwrap_or_else(|error| {
        panic!("{step:?} failed for {video_id}: {error}\nattempts: {attempts:?}");
    });
    assert!(
        matches!(stream.location, Location::Url(_)),
        "a live step returns a URL"
    );
    stream.tier
}

fn futures_block_on(
    resolver: &Resolver,
    video_id: &str,
    step: Source,
) -> (crate::Result<crate::Stream>, Vec<crate::Attempt>) {
    runtime().block_on(resolver.resolve_traced(video_id, Some(step)))
}

#[test]
#[ignore = "talks to YouTube"]
fn signed_in_resolve_gives_the_premium_tier() {
    let tier = tier_from(&resolver(), HIGH_TIER_TRACK, Source::NativeSignedIn);
    assert!(
        tier.is_high(),
        "{HIGH_TIER_TRACK} has a premium encode, got {}",
        tier.label()
    );
}

#[test]
#[ignore = "talks to YouTube"]
fn signed_in_resolve_falls_to_the_standard_tier_when_there_is_no_premium_encode() {
    let tier = tier_from(&resolver(), NO_HIGH_TIER_TRACK, Source::NativeSignedIn);
    assert!(
        !tier.is_high(),
        "{NO_HIGH_TIER_TRACK} has no premium encode, got {}",
        tier.label()
    );
}

#[test]
#[ignore = "talks to YouTube"]
fn anonymous_resolve_gives_a_standard_tier() {
    let tier = tier_from(&resolver(), HIGH_TIER_TRACK, Source::NativeAnonymous);
    assert!(
        !tier.is_high(),
        "the anonymous identity never sees a premium encode"
    );
}

#[test]
#[ignore = "talks to YouTube"]
fn the_yt_dlp_step_resolves() {
    let tier = tier_from(&resolver(), HIGH_TIER_TRACK, Source::YtDlp);
    assert!(Tier::PREFERRED.contains(&tier));
}

#[test]
#[ignore = "talks to YouTube"]
fn the_solver_returns_different_strings_than_it_was_given() {
    let solver = NodeSolver::new(home().join(".cache/redwax"));
    let challenges = SolveRequest {
        signatures: vec!["AOq0QJ8wRgIhAJ7lOaqk4Zq4d3kCnkPUEr9lS0i2H8xh6r7dLGYcVJnL".to_owned()],
        n_params: vec!["1lWJDNAaPKKKKKKK".to_owned()],
    };
    let solved = runtime().block_on(async {
        let script = solver
            .player_script("")
            .await
            .expect("the player script downloads");
        solver
            .solve(&script, challenges.clone())
            .await
            .expect("the solver answers")
    });
    assert_eq!(solved.signatures.len(), 1);
    assert_eq!(solved.n_params.len(), 1);
    assert_ne!(
        solved.signatures[0], challenges.signatures[0],
        "a signature is transformed"
    );
    assert_ne!(
        solved.n_params[0], challenges.n_params[0],
        "an n parameter is transformed"
    );
}

#[test]
#[ignore = "talks to YouTube"]
fn a_warm_solve_costs_a_handful_of_milliseconds() {
    let solver = NodeSolver::new(home().join(".cache/redwax"));
    let request = SolveRequest {
        signatures: Vec::new(),
        n_params: vec!["1lWJDNAaPKKKKKKK".to_owned()],
    };
    let script = runtime()
        .block_on(solver.player_script(""))
        .expect("the player script downloads");
    // The first solve starts the resident process and loads the player, so only
    // the second measures what a queued track pays.
    runtime()
        .block_on(solver.solve(&script, request.clone()))
        .expect("the solver answers");
    let started = std::time::Instant::now();
    runtime()
        .block_on(solver.solve(&script, request))
        .expect("the solver answers");
    let warm = started.elapsed();
    println!("warm solve {} ms", warm.as_millis());
    assert!(
        warm < std::time::Duration::from_millis(25),
        "warm solve took {} ms",
        warm.as_millis()
    );
}
