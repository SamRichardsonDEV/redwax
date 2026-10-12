//! Stream resolution: a video id in, the best playable location out, fast.
//!
//! Chain order per track, first success wins:
//! 1. `LocalLibrary`: a file already downloaded.
//! 2. the prefetch cache: a URL resolved earlier and not near expiry.
//! 3. `NativeSignedIn`: one player request as the `WEB_REMIX` client with the
//!    account cookies, cipher and n solved by yt-dlp's solver library under
//!    node. 774, 141, 251, 140.
//! 4. `NativeAnonymous`: an anonymous client, no cookies, plain URLs, 251 or
//!    140. Starts standard audio before subprocess recovery.
//! 5. `YtDlp`: subprocess with cookies, node solver, token provider. About 4 s.

mod chain;
mod library;
mod trace;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use library::LibraryIndex;
pub use trace::RuntimeSink;

/// Audio encodes YouTube serves, best first. The discriminant is the itag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Tier {
    /// Premium opus, about 283 kbps, 48 kHz.
    Opus283 = 774,
    /// Premium AAC, 256 kbps.
    Aac256 = 141,
    /// Standard opus, about 130 to 160 kbps.
    Opus160 = 251,
    /// Standard AAC, 128 kbps.
    Aac128 = 140,
}

impl Tier {
    pub fn itag(self) -> u32 {
        self as u32
    }

    pub fn from_itag(itag: u32) -> Option<Tier> {
        match itag {
            774 => Some(Tier::Opus283),
            141 => Some(Tier::Aac256),
            251 => Some(Tier::Opus160),
            140 => Some(Tier::Aac128),
            _ => None,
        }
    }

    pub fn is_high(self) -> bool {
        matches!(self, Tier::Opus283 | Tier::Aac256)
    }

    /// Short label for the player bar, for example `774 opus 283k`.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Opus283 => "774 opus 283k",
            Tier::Aac256 => "141 aac 256k",
            Tier::Opus160 => "251 opus 160k",
            Tier::Aac128 => "140 aac 128k",
        }
    }

    /// Preference order for the picker, best first.
    pub const PREFERRED: [Tier; 4] = [Tier::Opus283, Tier::Aac256, Tier::Opus160, Tier::Aac128];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Source {
    LocalFile,
    Cache,
    NativeSignedIn,
    NativeAnonymous,
    YtDlp,
}

impl Source {
    /// The name the CLI prints and `scripts/measure.sh` reads.
    pub fn label(self) -> &'static str {
        match self {
            Source::LocalFile => "local",
            Source::Cache => "cache",
            Source::NativeSignedIn => "signed-in",
            Source::NativeAnonymous => "anonymous",
            Source::YtDlp => "yt-dlp",
        }
    }

    pub fn from_label(label: &str) -> Option<Source> {
        match label {
            "local" => Some(Source::LocalFile),
            "cache" => Some(Source::Cache),
            "signed-in" => Some(Source::NativeSignedIn),
            "anonymous" => Some(Source::NativeAnonymous),
            "yt-dlp" => Some(Source::YtDlp),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Location {
    File(PathBuf),
    Url(String),
}

impl std::fmt::Debug for Location {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Location::Url(_) => formatter.debug_tuple("Url").field(&"[redacted]").finish(),
        }
    }
}

impl Location {
    /// What mpv is given: the path, or the URL unchanged.
    pub fn as_mpv_target(&self) -> String {
        match self {
            Location::File(path) => path.to_string_lossy().into_owned(),
            Location::Url(url) => url.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub video_id: String,
    pub location: Location,
    pub tier: Tier,
    pub source: Source,
    /// From the URL's own `expire` parameter, else five hours from resolution.
    /// Files never expire.
    pub expires_at: SystemTime,
}

impl Stream {
    /// True while the stream has more than `STREAM_MARGIN` of life left.
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        self.expires_at
            .duration_since(now)
            .map(|remaining_lifetime| remaining_lifetime > STREAM_MARGIN)
            .unwrap_or(false)
    }
}

pub const STREAM_MARGIN: Duration = Duration::from_secs(15 * 60);
pub const STREAM_LIFETIME: Duration = Duration::from_secs(5 * 3600);

/// Seconds since the epoch, which is the clock every YouTube signature uses.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// When Google stops serving `url`: its own `expire` parameter, else a safe guess.
pub fn stream_expiry(url: &str, now: SystemTime) -> SystemTime {
    let expiration_seconds = url
        .split_once('?')
        .map(|(_, query)| query)
        .and_then(|query| {
            query
                .split('&')
                .find_map(|segment| segment.strip_prefix("expire=").map(str::to_owned))
        })
        .and_then(|expiration_text| expiration_text.parse::<u64>().ok());
    match expiration_seconds {
        Some(expiration_seconds) => SystemTime::UNIX_EPOCH + Duration::from_secs(expiration_seconds),
        None => now + STREAM_LIFETIME,
    }
}

/// mpv's `--ytdl-raw-options` form (`key=value,flag=`) as yt-dlp arguments,
/// so one config line reaches both mpv and chain step 5 unchanged.
pub fn raw_options_to_args(raw_options: &str) -> Vec<String> {
    let mut yt_dlp_arguments = Vec::new();
    for option in raw_options
        .split(',')
        .map(str::trim)
        .filter(|option| !option.is_empty())
    {
        let (option_name, option_value) = option.split_once('=').unwrap_or((option, ""));
        yt_dlp_arguments.push(format!("--{option_name}"));
        if !option_value.is_empty() {
            yt_dlp_arguments.push(option_value.to_owned());
        }
    }
    yt_dlp_arguments
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Allow one second for Premium before trying standard native audio.
    High,
    /// Allow 250 milliseconds for Premium before trying standard native audio.
    Fast,
}

#[derive(Debug, Clone)]
pub struct ResolverOptions {
    /// Netscape `cookies.txt` written by `redwax login`. `None` means anonymous only.
    pub cookies_path: Option<PathBuf>,
    /// Account selection shared with the music client. `None` disables credentials
    /// until `reload_credentials` installs a selection, even if cookies exist.
    pub identity: Option<redwax_auth::SelectedIdentity>,
    pub quality: Quality,
    /// Download library, searched first. `None` disables step 1.
    pub library_dir: Option<PathBuf>,
    /// Extra yt-dlp arguments for step 5, from `[mpv] ytdl_raw_options` in the config.
    pub ytdlp_args: Vec<String>,
    pub parallel: usize,
    /// Directory for the player script cache and the on-disk URL cache.
    pub cache_dir: PathBuf,
}

/// Privacy-safe paths and capabilities used by resolver diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverDiagnostics {
    pub node: PathBuf,
    pub ytdlp: PathBuf,
    pub solver_library: Option<PathBuf>,
    pub solver_core: Option<PathBuf>,
    pub proof_token_provider: Option<PathBuf>,
    pub site_cache: PathBuf,
    pub stream_cache: PathBuf,
    pub player_cache: PathBuf,
}

impl ResolverDiagnostics {
    /// Inspect the same environment and cache precedence as the resolver.
    pub fn inspect(cache_dir: &Path) -> ResolverDiagnostics {
        let cached_library = cache_dir.join("solver/lib.json");
        let cached_core = cache_dir.join("solver/core.js");
        ResolverDiagnostics {
            node: chain::solver::find_node(),
            ytdlp: PathBuf::from(chain::YtDlp::binary()),
            solver_library: first_solver_script([
                Some(cached_library),
                Some(chain::solver::yt_dlp_library()),
            ]),
            solver_core: first_solver_script([Some(cached_core), chain::solver::yt_dlp_core()]),
            proof_token_provider: chain::potoken::Provider::discover(chain::solver::find_node())
                .map(|provider| provider.server_home),
            site_cache: cache_dir.join("innertube.json"),
            stream_cache: cache_dir.join("streams.json"),
            player_cache: cache_dir.join("player"),
        }
    }

    pub fn solver_ready(&self) -> bool {
        self.solver_library.is_some() && self.solver_core.is_some()
    }
}

fn first_solver_script(candidates: impl IntoIterator<Item = Option<PathBuf>>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| chain::solver::script_file_has_code(candidate))
}

/// A stable reason that callers can act on without parsing diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    MissingCredentials,
    AuthenticationRejected,
    ContentRestricted,
    Unavailable,
    MissingAudio,
    CdnRefused,
    Transport,
    Dependency,
    Deadline,
    UnknownResponse,
}

impl FailureKind {
    fn label(self) -> &'static str {
        match self {
            FailureKind::MissingCredentials => "login material is missing",
            FailureKind::AuthenticationRejected => "authentication was rejected",
            FailureKind::ContentRestricted => "content is restricted",
            FailureKind::Unavailable => "content is unavailable",
            FailureKind::MissingAudio => "no audio stream was found",
            FailureKind::CdnRefused => "the media server refused the stream",
            FailureKind::Transport => "the network request failed",
            FailureKind::Dependency => "a resolver dependency failed",
            FailureKind::Deadline => "the operation reached its deadline",
            FailureKind::UnknownResponse => "the service returned an unknown response",
        }
    }
}

/// One sanitized failure from the source that understood it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub evidence: String,
}

impl Failure {
    pub(crate) fn new(kind: FailureKind, context: &str, detail: impl std::fmt::Display) -> Failure {
        Failure {
            kind,
            evidence: format!("{context}: {detail}"),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.kind.label(), self.evidence)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Failed(Failure),
    #[error("every resolver step failed for {video_id}")]
    Exhausted {
        video_id: String,
        failures: Vec<Failure>,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

const PLAYBACK_UNAVAILABLE: &str = "Unavailable on YouTube Music.";
const PLAYBACK_NO_AUDIO: &str = "No YouTube Music audio stream.";
const PLAYBACK_LOGIN: &str = "YouTube Music login required.";
const PLAYBACK_REJECTED: &str = "YouTube rejected the audio stream.";
const PLAYBACK_TRANSIENT: &str = "Could not reach YouTube Music. Retry.";
const PLAYBACK_UNKNOWN: &str = "Could not resolve YouTube Music.";

impl Error {
    pub fn kind(&self) -> FailureKind {
        match self {
            Error::Failed(failure) => failure.kind,
            Error::Exhausted { failures, .. } => preferred_failure(failures),
            Error::Io(_) => FailureKind::Transport,
        }
    }

    /// A playback-safe message that never includes upstream diagnostics.
    pub fn playback_message(&self) -> &'static str {
        match self.kind() {
            FailureKind::MissingCredentials | FailureKind::AuthenticationRejected => PLAYBACK_LOGIN,
            FailureKind::ContentRestricted | FailureKind::Unavailable => PLAYBACK_UNAVAILABLE,
            FailureKind::MissingAudio => PLAYBACK_NO_AUDIO,
            FailureKind::CdnRefused => PLAYBACK_REJECTED,
            FailureKind::Transport | FailureKind::Deadline => PLAYBACK_TRANSIENT,
            FailureKind::Dependency | FailureKind::UnknownResponse => PLAYBACK_UNKNOWN,
        }
    }
}

fn preferred_failure(failures: &[Failure]) -> FailureKind {
    for preferred in [
        FailureKind::AuthenticationRejected,
        FailureKind::MissingCredentials,
        FailureKind::ContentRestricted,
        FailureKind::Unavailable,
        FailureKind::CdnRefused,
        FailureKind::Transport,
        FailureKind::Deadline,
        FailureKind::MissingAudio,
        FailureKind::Dependency,
        FailureKind::UnknownResponse,
    ] {
        if failures.iter().any(|failure| failure.kind == preferred) {
            return preferred;
        }
    }
    FailureKind::UnknownResponse
}

pub type Result<T> = std::result::Result<T, Error>;

/// One report line per step from `Resolver::health_check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepHealth {
    pub step: Source,
    pub ok: bool,
    pub elapsed: Duration,
    pub failure: Option<FailureKind>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Success { status: Option<u16> },
    Missing,
    Failed(FailureKind),
}

/// One step's report line: what it was asked, how long it took, what it gave.
/// The CLI prints these, and `scripts/measure.sh` reads the elapsed column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub source: Source,
    pub tier: Option<Tier>,
    pub elapsed: Duration,
    pub outcome: AttemptOutcome,
    /// Sanitized source evidence. Stream locations remain on `Stream`.
    pub evidence: String,
}

/// The resolver. Cheap to clone, shares the cache and the in-flight table.
///
/// `resolve` for the same id from two tasks joins one request. A failed
/// resolution is forgotten so the next call retries. The URL cache lives in
/// memory and is written to `cache_dir/streams.json` on `flush`, so a relaunch
/// within a URL's life plays at once.
#[derive(Clone)]
pub struct Resolver {
    inner: std::sync::Arc<chain::Inner>,
}

impl Resolver {
    pub fn new(resolver_options: ResolverOptions) -> Resolver {
        Resolver {
            inner: std::sync::Arc::new(chain::Inner::new(resolver_options)),
        }
    }

    /// Install the timing sink used only by this resolver and its clones.
    pub fn set_runtime_sink(&self, runtime_sink: Option<std::sync::Arc<RuntimeSink>>) {
        self.inner.set_runtime_sink(runtime_sink);
    }

    /// Record detailed phase timings for a later `take_trace` call.
    pub fn enable_trace(&self) {
        self.inner.enable_cli_trace();
    }

    /// Return this resolver's recorded phase timings and clear its log.
    pub fn take_trace(&self) -> Vec<(String, Duration)> {
        self.inner.take_cli_trace()
    }

    /// Publish a new account selection and forget state derived from older credentials.
    /// `None` disables signed-in resolution until a later replacement is published.
    pub async fn reload_credentials(&self, selected_identity: Option<redwax_auth::SelectedIdentity>) {
        self.inner.reload_credentials(selected_identity).await;
    }

    /// A fresh cached stream, or `None`.
    pub fn cached(&self, video_id: &str) -> Option<Stream> {
        self.inner.cached(video_id)
    }

    pub fn forget(&self, video_id: &str) {
        self.inner.forget(video_id)
    }

    /// Warm available streams without queuing ahead of foreground playback.
    /// Cached and in-flight IDs are skipped, with one slot reserved for playback.
    pub fn prefetch(&self, video_ids: impl IntoIterator<Item = String>) {
        self.inner.prefetch(video_ids)
    }

    /// The chain, first success wins. Never returns a stale cache entry.
    pub async fn resolve(&self, video_id: &str) -> Result<Stream> {
        self.inner.resolve(video_id).await
    }

    /// Resolve only when background capacity is available, preserving a playback slot.
    /// Returns `None` when busy or unavailable. Cached streams are always returned.
    pub async fn resolve_prefetch(&self, video_id: &str) -> Option<Stream> {
        self.inner.resolve_prefetch(video_id).await
    }

    /// The chain with one report line per step, for the CLI and for
    /// measurement. `selected_source` runs a single step instead of the chain.
    pub async fn resolve_traced(
        &self,
        video_id: &str,
        selected_source: Option<Source>,
    ) -> (Result<Stream>, Vec<Attempt>) {
        self.inner.resolve_traced(video_id, selected_source).await
    }

    /// Resolve step 3 for a known track so a dead identity costs one failed
    /// request at launch, not an evening of slow starts.
    pub async fn health_check(&self, video_id: &str) -> Vec<StepHealth> {
        self.inner.health_check(video_id).await
    }

    /// Download `video_id` at the best tier to `destination_directory`,
    /// tagged, through yt-dlp (step 5, which also writes metadata). Returns the
    /// file written.
    pub async fn download(&self, video_id: &str, destination_directory: &Path) -> Result<PathBuf> {
        self.inner.download(video_id, destination_directory).await
    }

    /// Write the URL cache to disk. Called on exit and every few minutes.
    pub fn flush(&self) -> std::io::Result<()> {
        self.inner.flush()
    }

    /// Write the scrubbed player response for `video_id`, which is how the
    /// fixtures under `tests/fixtures` are captured.
    pub async fn dump_player_response(&self, video_id: &str, output_path: &Path) -> Result<()> {
        self.inner.dump_player_response(video_id, output_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_reject_empty_or_invalid_solver_cache_envelopes() {
        let directory = std::env::temp_dir().join(format!("redwax-empty-solver-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let invalid = directory.join("invalid.json");
        let valid = directory.join("valid.js");
        std::fs::write(&invalid, r#"{"data":{"code":""}}"#).unwrap();
        std::fs::write(&valid, "const solver = {};").unwrap();
        assert_eq!(first_solver_script([Some(invalid.clone())]), None);
        assert_eq!(
            first_solver_script([Some(invalid), Some(valid.clone())]),
            Some(valid)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn test_expiry_and_raw_option_parsing() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            stream_expiry("https://cdn/x?expire=1700000000&id=1", now),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        assert!(stream_expiry("https://cdn/x", now) > now);
        assert_eq!(
            raw_options_to_args("cookies=/c.txt,no-check-certificates=,x=1"),
            ["--cookies", "/c.txt", "--no-check-certificates", "--x", "1"]
        );
        assert!(raw_options_to_args("").is_empty());
    }

    #[test]
    fn stream_expiry_falls_back_to_five_hours() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            stream_expiry("https://host/videoplayback?itag=774", now),
            now + STREAM_LIFETIME
        );
        assert_eq!(stream_expiry("https://host/plain", now), now + STREAM_LIFETIME);
        assert_eq!(
            stream_expiry("https://host/x?expire=soon", now),
            now + STREAM_LIFETIME
        );
    }

    #[test]
    fn a_stream_is_fresh_only_outside_the_margin() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let make = |left: Duration| Stream {
            video_id: "abc".to_owned(),
            location: Location::Url("https://host/x".to_owned()),
            tier: Tier::Opus283,
            source: Source::NativeSignedIn,
            expires_at: now + left,
        };
        assert!(make(STREAM_MARGIN + Duration::from_secs(1)).is_fresh(now));
        assert!(!make(STREAM_MARGIN).is_fresh(now));
        assert!(!make(Duration::ZERO).is_fresh(now));
    }

    #[test]
    fn stream_debug_redacts_signed_urls() {
        let stream = Stream {
            video_id: "abc".to_owned(),
            location: Location::Url("https://cdn.invalid/audio?sig=private".to_owned()),
            tier: Tier::Opus283,
            source: Source::NativeSignedIn,
            expires_at: SystemTime::UNIX_EPOCH,
        };

        let debug = format!("{stream:?}");
        assert!(!debug.contains("private"));
        assert!(!debug.contains("cdn.invalid"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn tiers_order_best_first_and_name_the_premium_pair() {
        assert_eq!(Tier::PREFERRED[0].itag(), 774);
        assert_eq!(Tier::from_itag(141), Some(Tier::Aac256));
        assert_eq!(Tier::from_itag(999), None);
        assert!(Tier::Opus283.is_high() && Tier::Aac256.is_high());
        assert!(!Tier::Opus160.is_high() && !Tier::Aac128.is_high());
    }

    #[test]
    fn every_source_round_trips_through_its_label() {
        for source in [
            Source::LocalFile,
            Source::Cache,
            Source::NativeSignedIn,
            Source::NativeAnonymous,
            Source::YtDlp,
        ] {
            assert_eq!(Source::from_label(source.label()), Some(source));
        }
    }

    #[test]
    fn diagnostics_prefer_cache_assets_without_exposing_resolver_internals() {
        let dir = std::env::temp_dir().join(format!(
            "redwax-resolver-diagnostics-{}-{}",
            std::process::id(),
            unix_now()
        ));
        std::fs::create_dir_all(dir.join("solver")).expect("create diagnostics fixture");
        std::fs::write(dir.join("solver/lib.json"), "library").expect("write library fixture");
        std::fs::write(dir.join("solver/core.js"), "core").expect("write core fixture");

        let diagnostics = ResolverDiagnostics::inspect(&dir);
        let library = dir.join("solver/lib.json");
        let core = dir.join("solver/core.js");
        assert_eq!(diagnostics.solver_library, Some(library));
        assert_eq!(diagnostics.solver_core, Some(core));
        assert!(diagnostics.solver_ready());
        assert_eq!(diagnostics.site_cache, dir.join("innertube.json"));
        assert_eq!(diagnostics.stream_cache, dir.join("streams.json"));
        assert_eq!(diagnostics.player_cache, dir.join("player"));

        std::fs::remove_dir_all(&dir).expect("remove diagnostics fixture");
    }

    #[test]
    fn playback_message_distinguishes_unavailable_video_from_missing_audio() {
        let unavailable = Error::Failed(Failure::new(
            FailureKind::Unavailable,
            "player",
            "status=UNPLAYABLE",
        ));
        let no_audio = Error::Exhausted {
            video_id: "abc".to_owned(),
            failures: vec![
                Failure::new(FailureKind::MissingAudio, "anonymous", "no stream"),
                Failure::new(FailureKind::MissingAudio, "yt-dlp", "no format"),
            ],
        };

        assert_eq!(unavailable.playback_message(), "Unavailable on YouTube Music.");
        assert_eq!(no_audio.playback_message(), "No YouTube Music audio stream.");
    }

    #[test]
    fn playback_message_distinguishes_rejected_stream_from_unknown_failure() {
        let rejected = Error::Exhausted {
            video_id: "abc".to_owned(),
            failures: vec![Failure::new(
                FailureKind::CdnRefused,
                "signed-in",
                "status=403 itag=774",
            )],
        };
        let unknown = Error::Failed(Failure::new(
            FailureKind::UnknownResponse,
            "player",
            "status=MISSING",
        ));

        assert_eq!(rejected.playback_message(), "YouTube rejected the audio stream.");
        assert_eq!(unknown.playback_message(), "Could not resolve YouTube Music.");
    }

    #[test]
    fn playback_message_uses_only_the_typed_failure() {
        let error = Error::Exhausted {
            video_id: "abc".to_owned(),
            failures: vec![Failure::new(
                FailureKind::UnknownResponse,
                "player",
                "valid_json=false",
            )],
        };

        assert_eq!(error.playback_message(), "Could not resolve YouTube Music.");
    }

    #[test]
    fn observed_rejection_outranks_missing_material_in_an_exhausted_chain() {
        let error = Error::Exhausted {
            video_id: "abc".to_owned(),
            failures: vec![
                Failure::new(FailureKind::MissingCredentials, "fallback", "cookies missing"),
                Failure::new(FailureKind::AuthenticationRejected, "signed-in", "status=403"),
            ],
        };

        assert_eq!(error.kind(), FailureKind::AuthenticationRejected);
    }

    #[test]
    fn playback_message_ignores_local_misses_and_prefers_network_failure() {
        let error = Error::Exhausted {
            video_id: "abc".to_owned(),
            failures: vec![
                Failure::new(FailureKind::MissingAudio, "local", "no stream"),
                Failure::new(FailureKind::MissingAudio, "cache", "no fresh entry"),
                Failure::new(FailureKind::Transport, "anonymous", "connection failed"),
                Failure::new(FailureKind::MissingAudio, "yt-dlp", "no stream"),
            ],
        };

        assert_eq!(error.playback_message(), "Could not reach YouTube Music. Retry.");
    }
}

#[cfg(test)]
#[path = "tests/live.rs"]
mod live_tests;
