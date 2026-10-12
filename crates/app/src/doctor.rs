//! Offline installation diagnostics and opt-in live compatibility checks.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use redwax_resolve::{Source, Tier};
use redwax_ytm::{Client, ClientOptions, Item, MusicSource, Page, Session};

use crate::config::{self, Config, Quality};
use crate::terminal::{self, Renderer};

/// Said wherever a signed-in call comes back refused. Firefox rotates the
/// session cookies about once an hour, so this is the common cause by far.
const ROTATED: &str = "the session has probably rotated, run `redwax login`";

/// The counts each live page must reach. Set below what YouTube serves today,
/// so a change in the shelf mix stays quiet and a parser that has stopped
/// reading is loud.
const MIN_HOME_SHELVES: usize = 3;
const MIN_SEARCH_ITEMS: usize = 5;
const MIN_ALBUM_TRACKS: usize = 3;
const MIN_PLAYLIST_TRACKS: usize = 5;

/// These public tracks exercise Premium availability and lower-tier fallback.
const HIGH_TIER_TRACK: &str = "1-Fxb9jsOuI";
const NO_HIGH_TIER_TRACK: &str = "Bq56cLjoIgo";
const SEARCH_QUERY: &str = "oasis wonderwall";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Degraded, named and not counted against the exit code.
    Warn,
    Fail,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| check.status == Status::Fail)
    }

    pub fn get(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name == name)
    }

    pub fn lines(&self) -> Vec<String> {
        self.checks
            .iter()
            .map(|check| format!("{:<4} {:<18} {}", check.status.label(), check.name, check.detail))
            .collect()
    }

    /// The one place a detail reaches the report, so the address scrub is not a
    /// rule any caller can forget.
    fn push(&mut self, status: Status, name: impl Into<String>, detail: impl AsRef<str>) {
        self.checks.push(Check {
            status,
            name: name.into(),
            detail: scrub(detail.as_ref()),
        });
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub config_path: Option<PathBuf>,
    pub live: bool,
}

/// Every check in printing order. Offline first, so a broken install is named
/// before a live call waits on a network that will never answer.
pub async fn run(doctor_options: &Options) -> Report {
    let mut report = Report::default();
    let config = check_config(&mut report, doctor_options.config_path.as_deref());
    let cache_directory = config::cache_dir();
    let prerequisites = crate::prerequisites::Prerequisites::inspect(&config).await;
    check_session(&mut report, &config, &prerequisites.session);
    check_cookies(&mut report, &config);
    check_program(&mut report, "mpv", &prerequisites.mpv);
    check_program(&mut report, "yt-dlp", &prerequisites.ytdlp);
    check_program(&mut report, "node", &prerequisites.node);
    check_solver(&mut report, &cache_directory);
    check_proof_token_provider(&mut report, &cache_directory);
    check_innertube(&mut report, &cache_directory);
    check_player_scripts(&mut report, &cache_directory);
    check_pages(&mut report, &config);
    check_state(&mut report, &config);
    check_crashes(&mut report);
    let library_marks = check_marks(&mut report, &config);
    check_library(&mut report, &config, library_marks.as_ref());
    check_resolver_cookies(&mut report, &config, &prerequisites.session);
    check_terminal(&mut report, &config);
    check_bus(&mut report).await;
    if doctor_options.live {
        // The resolver future is too large to keep inline on small worker stacks.
        Box::pin(live_checks(&mut report, &config)).await;
    }
    report
}

// ------------------------------------------------------------------ offline

fn check_config(report: &mut Report, path: Option<&Path>) -> Config {
    let config_path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config::config_dir().join("config.toml"));
    match Config::load(path) {
        Ok(config) => {
            let config_summary = format!(
                "{} theme={} icons={} art={} quality={} transparent={} library_dir={}",
                config_path.display(),
                config.theme,
                config.icons,
                config.art,
                quality_name(config.quality),
                config.transparent,
                config.library_dir.display(),
            );
            match config_path.exists() {
                true => report.push(Status::Ok, "config", config_summary),
                false => report.push(
                    Status::Warn,
                    "config",
                    format!("{config_summary} (no file, defaults in use)"),
                ),
            }
            config
        }
        Err(error) => {
            report.push(Status::Fail, "config", error);
            Config::default()
        }
    }
}

fn check_session(report: &mut Report, config: &Config, session_status: &crate::prerequisites::SessionStatus) {
    use crate::prerequisites::SessionStatus;
    let (status, detail) = match session_status {
        SessionStatus::Ready => {
            let identity = Session::read(&config.auth_path)
                .ok()
                .flatten()
                .map(|session| {
                    format!(
                        "authuser {}, page id {}",
                        session.identity().authuser(),
                        if session.identity().page_id().is_some() {
                            "set"
                        } else {
                            "unset"
                        }
                    )
                })
                .unwrap_or_else(|| "identity unavailable".to_owned());
            (
                Status::Ok,
                format!(
                    "{} signed in, {identity}, written {}",
                    config.auth_path.display(),
                    age_text(&config.auth_path)
                ),
            )
        }
        SessionStatus::Missing => (Status::Fail, "not there, run `redwax login`".to_owned()),
        SessionStatus::Empty => (Status::Fail, "holds no cookie, run `redwax login`".to_owned()),
        SessionStatus::Invalid => (Status::Fail, "not a valid session, run `redwax login`".to_owned()),
    };
    report.push(status, "session", detail);
}

fn check_cookies(report: &mut Report, config: &Config) {
    let cookie_export_path = &config.cookies_path;
    match redwax_auth::inspect_netscape(cookie_export_path) {
        Ok(cookie_capabilities) => report.push(
            if cookie_capabilities.authenticated() {
                Status::Ok
            } else {
                Status::Fail
            },
            "cookies",
            format!(
                "{}, {} cookies, login={} signing={}, written {}",
                cookie_export_path.display(),
                cookie_capabilities.count,
                cookie_capabilities.has_login,
                cookie_capabilities.has_signing_cookie,
                age_text(cookie_export_path),
            ),
        ),
        Err(_) => report.push(
            Status::Fail,
            "cookies",
            format!(
                "{} is not readable, run `redwax login`",
                cookie_export_path.display()
            ),
        ),
    }
}

fn check_program(
    report: &mut Report,
    name: &'static str,
    program_status: &crate::prerequisites::ProgramStatus,
) {
    use crate::prerequisites::ProgramStatus;
    match program_status {
        ProgramStatus::Ready { path, version } => {
            report.push(Status::Ok, name, format!("{version} at {}", path.display()))
        }
        ProgramStatus::Missing => report.push(Status::Fail, name, "not on PATH"),
        ProgramStatus::Unusable { path } => report.push(
            Status::Fail,
            name,
            format!("{} does not answer --version", path.display()),
        ),
        ProgramStatus::Unsupported {
            version, requirement, ..
        } => report.push(Status::Fail, name, format!("{version}, {requirement}")),
    }
}

fn check_solver(report: &mut Report, cache: &Path) {
    let resolver_diagnostics = redwax_resolve::ResolverDiagnostics::inspect(cache);
    let solver_library_path = resolver_diagnostics.solver_library;
    let solver_core_path = resolver_diagnostics.solver_core;
    match (solver_library_path, solver_core_path) {
        (Some(solver_library), Some(solver_core)) => report.push(
            Status::Ok,
            "solver",
            format!(
                "library {}, core {}",
                solver_library.display(),
                solver_core.display()
            ),
        ),
        (solver_library, solver_core) => report.push(
            Status::Fail,
            "solver",
            format!(
                "step 3 cannot solve the cipher: library {}, core {}",
                missing_or(solver_library.as_deref()),
                missing_or(solver_core.as_deref()),
            ),
        ),
    }
}

fn check_proof_token_provider(report: &mut Report, cache: &Path) {
    match redwax_resolve::ResolverDiagnostics::inspect(cache).proof_token_provider {
        Some(provider) => report.push(Status::Ok, "proof tokens", provider.display().to_string()),
        None => report.push(
            Status::Warn,
            "proof tokens",
            "optional provider absent. Token-required streams use yt-dlp fallback. Install with scripts/install_resolver.sh --with-provider",
        ),
    }
}

fn check_innertube(report: &mut Report, cache: &Path) {
    let resolver_diagnostics = redwax_resolve::ResolverDiagnostics::inspect(cache);
    let site_cache_path = &resolver_diagnostics.site_cache;
    match site_cache_path.is_file() {
        // The file holds the client version, the player id and the visitor id
        // for twelve hours, so an old one only costs one extra request.
        true => report.push(
            Status::Ok,
            "innertube cache",
            format!(
                "{}, written {}",
                site_cache_path.display(),
                age_text(site_cache_path)
            ),
        ),
        false => report.push(
            Status::Warn,
            "innertube cache",
            format!(
                "{} not written yet, the first resolve makes it",
                site_cache_path.display()
            ),
        ),
    }
}

fn check_player_scripts(report: &mut Report, cache: &Path) {
    let resolver_diagnostics = redwax_resolve::ResolverDiagnostics::inspect(cache);
    let player_cache_directory = &resolver_diagnostics.player_cache;
    let script_count = count_files(player_cache_directory);
    match script_count {
        0 => report.push(
            Status::Warn,
            "player cache",
            format!("{} holds no player script yet", player_cache_directory.display()),
        ),
        script_count => report.push(
            Status::Ok,
            "player cache",
            format!(
                "{}, {script_count} files, newest {}",
                player_cache_directory.display(),
                newest_age_text(player_cache_directory)
            ),
        ),
    }
}

fn check_pages(report: &mut Report, config: &Config) {
    let page_cache_directory = &config.page_cache_dir;
    let page_count = count_files(page_cache_directory);
    match page_count {
        0 => report.push(
            Status::Warn,
            "page cache",
            format!(
                "{} holds no page, the first frame will be empty",
                page_cache_directory.display()
            ),
        ),
        page_count => report.push(
            Status::Ok,
            "page cache",
            format!(
                "{}, {page_count} pages, newest {}",
                page_cache_directory.display(),
                newest_age_text(page_cache_directory)
            ),
        ),
    }
}

/// The endings redwax recorded. A run launched from the bar icon closes its
/// window with it, so the log is the only place those endings survive.
fn check_crashes(report: &mut Report) {
    let crash_log_path = crate::config::cache_dir().join("crash.log");
    let Ok(crash_log_contents) = std::fs::read_to_string(&crash_log_path) else {
        report.push(Status::Ok, "endings", "none recorded");
        return;
    };
    let crash_log_lines: Vec<&str> = crash_log_contents
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    let Some(last_crash_line) = crash_log_lines.last() else {
        report.push(Status::Ok, "endings", "none recorded");
        return;
    };
    let last_crash_message = last_crash_line
        .split_once(' ')
        .map(|(_, rest)| rest)
        .unwrap_or(last_crash_line);
    report.push(
        Status::Warn,
        "endings",
        format!(
            "{} recorded in {}, last: {last_crash_message}",
            crash_log_lines.len(),
            crash_log_path.display()
        ),
    );
}

fn check_state(report: &mut Report, config: &Config) {
    let state_path = &config.state_path;
    if !state_path.is_file() {
        report.push(
            Status::Warn,
            "state",
            format!("{} not written yet, nothing to restore", state_path.display()),
        );
        return;
    }
    let loaded_state = crate::state::SavedState::load(state_path);
    if let Some(warning) = loaded_state.warning {
        report.push(
            Status::Warn,
            "state",
            format!("{}: {warning}", state_path.display()),
        );
        return;
    }
    let saved_state = loaded_state.state;
    report.push(
        Status::Ok,
        "state",
        format!(
            "{}, {} queued, index {}, volume {}, repeat {}, shuffle {}, written {}",
            state_path.display(),
            saved_state.queue.len(),
            saved_state
                .index
                .map(|index| index.to_string())
                .unwrap_or_else(|| "none".to_owned()),
            saved_state
                .volume
                .map(|volume| volume.to_string())
                .unwrap_or_else(|| "default".to_owned()),
            saved_state.repeat.as_str(),
            saved_state.shuffle,
            age_text(state_path)
        ),
    );
}

/// Download marks require Firefox even when playback uses another browser.
fn check_marks(report: &mut Report, config: &Config) -> Option<redwax_marks::Marks> {
    let remembered = crate::session::RememberedSession::read(&config.session_path);
    if remembered
        .as_ref()
        .is_some_and(|saved| saved.source.browser != crate::browser_login::Browser::Firefox)
    {
        report.push(
            Status::Ok,
            "marks",
            "download-mark sync is unavailable for this browser",
        );
        return None;
    }
    let requested_profile_name = remembered
        .as_ref()
        .and_then(|saved| saved.source.profile.as_deref());
    let firefox_profile = match redwax_marks::FirefoxProfile::find_for(
        requested_profile_name,
        redwax_marks::ProfileCapability::Marks,
    ) {
        Ok(firefox_profile) => firefox_profile,
        Err(error) => {
            report.push(
                Status::Warn,
                "marks",
                format!("{error}, Firefox download-mark sync is unavailable"),
            );
            return None;
        }
    };
    match redwax_marks::read_marks(&firefox_profile) {
        Ok(marks) => {
            report.push(
                Status::Ok,
                "marks",
                format!(
                    "{}, {} tracks, {} playlists, {} albums, {} stored in the browser",
                    firefox_profile.dir.display(),
                    marks.tracks.len(),
                    marks.playlists.len(),
                    marks.albums.len(),
                    marks.stored_locally.len()
                ),
            );
            Some(marks)
        }
        Err(error) => {
            report.push(
                Status::Warn,
                "marks",
                format!("{}: {error}", firefox_profile.dir.display()),
            );
            None
        }
    }
}

/// The download library, and how far behind the marks it is. Playlists and
/// albums are not expanded here, because that costs a request each.
fn check_library(report: &mut Report, config: &Config, marks: Option<&redwax_marks::Marks>) {
    let library_directory = &config.library_dir;
    let library_files = crate::sync::library_index(library_directory);
    let missing_download_count = marks.map(|marks| {
        marks
            .tracks
            .iter()
            .filter(|video_id| !library_files.contains_key(*video_id))
            .count()
    });
    let sync_status = match missing_download_count {
        Some(0) => ", every marked track is here".to_owned(),
        Some(missing_track_count) => format!(", {missing_track_count} marked tracks not downloaded yet"),
        None => String::new(),
    };
    let download_policy = match config.downloads_enabled {
        true => format!("sync on, grace {} days", config.downloads_grace_days),
        false => "sync off".to_owned(),
    };
    report.push(
        Status::Ok,
        "library",
        format!(
            "{}, {} tracks, {download_policy}{sync_status}",
            library_directory.display(),
            library_files.len()
        ),
    );
}

fn check_resolver_cookies(
    report: &mut Report,
    config: &Config,
    session_status: &crate::prerequisites::SessionStatus,
) {
    if *session_status != crate::prerequisites::SessionStatus::Ready {
        report.push(
            Status::Warn,
            "resolver cookies",
            "no valid selected session, signed-in resolution is disabled",
        );
        return;
    }
    match redwax_auth::inspect_netscape(&config.cookies_path) {
        Ok(summary) if summary.authenticated() => {}
        Ok(_) | Err(_) => {
            report.push(
                Status::Warn,
                "resolver cookies",
                "no usable signing cookies, signed-in resolution is unavailable",
            );
            return;
        }
    }
    let mpv_cookie_policy = match config.use_login_cookies {
        true => "and mpv's own yt-dlp hook has them too",
        false => "and use_login_cookies keeps them from mpv's own yt-dlp hook",
    };
    report.push(
        Status::Ok,
        "resolver cookies",
        format!("chain step 3 runs signed in, {mpv_cookie_policy}"),
    );
}

fn check_terminal(report: &mut Report, config: &Config) {
    let art_setup = terminal::detect(&config.art, None, None);
    let renderer_description = match art_setup.renderer() {
        Some(Renderer::Kitty) => "kitty graphics",
        Some(Renderer::Sixel) => "sixel",
        Some(Renderer::Halfblocks) => "half blocks",
        None => "asked of the terminal at launch",
    };
    report.push(
        Status::Ok,
        "terminal",
        format!(
            "TERM={} TERM_PROGRAM={} tmux={} outer={} art={renderer_description} ({})",
            env_or_dash("TERM"),
            env_or_dash("TERM_PROGRAM"),
            art_setup.in_tmux,
            art_setup.outer_terminal.as_deref().unwrap_or("-"),
            art_setup.reason
        ),
    );
}

/// No session bus costs the media keys and nothing else, so it is a warning.
async fn check_bus(report: &mut Report) {
    match mpris_server::zbus::Connection::session().await {
        Ok(connection) => {
            let name = connection
                .unique_name()
                .map(|name| name.to_string())
                .unwrap_or_else(|| "unnamed".to_owned());
            report.push(Status::Ok, "session bus", format!("reachable as {name}"));
        }
        Err(error) => report.push(
            Status::Warn,
            "session bus",
            format!("not reachable, media keys will not work: {error}"),
        ),
    }
}

// --------------------------------------------------------------------- live

async fn live_checks(report: &mut Report, config: &Config) {
    let session = match Session::read(&config.auth_path) {
        Ok(session) => session,
        Err(error) => {
            report.push(Status::Fail, "whoami", error.to_string());
            return;
        }
    };
    if session.is_none() {
        report.push(
            Status::Fail,
            "whoami",
            "no session to check with, run `redwax login`",
        );
        return;
    }
    let client = Client::new(ClientOptions {
        session: session.clone(),
        dump_dir: config::cache_dir().join("dumps"),
        home_rows: config.home_rows,
    });

    let identity_check_started = Instant::now();
    let identity_result = client.whoami().await;
    let identity_check_elapsed = identity_check_started.elapsed();
    match identity_result {
        // The account name is never printed, per the worker brief.
        Ok(Some(_)) => report.push(
            Status::Ok,
            "whoami",
            format!("signed in, {}", ms(identity_check_elapsed)),
        ),
        Ok(None) => report.push(
            Status::Fail,
            "whoami",
            format!("no identity came back, {ROTATED}, {}", ms(identity_check_elapsed)),
        ),
        Err(error) => report.push(
            Status::Fail,
            "whoami",
            format!("{error}, {}", ms(identity_check_elapsed)),
        ),
    }

    let home = timed_page(
        report,
        "home",
        MIN_HOME_SHELVES,
        "shelves",
        client.home(),
        |page| page.shelves.len(),
    )
    .await;
    let search = timed_page(
        report,
        "search",
        MIN_SEARCH_ITEMS,
        "results",
        client.search(SEARCH_QUERY),
        |page| page.shelves.iter().map(|shelf| shelf.items.len()).sum(),
    )
    .await;

    match search.as_ref().and_then(first_album) {
        Some(album) => {
            timed_page(
                report,
                "album",
                MIN_ALBUM_TRACKS,
                "tracks",
                client.browse(&album),
                |page| page.tracks().count(),
            )
            .await;
        }
        None => report.push(
            Status::Fail,
            "album",
            format!("no album among the results for {SEARCH_QUERY}"),
        ),
    }

    match home.as_ref().and_then(first_playlist) {
        Some(playlist) => {
            timed_page(
                report,
                "playlist",
                MIN_PLAYLIST_TRACKS,
                "tracks",
                client.browse(&playlist),
                |page| page.tracks().count(),
            )
            .await;
        }
        None => report.push(Status::Fail, "playlist", "no playlist on the home feed"),
    }

    live_resolver(report, config, session.as_ref()).await;
}

async fn live_resolver(report: &mut Report, config: &Config, session: Option<&Session>) {
    let resolver = redwax_resolve::Resolver::new(crate::services::resolver_options(config, session));
    for step in resolver.health_check(HIGH_TIER_TRACK).await {
        let hint = match step.ok {
            true => String::new(),
            false => rotation_hint(step.failure),
        };
        report.push(
            step_status(step.step, step.ok),
            format!("resolve {}", step.step.label()),
            format!("{HIGH_TIER_TRACK} {}{hint}, {}", step.detail, ms(step.elapsed)),
        );
    }

    // A hit in the on-disk cache would report a tier nothing measured today.
    resolver.forget(NO_HIGH_TIER_TRACK);
    let resolve_started = Instant::now();
    let fallback_resolution = resolver.resolve(NO_HIGH_TIER_TRACK).await;
    let resolve_elapsed = resolve_started.elapsed();
    match fallback_resolution {
        Ok(stream) => {
            // No track this old has a Premium encode, so anything but 251 means
            // the tier picker read the response differently than it used to.
            let status = match stream.tier {
                Tier::Opus160 => Status::Ok,
                _ => Status::Warn,
            };
            report.push(
                status,
                "fallback tier",
                format!(
                    "{NO_HIGH_TIER_TRACK} {} via {}, 251 expected, {}",
                    stream.tier.label(),
                    stream.source.label(),
                    ms(resolve_elapsed)
                ),
            );
        }
        Err(error) => report.push(
            Status::Fail,
            "fallback tier",
            format!("{NO_HIGH_TIER_TRACK} {error}, {}", ms(resolve_elapsed)),
        ),
    }
    let _ = resolver.flush();
}

/// One live page: how long it took, how many items came back, and the floor
/// below which the walker has stopped reading rather than YouTube having
/// changed its mix.
async fn timed_page(
    report: &mut Report,
    check_name: &'static str,
    minimum_item_count: usize,
    item_unit: &str,
    page_request: impl Future<Output = redwax_ytm::Result<Page>>,
    count_items: impl Fn(&Page) -> usize,
) -> Option<Page> {
    let request_started = Instant::now();
    let page_result = page_request.await;
    let request_elapsed = request_started.elapsed();
    match page_result {
        Ok(page) => {
            let item_count = count_items(&page);
            let status = match item_count >= minimum_item_count {
                true => Status::Ok,
                false => Status::Fail,
            };
            report.push(
                status,
                check_name,
                format!(
                    "{item_count} {item_unit}, {minimum_item_count} the minimum, {}",
                    ms(request_elapsed)
                ),
            );
            Some(page)
        }
        Err(error) => {
            let hint = match error {
                redwax_ytm::Error::Rejected => format!(", {ROTATED}"),
                _ => String::new(),
            };
            report.push(
                Status::Fail,
                check_name,
                format!("{error}{hint}, {}", ms(request_elapsed)),
            );
            None
        }
    }
}

/// A track absent from the download library is the ordinary case, and the
/// anonymous step is a fallback, so neither failing is the alarm.
fn step_status(source: Source, ok: bool) -> Status {
    if ok {
        return Status::Ok;
    }
    match source {
        Source::LocalFile | Source::Cache | Source::NativeAnonymous => Status::Warn,
        Source::NativeSignedIn | Source::YtDlp => Status::Fail,
    }
}

fn rotation_hint(failure: Option<redwax_resolve::FailureKind>) -> String {
    if failure == Some(redwax_resolve::FailureKind::AuthenticationRejected) {
        format!(", {ROTATED}")
    } else {
        String::new()
    }
}

fn first_album(page: &Page) -> Option<redwax_ytm::Browsable> {
    page.shelves
        .iter()
        .flat_map(|shelf| shelf.items.iter())
        .find(|item| matches!(item, Item::Album(_)))
        .and_then(Item::as_browsable)
}

fn first_playlist(page: &Page) -> Option<redwax_ytm::Browsable> {
    page.shelves
        .iter()
        .flat_map(|shelf| shelf.items.iter())
        .find(|item| matches!(item, Item::Playlist(_)))
        .and_then(Item::as_browsable)
}

// ------------------------------------------------------------------ helpers

fn missing_or(path: Option<&Path>) -> String {
    match path {
        Some(path) => path.display().to_string(),
        None => "missing".to_owned(),
    }
}

fn quality_name(quality: Quality) -> &'static str {
    match quality {
        Quality::High => "high",
        Quality::Fast => "fast",
    }
}

fn env_or_dash(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| "-".to_owned())
}

fn count_files(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .count()
        })
        .unwrap_or(0)
}

fn newest_age_text(directory: &Path) -> String {
    let newest_modification = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter_map(|file_metadata| file_metadata.modified().ok())
        .max();
    match newest_modification.and_then(|modified_at| SystemTime::now().duration_since(modified_at).ok()) {
        Some(age) => format!("{} ago", human(age)),
        None => "age unknown".to_owned(),
    }
}

fn age_text(path: &Path) -> String {
    let age = std::fs::metadata(path)
        .and_then(|file_metadata| file_metadata.modified())
        .ok()
        .and_then(|modified_at| SystemTime::now().duration_since(modified_at).ok());
    match age {
        Some(age) => format!("{} ago", human(age)),
        None => "at an unknown time".to_owned(),
    }
}

fn human(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..=90 => format!("{seconds} s"),
        91..=5400 => format!("{} min", seconds / 60),
        5401..=172_800 => format!("{} h", seconds / 3600),
        _ => format!("{} days", seconds / 86_400),
    }
}

fn ms(elapsed: Duration) -> String {
    format!("{} ms", elapsed.as_millis())
}

/// A failed resolver step reports the URL the CDN refused, and every stream URL
/// carries the client address. The nightly timer writes this to the journal.
fn scrub(detail: &str) -> String {
    let mut redacted_detail = String::with_capacity(detail.len());
    let mut remaining_detail = detail;
    loop {
        let Some(ip_parameter_offset) = ip_parameter(remaining_detail) else {
            redacted_detail.push_str(remaining_detail);
            return redacted_detail;
        };
        redacted_detail.push_str(&remaining_detail[..ip_parameter_offset]);
        redacted_detail.push_str("ip=scrubbed");
        let address_and_suffix = &remaining_detail[ip_parameter_offset + "ip=".len()..];
        let address_end_offset = address_and_suffix
            .find(['&', ' '])
            .unwrap_or(address_and_suffix.len());
        remaining_detail = &address_and_suffix[address_end_offset..];
    }
}

/// Anchored to a query separator, because `fvip` is a real stream field that
/// holds no address.
fn ip_parameter(text: &str) -> Option<usize> {
    let mut search_offset = 0;
    while let Some(relative_offset) = text[search_offset..].find("ip=") {
        let ip_parameter_offset = search_offset + relative_offset;
        if matches!(
            text[..ip_parameter_offset].chars().next_back(),
            Some('?') | Some('&')
        ) {
            return Some(ip_parameter_offset);
        }
        search_offset = ip_parameter_offset + "ip=".len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_alone_do_not_report_signed_in_resolution() {
        let directory =
            std::env::temp_dir().join(format!("redwax-doctor-cookie-only-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let cookies_path = directory.join("cookies.txt");
        std::fs::write(
            &cookies_path,
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture\n",
        )
        .unwrap();
        let config = Config {
            cookies_path,
            ..Config::default()
        };
        let mut report = Report::default();

        check_resolver_cookies(
            &mut report,
            &config,
            &crate::prerequisites::SessionStatus::Missing,
        );

        let check = report.get("resolver cookies").unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("disabled"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_address_is_scrubbed_and_fvip_is_left_alone() {
        let line = "403 https://cdn/x?fvip=5&ip=203.0.113.7&itag=774 refused";
        assert_eq!(
            scrub(line),
            "403 https://cdn/x?fvip=5&ip=scrubbed&itag=774 refused"
        );
        assert_eq!(scrub("no url here"), "no url here");
        assert_eq!(scrub("https://cdn/x?ip=192.0.2.1"), "https://cdn/x?ip=scrubbed");
    }

    #[test]
    fn a_report_fails_only_on_a_fail_line() {
        let mut report = Report::default();
        report.push(Status::Ok, "config", "fine");
        report.push(Status::Warn, "state", "empty");
        assert!(!report.failed());
        report.push(Status::Fail, "session", "gone");
        assert!(report.failed());
        assert_eq!(
            report.get("session").map(|check| check.status),
            Some(Status::Fail)
        );
    }

    #[test]
    fn a_line_leads_with_the_status_then_the_name() {
        let mut report = Report::default();
        report.push(Status::Fail, "session", "gone");
        assert_eq!(report.lines()[0], "fail session            gone");
    }
}
