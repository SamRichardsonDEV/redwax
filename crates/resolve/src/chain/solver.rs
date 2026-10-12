//! Signature and `n` challenge solving, through yt-dlp's own solver library.
//!
//! yt-dlp keeps a JavaScript solver (the `ejs` project) that takes YouTube's
//! player script and returns the deciphered values. It is what the yt-dlp
//! subprocess runs, so tracking it means our step 3 breaks and heals on the
//! same day step 5 does. Current yt-dlp obtains the library and core from its
//! `yt-dlp-ejs` companion package or downloads them into
//! `$XDG_CACHE_HOME/yt-dlp/challenge-solver/`. Older installations can bundle
//! the core under the yt-dlp package. The Python driver `ejs.py` defines the
//! protocol, which is: stdin to node is the lib code, then
//! `Object.assign(globalThis, lib);`, then the core code, then
//! `console.log(JSON.stringify(jsc(<request>)));`. The request is
//! `{"type":"player","player":<script body>,"requests":[{"type":"sig"|"n",
//! "challenges":[...]}],"output_preprocessed":true}`, or
//! `{"type":"preprocessed","preprocessed_player":<cached>,"requests":[...]}`.
//! The reply is `{"type":"result","responses":[{"type":"result","data":
//! {challenge: solution}} | {"type":"error","error":...}],
//! "preprocessed_player":...}` or `{"type":"error","error":...}`. Cache the
//! preprocessed player per script URL so a warm solve skips parsing.
//!
//! One node process is kept alive per solver owner, reading one JSON request per line
//! and writing one reply per line. It holds the preprocessed player in memory
//! and reads it from `cache_dir` itself, which is what the 4 MB stdin write of
//! the one shot form cost. The one shot form stays as the fallback for a
//! harness that will not start. Both scripts are mirrored into `cache_dir/solver`
//! and refreshed when the yt-dlp source changes. A missing source can use the
//! last mirror. The harness exits when its stdin closes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Long enough for the harness to leave its read loop, short enough that an
/// exiting run is not held up by a wedged solve.
#[cfg(test)]
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SolveRequest {
    /// Values of `s` from `signatureCipher`, in order.
    pub signatures: Vec<String>,
    /// Values of the `n` query parameter, in order.
    pub n_params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SolveResponse {
    pub signatures: Vec<String>,
    pub n_params: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    #[error("solver library missing at {}", .0.display())]
    LibraryMissing(PathBuf),
    #[error("node not found, install it or set REDWAX_NODE")]
    NodeMissing,
    #[error("the solver did not answer before its deadline")]
    Deadline,
    #[error("solver: {0}")]
    Failed(String),
}

/// The player script, fetched once per version and cached on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerScript {
    /// For example `https://www.youtube.com/s/player/f572e43c/player_ias.vflset/en_US/base.js`.
    pub url: String,
    pub body: String,
}

impl PlayerScript {
    /// The eight hex characters YouTube versions its player by.
    pub fn id(&self) -> &str {
        self.url.split('/').nth(5).unwrap_or("unknown")
    }

    /// The timestamp binds a player API response to this script's ciphers.
    pub fn signature_timestamp(&self) -> std::result::Result<u32, SolverError> {
        for key in ["signatureTimestamp", "sts"] {
            let mut rest = self.body.as_str();
            while let Some(offset) = rest.find(key) {
                let candidate = &rest[offset..];
                let boundary = offset == 0 || !is_js_identifier(rest.as_bytes()[offset - 1]);
                if boundary && let Some(timestamp) = timestamp_after_key(candidate, key) {
                    return Ok(timestamp);
                }
                rest = &candidate[key.len()..];
            }
        }
        Err(SolverError::Failed(format!(
            "player {} names no signature timestamp",
            self.id()
        )))
    }
}

fn is_js_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn timestamp_after_key(text: &str, key: &str) -> Option<u32> {
    let mut rest = &text[key.len()..];
    if rest
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
    {
        rest = &rest[1..];
    }
    rest = rest.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits < 5
        || rest
            .as_bytes()
            .get(digits)
            .is_some_and(|byte| matches!(byte, b'.' | b'e' | b'E') || is_js_identifier(*byte))
    {
        return None;
    }
    rest[..digits].parse().ok()
}

#[derive(Debug)]
struct SolverScripts {
    library: String,
    core: String,
    namespace: String,
}

fn preprocessed_namespace(solver_namespace: &str, player_url: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(solver_namespace.as_bytes());
    digest.update([0]);
    digest.update(player_url.as_bytes());
    hex::encode(digest.finalize())
}

fn player_cache_namespace(player_url: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(player_url.as_bytes());
    hex::encode(digest.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResidentSolverKey {
    node: PathBuf,
    cache_dir: PathBuf,
    namespace: String,
}

pub struct NodeSolver {
    /// `REDWAX_NODE` overrides PATH. A bare fallback preserves ordinary command
    /// lookup and missing-program diagnosis.
    pub node: PathBuf,
    pub library: PathBuf,
    pub cache_dir: PathBuf,
    pub timeout: Duration,
    process_state: Arc<SolverProcessState>,
    site: Arc<super::innertube::SiteState>,
}

#[derive(Default)]
struct SolverProcessState {
    resident_process: Mutex<Option<ResidentSolverProcess>>,
    exchange_lock: tokio::sync::Mutex<()>,
    lifecycle_generation: AtomicU64,
}

impl Drop for SolverProcessState {
    fn drop(&mut self) {
        self.resident_process
            .lock()
            .expect("the resident slot is never held across an await")
            .take();
    }
}

pub(crate) fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_owned()))
}

fn yt_dlp_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("yt-dlp/challenge-solver")
}

/// The live solver library refreshes our mirror when yt-dlp updates it.
pub(crate) fn yt_dlp_library() -> PathBuf {
    if let Ok(explicit) = std::env::var("REDWAX_SOLVER_LIBRARY") {
        return PathBuf::from(explicit);
    }
    let cached_library = yt_dlp_cache_dir().join("lib.json");
    if script_file_has_code(&cached_library) {
        return cached_library;
    }
    installed_yt_dlp_script(&LIBRARY_PATHS).unwrap_or(cached_library)
}

fn solver_code(script_contents: &str) -> Option<String> {
    match serde_json::from_str::<Value>(script_contents) {
        Ok(cached_script) => cached_script
            .pointer("/data/code")
            .and_then(Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .map(str::to_owned),
        Err(_) => (!script_contents.trim().is_empty()).then(|| script_contents.to_owned()),
    }
}

const LIBRARY_PATHS: [&str; 2] = ["yt_dlp_ejs/yt/solver/lib.min.js", "yt_dlp_ejs/yt/solver/lib.js"];

const CORE_PATHS: [&str; 4] = [
    "yt_dlp_ejs/yt/solver/core.min.js",
    "yt_dlp_ejs/yt/solver/core.js",
    "yt_dlp/extractor/youtube/jsc/_builtin/vendor/yt.solver.core.min.js",
    "yt_dlp/extractor/youtube/jsc/_builtin/vendor/yt.solver.core.js",
];

fn script_in(package_directory: &Path, relative_paths: &[&str]) -> Option<PathBuf> {
    relative_paths
        .iter()
        .map(|relative_path| package_directory.join(relative_path))
        .find(|candidate| candidate.is_file())
}

fn script_under(installation_directory: &Path, relative_paths: &[&str]) -> Option<PathBuf> {
    for library_name in ["lib", "lib64"] {
        let library_directory = installation_directory.join(library_name);
        if let Some(script) = script_in(&library_directory, relative_paths) {
            return Some(script);
        }
        for package_directory_name in ["site-packages", "dist-packages"] {
            if let Some(script) = script_in(&library_directory.join(package_directory_name), relative_paths) {
                return Some(script);
            }
        }
        let mut version_directories: Vec<PathBuf> = std::fs::read_dir(&library_directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        version_directories.sort();
        for version_directory in version_directories {
            for package_directory_name in ["site-packages", "dist-packages"] {
                if let Some(script) =
                    script_in(&version_directory.join(package_directory_name), relative_paths)
                {
                    return Some(script);
                }
            }
        }
    }
    None
}

pub(crate) fn find_program(program: &str) -> PathBuf {
    let requested_program = PathBuf::from(program);
    if requested_program.components().count() != 1 {
        return requested_program;
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(&requested_program);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    requested_program
}

pub(crate) fn yt_dlp_binary() -> PathBuf {
    if let Ok(explicit) = std::env::var("REDWAX_YTDLP") {
        return find_program(&explicit);
    }
    let managed_binary = home().join(".local/share/redwax/yt-dlp/bin/yt-dlp");
    if managed_binary.is_file() {
        managed_binary
    } else {
        find_program("yt-dlp")
    }
}

fn installed_yt_dlp_script(relative_paths: &[&str]) -> Option<PathBuf> {
    let yt_dlp_binary = find_program(&super::YtDlp::binary());
    let mut executable_paths = Vec::new();
    if let Ok(canonical_path) = std::fs::canonicalize(&yt_dlp_binary)
        && canonical_path != yt_dlp_binary
    {
        executable_paths.push(canonical_path);
    }
    executable_paths.push(yt_dlp_binary);
    for executable_path in executable_paths {
        if let Some(installation_directory) = executable_path.parent().and_then(Path::parent)
            && let Some(script) = script_under(installation_directory, relative_paths)
        {
            return Some(script);
        }
    }
    None
}

pub(crate) fn script_file_has_code(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| solver_code(&contents))
        .is_some()
}

pub(crate) fn yt_dlp_core() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("REDWAX_SOLVER_CORE") {
        return Some(PathBuf::from(explicit));
    }
    let cached_core = yt_dlp_cache_dir().join("core.json");
    if script_file_has_code(&cached_core) {
        return Some(cached_core);
    }
    installed_yt_dlp_script(&CORE_PATHS)
}

pub(crate) fn find_node() -> PathBuf {
    if let Ok(explicit) = std::env::var("REDWAX_NODE") {
        return find_program(&explicit);
    }
    let managed_binary = home().join(".local/share/redwax/node/bin/node");
    if managed_binary.is_file() {
        managed_binary
    } else {
        find_program("node")
    }
}

impl NodeSolver {
    /// The node binary this machine runs, shared with the token provider.
    pub fn node_path() -> PathBuf {
        find_node()
    }

    #[cfg(test)]
    pub fn new(cache_dir: PathBuf) -> NodeSolver {
        let site = Arc::new(super::innertube::SiteState::new(&cache_dir));
        Self::with_site(cache_dir, site)
    }

    pub(crate) fn with_site(cache_dir: PathBuf, site: Arc<super::innertube::SiteState>) -> NodeSolver {
        NodeSolver {
            node: find_node(),
            library: cache_dir.join("solver/lib.json"),
            cache_dir,
            timeout: Duration::from_secs(30),
            process_state: Arc::new(SolverProcessState::default()),
            site,
        }
    }

    /// Ends this solver's resident process and waits for it.
    #[cfg(test)]
    pub async fn shutdown(&self) {
        self.process_state
            .lifecycle_generation
            .fetch_add(1, Ordering::AcqRel);
        let _exchange_guard = self.process_state.exchange_lock.lock().await;
        let resident_process = self
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is never held across an await")
            .take();
        if let Some(mut resident_process) = resident_process {
            resident_process.stop().await;
        }
    }

    /// The form a `Drop` can call, which cannot await. Dropping the resident
    /// closes its stdin and signals it, so the child goes either way.
    pub fn shutdown_now(&self) {
        self.process_state
            .lifecycle_generation
            .fetch_add(1, Ordering::AcqRel);
        self.process_state
            .resident_process
            .lock()
            .expect("the resident slot is never held across an await")
            .take();
    }

    fn player_dir(&self) -> PathBuf {
        self.cache_dir.join("player")
    }

    /// The player version YouTube serves now. The embed API is asked only when
    /// the cached id has aged out, so a resolve costs no request here.
    pub async fn current_player_url(&self) -> std::result::Result<String, SolverError> {
        if let Some(player_id) = self.site.cached_value("player_id") {
            return Ok(player_url(&player_id));
        }
        self.fetch_current_player_url().await
    }

    async fn fetch_current_player_url(&self) -> std::result::Result<String, SolverError> {
        let iframe_api_response = crate::trace::timed(
            "player id",
            super::innertube::http()
                .get("https://www.youtube.com/iframe_api")
                .send(),
        )
        .await
        .map_err(|error| SolverError::Failed(format!("iframe api: {error}")))?;
        let http_status = iframe_api_response.status();
        if !http_status.is_success() {
            return Err(SolverError::Failed(format!("iframe api: HTTP {http_status}")));
        }
        let iframe_api_body = iframe_api_response
            .text()
            .await
            .map_err(|error| SolverError::Failed(format!("iframe api body: {error}")))?;
        let player_id = scrape_player_id(&iframe_api_body)
            .ok_or_else(|| SolverError::Failed("iframe api names no player version".to_owned()))?;
        self.site.store_value("player_id", &player_id);
        Ok(player_url(&player_id))
    }

    /// The current player script URL, from the watch page or the `player`
    /// response's `jsUrl`, then its body from the disk cache or the network.
    pub async fn player_script(&self, js_url: &str) -> std::result::Result<PlayerScript, SolverError> {
        let player_script_url = if js_url.is_empty() {
            self.current_player_url().await?
        } else {
            js_url.to_owned()
        };
        self.load_player_script(player_script_url, false).await
    }

    /// Fetches the current player identity and script without consulting either cache.
    pub async fn refresh_player_script(&self) -> std::result::Result<PlayerScript, SolverError> {
        let player_script_url = self.fetch_current_player_url().await?;
        self.load_player_script(player_script_url, true).await
    }

    /// Fetches a page-selected player script without consulting its body cache.
    pub async fn refresh_player_script_at(
        &self,
        js_url: &str,
    ) -> std::result::Result<PlayerScript, SolverError> {
        if js_url.is_empty() {
            return self.refresh_player_script().await;
        }
        self.load_player_script(js_url.to_owned(), true).await
    }

    async fn load_player_script(
        &self,
        player_script_url: String,
        force_refresh: bool,
    ) -> std::result::Result<PlayerScript, SolverError> {
        let player_id = player_script_url
            .split('/')
            .nth(5)
            .unwrap_or("unknown")
            .to_owned();
        let cached_script_path = self.player_dir().join(format!(
            "{player_id}-{}.js",
            player_cache_namespace(&player_script_url)
        ));
        if !force_refresh
            && let Ok(player_script_body) =
                crate::trace::timed("player script", tokio::fs::read_to_string(&cached_script_path)).await
            && !player_script_body.is_empty()
        {
            return Ok(PlayerScript {
                url: player_script_url,
                body: player_script_body,
            });
        }
        let script_response = super::innertube::http()
            .get(&player_script_url)
            .send()
            .await
            .map_err(|error| SolverError::Failed(format!("player script: {error}")))?;
        let http_status = script_response.status();
        if !http_status.is_success() {
            return Err(SolverError::Failed(format!("player script: HTTP {http_status}")));
        }
        let player_script_body = script_response
            .text()
            .await
            .map_err(|error| SolverError::Failed(format!("player script body: {error}")))?;
        if player_script_body.is_empty() {
            return Err(SolverError::Failed("player script body is empty".to_owned()));
        }
        write_cache(&cached_script_path, player_script_body.as_bytes()).await;
        Ok(PlayerScript {
            url: player_script_url,
            body: player_script_body,
        })
    }

    async fn scripts(&self) -> std::result::Result<SolverScripts, SolverError> {
        let solver_library_code = self.cached_library().await?;
        let cached_core_path = self.cache_dir.join("solver/core.js");
        let solver_core = self.cached_or_refreshed(&cached_core_path, yt_dlp_core()).await?;
        let mut digest = Sha1::new();
        digest.update(solver_library_code.as_bytes());
        digest.update([0]);
        digest.update(solver_core.as_bytes());
        Ok(SolverScripts {
            library: solver_library_code,
            core: solver_core,
            namespace: hex::encode(digest.finalize()),
        })
    }

    async fn cached_library(&self) -> std::result::Result<String, SolverError> {
        let source_library_path = yt_dlp_library();
        let cached_library = tokio::fs::read_to_string(&self.library)
            .await
            .ok()
            .and_then(|contents| solver_code(&contents));
        match tokio::fs::read_to_string(&source_library_path).await {
            Ok(source_library) => match solver_code(&source_library) {
                Some(source_code) => {
                    if cached_library.as_deref() != Some(source_code.as_str()) {
                        write_cache(&self.library, source_code.as_bytes()).await;
                    }
                    Ok(source_code)
                }
                None => cached_library.ok_or_else(|| {
                    SolverError::Failed(format!(
                        "solver library at {} is invalid",
                        source_library_path.display()
                    ))
                }),
            },
            Err(_) => cached_library.ok_or(SolverError::LibraryMissing(source_library_path)),
        }
    }

    async fn cached_or_refreshed(
        &self,
        cached_path: &Path,
        source_path: Option<PathBuf>,
    ) -> std::result::Result<String, SolverError> {
        let cached_contents = tokio::fs::read_to_string(cached_path)
            .await
            .ok()
            .filter(|text| !text.is_empty());
        if let Some(source_path) = source_path {
            match tokio::fs::read_to_string(&source_path).await {
                Ok(source_contents) => {
                    if let Some(source_code) = solver_code(&source_contents) {
                        if cached_contents.as_deref() != Some(source_code.as_str()) {
                            write_cache(cached_path, source_code.as_bytes()).await;
                        }
                        return Ok(source_code);
                    }
                    if cached_contents.is_none() {
                        return Err(SolverError::LibraryMissing(source_path));
                    }
                }
                Err(_) if cached_contents.is_none() => {
                    return Err(SolverError::LibraryMissing(source_path));
                }
                _ => {}
            }
        }
        cached_contents.ok_or_else(|| SolverError::LibraryMissing(cached_path.to_owned()))
    }

    pub async fn solve(
        &self,
        player_script: &PlayerScript,
        solve_request: SolveRequest,
    ) -> std::result::Result<SolveResponse, SolverError> {
        if solve_request.signatures.is_empty() && solve_request.n_params.is_empty() {
            return Ok(SolveResponse::default());
        }
        let solver_scripts = self.scripts().await?;

        let namespace = preprocessed_namespace(&solver_scripts.namespace, &player_script.url);
        let preprocessed_path = self
            .player_dir()
            .join(format!("{}-{namespace}.pre.json", player_script.id()));
        let first_solve_result = crate::trace::timed(
            "solver",
            self.solve_once(
                player_script,
                &solve_request,
                &preprocessed_path,
                &solver_scripts,
                false,
            ),
        )
        .await;
        if first_solve_result.is_ok() {
            return first_solve_result;
        }
        let _ = tokio::fs::remove_file(&preprocessed_path).await;
        self.discard_resident(&solver_scripts.namespace).await;
        crate::trace::timed(
            "solver retry",
            self.solve_once(
                player_script,
                &solve_request,
                &preprocessed_path,
                &solver_scripts,
                true,
            ),
        )
        .await
    }

    async fn solve_once(
        &self,
        player_script: &PlayerScript,
        solve_request: &SolveRequest,
        preprocessed_path: &Path,
        solver_scripts: &SolverScripts,
        force_raw_player: bool,
    ) -> std::result::Result<SolveResponse, SolverError> {
        let mut request_kinds: Vec<&str> = Vec::new();
        let mut solver_requests: Vec<Value> = Vec::new();
        if !solve_request.signatures.is_empty() {
            request_kinds.push("sig");
            solver_requests.push(json!({
                "type": "sig",
                "challenges": solve_request.signatures
            }));
        }
        if !solve_request.n_params.is_empty() {
            request_kinds.push("n");
            solver_requests.push(json!({
                "type": "n",
                "challenges": solve_request.n_params
            }));
        }

        let solver_reply = self
            .ask(
                player_script,
                preprocessed_path,
                &solver_requests,
                solver_scripts,
                force_raw_player,
            )
            .await?;
        if solver_reply.get("type").and_then(Value::as_str) == Some("error") {
            return Err(SolverError::Failed(solver_reply["error"].to_string()));
        }

        let solver_responses = solver_reply
            .get("responses")
            .and_then(Value::as_array)
            .ok_or_else(|| SolverError::Failed("solver reply carries no responses".to_owned()))?;
        if solver_responses.len() != request_kinds.len() {
            return Err(SolverError::Failed(format!(
                "solver returned {} responses for {} requests",
                solver_responses.len(),
                request_kinds.len()
            )));
        }
        let mut solve_response = SolveResponse::default();
        for (request_kind, solver_response) in request_kinds.iter().zip(solver_responses) {
            if solver_response.get("type").and_then(Value::as_str) == Some("error") {
                return Err(SolverError::Failed(format!(
                    "{request_kind}: {}",
                    solver_response["error"]
                )));
            }
            let solutions_by_challenge = solver_response
                .get("data")
                .ok_or_else(|| SolverError::Failed(format!("{request_kind}: no solutions returned")))?;
            let challenges = if *request_kind == "sig" {
                &solve_request.signatures
            } else {
                &solve_request.n_params
            };
            let output_solutions = if *request_kind == "sig" {
                &mut solve_response.signatures
            } else {
                &mut solve_response.n_params
            };
            for challenge in challenges {
                let solution = solutions_by_challenge
                    .get(challenge)
                    .and_then(Value::as_str)
                    .filter(|solution| !solution.is_empty())
                    .ok_or_else(|| {
                        SolverError::Failed(format!("{request_kind}: a challenge went unanswered"))
                    })?;
                output_solutions.push(solution.to_owned());
            }
        }
        if let Some(preprocessed_player) = solver_reply.get("preprocessed_player") {
            write_cache(preprocessed_path, preprocessed_player.to_string().as_bytes()).await;
        }
        Ok(solve_response)
    }

    /// One exchange with the resident process, falling back to a fresh one shot
    /// process when the resident will not start or has died.
    async fn ask(
        &self,
        player_script: &PlayerScript,
        preprocessed_path: &Path,
        solver_requests: &[Value],
        solver_scripts: &SolverScripts,
        force_raw_player: bool,
    ) -> std::result::Result<Value, SolverError> {
        match self
            .ask_resident(
                player_script,
                preprocessed_path,
                solver_requests,
                solver_scripts,
                force_raw_player,
            )
            .await
        {
            Ok(solver_reply) => Ok(solver_reply),
            Err(resident_error) => {
                if matches!(resident_error, SolverError::NodeMissing) {
                    return Err(resident_error);
                }
                self.ask_one_shot(
                    player_script,
                    preprocessed_path,
                    solver_requests,
                    solver_scripts,
                    force_raw_player,
                )
                .await
                .map_err(|one_shot_error| {
                    SolverError::Failed(format!(
                        "resident failed ({resident_error}); one shot failed ({one_shot_error})"
                    ))
                })
            }
        }
    }

    async fn ask_one_shot(
        &self,
        player_script: &PlayerScript,
        preprocessed_path: &Path,
        solver_requests: &[Value],
        solver_scripts: &SolverScripts,
        force_raw_player: bool,
    ) -> std::result::Result<Value, SolverError> {
        let cached_preprocessed_player = if force_raw_player {
            None
        } else {
            tokio::fs::read_to_string(preprocessed_path).await.ok()
        };
        let solver_payload = match cached_preprocessed_player {
            Some(preprocessed_text) => json!({
                "type": "preprocessed",
                "preprocessed_player": serde_json::from_str::<Value>(&preprocessed_text)
                    .map_err(|error| SolverError::Failed(format!("preprocessed player: {error}")))?,
                "requests": solver_requests,
            }),
            None => json!({
                "type": "player",
                "player": player_script.body,
                "requests": solver_requests,
                "output_preprocessed": true,
            }),
        };
        let node_input = format!(
            "{}\nObject.assign(globalThis, lib);\n{}\nconsole.log(JSON.stringify(jsc({solver_payload})));\n",
            solver_scripts.library, solver_scripts.core,
        );
        let node_output = self.run_node(node_input).await?;
        serde_json::from_str(&node_output)
            .map_err(|error| SolverError::Failed(format!("solver reply: {error}")))
    }

    async fn ask_resident(
        &self,
        player_script: &PlayerScript,
        preprocessed_path: &Path,
        solver_requests: &[Value],
        solver_scripts: &SolverScripts,
        force_raw_player: bool,
    ) -> std::result::Result<Value, SolverError> {
        let _exchange_guard = self.process_state.exchange_lock.lock().await;
        let lifecycle_generation = self.process_state.lifecycle_generation.load(Ordering::Acquire);
        let resident_key = self.resident_process_key(&solver_scripts.namespace);
        let existing_resident = self
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is never held across an await")
            .take();
        let mut resident_process = match existing_resident {
            Some(resident_process) if resident_process.key == resident_key => resident_process,
            _ => {
                self.start_resident(resident_key, &solver_scripts.library, &solver_scripts.core)
                    .await?
            }
        };
        let solver_request = json!({
            "key": preprocessed_path.display().to_string(),
            "path": preprocessed_path.display().to_string(),
            "refresh": force_raw_player,
            "player": force_raw_player.then_some(player_script.body.as_str()),
            "requests": solver_requests,
        });
        let solver_reply = tokio::time::timeout(self.timeout, resident_process.exchange(&solver_request))
            .await
            .map_err(|_| SolverError::Deadline)??;
        let solver_reply = if solver_reply.get("type").and_then(Value::as_str) == Some("need_player") {
            let solver_request = json!({
                "key": preprocessed_path.display().to_string(),
                "path": preprocessed_path.display().to_string(),
                "player": player_script.body,
                "requests": solver_requests,
            });
            tokio::time::timeout(self.timeout, resident_process.exchange(&solver_request))
                .await
                .map_err(|_| SolverError::Deadline)??
        } else {
            solver_reply
        };
        let mut resident_slot = self
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is never held across an await");
        if self.process_state.lifecycle_generation.load(Ordering::Acquire) == lifecycle_generation {
            *resident_slot = Some(resident_process);
        }
        Ok(solver_reply)
    }

    fn resident_process_key(&self, namespace: &str) -> ResidentSolverKey {
        ResidentSolverKey {
            node: self.node.clone(),
            cache_dir: self.cache_dir.clone(),
            namespace: namespace.to_owned(),
        }
    }

    async fn discard_resident(&self, namespace: &str) {
        let _exchange_guard = self.process_state.exchange_lock.lock().await;
        let resident_key = self.resident_process_key(namespace);
        let mut resident_slot = self
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is never held across an await");
        if resident_slot
            .as_ref()
            .is_some_and(|resident_process| resident_process.key == resident_key)
        {
            resident_slot.take();
        }
    }

    async fn start_resident(
        &self,
        resident_key: ResidentSolverKey,
        solver_library: &str,
        solver_core: &str,
    ) -> std::result::Result<ResidentSolverProcess, SolverError> {
        let harness_path = self.cache_dir.join("solver/harness.js");
        let harness_source = harness_source(solver_library, solver_core);
        let cached_harness = tokio::fs::read_to_string(&harness_path).await.ok();
        if cached_harness.as_deref() != Some(harness_source.as_str()) {
            write_cache(&harness_path, harness_source.as_bytes()).await;
        }
        let mut node_process = tokio::process::Command::new(&self.node)
            .arg("--permission")
            .arg(format!("--allow-fs-read={}", self.cache_dir.display()))
            .arg(&harness_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => SolverError::NodeMissing,
                _ => SolverError::Failed(format!("node: {error}")),
            })?;
        let node_stdin = node_process
            .stdin
            .take()
            .expect("stdin was piped three lines above");
        let node_stdout = node_process
            .stdout
            .take()
            .expect("stdout was piped three lines above");
        Ok(ResidentSolverProcess {
            key: resident_key,
            node_process,
            input_pipe: Some(node_stdin),
            output_lines: BufReader::new(node_stdout).lines(),
        })
    }

    /// Node runs under its permission model with no allowances, so the solver
    /// script reaches neither the filesystem nor the network.
    async fn run_node(&self, node_input: String) -> std::result::Result<String, SolverError> {
        let mut node_process = tokio::process::Command::new(&self.node)
            .arg("--permission")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => SolverError::NodeMissing,
                _ => SolverError::Failed(format!("node: {error}")),
            })?;
        let mut node_stdin = node_process.stdin.take().expect("stdin was piped one line above");
        let node_run = async move {
            let feed_stdin = async move {
                node_stdin.write_all(node_input.as_bytes()).await?;
                node_stdin.shutdown().await
            };
            let (stdin_result, process_result) = tokio::join!(feed_stdin, node_process.wait_with_output());
            stdin_result.map_err(|error| SolverError::Failed(format!("node stdin: {error}")))?;
            process_result.map_err(|error| SolverError::Failed(format!("node: {error}")))
        };
        let process_result = tokio::time::timeout(self.timeout, node_run).await;
        let process_output = match process_result {
            Ok(Ok(process_output)) => process_output,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(SolverError::Deadline),
        };
        if !process_output.status.success() {
            let standard_error = String::from_utf8_lossy(&process_output.stderr);
            return Err(SolverError::Failed(format!(
                "node exited {}: {}",
                process_output.status,
                standard_error.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&process_output.stdout).into_owned())
    }
}

fn player_url(player_id: &str) -> String {
    format!("https://www.youtube.com/s/player/{player_id}/player_ias.vflset/en_US/base.js")
}

/// The line protocol the harness speaks. One request in, one reply out.
struct ResidentSolverProcess {
    key: ResidentSolverKey,
    node_process: tokio::process::Child,
    /// Taken to end the process: the harness exits when its stdin closes.
    input_pipe: Option<tokio::process::ChildStdin>,
    output_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl ResidentSolverProcess {
    /// Closing stdin is what the harness waits on, so the kill is only for a
    /// process wedged inside a solve.
    #[cfg(test)]
    async fn stop(&mut self) {
        self.input_pipe = None;
        if tokio::time::timeout(SHUTDOWN_BUDGET, self.node_process.wait())
            .await
            .is_err()
        {
            let _ = self.node_process.kill().await;
        }
    }

    async fn exchange(&mut self, solver_request: &Value) -> std::result::Result<Value, SolverError> {
        let mut request_line =
            serde_json::to_string(solver_request).map_err(|error| SolverError::Failed(error.to_string()))?;
        request_line.push('\n');
        let input_pipe = self
            .input_pipe
            .as_mut()
            .ok_or_else(|| SolverError::Failed("the solver was shut down".to_owned()))?;
        input_pipe
            .write_all(request_line.as_bytes())
            .await
            .map_err(|error| SolverError::Failed(format!("solver stdin: {error}")))?;
        input_pipe
            .flush()
            .await
            .map_err(|error| SolverError::Failed(format!("solver stdin: {error}")))?;
        let reply_line = self
            .output_lines
            .next_line()
            .await
            .map_err(|error| SolverError::Failed(format!("solver stdout: {error}")))?
            .ok_or_else(|| SolverError::Failed("the solver process closed".to_owned()))?;
        serde_json::from_str(&reply_line)
            .map_err(|error| SolverError::Failed(format!("solver reply: {error}")))
    }
}

impl Drop for ResidentSolverProcess {
    fn drop(&mut self) {
        self.input_pipe = None;
        let _ = self.node_process.start_kill();
    }
}

/// The harness: the two yt-dlp scripts, then a loop that answers one line at a
/// time and keeps each preprocessed player under the version that produced it.
fn harness_source(library: &str, core: &str) -> String {
    format!(
        r#"{library}
Object.assign(globalThis, lib);
{core}
const fs = require("node:fs");
const held = {{}};
const reply = (value) => process.stdout.write(JSON.stringify(value) + "\n");
let buffered = "";
process.stdin.setEncoding("utf8");
process.stdin.on("end", () => process.exit(0));
process.stdin.on("data", (chunk) => {{
  buffered += chunk;
  let end;
  while ((end = buffered.indexOf("\n")) >= 0) {{
    const line = buffered.slice(0, end);
    buffered = buffered.slice(end + 1);
    if (!line) continue;
    try {{
      const ask = JSON.parse(line);
      if (ask.refresh) delete held[ask.key];
      if (!ask.refresh && !held[ask.key] && ask.path) {{
        try {{ held[ask.key] = JSON.parse(fs.readFileSync(ask.path, "utf8")); }} catch (e) {{}}
      }}
      if (held[ask.key]) {{
        reply(jsc({{type: "preprocessed", preprocessed_player: held[ask.key], requests: ask.requests}}));
      }} else if (ask.player) {{
        const out = jsc({{type: "player", player: ask.player, requests: ask.requests, output_preprocessed: true}});
        if (out.preprocessed_player) held[ask.key] = out.preprocessed_player;
        reply(out);
      }} else {{
        reply({{type: "need_player"}});
      }}
    }} catch (e) {{
      reply({{type: "error", error: String(e)}});
    }}
  }}
}});
"#
    )
}

/// The player version the embed API points at, for example `f572e43c`.
fn scrape_player_id(iframe: &str) -> Option<String> {
    for marker in ["player\\/", "player/"] {
        for (offset, _) in iframe.match_indices(marker) {
            let start = offset + marker.len();
            let id: String = iframe[start..]
                .chars()
                .take_while(|character| character.is_ascii_hexdigit())
                .collect();
            if id.len() == 8 {
                return Some(id);
            }
        }
    }
    None
}

/// A cache write is an optimisation, so a failure to store is never a failure
/// to solve.
async fn write_cache(cache_path: &Path, cache_contents: &[u8]) {
    if let Some(parent_directory) = cache_path.parent()
        && tokio::fs::create_dir_all(parent_directory).await.is_err()
    {
        return;
    }
    // Concurrent prefetches must never read a partial script.
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let cache_filename = cache_path.file_name().unwrap_or_default().to_string_lossy();
    let temporary_path = cache_path.with_file_name(format!(
        ".{cache_filename}.{}.{}.tmp",
        std::process::id(),
        NEXT_WRITE.fetch_add(1, Ordering::Relaxed),
    ));
    if tokio::fs::write(&temporary_path, cache_contents).await.is_ok() {
        let _ = tokio::fs::rename(&temporary_path, cache_path).await;
    }
    let _ = tokio::fs::remove_file(temporary_path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_player_version_is_read_from_the_embed_api() {
        let iframe = r#"...,"player\/f572e43c\/www-widgetapi.vflset",...);"#;
        assert_eq!(scrape_player_id(iframe).as_deref(), Some("f572e43c"));
        assert_eq!(
            scrape_player_id("https://www.youtube.com/s/player/f572e43c/base.js").as_deref(),
            Some("f572e43c")
        );
        assert_eq!(scrape_player_id("no player here"), None);
    }

    /// Stand in for the two yt-dlp scripts, so the harness starts on a machine
    /// that holds neither.
    const STUB_LIBRARY: &str = "const lib = {};";
    const STUB_CORE: &str = "function jsc() { return {type: \"result\", responses: []}; }";

    fn stub_key(solver: &NodeSolver) -> ResidentSolverKey {
        solver.resident_process_key("stub")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("redwax-solver-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn shared_resident_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        &LOCK
    }

    #[tokio::test]
    async fn the_resident_solver_never_outlives_its_owner() {
        if !find_node().is_file() {
            // Without node there is no step 3 and no process to orphan.
            return;
        }
        let dir = scratch("resident");
        let solver = NodeSolver::new(dir.clone());
        let resident = solver
            .start_resident(stub_key(&solver), STUB_LIBRARY, STUB_CORE)
            .await
            .expect("node starts the harness");
        let pid = resident.node_process.id().expect("a running child has a pid");
        let entry = PathBuf::from(format!("/proc/{pid}"));
        assert!(entry.exists(), "the harness is running");

        drop(resident);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while entry.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!entry.exists(), "node {pid} outlived the solver that spawned it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_resident_solver_leaves_when_its_stdin_closes() {
        if !find_node().is_file() {
            return;
        }
        let dir = scratch("stdin");
        let solver = NodeSolver::new(dir.clone());
        let mut resident = solver
            .start_resident(stub_key(&solver), STUB_LIBRARY, STUB_CORE)
            .await
            .expect("node starts the harness");

        resident.input_pipe = None;
        let status = tokio::time::timeout(SHUTDOWN_BUDGET, resident.node_process.wait())
            .await
            .expect("the harness leaves its read loop when the pipe closes")
            .expect("the child is waited on once");
        assert!(status.success(), "the harness exits, it is not killed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shutdown_ends_the_resident_owned_by_the_solver() {
        let _serial = shared_resident_test_lock().lock().await;
        if !find_node().is_file() {
            return;
        }
        let dir = scratch("shutdown");
        let solver = NodeSolver::new(dir.clone());
        let resident = solver
            .start_resident(stub_key(&solver), STUB_LIBRARY, STUB_CORE)
            .await
            .expect("node starts the harness");
        let pid = resident.node_process.id().expect("a running child has a pid");
        *solver
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is available") = Some(resident);

        solver.shutdown().await;
        assert!(
            solver
                .process_state
                .resident_process
                .lock()
                .expect("the resident slot is available")
                .is_none()
        );
        let entry = PathBuf::from(format!("/proc/{pid}"));
        assert!(!entry.exists(), "shutdown waits for the process it ended");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shutting_down_one_solver_leaves_another_resident_running() {
        if !find_node().is_file() {
            return;
        }
        let first_dir = scratch("isolated-first");
        let second_dir = scratch("isolated-second");
        let first = NodeSolver::new(first_dir.clone());
        let second = NodeSolver::new(second_dir.clone());
        for solver in [&first, &second] {
            let resident = solver
                .start_resident(stub_key(solver), STUB_LIBRARY, STUB_CORE)
                .await
                .expect("node starts the harness");
            *solver
                .process_state
                .resident_process
                .lock()
                .expect("the resident slot is available") = Some(resident);
        }
        let second_pid = second
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is available")
            .as_ref()
            .and_then(|resident| resident.node_process.id())
            .expect("the second solver has a running child");

        first.shutdown().await;

        assert!(
            second
                .process_state
                .resident_process
                .lock()
                .expect("the resident slot is available")
                .is_some()
        );
        assert!(PathBuf::from(format!("/proc/{second_pid}")).exists());
        second.shutdown().await;
        let _ = std::fs::remove_dir_all(first_dir);
        let _ = std::fs::remove_dir_all(second_dir);
    }

    #[tokio::test]
    async fn cancelled_exchange_discards_its_resident_before_the_next_request() {
        let _serial = shared_resident_test_lock().lock().await;
        if !find_node().is_file() {
            return;
        }
        let dir = scratch("cancelled-exchange");
        let solver = NodeSolver::new(dir.clone());
        let scripts = SolverScripts {
            library: STUB_LIBRARY.to_owned(),
            core: r#"function jsc(ask) {
                if (ask.requests[0].cancel) { while (true) {} }
                return {type: "result", responses: [{data: {fresh: "reply"}}]};
            }"#
            .to_owned(),
            namespace: "cancel-stub".to_owned(),
        };
        let resident = solver
            .start_resident(
                solver.resident_process_key(&scripts.namespace),
                &scripts.library,
                &scripts.core,
            )
            .await
            .expect("node starts the harness");
        *solver
            .process_state
            .resident_process
            .lock()
            .expect("the resident slot is available") = Some(resident);
        let player = script("player");
        let cache = dir.join("cancelled.pre.json");
        let requests = [json!({"cancel": true})];
        let mut cancelled = Box::pin(solver.ask_resident(&player, &cache, &requests, &scripts, false));
        tokio::select! {
            biased;
            _ = &mut cancelled => panic!("the stub cannot answer the cancelled request"),
            _ = std::future::ready(()) => {}
        }
        drop(cancelled);
        assert!(
            solver
                .process_state
                .resident_process
                .lock()
                .expect("the resident slot is available")
                .is_none()
        );

        let response = solver
            .ask_resident(&player, &cache, &[json!({"cancel": false})], &scripts, false)
            .await
            .expect("the next request uses a new resident");
        assert_eq!(response["responses"][0]["data"]["fresh"], "reply");
        solver.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_names_its_version_from_the_url() {
        let script = PlayerScript {
            url: "https://www.youtube.com/s/player/f572e43c/player_ias.vflset/en_US/base.js".to_owned(),
            body: String::new(),
        };
        assert_eq!(script.id(), "f572e43c");
    }

    fn script(body: &str) -> PlayerScript {
        PlayerScript {
            url: player_url("f572e43c"),
            body: body.to_owned(),
        }
    }

    #[test]
    fn signature_timestamp_accepts_real_player_forms() {
        assert_eq!(
            script("a={signatureTimestamp:20719}").signature_timestamp().ok(),
            Some(20719)
        );
        assert_eq!(
            script("a={ sts : 20718 }").signature_timestamp().ok(),
            Some(20718)
        );
        assert_eq!(
            script("a={'signatureTimestamp':20717}")
                .signature_timestamp()
                .ok(),
            Some(20717)
        );
    }

    #[test]
    fn signature_timestamp_ignores_longer_identifiers() {
        let player = script("a={old_signatureTimestamp:11111,signatureTimestamp:x,sts:20719}");
        assert_eq!(player.signature_timestamp().ok(), Some(20719));
    }

    #[test]
    fn signature_timestamp_rejects_missing_short_and_overflowing_values() {
        assert!(script("a={name:20719}").signature_timestamp().is_err());
        assert!(script("a={sts:1234}").signature_timestamp().is_err());
        assert!(script("a={sts:20719.5}").signature_timestamp().is_err());
        assert!(script("a={sts:20719e2}").signature_timestamp().is_err());
        assert!(
            script("a={sts:99999999999999999999}")
                .signature_timestamp()
                .is_err()
        );
    }

    #[test]
    fn cache_namespaces_include_player_variant_and_solver_code() {
        let main = "https://www.youtube.com/s/player/f572e43c/player_ias.vflset/en_US/base.js";
        let tv = "https://www.youtube.com/s/player/f572e43c/player_es6.vflset/en_US/base.js";
        assert_ne!(player_cache_namespace(main), player_cache_namespace(tv));
        assert_ne!(
            preprocessed_namespace("solver-a", main),
            preprocessed_namespace("solver-a", tv)
        );
        assert_ne!(
            preprocessed_namespace("solver-a", main),
            preprocessed_namespace("solver-b", main)
        );
    }

    #[test]
    fn solver_code_accepts_raw_scripts_and_the_yt_dlp_cache_envelope() {
        assert_eq!(
            solver_code("const core = {};"),
            Some("const core = {};".to_owned())
        );
        assert_eq!(
            solver_code(r#"{"data":{"code":"const core = {};"}}"#),
            Some("const core = {};".to_owned())
        );
        assert_eq!(solver_code(r#"{"data":{"code":""}}"#), None);
    }

    #[test]
    fn script_discovery_supports_the_companion_python_package() {
        let installation_directory = scratch("yt-dlp-ejs-scripts");
        let library_path =
            installation_directory.join("lib/python-test/site-packages/yt_dlp_ejs/yt/solver/lib.min.js");
        let core_path =
            installation_directory.join("lib/python-test/site-packages/yt_dlp_ejs/yt/solver/core.min.js");
        std::fs::create_dir_all(core_path.parent().expect("the core has a package directory"))
            .expect("the package directory is writable");
        std::fs::write(&library_path, "const lib = {};").expect("the library fixture is writable");
        std::fs::write(&core_path, "const core = {};").expect("the core fixture is writable");

        assert_eq!(
            script_under(&installation_directory, &LIBRARY_PATHS),
            Some(library_path)
        );
        assert_eq!(
            script_under(&installation_directory, &CORE_PATHS),
            Some(core_path)
        );

        let _ = std::fs::remove_dir_all(installation_directory);
    }

    #[tokio::test]
    async fn concurrent_cache_writes_publish_a_complete_file() {
        let dir = scratch("concurrent-writes");
        let path = dir.join("player.js");
        let first = vec![b'a'; 65_536];
        let second = vec![b'b'; 32_768];
        tokio::join!(write_cache(&path, &first), write_cache(&path, &second));
        let written = tokio::fs::read(&path)
            .await
            .expect("one complete write was published");
        assert!(written == first || written == second);
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .expect("the cache directory exists");
        assert!(entries.next_entry().await.unwrap().is_some());
        assert!(entries.next_entry().await.unwrap().is_none());
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
