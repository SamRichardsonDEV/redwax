//! `redwax`: the terminal YouTube Music player.
//!
//! Launch reads config, state file,
//! page cache, first frame, and only then mpv, MPRIS, the network and the art
//! protocol query. Nothing here waits on a socket before the first draw.

mod diagnostics;

use std::io::{self, Stdout, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream,
    KeyEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, terminal};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;
use redwax::app::{App, AppEvent, Deps, Level, SessionRefresh, TICK_MILLIS, protocol_for};
use redwax::config::{self, Config};
use redwax::state::SavedState;
use redwax::sync::{EvictionDecision, SyncOptions, SyncReport, sync_once};
use redwax::terminal::{ArtSetup, Renderer};
use redwax::{art, icons, page_cache, session, theme};
use redwax_player::{Playable, Player, StreamSource};
use redwax_resolve::{Resolver, Source, Stream};
use redwax_ytm::{Client, ClientOptions, MusicSource, Session};

/// Set while the alternate screen is ours, so a panic anywhere restores the
/// terminal exactly once and the message is printed on a normal screen.
static ENABLED_TERMINAL_STAGES: AtomicU8 = AtomicU8::new(0);
const RAW_MODE: u8 = 1;
const ALTERNATE_SCREEN: u8 = 2;
const MOUSE_CAPTURE: u8 = 4;
const HIDDEN_CURSOR: u8 = 8;
const BRACKETED_PASTE: u8 = 16;

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}
static PANIC_MESSAGE: Mutex<Option<String>> = Mutex::new(None);

const HELP: &str = "usage: redwax [--config PATH] [--version]
       redwax setup  [--config PATH]   what a fresh install still needs
       redwax login  [--browser NAME] [--profile NAME] [--account QUERY] [--list-accounts] [--config PATH]
       redwax logout [--config PATH]
       redwax doctor [--live] [--config PATH]
       redwax sync   [--dry-run] [--config PATH]";

/// The sync waits this long after the first frame, so the first page fetches
/// and the first play have the network and the resolver to themselves.
const SYNC_DELAY: Duration = Duration::from_secs(20);
const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

fn doctor_main(config_path: Option<PathBuf>, live: bool) -> ExitCode {
    let doctor_options = redwax::doctor::Options { config_path, live };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("redwax doctor: {error}");
            return ExitCode::FAILURE;
        }
    };
    let doctor_report = runtime.block_on(redwax::doctor::run(&doctor_options));
    for line in doctor_report.lines() {
        println!("{line}");
    }
    match doctor_report.failed() {
        true => ExitCode::FAILURE,
        false => ExitCode::SUCCESS,
    }
}

/// The banner a first run prints, and `redwax setup` on demand.
///
/// A person who has just installed this wants one screen and one command, not
/// the twenty line maintenance report `doctor` exists for.
fn print_welcome(welcome: &redwax::onboard::Welcome, config: &Config) {
    println!("redwax needs a few things before it can play.\n");
    for line in welcome.lines() {
        println!("{line}");
    }
    if !welcome.ready() {
        println!(
            "\nredwax imports your browser session without asking for your password.\nIt keeps its private login files under {}.",
            redwax::onboard::kept_under(config).display()
        );
    }
}

/// `redwax setup`: the same checks, asked for rather than run into.
fn setup_main(config_path: Option<PathBuf>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            report(&format!("redwax setup: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let config = match Config::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            report(&format!("redwax setup: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let welcome = runtime.block_on(redwax::onboard::welcome(&config));
    match welcome.ready() {
        true => {
            println!(
                "Local setup is ready. Run `{}` to play.\n",
                redwax::cli::program_name()
            );
            for line in welcome.lines() {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        false => {
            print_welcome(&welcome, &config);
            ExitCode::FAILURE
        }
    }
}

/// One runtime, one printed error, one exit code, for every subcommand that is
/// not the launch path.
fn subcommand_main(run_command: impl FnOnce(&tokio::runtime::Runtime) -> Result<bool, String>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("redwax: {error}");
            return ExitCode::FAILURE;
        }
    };
    match run_command(&runtime) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("redwax: {message}");
            ExitCode::from(2)
        }
    }
}

/// Confirm an imported browser session before reporting a successful login.
fn login_main(
    runtime: &tokio::runtime::Runtime,
    config_path: Option<PathBuf>,
    browser_name: Option<String>,
    profile_name: Option<String>,
    account_query: Option<String>,
    list_accounts: bool,
) -> Result<bool, String> {
    let config = Config::load(config_path.as_deref())?;
    let browser = browser_name
        .as_deref()
        .map(redwax::browser_login::Browser::parse)
        .transpose()?;
    let mut prepared = match session::prepare_from_browser(&config, browser, profile_name.as_deref()) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("redwax login: {error}");
            return Ok(false);
        }
    };
    let client = Client::new(ClientOptions {
        session: Some(prepared.session().clone()),
        dump_dir: config::cache_dir().join("dumps"),
        home_rows: config.home_rows,
    });
    if let Some(requested_account) = &account_query
        && let Err(error) = runtime.block_on(choose_account(&client, &mut prepared, requested_account))
    {
        eprintln!("redwax login: {error}");
        return Ok(false);
    }

    match runtime.block_on(client.whoami()) {
        Ok(Some(handle)) => {
            let selected_browser = prepared.source().browser;
            prepared.publish(&config)?;
            println!("Signed in as {handle}.");
            println!("Browser: {}.", selected_browser.name());
            if list_accounts {
                let accounts = runtime
                    .block_on(client.accounts())
                    .map_err(|error| error.to_string())?;
                print_accounts(&accounts);
            }
            println!("Run `{}` to open your library.", redwax::cli::program_name());
            Ok(true)
        }
        Ok(None) => {
            eprintln!(
                "Login was not confirmed. Open https://music.youtube.com in your browser and sign in, then retry."
            );
            Ok(false)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn print_accounts(accounts: &[redwax_ytm::Account]) {
    if accounts.is_empty() {
        println!("YouTube did not list any switchable accounts.");
        return;
    }
    println!("Available YouTube Music accounts:");
    for (index, account) in accounts.iter().enumerate() {
        println!("  {}. {}", index + 1, account.label());
    }
    println!(
        "Choose one with `{} login --account NUMBER`.",
        redwax::cli::program_name()
    );
}

fn logout_main(config_path: Option<PathBuf>) -> Result<bool, String> {
    let config = Config::load(config_path.as_deref())?;
    session::remove_exports(&config)?;
    println!("Removed redwax credential exports and remembered account identity.");
    println!("A running redwax keeps its loaded session until restart or in-app logout.");
    Ok(true)
}

async fn choose_account(
    client: &Client,
    prepared: &mut session::PreparedBrowserSession,
    account_query: &str,
) -> Result<(), String> {
    let available_accounts = client.accounts().await.map_err(|error| error.to_string())?;
    let selected_account = redwax_ytm::find_account(&available_accounts, account_query).ok_or_else(|| {
        print_accounts(&available_accounts);
        format!("no account matches {account_query}")
    })?;
    prepared.select_account(selected_account)?;
    client.reload(Some(prepared.session().clone()));
    Ok(())
}

/// `redwax sync [--dry-run]`: the marks in, the library brought level.
fn sync_main(
    runtime: &tokio::runtime::Runtime,
    config_path: Option<PathBuf>,
    dry_run: bool,
) -> Result<bool, String> {
    let config = Config::load(config_path.as_deref())?;
    if !uses_firefox(&config) {
        return Err("Download-mark sync reads Firefox storage. Other browsers support browsing, library access and playback.".to_owned());
    }
    let initial_session = Session::read(&config.auth_path).map_err(|error| error.to_string())?;
    let music_source = Arc::new(Client::new(ClientOptions {
        session: initial_session.clone(),
        dump_dir: config::cache_dir().join("dumps"),
        home_rows: config.home_rows,
    }));
    let sync_settings = sync_options(&config);
    if dry_run {
        return runtime.block_on(sync_dry_run(&sync_settings, music_source.as_ref()));
    }
    let resolver = Resolver::new(redwax::services::resolver_options(
        &config,
        initial_session.as_ref(),
    ));
    let sync_report = runtime
        .block_on(sync_once(&sync_settings, &resolver, music_source.as_ref()))
        .map_err(|error| format!("sync: {error}"))?;
    print_sync(&sync_report);
    Ok(sync_report.failed.is_empty())
}

/// The plan alone, with nothing downloaded and nothing moved.
async fn sync_dry_run(sync_settings: &SyncOptions, music_source: &Client) -> Result<bool, String> {
    let firefox_profile = redwax_marks::FirefoxProfile::find_for(
        sync_settings.profile.as_deref(),
        redwax_marks::ProfileCapability::Marks,
    )
    .map_err(|error| error.to_string())?;
    let library_marks = redwax_marks::read_marks(&firefox_profile).map_err(|error| error.to_string())?;
    println!("profile {}", firefox_profile.dir.display());
    println!(
        "marked {} tracks, {} playlists, {} albums, {} stored in the browser",
        library_marks.tracks.len(),
        library_marks.playlists.len(),
        library_marks.albums.len(),
        library_marks.stored_locally.len()
    );
    let library_files = redwax::sync::library_index(&sync_settings.library_dir);
    let sync_assessment = redwax::sync::assess(&library_marks, music_source, &library_files).await;
    let sync_plan = sync_assessment.plan();
    println!(
        "library {} holds {} of {} marked tracks",
        sync_settings.library_dir.display(),
        sync_plan.present,
        sync_plan.marked
    );
    println!("would download {}", sync_plan.download.len());
    match sync_assessment.eviction().reason() {
        None => println!(
            "would move aside {} after {} days",
            sync_plan.evict.len(),
            sync_settings.grace.as_secs() / 86_400
        ),
        Some(eviction_reason) => println!(
            "would move aside 0 of {} candidates: {eviction_reason}",
            sync_assessment.eviction_candidates()
        ),
    }
    for (failed_id, failure_reason) in sync_assessment.failures() {
        println!("failed {failed_id} {failure_reason}");
    }
    Ok(sync_assessment.failures().is_empty())
}

fn print_sync(sync_report: &SyncReport) {
    println!(
        "marked {}, already present {}, downloaded {}, moved aside {}",
        sync_report.marked,
        sync_report.present,
        sync_report.downloaded.len(),
        sync_report.evicted.len()
    );
    for path in &sync_report.downloaded {
        println!("downloaded {}", path.display());
    }
    for path in &sync_report.evicted {
        println!("moved aside {}", path.display());
    }
    for (failed_id, failure_reason) in &sync_report.failed {
        println!("failed {failed_id} {failure_reason}");
    }
    if let Some(eviction_reason) = sync_report.eviction.reason() {
        println!(
            "moved aside 0 of {} candidates: {eviction_reason}",
            sync_report.eviction_candidates
        );
    }
}

fn sync_options(config: &Config) -> SyncOptions {
    SyncOptions {
        library_dir: config.library_dir.clone(),
        profile: session::RememberedSession::read(&config.session_path)
            .filter(|saved| saved.source.browser == redwax::browser_login::Browser::Firefox)
            .and_then(|saved| saved.source.profile.as_deref().map(str::to_owned)),
        grace: config.download_grace(),
    }
}

fn uses_firefox(config: &Config) -> bool {
    session::RememberedSession::read(&config.session_path)
        .is_none_or(|saved| saved.source.browser == redwax::browser_login::Browser::Firefox)
}

/// Blocking-pool callers share recovery and logout through one coordinator.
fn session_refresher(
    config: &Config,
    music_source: Arc<Client>,
    resolver: Option<Resolver>,
) -> (SessionRefresh, SessionRefresh, Arc<session::SessionCoordinator>) {
    let logout_config = config.clone();
    let logout_source = music_source.clone();
    let logout_resolver = resolver.clone();
    let config = config.clone();
    let runtime_handle = tokio::runtime::Handle::current();
    let logout_runtime_handle = runtime_handle.clone();
    let import_session: SessionRefresh = Arc::new(move || {
        let refreshed_session = session::prepare_from_browser(&config, None, None)?.publish(&config)?;
        if let Some(resolver) = &resolver {
            runtime_handle.block_on(resolver.reload_credentials(Some(refreshed_session.identity().clone())));
        }
        music_source.reload(Some(refreshed_session));
        Ok(())
    });
    let session_coordinator = Arc::new(session::SessionCoordinator::new(
        import_session,
        Duration::from_secs(60),
    ));
    let refresh_owner = session_coordinator.clone();
    let logout_coordinator = session_coordinator.clone();
    let log_out = Arc::new(move || {
        logout_coordinator.logout(|| {
            let removal_result = session::remove_exports(&logout_config);
            if let Some(resolver) = &logout_resolver {
                logout_runtime_handle.block_on(resolver.reload_credentials(None));
            }
            logout_source.reload(None);
            removal_result
        })
    });
    (
        Arc::new(move || refresh_owner.refresh()),
        log_out,
        session_coordinator,
    )
}

fn main() -> ExitCode {
    use redwax::cli::Command;
    let command = match redwax::cli::parse(
        std::env::args().skip(1),
        std::env::var_os("REDWAX_CONFIG").map(PathBuf::from),
    ) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("redwax: {error}");
            return ExitCode::from(2);
        }
    };
    let config_path = match command {
        Command::Help => {
            println!("{}", HELP.replace("redwax", redwax::cli::program_name()));
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("redwax {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Command::Setup { config } => return setup_main(config),
        Command::Doctor { config, live } => return doctor_main(config, live),
        Command::Login {
            config,
            browser,
            profile,
            account,
            list_accounts,
        } => {
            return subcommand_main(move |runtime| {
                login_main(runtime, config, browser, profile, account, list_accounts)
            });
        }
        Command::Logout { config } => return subcommand_main(move |_| logout_main(config)),
        Command::Sync { config, dry_run } => {
            return subcommand_main(move |runtime| sync_main(runtime, config, dry_run));
        }
        Command::Run { config } => config,
    };

    install_panic_hook();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("redwax: {error}");
            return ExitCode::FAILURE;
        }
    };
    // The whole run is a task so a panic inside it comes back as a join error,
    // which leaves the restore below as the only path that touches the screen.
    let app_task_result = runtime.block_on(async { tokio::spawn(run(config_path)).await });
    restore_terminal();
    match app_task_result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            // The welcome path returns an empty error, having printed a screen
            // of its own that a second line would only get in the way of.
            if !error.is_empty() {
                record(&format!("redwax: {error}"));
                report(&format!("redwax: {error}"));
            }
            ExitCode::FAILURE
        }
        Err(_) => {
            let message = PANIC_MESSAGE
                .lock()
                .ok()
                .and_then(|panic_message_guard| panic_message_guard.clone())
                .unwrap_or_else(|| "redwax stopped unexpectedly".to_string());
            report(&message);
            ExitCode::FAILURE
        }
    }
}

/// A panic must never scribble on the alternate screen, and a panic in a
/// background task must not be mistaken for the end of the run. Both are
/// recorded and printed once the terminal is back.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let panic_location = panic_info
            .location()
            .map(|source_location| format!("{}:{}", source_location.file(), source_location.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = format!("redwax panicked at {panic_location}: {panic_info}");
        record(&message);
        if let Ok(mut panic_message_guard) = PANIC_MESSAGE.lock() {
            *panic_message_guard = Some(message);
        }
    }));
}

/// Say why redwax stopped, on the way out.
///
/// A terminal that has already gone takes stderr with it, and `eprintln!`
/// panics on the write. The message is in the crash log either way, so a
/// window that closed first costs nothing here.
fn report(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

/// Append one line to `~/.cache/redwax/crash.log`.
///
/// A message printed after the screen is restored is lost whenever redwax runs
/// in a window that closes with it, which is every launch from the bar icon.
/// The log is the only account of those endings, so `doctor` can show them.
fn record(message: &str) {
    let timestamp_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs())
        .unwrap_or_default();
    let cache_directory = config::cache_dir();
    if std::fs::create_dir_all(&cache_directory).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o700));
    let crash_log_path = cache_directory.join("crash.log");
    if crash_log_path.is_file() {
        let _ = std::fs::set_permissions(&crash_log_path, std::fs::Permissions::from_mode(0o600));
    }
    trim_log(&crash_log_path);
    if let Ok(mut crash_log_file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&crash_log_path)
    {
        let _ = writeln!(crash_log_file, "{timestamp_seconds} {message}");
    }
}

/// Keep the log to its last 200 lines, so it never grows without an end.
fn trim_log(crash_log_path: &Path) {
    let Ok(log_contents) = std::fs::read_to_string(crash_log_path) else {
        return;
    };
    let log_lines: Vec<&str> = log_contents.lines().collect();
    if log_lines.len() <= 200 {
        return;
    }
    let retained_lines = log_lines[log_lines.len() - 200..].join("\n");
    let _ = std::fs::write(crash_log_path, format!("{retained_lines}\n"));
}

fn enter_terminal() -> io::Result<(Terminal<CrosstermBackend<Stdout>>, TerminalGuard)> {
    let terminal_guard = TerminalGuard;
    terminal::enable_raw_mode()?;
    ENABLED_TERMINAL_STAGES.fetch_or(RAW_MODE, Ordering::SeqCst);
    let mut terminal_output = io::stdout();
    // Record each stage before its write because a failed write can be partial.
    ENABLED_TERMINAL_STAGES.fetch_or(ALTERNATE_SCREEN, Ordering::SeqCst);
    execute!(terminal_output, EnterAlternateScreen)?;
    ENABLED_TERMINAL_STAGES.fetch_or(MOUSE_CAPTURE, Ordering::SeqCst);
    execute!(terminal_output, EnableMouseCapture)?;
    ENABLED_TERMINAL_STAGES.fetch_or(HIDDEN_CURSOR, Ordering::SeqCst);
    execute!(terminal_output, cursor::Hide)?;
    ENABLED_TERMINAL_STAGES.fetch_or(BRACKETED_PASTE, Ordering::SeqCst);
    execute!(terminal_output, EnableBracketedPaste)?;
    Ok((
        Terminal::new(CrosstermBackend::new(terminal_output))?,
        terminal_guard,
    ))
}

fn restore_terminal() {
    let enabled_stages = ENABLED_TERMINAL_STAGES.swap(0, Ordering::SeqCst);
    if enabled_stages == 0 {
        return;
    }
    let mut terminal_output = io::stdout();
    if enabled_stages & BRACKETED_PASTE != 0 {
        let _ = execute!(terminal_output, DisableBracketedPaste);
    }
    if enabled_stages & HIDDEN_CURSOR != 0 {
        let _ = execute!(terminal_output, cursor::Show);
    }
    if enabled_stages & MOUSE_CAPTURE != 0 {
        let _ = execute!(terminal_output, DisableMouseCapture);
    }
    if enabled_stages & ALTERNATE_SCREEN != 0 {
        let _ = execute!(terminal_output, LeaveAlternateScreen);
    }
    if enabled_stages & RAW_MODE != 0 {
        let _ = terminal::disable_raw_mode();
    }
    let _ = terminal_output.flush();
}

/// The resolver behind the player's `StreamSource`. Written against the
/// contract in `crates/resolve/src/lib.rs`.
struct Streams {
    resolver: Resolver,
    refresh_session: Option<SessionRefresh>,
}

const YTDLP_FALLBACK_WARNING: &str = "Using slower yt-dlp fallback. Run `redwax doctor --live`.";
const PLAYBACK_RESOLUTION_BUDGET: Duration = Duration::from_secs(12);

fn playable(stream: Stream) -> Playable {
    let warning = (stream.source == Source::YtDlp).then(|| YTDLP_FALLBACK_WARNING.to_string());
    Playable {
        target: stream.location.as_mpv_target(),
        tier_label: stream.tier.label().to_string(),
        warning,
    }
}

async fn resolve_with_refresh(
    initial_resolution: impl Future<Output = redwax_resolve::Result<Stream>>,
    credential_refresh: impl Future<Output = bool>,
    native_resolution_retry: impl Future<Output = redwax_resolve::Result<Stream>>,
) -> redwax_resolve::Result<Stream> {
    let initial_failure = match initial_resolution.await {
        Ok(stream) => return Ok(stream),
        Err(initial_failure) => initial_failure,
    };
    if initial_failure.kind() == redwax_resolve::FailureKind::AuthenticationRejected
        && credential_refresh.await
        && let Ok(stream) = native_resolution_retry.await
    {
        return Ok(stream);
    }
    Err(initial_failure)
}

impl StreamSource for Streams {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        self.resolver.cached(video_id).map(playable)
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Playable>, String> {
        let credential_refresh = async {
            let Some(import_session) = self.refresh_session.clone() else {
                return false;
            };
            let refresh_result = tokio::task::spawn_blocking(move || import_session()).await;
            matches!(refresh_result, Ok(Ok(())))
        };
        let native_resolution_retry = async {
            // Refreshed cookies permit native recovery, never another subprocess chain.
            self.resolver
                .resolve_traced(video_id, Some(Source::NativeSignedIn))
                .await
                .0
        };
        tokio::time::timeout(
            PLAYBACK_RESOLUTION_BUDGET,
            resolve_with_refresh(
                self.resolver.resolve(video_id),
                credential_refresh,
                native_resolution_retry,
            ),
        )
        .await
        .map_err(|_| "YouTube Music audio timed out. Retry.".to_string())?
        .map(|stream| Some(playable(stream)))
        .map_err(|error| error.playback_message().to_string())
    }

    fn allow_mpv_fallback(&self) -> bool {
        false
    }

    async fn resolve_prefetch(&self, video_id: &str) -> Option<Playable> {
        self.resolver.resolve_prefetch(video_id).await.map(playable)
    }

    fn prefetch(&self, video_ids: Vec<String>) {
        self.resolver.prefetch(video_ids);
    }

    fn forget(&self, video_id: &str) {
        self.resolver.forget(video_id);
    }
}

/// The theme the first frame paints: the one the last run saved when the config
/// asks for the terminal palette, else the built-in.
fn first_theme(config: &Config) -> theme::Theme {
    let theme_controller = redwax::themes::ThemeController::new(&config.theme);
    theme_controller.present(
        theme_controller.resolve_or_cached(config),
        config.transparent,
        None,
    )
}

async fn run(config_path: Option<PathBuf>) -> Result<(), String> {
    let config = Config::load(config_path.as_deref())?;
    let mut initial_session = match Session::read(&config.auth_path) {
        Ok(session) => session,
        Err(error) => {
            println!("Saved login could not be read: {error}. Trying your browser session.");
            None
        }
    };
    // Setup stays on the normal terminal so its instructions survive the exit.
    // Returning users avoid the subprocess probes when their saved login is readable.
    if initial_session.is_none() {
        let welcome = redwax::onboard::welcome(&config).await;
        if welcome
            .steps
            .iter()
            .any(|step| !step.ready && !step.package.is_empty())
        {
            print_welcome(&welcome, &config);
            return Err(String::new());
        }
        let import_config = config.clone();
        match tokio::task::spawn_blocking(move || {
            let prepared = session::prepare_from_browser(&import_config, None, None)?;
            let browser = prepared.source().browser;
            let browser_session = prepared.publish(&import_config)?;
            Ok::<_, String>((browser_session, browser))
        })
        .await
        {
            Ok(Ok((browser_session, browser))) => {
                println!("Imported your {} session. Opening redwax.", browser.name());
                initial_session = Some(browser_session);
            }
            Ok(Err(error)) => {
                print_welcome(&welcome, &config);
                return Err(format!("Automatic browser login could not finish: {error}"));
            }
            Err(_) => {
                return Err(
                    "Automatic browser login did not finish. Run login with --browser to choose a source."
                        .to_owned(),
                );
            }
        }
    }
    let cache_directory = config::cache_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cache_directory)
        .and_then(|()| std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o700)))
        .map_err(|error| format!("could not protect the redwax cache directory: {error}"))?;
    let loaded_state = SavedState::load(&config.state_path);
    let state_warning = loaded_state.warning;
    let saved_playback = loaded_state.state;
    let music_source = Arc::new(Client::new(ClientOptions {
        session: initial_session.clone(),
        dump_dir: config::cache_dir().join("dumps"),
        home_rows: config.home_rows,
    }));
    let resolver = if std::env::var_os("REDWAX_NO_RESOLVER").is_some() {
        None
    } else {
        Some(Resolver::new(redwax::services::resolver_options(
            &config,
            initial_session.as_ref(),
        )))
    };
    let (refresh_session, log_out, session_coordinator) =
        session_refresher(&config, music_source.clone(), resolver.clone());
    let dependencies = Deps {
        icons: icons::icon_set(&config.icons),
        theme: first_theme(&config),
        page_cache: page_cache::PageCache::new(config.page_cache_dir.clone()),
        art_cache: Arc::new(art::ArtCache::new(config.art_cache_dir.clone())),
        // The real detection can ask tmux, which is a subprocess. It runs after
        // the first frame and replaces this.
        art_setup: redwax::terminal::ArtSetup::pending(),
        saved: saved_playback,
        state_store: redwax::state::PlaybackStateStore::new(config.state_path.clone()),
        session_logout: Some(log_out),
        session_refresh: Some(refresh_session.clone()),
        config,
    };
    let player_options = redwax::services::player_options(&dependencies.config);
    let player = match resolver.clone() {
        Some(resolver) => Player::new(
            player_options,
            Some(Streams {
                resolver,
                refresh_session: Some(refresh_session.clone()),
            }),
        ),
        None => Player::new::<Streams>(player_options, None),
    };

    let (app_event_sender, mut app_events) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        music_source.clone(),
        player.clone(),
        dependencies,
        app_event_sender,
    );
    if let Some(warning) = state_warning {
        app.notify(warning, Some("Saved playback"), Level::Warning);
    }
    let (mut terminal_screen, _terminal_guard) = enter_terminal().map_err(|error| error.to_string())?;
    let mut runtime_tasks = tokio::task::JoinSet::new();
    let mut timing_log = None;
    let event_loop_result = async {
        terminal_screen
            .draw(|frame| app.draw(frame))
            .map_err(|error| error.to_string())?;

        timing_log = if std::env::var_os("REDWAX_PLAYBACK_TIMINGS").is_some_and(|value| value == "0") {
            None
        } else {
            match diagnostics::TimingLog::start(config::cache_dir().join("playback-timings.jsonl")).await {
                Ok(log) => Some(log),
                Err(error) => {
                    record(&format!("Could not start playback timing log: {error}"));
                    None
                }
            }
        };

        let app_timing_sink = timing_log.as_ref().map(|log| log.sink("app"));
        if let Some(sink) = &app_timing_sink {
            sink(0, "", "ui", "session_start", Duration::ZERO, None);
        }
        if let Some(resolver) = &resolver {
            resolver.set_runtime_sink(timing_log.as_ref().map(|log| log.sink("resolver")));
        }
        if let Some(log) = &timing_log {
            let player_timing_sink = log.sink("player");
            player.set_timing_sink(Some(Arc::new(
                move |playback_request_id, video_id, playback_phase, elapsed| {
                    player_timing_sink(playback_request_id, video_id, "foreground", playback_phase, elapsed, None);
                },
            )));
        }
        // Everything past the first frame. The protocol query writes and reads
        // stdio, so it goes before the event stream takes the keyboard.
        let art_setup = redwax::terminal::detect(&app.deps.config.art, None, None);
        if art_setup.in_tmux && art_setup.renderer().is_some_and(Renderer::draws_pixels) {
            redwax::terminal::enable_tmux_passthrough();
        }
        if let Some(picker) = build_picker(&art_setup) {
            app.set_picker(picker);
        }
        app.deps.art_setup = art_setup;
        app.start_background();
        if let Some(resolver) = resolver.clone() {
            start_sync_loop(&mut runtime_tasks, &app.deps.config, music_source.clone(), resolver, app.events.clone());
        }

        let mut terminal_events = EventStream::new();
        let mut player_state_updates = player.state();
        let mut ticker = tokio::time::interval(Duration::from_millis(TICK_MILLIS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut input_request_id = 0_u64;
        let mut timing_writer_failure_reported = false;
        loop {
            tokio::select! {
                terminal_event = terminal_events.next() => match terminal_event {
                    Some(Ok(Event::Key(key_event))) if key_event.kind == KeyEventKind::Press => {
                        let input_action = app.timing_action(key_event);
                        let input_phase = input_action.and_then(redwax::commands::input_phase);
                        if let (Some(sink), Some(input_phase)) = (&app_timing_sink, input_phase) {
                            input_request_id += 1;
                            let timing_track = match input_action.map(redwax::commands::subject) {
                                Some(redwax::commands::SubjectRule::Selected) => app.highlighted_track(),
                                Some(redwax::commands::SubjectRule::SelectedOrPlaying) => {
                                    app.highlighted_track().or_else(|| app.player_state.track())
                                }
                                Some(redwax::commands::SubjectRule::Playing) => app.player_state.track(),
                                Some(redwax::commands::SubjectRule::None) | None => None,
                            };
                            sink(
                                input_request_id,
                                timing_track.map_or("", |timing_track| timing_track.video_id.as_str()),
                                "ui",
                                input_phase,
                                Duration::ZERO,
                                None,
                            );
                        }
                        let input_started = Instant::now();
                        app.handle_key(key_event);
                        if let Some(sink) = &app_timing_sink
                            && input_started.elapsed() >= Duration::from_millis(20)
                        {
                            sink(
                                input_request_id,
                                "",
                                "ui",
                                "input.handler_slow",
                                input_started.elapsed(),
                                None,
                            );
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
                    Some(Ok(Event::Resize(_, _))) => app.resized(),
                    Some(Ok(Event::Paste(text))) => app.handle_paste(&text),
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => return Err(error.to_string()),
                    None => {
                        record("the terminal closed its input");
                        break;
                    }
                },
                event = app_events.recv() => match event {
                    Some(event) => {
                        let event_started = Instant::now();
                        app.handle_event(event);
                        if let Some(sink) = &app_timing_sink
                            && event_started.elapsed() >= Duration::from_millis(20)
                        {
                            sink(0, "", "ui", "event.handler_slow", event_started.elapsed(), None);
                        }
                    }
                    None => {
                        record("the event channel closed");
                        break;
                    }
                },
                state_update_result = player_state_updates.changed() => {
                    if state_update_result.is_err() {
                        record("the player stopped reporting");
                        break;
                    }
                    let observed_player_state = player_state_updates.borrow_and_update().clone();
                    app.handle_event(AppEvent::PlayerChanged(observed_player_state));
                }
                completed_task = runtime_tasks.join_next(), if !runtime_tasks.is_empty() => {
                    if let Some(Err(error)) = completed_task {
                        app.notify(format!("Background service stopped: {error}"), Some("Runtime"), Level::Warning);
                    }
                },
                scheduled_tick = ticker.tick() => {
                    if !timing_writer_failure_reported && timing_log.as_ref().is_some_and(diagnostics::TimingLog::stopped) {
                        timing_writer_failure_reported = true;
                        app.notify("Playback timing storage stopped", Some("Diagnostics"), Level::Warning);
                    }
                    let tick_delay = scheduled_tick.elapsed();
                    if let Some(sink) = &app_timing_sink
                        && tick_delay >= Duration::from_millis(100)
                    {
                        sink(0, "", "ui", "tick.late", tick_delay, None);
                    }
                    app.handle_event(AppEvent::Tick);
                },
            }
            let event_drain_started = Instant::now();
            while let Ok(event) = app_events.try_recv() {
                app.handle_event(event);
            }
            if let Some(sink) = &app_timing_sink
                && event_drain_started.elapsed() >= Duration::from_millis(20)
            {
                sink(0, "", "ui", "event.drain_slow", event_drain_started.elapsed(), None);
            }
            let frame_started = Instant::now();
            terminal_screen
                .draw(|frame| app.draw(frame))
                .map_err(|error| error.to_string())?;
            if let Some(sink) = &app_timing_sink
                && frame_started.elapsed() >= Duration::from_millis(20)
            {
                sink(0, "", "ui", "frame.slow", frame_started.elapsed(), None);
            }
            if app.quit {
                break;
            }
        }

        Ok::<(), String>(())
    }.await;
    runtime_tasks.shutdown().await;
    let shutdown_result = app.shutdown().await;
    let _ = tokio::task::spawn_blocking(move || session_coordinator.close()).await;
    player.set_timing_sink(None);
    if let Some(resolver) = &resolver {
        resolver.set_runtime_sink(None);
    }
    if let Some(timing_log) = timing_log {
        timing_log.shutdown().await;
    }
    if let Some(resolver) = resolver {
        let _ = resolver.flush();
    }
    restore_terminal();
    match (event_loop_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}\nCleanup failed: {cleanup}")),
    }
}

/// Downloads run behind everything else: 20 s after the first frame, then every
/// 15 minutes. A quiet pass says nothing, because most passes are quiet.
fn start_sync_loop(
    runtime_tasks: &mut tokio::task::JoinSet<()>,
    config: &Config,
    music_source: Arc<Client>,
    resolver: Resolver,
    app_events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    if !config.downloads_enabled || !uses_firefox(config) {
        return;
    }
    let sync_settings = sync_options(config);
    runtime_tasks.spawn(async move {
        tokio::time::sleep(SYNC_DELAY).await;
        loop {
            match sync_once(&sync_settings, &resolver, music_source.as_ref()).await {
                Ok(sync_report) => {
                    if !sync_report.downloaded.is_empty() {
                        let _ = app_events.send(AppEvent::Notify {
                            text: format!("{} tracks downloaded", sync_report.downloaded.len()),
                            title: None,
                            level: Level::Info,
                        });
                    }
                    if sync_report.eviction == EvictionDecision::IncompleteMarks {
                        let _ = app_events.send(AppEvent::Notify {
                            text: format!(
                                "Kept {} unmarked files: {}",
                                sync_report.eviction_candidates, sync_report.eviction
                            ),
                            title: Some("Download sync".to_string()),
                            level: Level::Warning,
                        });
                    }
                }
                // A profile that cannot be read is a warning once per pass and
                // never a reason to stop trying.
                Err(error) => {
                    let _ = app_events.send(AppEvent::Notify {
                        text: error,
                        title: Some("Download sync".to_string()),
                        level: Level::Warning,
                    });
                }
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    });
}

/// The font size has to come from the terminal even when the config names the
/// protocol, so only half blocks skip the measurement.
///
/// The measurement is an ioctl wherever the terminal answers one, because the
/// capability query reads stdin and swallows whatever was typed while it
/// waited, which cost the user the first keystroke after launch. Only a
/// terminal that has to be asked which protocol it speaks pays that price.
fn build_picker(art_setup: &ArtSetup) -> Option<Picker> {
    if art_setup.is_pending() {
        return None;
    }
    match art_setup.renderer() {
        Some(Renderer::Halfblocks) => Some(Picker::halfblocks()),
        Some(renderer) => {
            let mut image_picker = picker_from_ioctl().or_else(|| Picker::from_query_stdio().ok())?;
            image_picker.set_protocol_type(protocol_for(renderer));
            Some(image_picker)
        }
        None => Picker::from_query_stdio().ok(),
    }
}

/// A picker sized from `TIOCGWINSZ`, or `None` when the terminal reports no
/// pixel size. `from_fontsize` is deprecated in favour of the query this
/// exists to avoid, so the warning is allowed here on purpose.
fn picker_from_ioctl() -> Option<Picker> {
    let terminal_size = terminal::window_size().ok()?;
    if terminal_size.width == 0
        || terminal_size.height == 0
        || terminal_size.columns == 0
        || terminal_size.rows == 0
    {
        return None;
    }
    #[allow(deprecated)]
    Some(Picker::from_fontsize(
        (
            terminal_size.width / terminal_size.columns,
            terminal_size.height / terminal_size.rows,
        )
            .into(),
    ))
}

#[cfg(test)]
mod stream_tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    use redwax_resolve::{Error, Location, Source, Stream, Tier};

    use super::{Streams, resolve_with_refresh};
    use redwax_player::StreamSource;
    use redwax_resolve::{Quality, Resolver, ResolverOptions};

    #[tokio::test]
    async fn stream_futures_fit_default_worker_stacks() {
        fn future_size<F: Future>(_: impl FnOnce() -> F) -> usize {
            std::mem::size_of::<F>()
        }
        let cache_dir = std::env::temp_dir().join(format!("redwax-future-size-{}", std::process::id()));
        let streams = Streams {
            resolver: Resolver::new(ResolverOptions {
                cookies_path: None,
                identity: None,
                quality: Quality::High,
                library_dir: None,
                ytdlp_args: Vec::new(),
                parallel: 3,
                cache_dir: cache_dir.clone(),
            }),
            refresh_session: None,
        };
        for (name, bytes) in [
            ("selected", future_size(|| streams.resolve("synthetic"))),
            ("prefetch", future_size(|| streams.resolve_prefetch("synthetic"))),
        ] {
            assert!(bytes <= 64 * 1024, "{name} future uses {bytes} bytes");
        }
        let _ = std::fs::remove_dir_all(cache_dir);
    }

    fn native_stream() -> Stream {
        Stream {
            video_id: "sample".to_owned(),
            location: Location::Url("https://example.invalid/audio".to_owned()),
            tier: Tier::Opus283,
            source: Source::NativeSignedIn,
            expires_at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn successful_playback_does_not_refresh_or_retry() {
        let stream = resolve_with_refresh(
            async { Ok(native_stream()) },
            async { panic!("successful playback must not refresh cookies") },
            async { panic!("successful playback must not repeat resolution") },
        )
        .await
        .expect("native playback succeeds");
        assert_eq!(stream.source, Source::NativeSignedIn);
    }

    #[tokio::test]
    async fn cookie_recovery_runs_the_full_chain_once_and_retries_only_native() {
        let full_chains = AtomicUsize::new(0);
        let native_retries = AtomicUsize::new(0);
        let first = async {
            full_chains.fetch_add(1, Ordering::SeqCst);
            Err(Error::Failed(redwax_resolve::Failure {
                kind: redwax_resolve::FailureKind::AuthenticationRejected,
                evidence: "synthetic rejection".to_owned(),
            }))
        };
        let native = async {
            native_retries.fetch_add(1, Ordering::SeqCst);
            Err(Error::Failed(redwax_resolve::Failure {
                kind: redwax_resolve::FailureKind::AuthenticationRejected,
                evidence: "synthetic rejection".to_owned(),
            }))
        };
        assert!(resolve_with_refresh(first, async { true }, native).await.is_err());
        assert_eq!(full_chains.load(Ordering::SeqCst), 1);
        assert_eq!(native_retries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_cookie_refresh_does_not_repeat_resolution() {
        let first = async {
            Err(Error::Failed(redwax_resolve::Failure {
                kind: redwax_resolve::FailureKind::AuthenticationRejected,
                evidence: "synthetic rejection".to_owned(),
            }))
        };
        let result = resolve_with_refresh(first, async { false }, async {
            panic!("failed cookie refresh must not repeat resolution")
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn content_restrictions_and_transport_failures_do_not_import_firefox() {
        for kind in [
            redwax_resolve::FailureKind::ContentRestricted,
            redwax_resolve::FailureKind::Transport,
            redwax_resolve::FailureKind::MissingCredentials,
        ] {
            let result = resolve_with_refresh(
                async {
                    Err(Error::Failed(redwax_resolve::Failure {
                        kind,
                        evidence: "fixture".to_owned(),
                    }))
                },
                async { panic!("unrelated failures must not refresh credentials") },
                async { panic!("unrelated failures must not repeat resolution") },
            )
            .await;
            assert!(matches!(result, Err(Error::Failed(failure)) if failure.kind == kind));
        }
    }

    #[tokio::test]
    async fn a_failed_refresh_retry_preserves_the_original_failure() {
        let result = resolve_with_refresh(
            async {
                Err(Error::Failed(redwax_resolve::Failure {
                    kind: redwax_resolve::FailureKind::AuthenticationRejected,
                    evidence: "synthetic rejection".to_owned(),
                }))
            },
            async { true },
            async { Err(Error::Io(std::io::Error::other("temporary network failure"))) },
        )
        .await;
        assert!(
            matches!(result, Err(Error::Failed(failure)) if failure.kind == redwax_resolve::FailureKind::AuthenticationRejected && failure.evidence == "synthetic rejection")
        );
    }
}
