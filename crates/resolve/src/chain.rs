//! Resolution shares work per video and reserves capacity for foreground playback.

pub mod solver;

mod auth;
pub(crate) mod innertube;
pub(crate) mod potoken;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use redwax_auth::{CookieJar, SelectedIdentity};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::{
    Attempt, AttemptOutcome, Error, Failure, FailureKind, LibraryIndex, Location, Quality, ResolverOptions,
    Result, Source, StepHealth, Stream, Tier, stream_expiry, unix_now,
};

pub(crate) struct CheckedStream {
    stream: Stream,
    status: Option<u16>,
}

impl CheckedStream {
    fn is_playable(&self) -> bool {
        self.status.is_none_or(|status| (200..300).contains(&status))
    }
}

const NATIVE_COOLDOWN: Duration = Duration::from_secs(30);
const ANONYMOUS_BUDGET: Duration = Duration::from_secs(2);
const PLAYBACK_BUDGET: Duration = Duration::from_secs(12);

#[derive(Default)]
struct NativeRecovery {
    retry_at: Mutex<Option<Instant>>,
    repair_after: Mutex<Option<Instant>>,
}

impl NativeRecovery {
    fn reset(&self) {
        *self.retry_at.lock().expect("native recovery holds no await") = None;
        *self.repair_after.lock().expect("native recovery holds no await") = None;
    }

    fn admit(&self, now: Instant) -> bool {
        let mut retry_at = self.retry_at.lock().expect("native recovery holds no await");
        match *retry_at {
            None => true,
            Some(deadline) if now < deadline => false,
            Some(_) => {
                // Reserve the recovery probe so queued tracks do not retry together.
                *retry_at = Some(now + NATIVE_COOLDOWN);
                true
            }
        }
    }

    fn observe(&self, resolution_outcome: &Result<Option<CheckedStream>>, now: Instant) {
        let mut retry_at = self.retry_at.lock().expect("native recovery holds no await");
        match resolution_outcome {
            Ok(Some(verified_stream)) if verified_stream.is_playable() => *retry_at = None,
            Ok(Some(verified_stream)) if verified_stream.status == Some(403) => {
                *retry_at = Some(now + NATIVE_COOLDOWN)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    FailureKind::Transport | FailureKind::Deadline | FailureKind::CdnRefused
                ) =>
            {
                *retry_at = Some(now + NATIVE_COOLDOWN);
            }
            _ => {}
        }
    }

    fn repair_due(&self, now: Instant) -> bool {
        if self
            .retry_at
            .lock()
            .expect("native recovery holds no await")
            .is_none()
        {
            return false;
        }
        let mut repair_after = self.repair_after.lock().expect("native recovery holds no await");
        if repair_after.is_some_and(|deadline| now < deadline) {
            return false;
        }
        *repair_after = Some(now + NATIVE_COOLDOWN);
        true
    }

    async fn run(
        &self,
        resolution_work: impl Future<Output = Result<Option<CheckedStream>>>,
        timeout_budget: Duration,
    ) -> Result<Option<CheckedStream>> {
        if !self.admit(Instant::now()) {
            crate::trace::record_runtime_evidence("native.cooldown", Duration::ZERO, "retry=false");
            return Err(innertube::failed(
                FailureKind::Deadline,
                "native resolution",
                "temporary recovery cooldown",
            ));
        }
        let resolution_outcome = match tokio::time::timeout(timeout_budget, resolution_work).await {
            Ok(resolution_outcome) => resolution_outcome,
            Err(_) => {
                crate::trace::record_runtime("native.deadline", timeout_budget);
                Err(innertube::failed(
                    FailureKind::Deadline,
                    "native resolution",
                    "playback deadline exceeded",
                ))
            }
        };
        self.observe(&resolution_outcome, Instant::now());
        resolution_outcome
    }
}

fn native_playback_budget(quality: Quality) -> Duration {
    match quality {
        Quality::High => Duration::from_secs(1),
        Quality::Fast => Duration::from_millis(250),
    }
}

async fn bounded_playback<T>(resolution_work: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(PLAYBACK_BUDGET, resolution_work)
        .await
        .map_err(|_| {
            innertube::failed(
                FailureKind::Deadline,
                "playback resolution",
                "12 second deadline exceeded",
            )
        })?
}

/// One way of turning a video id into a stream.
pub(crate) trait Step: Send + Sync {
    fn source(&self) -> Source;

    /// `Ok(None)` means "not here, try the next step" and is not an error.
    fn resolve(&self, video_id: &str) -> impl Future<Output = Result<Option<Stream>>> + Send;

    fn resolve_checked(&self, video_id: &str) -> impl Future<Output = Result<Option<CheckedStream>>> + Send {
        async move {
            let Some(stream) = self.resolve(video_id).await? else {
                return Ok(None);
            };
            if matches!(&stream.location, Location::File(_)) {
                return Ok(Some(CheckedStream { stream, status: None }));
            }
            let mut verified_stream = check_stream(stream, self.user_agent()).await?;
            if verified_stream.status != Some(403) {
                return Ok(Some(verified_stream));
            }
            if let Some(retried) =
                crate::trace::timed("step.token_retry", self.retry_with_token(video_id)).await?
            {
                verified_stream = check_stream(retried, self.user_agent()).await?;
            }
            Ok(Some(verified_stream))
        }
    }

    /// The first token try after the CDN refused the unsigned URL.
    /// A step with no token to offer returns `Ok(None)`.
    fn retry_with_token(&self, video_id: &str) -> impl Future<Output = Result<Option<Stream>>> + Send {
        async move {
            let _ = video_id;
            Ok(None)
        }
    }

    /// The agent the CDN is asked with, which must match the identity that
    /// obtained the URL.
    fn user_agent(&self) -> &'static str {
        innertube::WEB_REMIX.user_agent
    }
}

/// Step 1: a file in the download library named by video id.
pub struct LocalLibrary {
    pub dir: PathBuf,
}

/// Step 3: a paired `WEB_REMIX` player request and signature solver.
/// It verifies the result and owns the bounded token and stale-context retries.
pub struct NativeSignedIn {
    pub cookies_path: PathBuf,
    pub solver: solver::NodeSolver,
    credentials: Arc<CredentialClock>,
    selection: std::sync::RwLock<CredentialSelection>,
    site: Arc<innertube::SiteState>,
    context_cache: tokio::sync::Mutex<Option<SignedInContext>>,
}

#[derive(Clone)]
struct CredentialSelection {
    credential_generation: CredentialGeneration,
    identity: Option<SelectedIdentity>,
}

struct CredentialSnapshot {
    credential_generation: CredentialGeneration,
    selected_identity: Option<SelectedIdentity>,
    cookie_jar: CookieJar,
}

const SIGNED_IN_CONTEXT_LIFETIME: Duration = Duration::from_secs(5 * 60);
const SIGNED_IN_BUDGET: Duration = Duration::from_secs(20);
const ACCOUNT_PAGE_BUDGET: Duration = Duration::from_secs(2);
const CLIENT_VERSION_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct SignedInContext {
    credential_generation: CredentialGeneration,
    auth_context: Arc<auth::AuthContext>,
    signed_in_page: Option<innertube::SignedInPage>,
    player_script: Arc<solver::PlayerScript>,
    signature_timestamp: u32,
    client_version: String,
    fetched_at: Instant,
}

enum NativeResolutionCycle {
    Complete(Option<CheckedStream>),
    Refresh,
}

enum StreamBuildError {
    Solver(Error),
    Other(Error),
}

fn solver_failure(context: &str, error: solver::SolverError) -> Error {
    let kind = match &error {
        solver::SolverError::Deadline => FailureKind::Deadline,
        _ => FailureKind::Dependency,
    };
    innertube::failed(kind, context, error)
}

fn credentials_changed() -> Error {
    innertube::failed(
        FailureKind::MissingCredentials,
        "credentials",
        "generation changed during the request",
    )
}

/// Step 4: the same POST as an anonymous client, no cookies. Its URLs come
/// plain, so no solve is needed. Measured 0.16 to 0.20 s for the request.
pub struct NativeAnonymous {
    site: Arc<innertube::SiteState>,
}

/// Step 5: yt-dlp subprocess, cookies plus `--js-runtimes node --remote-components ejs:github`.
pub struct YtDlp {
    pub cookies_path: Option<PathBuf>,
    pub extra_args: Vec<String>,
    pub timeout: Duration,
    credentials: Arc<CredentialClock>,
}

pub struct Inner {
    resolver_state: Arc<ResolverState>,
}

/// A file on disk outlives any session, so step 1 gives it a date no run reaches.
const NEVER: Duration = Duration::from_secs(4_102_444_800);

/// What a joined caller receives: the stream, or the reasons every step gave.
type ResolutionOutcome = std::result::Result<Stream, Vec<Failure>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CredentialGeneration(u64);

struct CredentialClock {
    encoded_generation_and_enabled: AtomicU64,
}

impl CredentialClock {
    fn new(signed_in_enabled: bool) -> CredentialClock {
        CredentialClock {
            encoded_generation_and_enabled: AtomicU64::new(u64::from(signed_in_enabled)),
        }
    }
    fn current(&self) -> CredentialGeneration {
        CredentialGeneration(self.encoded_generation_and_enabled.load(Ordering::Acquire) >> 1)
    }

    fn advance(&self, signed_in_enabled: bool) -> CredentialGeneration {
        let mut observed_state = self.encoded_generation_and_enabled.load(Ordering::Acquire);
        loop {
            let credential_generation = (observed_state >> 1) + 1;
            let next_state = (credential_generation << 1) | if signed_in_enabled { 1 } else { 0 };
            match self.encoded_generation_and_enabled.compare_exchange_weak(
                observed_state,
                next_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return CredentialGeneration(credential_generation),
                Err(actual_state) => observed_state = actual_state,
            }
        }
    }

    fn is_current(&self, credential_generation: CredentialGeneration) -> bool {
        self.current() == credential_generation
    }

    fn enabled(&self) -> bool {
        self.encoded_generation_and_enabled.load(Ordering::Acquire) & 1 == 1
    }
}

fn credential_bound(stream: &Stream) -> bool {
    matches!(&stream.location, Location::Url(_)) && stream.source != Source::NativeAnonymous
}

// ------------------------------------------------------------------ step 1

fn tier_of_file(path: &Path) -> Tier {
    // Downloads are remuxed to webm or m4a by step 5, which asks for the best
    // encode the track has, so the container names the family, not the bitrate.
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
    {
        "m4a" | "mp4" | "aac" => Tier::Aac256,
        _ => Tier::Opus283,
    }
}

impl Step for LocalLibrary {
    fn source(&self) -> Source {
        Source::LocalFile
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Stream>> {
        let library_directory = self.dir.clone();
        let video_id = video_id.to_owned();
        let requested_video_id = video_id.clone();
        let library_path = tokio::task::spawn_blocking(move || {
            LibraryIndex::scan(&library_directory)
                .get(&requested_video_id)
                .map(Path::to_owned)
        })
        .await
        .map_err(|error| innertube::failed(FailureKind::Dependency, "library search", error))?;
        Ok(library_path.map(|audio_path| Stream {
            video_id,
            tier: tier_of_file(&audio_path),
            location: Location::File(audio_path),
            source: Source::LocalFile,
            expires_at: SystemTime::UNIX_EPOCH + NEVER,
        }))
    }
}

// ------------------------------------------------------------------ step 3

impl NativeSignedIn {
    fn ensure_context_current<T>(
        &self,
        expected_credential_generation: CredentialGeneration,
        value: T,
    ) -> Result<T> {
        self.credentials
            .is_current(expected_credential_generation)
            .then_some(value)
            .ok_or_else(credentials_changed)
    }

    fn publish_credentials(
        &self,
        credential_generation: CredentialGeneration,
        identity: Option<SelectedIdentity>,
    ) {
        *self
            .selection
            .write()
            .expect("credential selection is never held across a request") = CredentialSelection {
            credential_generation,
            identity,
        };
    }

    async fn clear_context(&self) {
        *self.context_cache.lock().await = None;
    }

    async fn credentials(&self) -> Result<CredentialSnapshot> {
        let selection = self
            .selection
            .read()
            .expect("credential selection is never held across a request")
            .clone();
        if !self.credentials.enabled() {
            return Err(innertube::failed(
                FailureKind::MissingCredentials,
                "cookies",
                "session is disabled",
            ));
        }
        let netscape_cookie_text = tokio::fs::read_to_string(&self.cookies_path)
            .await
            .map_err(|error| {
                innertube::failed(
                    FailureKind::MissingCredentials,
                    "cookies",
                    match error.kind() {
                        std::io::ErrorKind::NotFound => "file is missing",
                        std::io::ErrorKind::PermissionDenied => "file is not readable",
                        _ => "file could not be read",
                    },
                )
            })?;
        if !self.credentials.is_current(selection.credential_generation) {
            return Err(credentials_changed());
        }
        Ok(CredentialSnapshot {
            credential_generation: selection.credential_generation,
            selected_identity: selection.identity,
            cookie_jar: CookieJar::from_netscape(&netscape_cookie_text),
        })
    }

    async fn context(
        &self,
        force_refresh: bool,
        previously_observed_fetch_time: Option<Instant>,
    ) -> Result<SignedInContext> {
        let CredentialSnapshot {
            credential_generation,
            selected_identity,
            cookie_jar,
        } = self.credentials().await?;
        let selected_identity = selected_identity.as_ref();
        let mut cached_context = self.context_cache.lock().await;
        if let Some(existing_context) = cached_context.as_ref()
            && existing_context.credential_generation == credential_generation
            && existing_context.auth_context.matches(
                &cookie_jar,
                selected_identity.map(SelectedIdentity::authuser),
                selected_identity.and_then(SelectedIdentity::page_id),
            )
            && existing_context.fetched_at.elapsed() < SIGNED_IN_CONTEXT_LIFETIME
            && (!force_refresh
                || previously_observed_fetch_time.is_none()
                || previously_observed_fetch_time != Some(existing_context.fetched_at))
        {
            return Ok(existing_context.clone());
        }

        let mut auth_context = auth::AuthContext::new(
            cookie_jar,
            selected_identity.map(SelectedIdentity::authuser),
            selected_identity.and_then(SelectedIdentity::page_id),
            unix_now(),
        )?;
        let signed_in_page =
            match tokio::time::timeout(ACCOUNT_PAGE_BUDGET, innertube::signed_in_page(&auth_context)).await {
                Ok(Ok(signed_in_page)) => Some(signed_in_page),
                Ok(Err(_)) => None,
                Err(_) => {
                    crate::trace::record_runtime_evidence(
                        "native.context",
                        Duration::ZERO,
                        "account_page=timeout",
                    );
                    None
                }
            };
        if let Some(signed_in_page) = signed_in_page.as_ref() {
            auth_context = auth_context.with_page(&signed_in_page.auth)?;
        }
        let player_url = signed_in_page
            .as_ref()
            .and_then(|page| page.player_url.as_deref())
            .unwrap_or_default();
        let player_script = if force_refresh {
            self.solver.refresh_player_script_at(player_url).await
        } else {
            self.solver.player_script(player_url).await
        }
        .map_err(|error| solver_failure("player script", error))?;
        let signature_timestamp = player_script
            .signature_timestamp()
            .map_err(|error| solver_failure("signature timestamp", error))?;
        let live_client_version = signed_in_page
            .as_ref()
            .and_then(|page| page.client_version.clone());
        let client_version = live_client_version
            .clone()
            .unwrap_or_else(|| self.site.music_version());
        if let Some(version) = live_client_version {
            self.site.store_value("web_remix_version", &version);
        }
        let signed_in_context = SignedInContext {
            credential_generation,
            auth_context: Arc::new(auth_context),
            signed_in_page,
            player_script: Arc::new(player_script),
            signature_timestamp,
            client_version,
            fetched_at: Instant::now(),
        };
        if !self
            .credentials
            .is_current(signed_in_context.credential_generation)
        {
            return Err(credentials_changed());
        }
        *cached_context = Some(signed_in_context.clone());
        Ok(signed_in_context)
    }

    async fn player_with_version(
        &self,
        video_id: &str,
        signed_in_context: &SignedInContext,
        client_version: &str,
    ) -> Result<serde_json::Value> {
        let player_response = innertube::player(innertube::PlayerRequest {
            identity: &innertube::WEB_REMIX,
            client_version,
            video_id,
            auth: Some(&signed_in_context.auth_context),
            visitor_id: None,
            signature_timestamp: Some(signed_in_context.signature_timestamp),
            po_token: None,
        })
        .await?;
        let evidence = innertube::player_response_evidence(&player_response, client_version, false);
        crate::trace::record_runtime_evidence("native.response", Duration::ZERO, &evidence);
        Ok(player_response)
    }

    async fn player(&self, video_id: &str, signed_in_context: &SignedInContext) -> Result<serde_json::Value> {
        let can_refresh_client_version = signed_in_context
            .signed_in_page
            .as_ref()
            .and_then(|page| page.client_version.as_ref())
            .is_none();
        let first_request_result = self
            .player_with_version(video_id, signed_in_context, &signed_in_context.client_version)
            .await;
        let original_error = match first_request_result {
            Ok(player_response) => match innertube::playability(&player_response, video_id) {
                Ok(()) => return Ok(player_response),
                Err(error)
                    if can_refresh_client_version
                        && innertube::can_refresh_client_version(&player_response) =>
                {
                    error
                }
                Err(error) => return Err(error),
            },
            Err(error) if can_refresh_client_version => error,
            Err(error) => return Err(error),
        };
        let refreshed_version =
            tokio::time::timeout(CLIENT_VERSION_BUDGET, innertube::refresh_music_version())
                .await
                .ok()
                .flatten();
        let Some(version) = refreshed_version.filter(|version| version != &signed_in_context.client_version)
        else {
            return Err(original_error);
        };
        let retry_result = self
            .player_with_version(video_id, signed_in_context, &version)
            .await
            .and_then(|player_response| {
                innertube::playability(&player_response, video_id)?;
                Ok(player_response)
            });
        let Ok(player_response) = retry_result else {
            return Err(original_error);
        };
        let mut cached_context = self.context_cache.lock().await;
        let version_was_published = if self
            .credentials
            .is_current(signed_in_context.credential_generation)
            && let Some(existing_context) = cached_context.as_mut()
            && existing_context.credential_generation == signed_in_context.credential_generation
            && existing_context.fetched_at == signed_in_context.fetched_at
        {
            existing_context.client_version.clone_from(&version);
            true
        } else {
            false
        };
        drop(cached_context);
        if version_was_published {
            self.site.store_value("web_remix_version", &version);
        }
        Ok(player_response)
    }

    fn token_bindings(
        video_id: &str,
        player_response: &serde_json::Value,
        signed_in_page: Option<&innertube::SignedInPage>,
    ) -> Vec<(&'static str, String)> {
        let account_binding = signed_in_page
            .and_then(|page| page.auth.data_sync_id().map(str::to_owned))
            .or_else(|| innertube::datasync_id(player_response));
        let video_binding = ("video", video_id.to_owned());
        match (signed_in_page.map(|page| page.video_token), account_binding) {
            (Some(true), Some(account_binding)) => {
                vec![video_binding, ("account", account_binding)]
            }
            (_, Some(account_binding)) => vec![("account", account_binding), video_binding],
            (_, None) => vec![video_binding],
        }
    }

    async fn stream(
        &self,
        video_id: &str,
        player_response: &serde_json::Value,
        player_script: &solver::PlayerScript,
    ) -> std::result::Result<Option<Stream>, StreamBuildError> {
        let Some(audio_format) = innertube::best_audio(player_response) else {
            crate::trace::record_runtime_evidence(
                "native.format_missing",
                Duration::ZERO,
                "supported_audio=false",
            );
            return Ok(None);
        };
        let delivery = match (audio_format.url.is_some(), audio_format.cipher.is_some()) {
            (true, _) => "url",
            (false, true) => "cipher",
            (false, false) => "missing",
        };
        let base_stream_url = innertube::base_url(&audio_format).ok_or_else(|| {
            StreamBuildError::Other(innertube::failed(
                FailureKind::MissingAudio,
                video_id,
                "the chosen format carries no URL",
            ))
        })?;

        let mut solve_request = solver::SolveRequest::default();
        if let Some(signature_cipher) = audio_format.cipher.as_deref().and_then(innertube::parse_cipher) {
            solve_request.signatures.push(signature_cipher.s);
        }
        if let Some(n_challenge) = innertube::query_value(&base_stream_url, "n") {
            solve_request.n_params.push(n_challenge);
        }
        let (solved_signature, solved_n_parameter) = if solve_request.signatures.is_empty()
            && solve_request.n_params.is_empty()
        {
            (None, None)
        } else {
            let signature_challenges = solve_request.signatures.len();
            let n_challenges = solve_request.n_params.len();
            let player_id = player_script.id();
            let player_id =
                match player_id.len() <= 16 && player_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    true => player_id,
                    false => "other",
                };
            let evidence = format!(
                "player={player_id} signature_challenges={signature_challenges} n_challenges={n_challenges}"
            );
            crate::trace::record_runtime_evidence("native.challenge", Duration::ZERO, &evidence);
            let solve_response = self
                .solver
                .solve(player_script, solve_request)
                .await
                .map_err(|error| StreamBuildError::Solver(solver_failure("solver", error)))?;
            (
                solve_response.signatures.first().cloned(),
                solve_response.n_params.first().cloned(),
            )
        };

        let stream_url = innertube::assemble(
            &audio_format,
            solved_signature.as_deref(),
            solved_n_parameter.as_deref(),
        )
        .ok_or_else(|| {
            StreamBuildError::Other(innertube::failed(
                FailureKind::Dependency,
                video_id,
                "a challenge went unsolved",
            ))
        })?;
        let evidence = format!(
            "itag={} delivery={delivery} signature={} n={} token=false",
            audio_format.tier.itag(),
            audio_format.cipher.is_some(),
            innertube::query_value(&base_stream_url, "n").is_some(),
        );
        crate::trace::record_runtime_evidence("native.format", Duration::ZERO, &evidence);
        Ok(Some(Stream {
            video_id: video_id.to_owned(),
            tier: audio_format.tier,
            expires_at: stream_expiry(&stream_url, SystemTime::now()),
            location: Location::Url(stream_url),
            source: Source::NativeSignedIn,
        }))
    }

    async fn cycle(
        &self,
        video_id: &str,
        signed_in_context: &SignedInContext,
        context_was_refreshed: bool,
    ) -> Result<NativeResolutionCycle> {
        let player_response = self.player(video_id, signed_in_context).await?;
        let stream = match self
            .stream(video_id, &player_response, &signed_in_context.player_script)
            .await
        {
            Ok(Some(stream)) => stream,
            Ok(None) => return Ok(NativeResolutionCycle::Complete(None)),
            Err(StreamBuildError::Solver(_)) if !context_was_refreshed => {
                crate::trace::record_runtime_evidence(
                    "native.context_refresh",
                    Duration::ZERO,
                    "reason=solver",
                );
                return Ok(NativeResolutionCycle::Refresh);
            }
            Err(StreamBuildError::Solver(error) | StreamBuildError::Other(error)) => {
                return Err(error);
            }
        };
        let mut verified_stream = check_stream(stream, innertube::WEB_REMIX.user_agent).await?;
        if verified_stream.status != Some(403) {
            return Ok(NativeResolutionCycle::Complete(Some(verified_stream)));
        }

        let Some(provider) = potoken::Provider::discover(self.solver.node.clone()) else {
            crate::trace::record_runtime_evidence(
                "token.provider_missing",
                Duration::ZERO,
                "provider=false status=403",
            );
            return if context_was_refreshed {
                Ok(NativeResolutionCycle::Complete(Some(verified_stream)))
            } else {
                Ok(NativeResolutionCycle::Refresh)
            };
        };
        let token_bindings = Self::token_bindings(
            video_id,
            &player_response,
            signed_in_context.signed_in_page.as_ref(),
        );
        for (binding_index, (binding_kind, _)) in token_bindings.iter().enumerate() {
            let evidence = format!("kind={binding_kind} alternate={}", binding_index > 0);
            crate::trace::record_runtime_evidence("token.binding", Duration::ZERO, &evidence);
        }
        let binding_values: Vec<String> = token_bindings
            .iter()
            .map(|(_, binding)| binding.clone())
            .collect();
        let token_results = crate::trace::timed(
            "token batch",
            provider.tokens(&binding_values, context_was_refreshed),
        )
        .await;
        let mut last_token_error = None;
        let mut tokens_checked = 0;
        for ((binding_kind, _), token_result) in token_bindings.into_iter().zip(token_results) {
            let proof_token = match token_result {
                Ok(proof_token) => proof_token,
                Err(error) => {
                    last_token_error = Some(innertube::failed(FailureKind::Dependency, "token", error));
                    let evidence = format!("kind={binding_kind} result=provider-error");
                    crate::trace::record_runtime_evidence("token.failed", Duration::ZERO, &evidence);
                    continue;
                }
            };
            crate::trace::record_runtime_evidence("token.acquired", Duration::ZERO, "token=true");
            let mut tokenized_stream = verified_stream.stream.clone();
            let Location::Url(stream_url) = &mut tokenized_stream.location else {
                unreachable!("the signed-in player only returns URLs");
            };
            *stream_url = innertube::with_query_value(stream_url, "pot", &proof_token);
            verified_stream = check_stream(tokenized_stream, innertube::WEB_REMIX.user_agent).await?;
            tokens_checked += 1;
            if verified_stream.status != Some(403) {
                return Ok(NativeResolutionCycle::Complete(Some(verified_stream)));
            }
        }
        if context_was_refreshed {
            if tokens_checked == 0
                && let Some(error) = last_token_error
            {
                return Err(error);
            }
            Ok(NativeResolutionCycle::Complete(Some(verified_stream)))
        } else {
            crate::trace::record_runtime_evidence("native.context_refresh", Duration::ZERO, "reason=cdn-403");
            Ok(NativeResolutionCycle::Refresh)
        }
    }

    async fn resolve_bounded(&self, video_id: &str) -> Result<Option<CheckedStream>> {
        let signed_in_context = match self.context(false, None).await {
            Ok(signed_in_context) => signed_in_context,
            Err(_) => {
                crate::trace::record_runtime_evidence(
                    "native.context_refresh",
                    Duration::ZERO,
                    "reason=context-build",
                );
                let refreshed_context = self.context(true, None).await?;
                return match self.cycle(video_id, &refreshed_context, true).await? {
                    NativeResolutionCycle::Complete(verified_stream) => {
                        self.ensure_context_current(refreshed_context.credential_generation, verified_stream)
                    }
                    NativeResolutionCycle::Refresh => {
                        unreachable!("a refreshed cycle cannot request another refresh")
                    }
                };
            }
        };
        match self.cycle(video_id, &signed_in_context, false).await? {
            NativeResolutionCycle::Complete(verified_stream) => {
                self.ensure_context_current(signed_in_context.credential_generation, verified_stream)
            }
            NativeResolutionCycle::Refresh => {
                let refreshed_context = self.context(true, Some(signed_in_context.fetched_at)).await?;
                match self.cycle(video_id, &refreshed_context, true).await? {
                    NativeResolutionCycle::Complete(verified_stream) => {
                        self.ensure_context_current(refreshed_context.credential_generation, verified_stream)
                    }
                    NativeResolutionCycle::Refresh => {
                        unreachable!("a refreshed cycle cannot request another refresh")
                    }
                }
            }
        }
    }
}

impl Step for NativeSignedIn {
    fn source(&self) -> Source {
        Source::NativeSignedIn
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Stream>> {
        let verified_stream = tokio::time::timeout(SIGNED_IN_BUDGET, self.resolve_bounded(video_id))
            .await
            .map_err(|_| {
                innertube::failed(
                    FailureKind::Deadline,
                    video_id,
                    "signed-in resolution exceeded 20 seconds",
                )
            })??;
        Ok(verified_stream.map(|verified_stream| verified_stream.stream))
    }

    async fn resolve_checked(&self, video_id: &str) -> Result<Option<CheckedStream>> {
        tokio::time::timeout(SIGNED_IN_BUDGET, self.resolve_bounded(video_id))
            .await
            .map_err(|_| {
                innertube::failed(
                    FailureKind::Deadline,
                    video_id,
                    "signed-in resolution exceeded 20 seconds",
                )
            })?
    }
}

// ------------------------------------------------------------------ step 4

impl NativeAnonymous {
    async fn player(&self, video_id: &str, po_token: Option<&str>) -> Result<serde_json::Value> {
        let visitor_id = self.site.visitor_id().await;
        let player_response = innertube::player(innertube::PlayerRequest {
            identity: &innertube::VISIONOS,
            client_version: innertube::VISIONOS_VERSION,
            video_id,
            auth: None,
            visitor_id: visitor_id.as_deref(),
            signature_timestamp: None,
            po_token,
        })
        .await?;
        innertube::playability(&player_response, video_id)?;
        Ok(player_response)
    }

    fn stream(
        &self,
        video_id: &str,
        player_response: &serde_json::Value,
        po_token: Option<&str>,
    ) -> Result<Option<Stream>> {
        let Some(audio_format) = innertube::best_audio(player_response) else {
            return Ok(None);
        };
        // This identity is chosen because it answers with plain URLs, so a
        // ciphered format here means the identity has stopped working.
        let mut stream_url = innertube::assemble(&audio_format, None, None).ok_or_else(|| {
            innertube::failed(
                FailureKind::MissingAudio,
                video_id,
                "the anonymous identity returned no plain URL",
            )
        })?;
        if let Some(proof_token) = po_token {
            stream_url = innertube::with_query_value(&stream_url, "pot", proof_token);
        }
        Ok(Some(Stream {
            video_id: video_id.to_owned(),
            tier: audio_format.tier,
            expires_at: stream_expiry(&stream_url, SystemTime::now()),
            location: Location::Url(stream_url),
            source: Source::NativeAnonymous,
        }))
    }
}

impl Step for NativeAnonymous {
    fn source(&self) -> Source {
        Source::NativeAnonymous
    }

    fn user_agent(&self) -> &'static str {
        innertube::VISIONOS.user_agent
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Stream>> {
        let player_response = self.player(video_id, None).await?;
        self.stream(video_id, &player_response, None)
    }

    async fn retry_with_token(&self, video_id: &str) -> Result<Option<Stream>> {
        let Some(token_provider) = potoken::Provider::discover(solver::NodeSolver::node_path()) else {
            return Ok(None);
        };
        // Anonymously the token is issued against the session, not an account.
        let content_binding = self.site.visitor_id().await.ok_or_else(|| {
            innertube::failed(FailureKind::Dependency, video_id, "no session to bind a token to")
        })?;
        let proof_token = token_provider
            .token(&content_binding)
            .await
            .map_err(|error| innertube::failed(FailureKind::Dependency, "token", error))?;
        let player_response = self.player(video_id, Some(&proof_token)).await?;
        self.stream(video_id, &player_response, Some(&proof_token))
    }
}

// ------------------------------------------------------------------ step 5

impl YtDlp {
    pub(crate) fn binary() -> String {
        solver::yt_dlp_binary().to_string_lossy().into_owned()
    }

    fn common_args(&self, video_id: &str) -> Vec<String> {
        let mut yt_dlp_arguments: Vec<String> = vec![
            "--no-warnings".into(),
            "--no-playlist".into(),
            "--js-runtimes".into(),
            format!("node:{}", solver::find_node().display()),
            "--remote-components".into(),
            "ejs:github".into(),
            "--extractor-args".into(),
            "youtube:player_client=web_music".into(),
        ];
        if let Some(token_provider) = potoken::Provider::discover(solver::NodeSolver::node_path()) {
            yt_dlp_arguments.push("--extractor-args".into());
            yt_dlp_arguments.push(format!(
                "youtubepot-bgutilscript:server_home={}",
                token_provider.server_home.display()
            ));
        }
        if self.credentials.enabled()
            && let Some(cookie_file_path) = &self.cookies_path
        {
            yt_dlp_arguments.push("--cookies".into());
            yt_dlp_arguments.push(cookie_file_path.display().to_string());
        }
        yt_dlp_arguments.extend(self.extra_args.iter().cloned());
        yt_dlp_arguments.push(format!("https://music.youtube.com/watch?v={video_id}"));
        yt_dlp_arguments
    }

    async fn run(&self, yt_dlp_arguments: Vec<String>, operation_name: &str) -> Result<Vec<String>> {
        let yt_dlp_process = tokio::process::Command::new(Self::binary())
            .args(&yt_dlp_arguments)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let process_output = match tokio::time::timeout(self.timeout, yt_dlp_process).await {
            Ok(Ok(process_output)) => process_output,
            Ok(Err(error)) => {
                return Err(innertube::failed(
                    FailureKind::Dependency,
                    operation_name,
                    match error.kind() {
                        std::io::ErrorKind::NotFound => "yt-dlp is not installed",
                        _ => "yt-dlp could not start",
                    },
                ));
            }
            Err(_) => {
                return Err(innertube::failed(
                    FailureKind::Deadline,
                    operation_name,
                    "yt-dlp did not answer in time",
                ));
            }
        };
        if !process_output.status.success() {
            return Err(innertube::failed(
                FailureKind::Dependency,
                operation_name,
                "yt-dlp exited without a usable stream",
            ));
        }
        Ok(String::from_utf8_lossy(&process_output.stdout)
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

const YT_DLP_ORIGINAL_LANGUAGE_PREFERENCE: u8 = 10;
const YT_DLP_DEFAULT_LANGUAGE_PREFERENCE: u8 = 5;
const YT_DLP_NOT_DUBBED_FILTER: &str = "[url!~=?'(?i)acont(?:%3d|=)dubbed']";
const YT_DLP_NOT_DESCRIPTIVE_FILTER: &str = "[language!$=?-desc]";

fn yt_dlp_audio_selector() -> String {
    let mut selectors = Vec::new();
    for tier in Tier::PREFERRED {
        selectors.push(format!(
            "ba[language_preference={YT_DLP_ORIGINAL_LANGUAGE_PREFERENCE}][format_id^={}]{}{}",
            tier.itag(),
            YT_DLP_NOT_DUBBED_FILTER,
            YT_DLP_NOT_DESCRIPTIVE_FILTER,
        ));
    }
    for tier in Tier::PREFERRED {
        selectors.push(format!(
            "ba[language_preference={YT_DLP_DEFAULT_LANGUAGE_PREFERENCE}][format_id^={}]{}{}",
            tier.itag(),
            YT_DLP_NOT_DUBBED_FILTER,
            YT_DLP_NOT_DESCRIPTIVE_FILTER,
        ));
    }
    for tier in Tier::PREFERRED {
        selectors.push(format!(
            "ba[format_id^={}]{}{}",
            tier.itag(),
            YT_DLP_NOT_DUBBED_FILTER,
            YT_DLP_NOT_DESCRIPTIVE_FILTER,
        ));
    }
    for tier in Tier::PREFERRED {
        selectors.push(format!("ba[format_id^={}]", tier.itag()));
    }
    selectors.join("/")
}

fn tier_from_yt_dlp_format_id(format_id: &str) -> Option<Tier> {
    let itag = format_id
        .trim()
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0_u32, |value, digit| {
            value.saturating_mul(10).saturating_add(u32::from(digit - b'0'))
        });
    Tier::from_itag(itag)
}

impl Step for YtDlp {
    fn source(&self) -> Source {
        Source::YtDlp
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Stream>> {
        let mut yt_dlp_arguments: Vec<String> = vec![
            "-g".into(),
            "-f".into(),
            yt_dlp_audio_selector(),
            "--print".into(),
            "%(format_id)s".into(),
        ];
        yt_dlp_arguments.extend(self.common_args(video_id));
        let output_lines = self.run(yt_dlp_arguments, "yt-dlp resolve").await?;
        let tier = output_lines
            .iter()
            .find_map(|line| tier_from_yt_dlp_format_id(line))
            .ok_or_else(|| {
                innertube::failed(
                    FailureKind::MissingAudio,
                    video_id,
                    "yt-dlp named no known format",
                )
            })?;
        let stream_url = output_lines
            .iter()
            .find(|line| line.starts_with("http"))
            .ok_or_else(|| {
                innertube::failed(FailureKind::UnknownResponse, video_id, "yt-dlp printed no URL")
            })?;
        Ok(Some(Stream {
            video_id: video_id.to_owned(),
            tier,
            expires_at: stream_expiry(stream_url, SystemTime::now()),
            location: Location::Url(stream_url.clone()),
            source: Source::YtDlp,
        }))
    }
}

// ------------------------------------------------------- cache and in flight

/// The URL cache and the table that joins two callers onto one resolution.
#[derive(Default)]
struct Table {
    entries: Mutex<HashMap<String, CacheEntry>>,
    next_flight_id: AtomicU64,
    in_flight: Mutex<HashMap<String, Flight>>,
}

struct Flight {
    flight_id: u64,
    credential_generation: CredentialGeneration,
    outcome_receiver: watch::Receiver<Option<ResolutionOutcome>>,
}

struct CacheEntry {
    stream: Stream,
    credential_generation: Option<CredentialGeneration>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedStream {
    video_id: String,
    url: String,
    tier: Tier,
    expires_at: u64,
    #[serde(default)]
    source: Option<Source>,
}

impl Table {
    /// A stream is served from cache only while it has more than the margin
    /// left, so a track never starts on a URL that expires mid play.
    fn fresh(
        &self,
        video_id: &str,
        now: SystemTime,
        credential_generation: CredentialGeneration,
    ) -> Option<Stream> {
        let entries = self
            .entries
            .lock()
            .expect("the cache lock is never held across an await");
        entries
            .get(video_id)
            .filter(|entry| {
                entry
                    .credential_generation
                    .is_none_or(|entry_generation| entry_generation == credential_generation)
            })
            .map(|entry| &entry.stream)
            .filter(|stream| stream.is_fresh(now))
            .cloned()
    }

    fn store(&self, stream: &Stream, credential_generation: CredentialGeneration) {
        let mut entries = self
            .entries
            .lock()
            .expect("the cache lock is never held across an await");
        if matches!(&stream.location, Location::Url(url) if innertube::is_dubbed_stream_url(url)) {
            entries.remove(&stream.video_id);
            return;
        }
        entries.insert(
            stream.video_id.clone(),
            CacheEntry {
                credential_generation: credential_bound(stream).then_some(credential_generation),
                stream: stream.clone(),
            },
        );
    }

    fn forget(&self, video_id: &str) {
        let mut entries = self
            .entries
            .lock()
            .expect("the cache lock is never held across an await");
        entries.remove(video_id);
    }

    fn invalidate_credentials(&self) {
        self.entries
            .lock()
            .expect("the cache lock is never held across an await")
            .retain(|_, entry| matches!(&entry.stream.location, Location::File(_)));
        self.in_flight
            .lock()
            .expect("the in flight lock is never held across an await")
            .clear();
    }

    fn in_flight(&self, video_id: &str) -> bool {
        let mut in_flight = self
            .in_flight
            .lock()
            .expect("the in flight lock is never held across an await");
        in_flight.retain(|_, flight| flight.outcome_receiver.has_changed().is_ok());
        in_flight.contains_key(video_id)
    }

    /// Runs `resolution_attempt` for the first caller and hands its result to every caller
    /// that arrives while it is still running.
    #[cfg(test)]
    async fn join<F>(&self, video_id: &str, resolution_attempt: F) -> Result<Stream>
    where
        F: Future<Output = Result<Stream>>,
    {
        self.join_if(video_id, CredentialGeneration(0), resolution_attempt, |_| true)
            .await
    }

    async fn join_if<F, P>(
        &self,
        video_id: &str,
        credential_generation: CredentialGeneration,
        resolution_attempt: F,
        may_publish: P,
    ) -> Result<Stream>
    where
        F: Future<Output = Result<Stream>>,
        P: Fn(&Stream) -> bool,
    {
        let mut pending_resolution_attempt = Some(resolution_attempt);
        loop {
            let leader_or_receiver = {
                let mut in_flight = self
                    .in_flight
                    .lock()
                    .expect("the in flight lock is never held across an await");
                // Cancellation closes the leader channel without producing an outcome.
                in_flight.retain(|_, flight| flight.outcome_receiver.has_changed().is_ok());
                match in_flight.get(video_id) {
                    Some(flight) if flight.credential_generation == credential_generation => {
                        Err(flight.outcome_receiver.clone())
                    }
                    _ => {
                        let (outcome_sender, outcome_receiver) = watch::channel(None);
                        let flight_id = self.next_flight_id.fetch_add(1, Ordering::Relaxed);
                        in_flight.insert(
                            video_id.to_owned(),
                            Flight {
                                flight_id,
                                credential_generation,
                                outcome_receiver,
                            },
                        );
                        Ok((flight_id, outcome_sender))
                    }
                }
            };
            let (flight_id, outcome_sender) = match leader_or_receiver {
                Ok(leader) => leader,
                Err(mut outcome_receiver) => {
                    let received_outcome =
                        crate::trace::timed("inflight.join_wait", outcome_receiver.wait_for(Option::is_some))
                            .await;
                    match received_outcome {
                        Ok(outcome_slot) => {
                            let shared_outcome = outcome_slot
                                .as_ref()
                                .expect("wait_for only returns on a filled slot")
                                .clone();
                            return shared_outcome.map_err(|failures| Error::Exhausted {
                                video_id: video_id.to_owned(),
                                failures,
                            });
                        }
                        Err(_) => continue,
                    }
                }
            };

            let resolution_result = pending_resolution_attempt
                .take()
                .expect("only a leader polls its own attempt")
                .await;
            let shared_outcome: ResolutionOutcome = match &resolution_result {
                Ok(stream) => {
                    if may_publish(stream) {
                        self.store(stream, credential_generation);
                    }
                    Ok(stream.clone())
                }
                Err(error) => Err(failures(error)),
            };
            let mut in_flight = self
                .in_flight
                .lock()
                .expect("the in flight lock is never held across an await");
            if in_flight
                .get(video_id)
                .is_some_and(|flight| flight.flight_id == flight_id)
            {
                in_flight.remove(video_id);
            }
            drop(in_flight);
            let _ = outcome_sender.send(Some(shared_outcome));
            return resolution_result;
        }
    }

    fn load(&self, path: &Path, now: SystemTime, credential_generation: CredentialGeneration) {
        let Ok(cache_text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(cached_streams) = serde_json::from_str::<Vec<CachedStream>>(&cache_text) else {
            return;
        };
        let mut entries = self
            .entries
            .lock()
            .expect("the cache lock is never held across an await");
        for cached_stream in cached_streams {
            if innertube::is_dubbed_stream_url(&cached_stream.url) {
                continue;
            }
            let stream = Stream {
                video_id: cached_stream.video_id.clone(),
                location: Location::Url(cached_stream.url),
                tier: cached_stream.tier,
                source: cached_stream.source.unwrap_or(Source::Cache),
                expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(cached_stream.expires_at),
            };
            if stream.is_fresh(now) {
                entries.insert(
                    cached_stream.video_id,
                    CacheEntry {
                        credential_generation: Some(credential_generation),
                        stream,
                    },
                );
            }
        }
    }

    fn save(
        &self,
        path: &Path,
        now: SystemTime,
        credential_generation: CredentialGeneration,
    ) -> std::io::Result<()> {
        let records: Vec<CachedStream> = {
            let entries = self
                .entries
                .lock()
                .expect("the cache lock is never held across an await");
            entries
                .values()
                .filter(|entry| {
                    entry
                        .credential_generation
                        .is_none_or(|entry_generation| entry_generation == credential_generation)
                })
                .map(|entry| &entry.stream)
                .filter(|stream| stream.is_fresh(now))
                .filter_map(|stream| match &stream.location {
                    // A file is found again by step 1, so only URLs are worth keeping.
                    Location::File(_) => None,
                    Location::Url(url) => Some(CachedStream {
                        video_id: stream.video_id.clone(),
                        url: url.clone(),
                        tier: stream.tier,
                        source: Some(stream.source),
                        expires_at: stream
                            .expires_at
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    }),
                })
                .collect()
        };
        write_private(path, &serde_json::to_vec(&records)?)
    }
}

/// A cache file that names the session or the streams it signed, so it is
/// written for this user only and put in place whole.
pub(crate) fn write_private(destination_path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent_directory) = destination_path.parent() {
        ensure_private_dir(parent_directory)?;
    }
    let destination_filename = destination_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let write_sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temporary_path = destination_path.with_file_name(format!(
        ".{destination_filename}.{}.{write_sequence}",
        std::process::id()
    ));
    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The mode is set at creation, so no reader ever sees the wider file.
        open_options.mode(0o600);
    }
    let write_result = open_options
        .open(&temporary_path)
        .and_then(|mut destination_file| destination_file.write_all(contents));
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    let rename_result = std::fs::rename(&temporary_path, destination_path);
    if rename_result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    rename_result
}

pub(crate) fn protect_private_file(file_path: &Path) -> std::io::Result<()> {
    if let Some(parent_directory) = file_path.parent() {
        ensure_private_dir(parent_directory)?;
    }
    #[cfg(unix)]
    if file_path.is_file() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn ensure_private_dir(directory_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory_path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn failures(error: &Error) -> Vec<Failure> {
    match error {
        Error::Failed(failure) => vec![failure.clone()],
        Error::Exhausted { failures, .. } => failures.clone(),
        Error::Io(_) => vec![Failure::new(FailureKind::Transport, "I/O", "operation failed")],
    }
}

async fn check_stream(stream: Stream, user_agent: &str) -> Result<CheckedStream> {
    let Location::Url(url) = &stream.location else {
        return Ok(CheckedStream { stream, status: None });
    };
    let verification_result = innertube::verify(url, user_agent).await;
    let evidence = format!(
        "status={} itag={} pot={} n={} sig={}",
        verification_result
            .as_ref()
            .map_or_else(|_| "transport-error".to_owned(), |code| code.to_string()),
        stream.tier.itag(),
        innertube::query_value(url, "pot").is_some(),
        innertube::query_value(url, "n").is_some(),
        innertube::query_value(url, "sig").is_some() || innertube::query_value(url, "signature").is_some(),
    );
    crate::trace::record_runtime_evidence("cdn.result", Duration::ZERO, &evidence);
    let http_status = verification_result?;
    Ok(CheckedStream {
        stream,
        status: Some(http_status),
    })
}

// ------------------------------------------------------------------- resolver_state

struct ResolverState {
    options: ResolverOptions,
    credentials: Arc<CredentialClock>,
    credential_transition: Mutex<()>,
    site: Arc<innertube::SiteState>,
    trace: crate::trace::Trace,
    table: Table,
    permits: Semaphore,
    prefetch_permits: Semaphore,
    prefetches: Mutex<JoinSet<()>>,
    library: Option<LocalLibrary>,
    signed_in: Option<NativeSignedIn>,
    native_recovery: NativeRecovery,
    native_repairs: Mutex<JoinSet<()>>,
    anonymous: NativeAnonymous,
    ytdlp: YtDlp,
}

impl ResolverState {
    fn new(options: ResolverOptions) -> ResolverState {
        let site = Arc::new(innertube::SiteState::new(&options.cache_dir));
        let credentials = Arc::new(CredentialClock::new(
            options.identity.is_some() && options.cookies_path.is_some(),
        ));
        let initial_credential_generation = credentials.current();
        let table = Table::default();
        let stream_cache = options.cache_dir.join("streams.json");
        let _ = protect_private_file(&stream_cache);
        table.load(&stream_cache, SystemTime::now(), initial_credential_generation);
        if !credentials.enabled() {
            table.invalidate_credentials();
        }
        ResolverState {
            library: options.library_dir.clone().map(|dir| LocalLibrary { dir }),
            signed_in: options.cookies_path.clone().map(|cookies_path| NativeSignedIn {
                cookies_path,
                solver: solver::NodeSolver::with_site(options.cache_dir.clone(), Arc::clone(&site)),
                credentials: Arc::clone(&credentials),
                selection: std::sync::RwLock::new(CredentialSelection {
                    credential_generation: initial_credential_generation,
                    identity: options.identity.clone(),
                }),
                site: Arc::clone(&site),
                context_cache: tokio::sync::Mutex::new(None),
            }),
            anonymous: NativeAnonymous {
                site: Arc::clone(&site),
            },
            native_recovery: NativeRecovery::default(),
            native_repairs: Mutex::new(JoinSet::new()),
            ytdlp: YtDlp {
                cookies_path: options.cookies_path.clone(),
                extra_args: options.ytdlp_args.clone(),
                timeout: Duration::from_secs(30),
                credentials: Arc::clone(&credentials),
            },
            permits: Semaphore::new(options.parallel.max(1)),
            prefetch_permits: Semaphore::new(options.parallel.saturating_sub(1)),
            prefetches: Mutex::new(JoinSet::new()),
            table,
            credentials,
            credential_transition: Mutex::new(()),
            site,
            trace: crate::trace::Trace::default(),
            options,
        }
    }

    fn stream_is_current(&self, stream: &Stream, credential_generation: CredentialGeneration) -> bool {
        !credential_bound(stream) || self.credentials.is_current(credential_generation)
    }

    fn publish_stream(&self, stream: Stream, credential_generation: CredentialGeneration) -> Result<Stream> {
        let _transition = self
            .credential_transition
            .lock()
            .expect("the credential transition lock is never held across an await");
        if !self.stream_is_current(&stream, credential_generation) {
            return Err(credentials_changed());
        }
        self.table.store(&stream, credential_generation);
        Ok(stream)
    }

    fn fresh(&self, video_id: &str) -> Option<Stream> {
        self.table
            .fresh(video_id, SystemTime::now(), self.credentials.current())
    }

    /// One step, timed, checked against the CDN, and phrased for the report
    /// line. A URL the CDN refuses is a failed step, not a quiet failure later
    /// in mpv, so a refusal buys token retries before the next chain step.
    async fn attempt<S: Step>(&self, step: &S, video_id: &str) -> (Option<Stream>, Attempt) {
        self.attempt_with_policy(step, video_id, false).await
    }

    async fn attempt_with_policy<S: Step>(
        &self,
        step: &S,
        video_id: &str,
        bypass_playback_budget: bool,
    ) -> (Option<Stream>, Attempt) {
        let attempt_started_at = Instant::now();
        let resolution_phase = match step.source() {
            Source::LocalFile => "local.resolve",
            Source::Cache => "cache.resolve",
            Source::NativeSignedIn => "signed-in.resolve",
            Source::NativeAnonymous => "anonymous.resolve",
            Source::YtDlp => "yt-dlp.resolve",
        };
        let bounded_step = async {
            // Native request state otherwise makes the playback future exceed worker stacks.
            let checked_resolution = Box::pin(step.resolve_checked(video_id));
            if bypass_playback_budget {
                return checked_resolution.await;
            }
            match step.source() {
                Source::NativeSignedIn => {
                    self.native_recovery
                        .run(checked_resolution, native_playback_budget(self.options.quality))
                        .await
                }
                Source::NativeAnonymous => tokio::time::timeout(ANONYMOUS_BUDGET, checked_resolution)
                    .await
                    .map_err(|_| {
                    innertube::failed(
                        FailureKind::Deadline,
                        "anonymous resolution",
                        "playback deadline exceeded",
                    )
                })?,
                _ => checked_resolution.await,
            }
        };
        let checked_outcome = crate::trace::timed(resolution_phase, bounded_step).await;
        if bypass_playback_budget && step.source() == Source::NativeSignedIn {
            self.native_recovery.observe(&checked_outcome, Instant::now());
        }
        let http_status = checked_outcome
            .as_ref()
            .ok()
            .and_then(|verified_stream| verified_stream.as_ref())
            .and_then(|verified_stream| verified_stream.status);
        let cdn_refused = matches!(
            &checked_outcome,
            Ok(Some(verified_stream)) if !verified_stream.is_playable()
        );
        let elapsed_time = attempt_started_at.elapsed();
        let outcome_phase = match &checked_outcome {
            Ok(Some(_)) if !cdn_refused => "step.success",
            Ok(None) => "step.miss",
            _ => "step.failure",
        };
        crate::trace::record_runtime(outcome_phase, elapsed_time);
        let outcome_label = match &checked_outcome {
            Ok(Some(_)) if cdn_refused => "cdn-refused",
            Ok(Some(_)) => "success",
            Ok(None) => "missing",
            Err(Error::Failed(_)) => "failed",
            Err(Error::Exhausted { .. }) => "exhausted",
            Err(Error::Io(_)) => "io-error",
        };
        let audio_format_id = match &checked_outcome {
            Ok(Some(verified_stream)) => verified_stream.stream.tier.itag().to_string(),
            _ => "none".to_owned(),
        };
        let evidence = format!(
            "source={} result={outcome_label} status={} itag={audio_format_id}",
            step.source().label(),
            http_status.map_or_else(|| "none".to_owned(), |code| code.to_string()),
        );
        crate::trace::record_runtime_evidence("step.result", elapsed_time, &evidence);
        match checked_outcome {
            Ok(Some(verified_stream)) if !cdn_refused => {
                let stream = verified_stream.stream;
                let attempt_report = Attempt {
                    source: step.source(),
                    tier: Some(stream.tier),
                    elapsed: elapsed_time,
                    outcome: AttemptOutcome::Success { status: http_status },
                    evidence: format!(
                        "status={} itag={}",
                        http_status.map_or_else(|| "none".to_owned(), |code| code.to_string()),
                        stream.tier.itag(),
                    ),
                };
                (Some(stream), attempt_report)
            }
            Ok(Some(verified_stream)) => {
                let stream = verified_stream.stream;
                (
                    None,
                    Attempt {
                        source: step.source(),
                        tier: None,
                        elapsed: elapsed_time,
                        outcome: AttemptOutcome::Failed(FailureKind::CdnRefused),
                        evidence: format!(
                            "status={} itag={}",
                            http_status.map_or_else(|| "none".to_owned(), |code| code.to_string()),
                            stream.tier.itag(),
                        ),
                    },
                )
            }
            Ok(None) => (
                None,
                Attempt {
                    source: step.source(),
                    tier: None,
                    elapsed: elapsed_time,
                    outcome: AttemptOutcome::Missing,
                    evidence: "source returned no stream".to_owned(),
                },
            ),
            Err(error) => (
                None,
                Attempt {
                    source: step.source(),
                    tier: None,
                    elapsed: elapsed_time,
                    outcome: AttemptOutcome::Failed(error.kind()),
                    evidence: failures(&error)
                        .into_iter()
                        .map(|failure| failure.evidence)
                        .next()
                        .unwrap_or_else(|| "source failed without evidence".to_owned()),
                },
            ),
        }
    }

    /// The chain, first success wins. `selected_source` runs a single step instead.
    async fn run_chain(
        &self,
        video_id: &str,
        selected_source: Option<Source>,
    ) -> (Result<Stream>, Vec<Attempt>) {
        let mut attempt_reports: Vec<Attempt> = Vec::new();
        let source_is_selected =
            |source: Source| selected_source.is_none_or(|chosen_source| chosen_source == source);

        if source_is_selected(Source::LocalFile)
            && let Some(library) = &self.library
        {
            let (resolved_stream, attempt_report) = self.attempt(library, video_id).await;
            attempt_reports.push(attempt_report);
            if let Some(stream) = resolved_stream {
                return (Ok(stream), attempt_reports);
            }
        }

        if source_is_selected(Source::Cache) {
            let cache_lookup_started_at = Instant::now();
            match self.fresh(video_id) {
                Some(stream) => {
                    attempt_reports.push(Attempt {
                        source: Source::Cache,
                        tier: Some(stream.tier),
                        elapsed: cache_lookup_started_at.elapsed(),
                        outcome: AttemptOutcome::Success { status: None },
                        evidence: format!("itag={}", stream.tier.itag()),
                    });
                    return (Ok(stream), attempt_reports);
                }
                None => attempt_reports.push(Attempt {
                    source: Source::Cache,
                    tier: None,
                    elapsed: cache_lookup_started_at.elapsed(),
                    outcome: AttemptOutcome::Missing,
                    evidence: "no fresh entry".to_owned(),
                }),
            }
        }

        if let Some(stream) = self
            .network_chain(
                video_id,
                selected_source,
                (self.signed_in.as_ref(), &self.anonymous, &self.ytdlp),
                &mut attempt_reports,
            )
            .await
        {
            return (Ok(stream), attempt_reports);
        }

        let failures = attempt_reports
            .iter()
            .filter_map(|attempt| match attempt.outcome {
                AttemptOutcome::Success { .. } => None,
                AttemptOutcome::Missing => Some(Failure::new(
                    FailureKind::MissingAudio,
                    attempt.source.label(),
                    &attempt.evidence,
                )),
                AttemptOutcome::Failed(kind) => {
                    Some(Failure::new(kind, attempt.source.label(), &attempt.evidence))
                }
            })
            .collect();
        (
            Err(Error::Exhausted {
                video_id: video_id.to_owned(),
                failures,
            }),
            attempt_reports,
        )
    }

    async fn network_chain<SignedInStep: Step, AnonymousStep: Step, YtDlpStep: Step>(
        &self,
        video_id: &str,
        selected_source: Option<Source>,
        network_steps: (Option<&SignedInStep>, &AnonymousStep, &YtDlpStep),
        attempt_reports: &mut Vec<Attempt>,
    ) -> Option<Stream> {
        let (signed_in, anonymous, ytdlp) = network_steps;
        let source_is_selected = |source| selected_source.is_none_or(|chosen_source| chosen_source == source);
        if source_is_selected(Source::NativeSignedIn)
            && let Some(signed_in) = signed_in
        {
            let (resolved_stream, attempt_report) = self
                .attempt_with_policy(signed_in, video_id, selected_source.is_some())
                .await;
            attempt_reports.push(attempt_report);
            if resolved_stream.is_some() {
                return resolved_stream;
            }
        }
        // A native standard stream avoids subprocess startup after Premium recovery fails.
        if source_is_selected(Source::NativeAnonymous) {
            let (resolved_stream, attempt_report) = self
                .attempt_with_policy(anonymous, video_id, selected_source.is_some())
                .await;
            attempt_reports.push(attempt_report);
            if resolved_stream.is_some() {
                return resolved_stream;
            }
        }
        if source_is_selected(Source::YtDlp) {
            let (resolved_stream, attempt_report) = self
                .attempt_with_policy(ytdlp, video_id, selected_source.is_some())
                .await;
            attempt_reports.push(attempt_report);
            return resolved_stream;
        }
        None
    }

    /// One step checked the way the chain checks it, so a health line and a
    /// resolve line agree about what is working.
    async fn health<S: Step>(&self, step: &S, video_id: &str) -> StepHealth {
        let (resolved_stream, attempt_report) = self.attempt_with_policy(step, video_id, true).await;
        StepHealth {
            step: attempt_report.source,
            ok: resolved_stream.is_some(),
            elapsed: attempt_report.elapsed,
            failure: match attempt_report.outcome {
                AttemptOutcome::Failed(kind) => Some(kind),
                AttemptOutcome::Missing => Some(FailureKind::MissingAudio),
                AttemptOutcome::Success { .. } => None,
            },
            detail: match resolved_stream {
                Some(stream) => stream.tier.label().to_owned(),
                None => attempt_report.evidence,
            },
        }
    }

    fn schedule_native_repair(self: &Arc<Self>, video_id: &str) {
        if self.signed_in.is_none() {
            return;
        }
        let mut native_repair_tasks = self
            .native_repairs
            .lock()
            .expect("native repair owns a join set only");
        while native_repair_tasks.try_join_next().is_some() {}
        if !native_repair_tasks.is_empty() || !self.native_recovery.repair_due(Instant::now()) {
            return;
        }
        let resolver_state = Arc::clone(self);
        let credential_generation = self.credentials.current();
        let video_id = video_id.to_owned();
        native_repair_tasks.spawn(async move {
            resolver_state
                .trace
                .scope(&video_id, "recovery", async {
                    let Some(signed_in) = &resolver_state.signed_in else {
                        return;
                    };
                    let (resolved_stream, _) = resolver_state
                        .attempt_with_policy(signed_in, &video_id, true)
                        .await;
                    if let Some(stream) = resolved_stream {
                        let _ = resolver_state.publish_stream(stream, credential_generation);
                    }
                })
                .await;
        });
    }

    async fn resolve(self: &Arc<Self>, video_id: &str) -> Result<Stream> {
        let credential_generation = self.credentials.current();
        let resolution_result = self
            .trace
            .scope(video_id, "foreground", async {
                let cached_stream = crate::trace::timed("cache.lookup", async { self.fresh(video_id) }).await;
                if let Some(stream) = cached_stream {
                    crate::trace::record_runtime("cache.hit", Duration::ZERO);
                    return Ok(stream);
                }
                crate::trace::record_runtime("cache.miss", Duration::ZERO);
                bounded_playback(self.table.join_if(
                    video_id,
                    credential_generation,
                    async {
                        let _permit = crate::trace::timed("semaphore.wait", self.permits.acquire()).await;
                        self.run_chain(video_id, None).await.0
                    },
                    |stream| self.stream_is_current(stream, credential_generation),
                ))
                .await
            })
            .await;
        let resolution_result =
            resolution_result.and_then(|stream| self.publish_stream(stream, credential_generation));
        if matches!(&resolution_result, Ok(stream) if stream.source == Source::NativeAnonymous) {
            self.schedule_native_repair(video_id);
        }
        resolution_result
    }

    async fn resolve_prefetch(self: &Arc<Self>, video_id: &str) -> Option<Stream> {
        let credential_generation = self.credentials.current();
        let prefetch_result = self
            .trace
            .scope(video_id, "prefetch", async {
                let cached_stream = crate::trace::timed("cache.lookup", async { self.fresh(video_id) }).await;
                if let Some(stream) = cached_stream {
                    crate::trace::record_runtime("cache.hit", Duration::ZERO);
                    return Some(stream);
                }
                crate::trace::record_runtime("cache.miss", Duration::ZERO);
                // Speculative work must never queue ahead of a song the user selects.
                let Ok(_prefetch_permit) = self.prefetch_permits.try_acquire() else {
                    crate::trace::record_runtime("prefetch.busy", Duration::ZERO);
                    return None;
                };
                let Ok(_resolution_permit) = self.permits.try_acquire() else {
                    crate::trace::record_runtime("prefetch.busy", Duration::ZERO);
                    return None;
                };
                crate::trace::record_runtime("prefetch.admitted", Duration::ZERO);
                bounded_playback(self.table.join_if(
                    video_id,
                    credential_generation,
                    async { self.run_chain(video_id, None).await.0 },
                    |stream| self.stream_is_current(stream, credential_generation),
                ))
                .await
                .ok()
            })
            .await;
        let prefetch_result =
            prefetch_result.and_then(|stream| self.publish_stream(stream, credential_generation).ok());
        if matches!(&prefetch_result, Some(stream) if stream.source == Source::NativeAnonymous) {
            self.schedule_native_repair(video_id);
        }
        prefetch_result
    }
}

// -------------------------------------------------------------------- inner

impl Drop for Inner {
    fn drop(&mut self) {
        self.resolver_state
            .native_repairs
            .lock()
            .expect("native repair owns a join set only")
            .abort_all();
        self.resolver_state
            .prefetches
            .lock()
            .expect("the prefetch lock guards a join set only")
            .abort_all();
        if let Some(signed_in) = &self.resolver_state.signed_in {
            signed_in.solver.shutdown_now();
        }
    }
}

impl Inner {
    pub fn new(resolver_options: ResolverOptions) -> Inner {
        Inner {
            resolver_state: Arc::new(ResolverState::new(resolver_options)),
        }
    }

    pub fn set_runtime_sink(&self, runtime_sink: Option<Arc<crate::trace::RuntimeSink>>) {
        self.resolver_state.trace.set_runtime_sink(runtime_sink);
    }

    pub fn enable_cli_trace(&self) {
        self.resolver_state.trace.enable_cli();
    }

    pub fn take_cli_trace(&self) -> Vec<(String, Duration)> {
        self.resolver_state.trace.take_cli()
    }

    pub async fn reload_credentials(&self, selected_identity: Option<SelectedIdentity>) {
        {
            // Flush and logout must have one publication order. Otherwise an
            // old flush can recreate the signed URL cache after logout removes it.
            let _transition = self
                .resolver_state
                .credential_transition
                .lock()
                .expect("the credential transition lock is never held across an await");
            let credential_generation = self
                .resolver_state
                .credentials
                .advance(selected_identity.is_some() && self.resolver_state.options.cookies_path.is_some());
            if let Some(signed_in) = &self.resolver_state.signed_in {
                signed_in.publish_credentials(credential_generation, selected_identity);
            }
            self.resolver_state.table.invalidate_credentials();
            let _ = std::fs::remove_file(self.resolver_state.options.cache_dir.join("streams.json"));
        }
        self.resolver_state.native_recovery.reset();
        self.resolver_state
            .native_repairs
            .lock()
            .expect("native repair owns a join set only")
            .abort_all();
        self.resolver_state
            .prefetches
            .lock()
            .expect("the prefetch lock guards a join set only")
            .abort_all();
        if let Some(signed_in) = &self.resolver_state.signed_in {
            signed_in.clear_context().await;
        }
    }

    pub fn cached(&self, video_id: &str) -> Option<Stream> {
        self.resolver_state.fresh(video_id)
    }

    pub fn forget(&self, video_id: &str) {
        self.resolver_state.table.forget(video_id)
    }

    pub fn prefetch(&self, video_ids: impl IntoIterator<Item = String>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut prefetch_tasks = self
            .resolver_state
            .prefetches
            .lock()
            .expect("the prefetch lock guards a join set only");
        while prefetch_tasks.try_join_next().is_some() {}
        for video_id in video_ids {
            // One resolution slot stays available for foreground playback.
            if prefetch_tasks.len() >= self.resolver_state.options.parallel.saturating_sub(1) {
                break;
            }
            if self.resolver_state.fresh(&video_id).is_some()
                || self.resolver_state.table.in_flight(&video_id)
            {
                continue;
            }
            let resolver_state = Arc::clone(&self.resolver_state);
            prefetch_tasks.spawn_on(
                async move {
                    let _ = resolver_state.resolve_prefetch(&video_id).await;
                },
                &handle,
            );
        }
    }

    pub async fn resolve(&self, video_id: &str) -> Result<Stream> {
        self.resolver_state.resolve(video_id).await
    }

    pub async fn resolve_prefetch(&self, video_id: &str) -> Option<Stream> {
        self.resolver_state.resolve_prefetch(video_id).await
    }

    /// Every step's line, for the CLI and for measurement. Bypasses the join
    /// table so the report describes this call and no other.
    pub async fn resolve_traced(
        &self,
        video_id: &str,
        selected_source: Option<Source>,
    ) -> (Result<Stream>, Vec<Attempt>) {
        let credential_generation = self.resolver_state.credentials.current();
        let _resolution_permit = self.resolver_state.permits.acquire().await;
        let (mut resolution_result, attempt_reports) = self
            .resolver_state
            .trace
            .scope(
                video_id,
                "trace",
                self.resolver_state.run_chain(video_id, selected_source),
            )
            .await;
        resolution_result = resolution_result
            .and_then(|stream| self.resolver_state.publish_stream(stream, credential_generation));
        (resolution_result, attempt_reports)
    }

    pub async fn health_check(&self, video_id: &str) -> Vec<StepHealth> {
        let mut health_report = Vec::new();
        if let Some(library) = &self.resolver_state.library {
            health_report.push(self.resolver_state.health(library, video_id).await);
        }
        if let Some(signed_in) = &self.resolver_state.signed_in {
            health_report.push(self.resolver_state.health(signed_in, video_id).await);
        }
        health_report.push(
            self.resolver_state
                .health(&self.resolver_state.anonymous, video_id)
                .await,
        );
        health_report.push(
            self.resolver_state
                .health(&self.resolver_state.ytdlp, video_id)
                .await,
        );
        health_report
    }

    pub async fn download(&self, video_id: &str, destination_directory: &Path) -> Result<PathBuf> {
        let output_template = format!(
            "{}/%(artist,uploader)s/%(album,playlist_title)s/%(title)s [%(id)s].%(ext)s",
            destination_directory.display()
        );
        let mut yt_dlp_arguments: Vec<String> = vec![
            "-f".into(),
            yt_dlp_audio_selector(),
            "-x".into(),
            "--audio-quality".into(),
            "0".into(),
            "--embed-metadata".into(),
            // No thumbnail embedding: yt-dlp refuses it for webm and fails the
            // whole download after the audio has landed.
            "--remux-video".into(),
            "webm/m4a".into(),
            "-o".into(),
            output_template,
            "--print".into(),
            "after_move:filepath".into(),
        ];
        yt_dlp_arguments.extend(self.resolver_state.ytdlp.common_args(video_id));
        let output_lines = self
            .resolver_state
            .ytdlp
            .run(yt_dlp_arguments, "yt-dlp download")
            .await?;
        let downloaded_path = output_lines
            .iter()
            .rev()
            .find(|line| line.starts_with('/'))
            .ok_or_else(|| {
                innertube::failed(
                    FailureKind::UnknownResponse,
                    video_id,
                    "yt-dlp printed no file path",
                )
            })?;
        Ok(PathBuf::from(downloaded_path))
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let _transition = self
            .resolver_state
            .credential_transition
            .lock()
            .expect("the credential transition lock is never held across an await");
        self.resolver_state.table.save(
            &self.resolver_state.options.cache_dir.join("streams.json"),
            SystemTime::now(),
            self.resolver_state.credentials.current(),
        )
    }

    /// Keeps parser evidence without retaining account data or signed stream values.
    pub async fn dump_player_response(&self, video_id: &str, output_path: &Path) -> Result<()> {
        let mut player_response = match &self.resolver_state.signed_in {
            Some(signed_in) => {
                let signed_in_context = signed_in.context(false, None).await?;
                signed_in.player(video_id, &signed_in_context).await?
            }
            None => {
                innertube::player(innertube::PlayerRequest {
                    identity: &innertube::VISIONOS,
                    client_version: innertube::VISIONOS_VERSION,
                    video_id,
                    auth: None,
                    visitor_id: self.resolver_state.site.visitor_id().await.as_deref(),
                    signature_timestamp: None,
                    po_token: None,
                })
                .await?
            }
        };
        innertube::scrub(&mut player_response);
        let diagnostic_response = redwax_auth::diagnostics::redact_response(&player_response);
        let scrubbed_json = serde_json::to_vec_pretty(&diagnostic_response)
            .map_err(|error| innertube::failed(FailureKind::UnknownResponse, "dump", error))?;
        redwax_auth::diagnostics::write_private(output_path, &scrubbed_json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const TEST_GENERATION: CredentialGeneration = CredentialGeneration(0);

    #[derive(Clone, Copy)]
    enum FakeAnswer {
        Playable,
        Refused,
        Restricted,
        Pending,
    }

    struct CheckedStep {
        source: Source,
        answer: FakeAnswer,
        calls: AtomicUsize,
    }

    impl CheckedStep {
        fn new(source: Source, answer: FakeAnswer) -> Self {
            Self {
                source,
                answer,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Step for CheckedStep {
        fn source(&self) -> Source {
            self.source
        }

        async fn resolve(&self, _video_id: &str) -> Result<Option<Stream>> {
            panic!("the fake owns its CDN result")
        }

        async fn resolve_checked(&self, video_id: &str) -> Result<Option<CheckedStream>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                FakeAnswer::Restricted => Err(innertube::failed(
                    FailureKind::ContentRestricted,
                    "fixture",
                    "content restricted",
                )),
                FakeAnswer::Pending => std::future::pending().await,
                FakeAnswer::Playable | FakeAnswer::Refused => {
                    let mut stream = url_stream(video_id, 3600);
                    stream.source = self.source;
                    Ok(Some(CheckedStream {
                        stream,
                        status: Some(if matches!(self.answer, FakeAnswer::Playable) {
                            206
                        } else {
                            403
                        }),
                    }))
                }
            }
        }
    }

    fn checked_status(status: u16) -> Result<Option<CheckedStream>> {
        Ok(Some(CheckedStream {
            stream: url_stream("sample", 3600),
            status: Some(status),
        }))
    }

    #[test]
    fn a_cdn_outage_allows_one_probe_after_cooldown_and_success_restores_native() {
        let recovery = NativeRecovery::default();
        let now = Instant::now();
        recovery.observe(&checked_status(403), now);
        assert!(!recovery.admit(now));
        assert!(!recovery.admit(now + NATIVE_COOLDOWN - Duration::from_nanos(1)));
        assert!(recovery.admit(now + NATIVE_COOLDOWN));
        assert!(!recovery.admit(now + NATIVE_COOLDOWN));
        recovery.observe(&checked_status(206), now + NATIVE_COOLDOWN);
        assert!(recovery.admit(now + NATIVE_COOLDOWN));
        assert!(recovery.admit(now + NATIVE_COOLDOWN));
    }

    #[test]
    fn a_restricted_track_does_not_disable_native_for_other_tracks() {
        let recovery = NativeRecovery::default();
        let now = Instant::now();
        recovery.observe(
            &Err(innertube::failed(
                FailureKind::ContentRestricted,
                "fixture",
                "content restricted",
            )),
            now,
        );
        recovery.observe(&Ok(None), now);
        recovery.observe(&checked_status(404), now);
        assert!(recovery.admit(now));
    }

    #[test]
    fn background_native_repair_is_paced_and_stops_when_native_recovers() {
        let recovery = NativeRecovery::default();
        let now = Instant::now();
        assert!(!recovery.repair_due(now));
        recovery.observe(&checked_status(403), now);
        assert!(recovery.repair_due(now));
        assert!(!recovery.repair_due(now));
        assert!(!recovery.repair_due(now + NATIVE_COOLDOWN - Duration::from_nanos(1)));
        assert!(recovery.repair_due(now + NATIVE_COOLDOWN));
        recovery.observe(&checked_status(206), now + NATIVE_COOLDOWN);
        assert!(!recovery.repair_due(now + NATIVE_COOLDOWN * 2));
    }

    #[tokio::test(start_paused = true)]
    async fn native_standard_audio_prevents_subprocess_startup_even_in_high_quality_mode() {
        let inner = prefetch_resolver(3);
        let signed_in = CheckedStep::new(Source::NativeSignedIn, FakeAnswer::Refused);
        let anonymous = CheckedStep::new(Source::NativeAnonymous, FakeAnswer::Playable);
        let ytdlp = CheckedStep::new(Source::YtDlp, FakeAnswer::Playable);
        for video in ["first", "second"] {
            let mut attempts = Vec::new();
            let stream = inner
                .resolver_state
                .network_chain(video, None, (Some(&signed_in), &anonymous, &ytdlp), &mut attempts)
                .await
                .expect("standard native audio starts");
            assert_eq!(stream.source, Source::NativeAnonymous);
        }
        assert_eq!(signed_in.calls.load(Ordering::SeqCst), 1);
        assert_eq!(anonymous.calls.load(Ordering::SeqCst), 2);
        assert_eq!(ytdlp.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_native_resolution_yields_to_standard_audio_at_its_budget() {
        let inner = prefetch_resolver(3);
        let signed_in = CheckedStep::new(Source::NativeSignedIn, FakeAnswer::Pending);
        let anonymous = CheckedStep::new(Source::NativeAnonymous, FakeAnswer::Playable);
        let ytdlp = CheckedStep::new(Source::YtDlp, FakeAnswer::Playable);
        let started = tokio::time::Instant::now();
        let stream = inner
            .resolver_state
            .network_chain(
                "selected",
                None,
                (Some(&signed_in), &anonymous, &ytdlp),
                &mut Vec::new(),
            )
            .await
            .expect("standard native audio bypasses the stalled request");
        assert_eq!(stream.source, Source::NativeAnonymous);
        assert_eq!(
            started.elapsed(),
            native_playback_budget(inner.resolver_state.options.quality)
        );
        assert_eq!(ytdlp.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn native_diagnostics_bypass_cooldown_and_cannot_escape_to_fallback() {
        let inner = prefetch_resolver(3);
        inner
            .resolver_state
            .native_recovery
            .observe(&checked_status(403), Instant::now());
        let signed_in = CheckedStep::new(Source::NativeSignedIn, FakeAnswer::Restricted);
        let anonymous = CheckedStep::new(Source::NativeAnonymous, FakeAnswer::Playable);
        let ytdlp = CheckedStep::new(Source::YtDlp, FakeAnswer::Playable);
        let mut attempts = Vec::new();
        let found = inner
            .resolver_state
            .network_chain(
                "sample",
                Some(Source::NativeSignedIn),
                (Some(&signed_in), &anonymous, &ytdlp),
                &mut attempts,
            )
            .await;
        assert!(found.is_none());
        assert_eq!(signed_in.calls.load(Ordering::SeqCst), 1);
        assert_eq!(anonymous.calls.load(Ordering::SeqCst), 0);
        assert_eq!(ytdlp.calls.load(Ordering::SeqCst), 0);
        assert_eq!(attempts.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn total_playback_deadline_cancels_stalled_work_and_permits_retry() {
        let table = Table::default();
        let started = tokio::time::Instant::now();
        let first = bounded_playback(table.join("sample", std::future::pending::<Result<Stream>>())).await;
        assert_eq!(
            first.expect_err("stalled resolution reaches its deadline").kind(),
            FailureKind::Deadline
        );
        assert_eq!(started.elapsed(), PLAYBACK_BUDGET);
        let retry = bounded_playback(table.join("sample", async { Ok(url_stream("sample", 3600)) })).await;
        assert!(retry.is_ok());
    }

    fn url_stream(video_id: &str, seconds_left: u64) -> Stream {
        Stream {
            video_id: video_id.to_owned(),
            location: Location::Url(format!("https://example.invalid/{video_id}")),
            tier: Tier::Opus283,
            source: Source::NativeSignedIn,
            expires_at: SystemTime::now() + Duration::from_secs(seconds_left),
        }
    }

    #[test]
    fn yt_dlp_selector_keeps_dubs_and_descriptions_in_the_final_fallback() {
        let selector = yt_dlp_audio_selector();
        let selectors: Vec<&str> = selector.split('/').collect();
        let safe_filters = format!("{YT_DLP_NOT_DUBBED_FILTER}{YT_DLP_NOT_DESCRIPTIVE_FILTER}");
        assert_eq!(selectors.len(), 16);
        assert_eq!(
            selectors[0],
            format!("ba[language_preference=10][format_id^=774]{safe_filters}")
        );
        assert_eq!(
            selectors[4],
            format!("ba[language_preference=5][format_id^=774]{safe_filters}")
        );
        assert_eq!(selectors[8], format!("ba[format_id^=774]{safe_filters}"));
        assert_eq!(selectors[12], "ba[format_id^=774]");
    }

    #[test]
    fn yt_dlp_format_id_accepts_a_multi_track_suffix() {
        assert_eq!(tier_from_yt_dlp_format_id("251-2"), Some(Tier::Opus160));
        assert_eq!(tier_from_yt_dlp_format_id("140-drc"), Some(Tier::Aac128));
        assert_eq!(tier_from_yt_dlp_format_id("not-a-format"), None);
    }

    #[test]
    fn credential_reload_discards_urls_but_keeps_local_files() {
        let table = Table::default();
        table.store(&url_stream("signed", 3600), TEST_GENERATION);
        table.store(
            &Stream {
                video_id: "local".to_owned(),
                location: Location::File(PathBuf::from("/music/local.opus")),
                tier: Tier::Opus283,
                source: Source::LocalFile,
                expires_at: SystemTime::UNIX_EPOCH + NEVER,
            },
            TEST_GENERATION,
        );

        table.invalidate_credentials();

        assert!(
            table
                .fresh("signed", SystemTime::now(), TEST_GENERATION)
                .is_none()
        );
        assert!(table.fresh("local", SystemTime::now(), TEST_GENERATION).is_some());
    }

    #[test]
    fn obsolete_generation_is_hidden_and_not_flushed() {
        let old = CredentialGeneration(4);
        let current = CredentialGeneration(5);
        let table = Table::default();
        table.store(&url_stream("signed", 3600), old);
        assert!(table.fresh("signed", SystemTime::now(), current).is_none());

        let dir = std::env::temp_dir().join(format!("redwax-obsolete-generation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("streams.json");
        table
            .save(&path, SystemTime::now(), current)
            .expect("the cache directory is writable");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn prefetch_resolver(parallel: usize) -> Inner {
        Inner::new(ResolverOptions {
            cookies_path: None,
            identity: None,
            quality: Quality::High,
            library_dir: None,
            ytdlp_args: Vec::new(),
            parallel,
            cache_dir: std::env::temp_dir()
                .join(format!("redwax-prefetch-{}-{parallel}", std::process::id())),
        })
    }

    #[tokio::test]
    async fn prefetch_leaves_the_only_slot_for_playback() {
        let inner = prefetch_resolver(1);
        inner.prefetch(["background".to_owned()]);
        let tasks = inner
            .resolver_state
            .prefetches
            .lock()
            .expect("the prefetch lock guards a join set only");
        assert!(tasks.is_empty());
        assert_eq!(inner.resolver_state.permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn prefetch_does_not_build_a_backlog() {
        let inner = prefetch_resolver(3);
        {
            let mut tasks = inner
                .resolver_state
                .prefetches
                .lock()
                .expect("the prefetch lock guards a join set only");
            tasks.spawn(std::future::pending::<()>());
            tasks.spawn(std::future::pending::<()>());
        }
        inner.prefetch((0..100).map(|id| id.to_string()));
        let tasks = inner
            .resolver_state
            .prefetches
            .lock()
            .expect("the prefetch lock guards a join set only");
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn busy_playback_skips_speculative_resolution() {
        let inner = prefetch_resolver(2);
        let _held = inner
            .resolver_state
            .permits
            .acquire_many(2)
            .await
            .expect("the resolver semaphore stays open");
        tokio::select! {
            biased;
            stream = inner.resolve_prefetch("background") => assert!(stream.is_none()),
            _ = std::future::ready(()) => panic!("prefetch must not wait for playback slots"),
        }
        assert!(!inner.resolver_state.table.in_flight("background"));
    }

    #[tokio::test]
    async fn awaited_prefetch_shares_the_background_limit() {
        let inner = prefetch_resolver(3);
        let _held = inner
            .resolver_state
            .prefetch_permits
            .acquire_many(2)
            .await
            .expect("the background semaphore stays open");
        assert!(inner.resolve_prefetch("background").await.is_none());
        assert_eq!(inner.resolver_state.permits.available_permits(), 3);
        assert!(!inner.resolver_state.table.in_flight("background"));

        inner
            .resolver_state
            .table
            .store(&url_stream("cached", 3600), TEST_GENERATION);
        let stream = inner
            .resolve_prefetch("cached")
            .await
            .expect("a cached stream needs no background capacity");
        assert_eq!(stream.video_id, "cached");
    }

    /// Counts how many times the chain behind the join table actually ran.
    struct CountingStep {
        calls: AtomicUsize,
        gate: tokio::sync::Semaphore,
    }

    impl Step for CountingStep {
        fn source(&self) -> Source {
            Source::NativeSignedIn
        }

        async fn resolve(&self, video_id: &str) -> Result<Option<Stream>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _held = self.gate.acquire().await;
            Ok(Some(url_stream(video_id, 3600)))
        }
    }

    #[test]
    fn cache_freshness_respects_the_margin() {
        let table = Table::default();
        let now = SystemTime::now();
        table.store(&url_stream("fresh", 3600), TEST_GENERATION);
        table.store(&url_stream("expiring", 60), TEST_GENERATION);
        assert!(table.fresh("fresh", now, TEST_GENERATION).is_some());
        assert!(
            table.fresh("expiring", now, TEST_GENERATION).is_none(),
            "inside the margin is not fresh"
        );
        assert!(table.fresh("missing", now, TEST_GENERATION).is_none());
        table.forget("fresh");
        assert!(table.fresh("fresh", now, TEST_GENERATION).is_none());
    }

    #[tokio::test]
    async fn cancelled_resolution_can_be_retried() {
        let table = Table::default();
        let mut first = Box::pin(table.join("a", std::future::pending::<Result<Stream>>()));
        tokio::select! {
            biased;
            _ = &mut first => panic!("the pending attempt cannot finish"),
            _ = std::future::ready(()) => {}
        }
        assert!(table.in_flight("a"));
        drop(first);

        let retried = table
            .join("a", async { Ok(url_stream("a", 3600)) })
            .await
            .expect("a cancelled leader must not block a retry");
        assert_eq!(retried.video_id, "a");
        assert!(!table.in_flight("a"));
    }

    #[tokio::test]
    async fn follower_takes_over_when_the_leading_resolution_is_cancelled() {
        let table = Table::default();
        let mut leader = Box::pin(table.join("a", std::future::pending::<Result<Stream>>()));
        tokio::select! {
            biased;
            _ = &mut leader => panic!("the leading attempt cannot finish"),
            _ = std::future::ready(()) => {}
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let follower_calls = Arc::clone(&calls);
        let mut follower = Box::pin(table.join("a", async move {
            follower_calls.fetch_add(1, Ordering::SeqCst);
            Ok(url_stream("a", 3600))
        }));
        tokio::select! {
            biased;
            _ = &mut follower => panic!("the follower must wait for the leader"),
            _ = std::future::ready(()) => {}
        }
        drop(leader);

        let stream = tokio::time::timeout(Duration::from_secs(1), follower)
            .await
            .expect("the follower must reclaim leadership")
            .expect("the follower resolves");
        assert_eq!(stream.video_id, "a");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn credential_generation_prevents_an_old_flight_from_publishing_or_removing_a_new_one() {
        let table = Arc::new(Table::default());
        let credentials = Arc::new(CredentialClock::new(true));
        let old_credential_generation = credentials.current();
        let old_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let old_table = Arc::clone(&table);
        let old_clock = Arc::clone(&credentials);
        let old_wait = Arc::clone(&old_gate);
        let old = tokio::spawn(async move {
            old_table
                .join_if(
                    "a",
                    old_credential_generation,
                    async move {
                        let _permit = old_wait.acquire().await;
                        Ok(url_stream("a", 3600))
                    },
                    move |_| old_clock.is_current(old_credential_generation),
                )
                .await
        });
        while !table.in_flight("a") {
            tokio::task::yield_now().await;
        }

        let new_credential_generation = credentials.advance(true);
        table.invalidate_credentials();
        let new_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let new_table = Arc::clone(&table);
        let new_clock = Arc::clone(&credentials);
        let new_wait = Arc::clone(&new_gate);
        let new = tokio::spawn(async move {
            new_table
                .join_if(
                    "a",
                    new_credential_generation,
                    async move {
                        let _permit = new_wait.acquire().await;
                        let mut stream = url_stream("a", 3600);
                        stream.tier = Tier::Aac256;
                        Ok(stream)
                    },
                    move |_| new_clock.is_current(new_credential_generation),
                )
                .await
        });
        while !table.in_flight("a") {
            tokio::task::yield_now().await;
        }

        old_gate.add_permits(1);
        old.await.unwrap().unwrap();
        assert!(
            table.in_flight("a"),
            "the old flight must not remove its replacement"
        );
        assert!(
            table
                .fresh("a", SystemTime::now(), new_credential_generation)
                .is_none()
        );

        new_gate.add_permits(1);
        new.await.unwrap().unwrap();
        assert_eq!(
            table
                .fresh("a", SystemTime::now(), new_credential_generation)
                .map(|stream| stream.tier),
            Some(Tier::Aac256)
        );
    }

    #[tokio::test]
    async fn credentials_stay_disabled_until_selected_and_are_cleared_on_logout() {
        let dir = std::env::temp_dir().join(format!("redwax-credential-generation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cookies = dir.join("cookies.txt");
        std::fs::write(&cookies, ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture\n").unwrap();
        let previous_cache = Table::default();
        previous_cache.store(&url_stream("previous-session", 3600), TEST_GENERATION);
        previous_cache
            .save(&dir.join("streams.json"), SystemTime::now(), TEST_GENERATION)
            .unwrap();
        let inner = Inner::new(ResolverOptions {
            cookies_path: Some(cookies),
            identity: None,
            quality: Quality::High,
            library_dir: None,
            ytdlp_args: Vec::new(),
            parallel: 1,
            cache_dir: dir.clone(),
        });
        assert!(inner.cached("previous-session").is_none());
        assert!(!inner.resolver_state.credentials.enabled());
        assert!(
            !inner
                .resolver_state
                .ytdlp
                .common_args("abcdefghijk")
                .iter()
                .any(|arg| arg == "--cookies")
        );
        let initial_error = inner
            .resolver_state
            .signed_in
            .as_ref()
            .unwrap()
            .credentials()
            .await
            .err()
            .expect("a cookie path alone cannot enable account requests");
        assert_eq!(initial_error.kind(), FailureKind::MissingCredentials);
        inner
            .reload_credentials(Some(SelectedIdentity::new(0, None)))
            .await;
        assert!(inner.resolver_state.credentials.enabled());
        inner.resolver_state.table.store(
            &url_stream("signed", 3600),
            inner.resolver_state.credentials.current(),
        );
        inner.flush().expect("the old URL cache can be staged");
        assert!(dir.join("streams.json").is_file());

        inner.reload_credentials(None).await;
        assert!(
            !dir.join("streams.json").exists(),
            "logout removes any URL cache published before its transition"
        );
        let disabled = match inner
            .resolver_state
            .signed_in
            .as_ref()
            .unwrap()
            .credentials()
            .await
        {
            Ok(_) => panic!("logout must disable the signed-in step"),
            Err(error) => error,
        };
        assert_eq!(disabled.kind(), FailureKind::MissingCredentials);
        assert!(
            !inner
                .resolver_state
                .ytdlp
                .common_args("abcdefghijk")
                .iter()
                .any(|arg| arg == "--cookies")
        );

        inner
            .reload_credentials(Some(SelectedIdentity::new(0, None)))
            .await;
        inner
            .resolver_state
            .signed_in
            .as_ref()
            .unwrap()
            .credentials()
            .await
            .expect("a replacement identity re-enables the signed-in step");
        assert!(
            inner
                .resolver_state
                .ytdlp
                .common_args("abcdefghijk")
                .iter()
                .any(|arg| arg == "--cookies")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cache_survives_the_round_trip_and_drops_stale_entries() {
        let dir = std::env::temp_dir().join(format!("redwax-resolve-{}", std::process::id()));
        let path = dir.join("streams.json");
        let now = SystemTime::now();
        let written = Table::default();
        written.store(&url_stream("keep", 3600), TEST_GENERATION);
        written.store(&url_stream("drop", 60), TEST_GENERATION);
        written
            .save(&path, now, TEST_GENERATION)
            .expect("the temp dir is writable");

        let read = Table::default();
        read.load(&path, now, TEST_GENERATION);
        assert_eq!(
            read.fresh("keep", now, TEST_GENERATION)
                .map(|stream| stream.source),
            Some(Source::NativeSignedIn)
        );
        assert!(read.fresh("drop", now, TEST_GENERATION).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_migration_drops_only_explicit_dubs() {
        let dir = std::env::temp_dir().join(format!("redwax-audio-cache-migration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temporary cache directory is writable");
        let path = dir.join("streams.json");
        let expires_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("the current time follows the Unix epoch")
            .as_secs()
            + 3600;
        let records = serde_json::json!([
            {
                "video_id": "dubbed",
                "url": "https://example.invalid/audio?xtags=acont%3Ddubbed-auto%3Alang%3Dbn",
                "tier": "Opus160",
                "expires_at": expires_at,
                "source": "NativeAnonymous"
            },
            {
                "video_id": "music",
                "url": "https://example.invalid/audio",
                "tier": "Opus160",
                "expires_at": expires_at,
                "source": "NativeAnonymous"
            }
        ]);
        let cache_contents = serde_json::to_vec(&records).expect("the synthetic cache serializes");
        std::fs::write(&path, cache_contents).expect("the synthetic cache is writable");

        let table = Table::default();
        table.load(&path, SystemTime::now(), TEST_GENERATION);
        assert!(
            table
                .fresh("dubbed", SystemTime::now(), TEST_GENERATION)
                .is_none()
        );
        assert!(table.fresh("music", SystemTime::now(), TEST_GENERATION).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ported from `test_resolver_caches_dedupes_and_forgets_failures` in the
    /// Python `test_player.py`, which is the specification for this behaviour.
    #[tokio::test]
    async fn test_resolver_caches_dedupes_and_forgets_failures() {
        let table = Arc::new(Table::default());
        let step = Arc::new(CountingStep {
            calls: AtomicUsize::new(0),
            gate: Semaphore::new(0),
        });

        let attempt = |table: Arc<Table>, step: Arc<CountingStep>, id: &'static str| async move {
            table
                .join(id, async {
                    Ok(step.resolve(id).await?.expect("the fake step finds one"))
                })
                .await
        };

        let first = tokio::spawn(attempt(Arc::clone(&table), Arc::clone(&step), "a"));
        // The gate holds the leader inside the chain until the follower has joined.
        while !table.in_flight("a") {
            tokio::task::yield_now().await;
        }
        let second = tokio::spawn(attempt(Arc::clone(&table), Arc::clone(&step), "a"));
        step.gate.add_permits(1);

        let one = first
            .await
            .expect("the task is not cancelled")
            .expect("the fake step succeeds");
        let two = second
            .await
            .expect("the task is not cancelled")
            .expect("the follower joins");
        assert_eq!(one, two);
        assert_eq!(
            table.fresh("a", SystemTime::now(), TEST_GENERATION).as_ref(),
            Some(&one)
        );
        assert_eq!(
            step.calls.load(Ordering::SeqCst),
            1,
            "a joined caller must not run the chain again"
        );

        for _ in 0..2 {
            let error = table
                .join("bad", async {
                    Err(Error::Exhausted {
                        video_id: "bad".to_owned(),
                        failures: vec![Failure::new(FailureKind::MissingAudio, "fixture", "no stream")],
                    })
                })
                .await
                .expect_err("the fake chain always fails");
            assert!(error.to_string().contains("bad"));
        }
        assert!(!table.in_flight("bad"));
        assert!(
            table.fresh("bad", SystemTime::now(), TEST_GENERATION).is_none(),
            "a failure is never cached"
        );
    }

    #[test]
    fn the_cache_file_is_written_for_this_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("redwax-cache-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let inner = Inner::new(ResolverOptions {
            cookies_path: None,
            identity: None,
            quality: Quality::High,
            library_dir: None,
            ytdlp_args: Vec::new(),
            parallel: 1,
            cache_dir: dir.clone(),
        });
        inner.flush().expect("the temp dir is writable");

        let written = std::fs::metadata(dir.join("streams.json")).expect("flush wrote the cache");
        assert_eq!(written.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&dir)
                .expect("cache directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_account_bound_caches_are_restricted_before_reading() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("redwax-existing-cache-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create cache directory");
        for name in ["streams.json", "innertube.json"] {
            let path = dir.join(name);
            std::fs::write(&path, if name == "streams.json" { "[]" } else { "{}" }).expect("write old cache");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("make old cache broad");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("make old cache directory broad");

        let _inner = Inner::new(ResolverOptions {
            cookies_path: None,
            identity: None,
            quality: Quality::High,
            library_dir: None,
            ytdlp_args: Vec::new(),
            parallel: 1,
            cache_dir: dir.clone(),
        });

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["streams.json", "innertube.json"] {
            assert_eq!(
                std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn signed_in_credentials_share_one_netscape_cookie_read() {
        let text = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-sid\n\
                    .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n\
                    .youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tf1=1\n";
        let auth = auth::AuthContext::new(CookieJar::from_netscape(text), None, None, 100)
            .expect("the synthetic cookies contain login and signing cookies");
        let headers = auth.headers(100).expect("the synthetic headers are valid");
        let authorization = headers["authorization"].to_str().unwrap();
        assert_eq!(
            headers["cookie"],
            "SAPISID=fixture-sid; LOGIN_INFO=fixture-login; PREF=f1=1"
        );
        assert!(authorization.starts_with("SAPISIDHASH "));
        assert!(!authorization.contains("fixture-sid"));
    }

    #[test]
    fn token_binding_tries_account_then_video_without_page_policy() {
        let response = serde_json::json!({
            "responseContext": {
                "mainAppWebResponseContext": {"datasyncId": "account"}
            }
        });
        assert_eq!(
            NativeSignedIn::token_bindings("video", &response, None),
            vec![("account", "account".to_owned()), ("video", "video".to_owned())]
        );
        assert_eq!(
            NativeSignedIn::token_bindings("video", &serde_json::json!({}), None),
            vec![("video", "video".to_owned())]
        );
    }
}
