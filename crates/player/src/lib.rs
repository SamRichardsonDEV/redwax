//! mpv as the audio engine, driven over its JSON IPC socket.
//!
//! The queue is mpv's playlist. Entries are stream URLs or file paths when
//! known and YouTube watch URLs otherwise. The load hook gives Rust a chance
//! to resolve each entry before mpv's yt-dlp hook runs.

pub mod ipc;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use redwax_ytm::Track;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::ipc::{BoxFuture, DynIpc, Ipc, MpvIpc, OBSERVED};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
}

/// Identity of one logical queue entry. Equal tracks still have different
/// identities, and stream replacement keeps the logical id it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueEntryId {
    player_instance_id: u64,
    entry_sequence: u64,
}

impl std::fmt::Display for QueueEntryId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:x}_{:x}",
            self.player_instance_id, self.entry_sequence
        )
    }
}

/// Version of the observed queue that supplied a logical entry id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueRevision(u64);

#[derive(Debug, Clone, PartialEq)]
pub struct QueueEntry {
    logical_id: QueueEntryId,
    track: Track,
}

impl QueueEntry {
    pub fn id(&self) -> QueueEntryId {
        self.logical_id
    }

    pub fn track(&self) -> &Track {
        &self.track
    }

    #[doc(hidden)]
    pub fn synthetic(entry_sequence: u64, track: Track) -> QueueEntry {
        QueueEntry {
            logical_id: QueueEntryId {
                player_instance_id: 0,
                entry_sequence,
            },
            track,
        }
    }
}

impl QueueRevision {
    #[doc(hidden)]
    pub fn synthetic(queue_revision_number: u64) -> QueueRevision {
        QueueRevision(queue_revision_number)
    }
}

/// Result of a queue edit addressed to an observed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEdit {
    Applied,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackAvailability {
    #[default]
    Ready,
    Reconnecting,
    RestoredPaused,
    Unavailable,
}

/// Everything the UI and MPRIS show, decoded from mpv's observed properties.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    /// Compatibility projection of `queue_entries` while callers migrate.
    pub queue: Vec<Track>,
    /// Entries in mpv's observed order. Each entry corresponds to the same
    /// position in `queue`.
    pub queue_entries: Vec<QueueEntry>,
    /// Changes on every observed playlist and when a new engine starts.
    pub queue_revision: QueueRevision,
    pub index: Option<usize>,
    pub position: f64,
    pub duration: Option<f64>,
    pub paused: bool,
    pub volume: u8,
    pub muted: bool,
    pub availability: PlaybackAvailability,
    /// The requested track can differ from the queue while resolution is pending.
    pub pending_track: Option<Track>,
    /// Resolution and mpv buffering both delay audio after a playback request.
    pub loading: bool,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// Kept until a playback command so rapid property updates cannot hide a failure.
    pub error: Option<String>,
    /// The tier of the playing entry when the stream source told us, for the bar.
    pub tier_label: Option<String>,
    /// A warning attached to the selected stream, for the interface to show once.
    pub warning: Option<String>,
}

impl Default for PlayerState {
    fn default() -> Self {
        PlayerState {
            queue: Vec::new(),
            queue_entries: Vec::new(),
            queue_revision: QueueRevision::default(),
            index: None,
            position: 0.0,
            duration: None,
            paused: false,
            volume: 80,
            muted: false,
            availability: PlaybackAvailability::Ready,
            pending_track: None,
            loading: false,
            repeat: Repeat::Off,
            shuffle: false,
            error: None,
            tier_label: None,
            warning: None,
        }
    }
}

impl PlayerState {
    pub fn track(&self) -> Option<&Track> {
        self.index.and_then(|queue_index| self.queue.get(queue_index))
    }

    /// Upcoming tracks wrap only when mpv repeats the queue.
    pub fn upcoming(&self, limit: usize) -> Vec<Track> {
        let (Some(index), false) = (self.index, self.queue.is_empty()) else {
            return Vec::new();
        };
        let queue_length = self.queue.len();
        let upcoming_track_count = match self.repeat {
            Repeat::All => (queue_length - 1).min(limit),
            _ => queue_length.saturating_sub(index + 1).min(limit),
        };
        (1..=upcoming_track_count)
            .map(|offset| self.queue[(index + offset) % queue_length].clone())
            .collect()
    }
}

/// A resolved playable location handed to mpv, plus what to show for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playable {
    /// A direct URL or a file path, exactly what mpv's `loadfile` takes.
    pub target: String,
    /// For the player bar, for example `774 opus 283k`.
    pub tier_label: String,
    /// A degraded path the interface should make visible when this stream plays.
    pub warning: Option<String>,
}

/// Stream resolution without coupling playback to one resolver.
pub trait StreamSource: Send + Sync + 'static {
    /// A fresh stream already known, without waiting.
    fn cached(&self, video_id: &str) -> Option<Playable>;

    /// A source that already exhausted its fallback can make failure terminal.
    fn allow_mpv_fallback(&self) -> bool {
        true
    }

    /// Resolve now. `Ok(None)` permits mpv fallback only when the source allows it.
    fn resolve(
        &self,
        video_id: &str,
    ) -> impl Future<Output = std::result::Result<Option<Playable>, String>> + Send;

    fn resolve_prefetch(&self, video_id: &str) -> impl Future<Output = Option<Playable>> + Send {
        async move { self.resolve(video_id).await.ok().flatten() }
    }

    /// Start resolving in the background.
    fn prefetch(&self, video_ids: Vec<String>);

    /// The URL turned out dead. Drop it so the next resolve fetches again.
    fn forget(&self, video_id: &str);
}

/// Playback operations shared by the terminal app, MPRIS, and their fakes.
pub trait PlayerControl: Clone + Send + Sync + 'static {
    fn state(&self) -> watch::Receiver<PlayerState>;
    fn snapshot(&self) -> PlayerState;
    fn start(&self) -> impl Future<Output = Result<()>> + Send;
    fn close(&self) -> impl Future<Output = Result<()>> + Send;
    fn retry_playback(&self) -> impl Future<Output = Result<()>> + Send;
    fn reconcile_queue(&self) -> impl Future<Output = Result<PlayerState>> + Send {
        async { Ok(self.snapshot()) }
    }
    fn play(&self, tracks: Vec<Track>, index: usize) -> impl Future<Output = Result<()>> + Send;
    fn play_from(
        &self,
        tracks: Vec<Track>,
        index: usize,
        seconds: f64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn enqueue(&self, tracks: Vec<Track>, play_next: bool) -> impl Future<Output = Result<()>> + Send;
    fn replace_upcoming(&self, tracks: Vec<Track>) -> impl Future<Output = Result<()>> + Send;
    fn jump(&self, index: usize) -> impl Future<Output = Result<()>> + Send;
    fn remove(&self, index: usize) -> impl Future<Output = Result<()>> + Send;
    fn remove_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
    ) -> impl Future<Output = Result<QueueEdit>> + Send;
    fn move_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
        destination_index: usize,
    ) -> impl Future<Output = Result<QueueEdit>> + Send;
    fn clear(&self) -> impl Future<Output = Result<()>> + Send;
    fn toggle_pause(&self) -> impl Future<Output = Result<()>> + Send;
    fn next(&self) -> impl Future<Output = Result<()>> + Send;
    fn previous(&self) -> impl Future<Output = Result<()>> + Send;
    fn seek(&self, delta_seconds: f64) -> impl Future<Output = Result<()>> + Send;
    fn seek_to(&self, seconds: f64) -> impl Future<Output = Result<()>> + Send;
    fn adjust_volume(&self, delta: i16) -> impl Future<Output = Result<()>> + Send;
    fn set_volume(&self, volume: u8) -> impl Future<Output = Result<()>> + Send;
    fn toggle_mute(&self) -> impl Future<Output = Result<()>> + Send;
    fn set_audio_filter(&self, graph: Option<&str>) -> impl Future<Output = Result<()>> + Send;
    fn set_repeat(&self, mode: Repeat) -> impl Future<Output = Result<()>> + Send;
    fn cycle_repeat(&self) -> impl Future<Output = Result<Repeat>> + Send;
    fn set_shuffle(&self, shuffle_enabled: bool) -> impl Future<Output = Result<()>> + Send;
    fn toggle_shuffle(&self) -> impl Future<Output = Result<bool>> + Send;
    fn prefetch(&self, tracks: Vec<Track>);
}

/// Object-safe `StreamSource`, so `Player` stays free of the source's type.
trait DynStreamSource: Send + Sync + 'static {
    fn cached(&self, video_id: &str) -> Option<Playable>;
    fn allow_mpv_fallback(&self) -> bool;
    fn resolve<'a>(
        &'a self,
        video_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Playable>, String>>;
    fn resolve_prefetch<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Option<Playable>>;
    fn forget(&self, video_id: &str);
}

impl<S: StreamSource> DynStreamSource for S {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        StreamSource::cached(self, video_id)
    }

    fn allow_mpv_fallback(&self) -> bool {
        StreamSource::allow_mpv_fallback(self)
    }

    fn resolve<'a>(
        &'a self,
        video_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<Playable>, String>> {
        Box::pin(StreamSource::resolve(self, video_id))
    }

    fn resolve_prefetch<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Option<Playable>> {
        Box::pin(StreamSource::resolve_prefetch(self, video_id))
    }

    fn forget(&self, video_id: &str) {
        StreamSource::forget(self, video_id)
    }
}

#[derive(Debug, Clone)]
pub struct PlayerOptions {
    pub volume: u8,
    /// Extra mpv arguments from `[mpv] args`.
    pub mpv_args: Vec<String>,
    /// mpv's `--ytdl-raw-options` value, for entries that reach mpv unresolved.
    pub ytdl_raw_options: String,
    /// Where the IPC socket is created. Defaults to the runtime dir.
    pub socket_dir: PathBuf,
    pub prefetch_ahead: usize,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions {
            volume: 80,
            mpv_args: Vec::new(),
            ytdl_raw_options: String::new(),
            socket_dir: runtime_dir(),
            prefetch_ahead: 3,
        }
    }
}

/// The session directory, which is cleaned up at logout, else the temporary one.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mpv is not installed or not on PATH")]
    MpvMissing,
    #[error("mpv: {0}")]
    Mpv(String),
    #[error("mpv exited")]
    Exited,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub type TimingSink = dyn Fn(u64, &str, &'static str, Duration) + Send + Sync;

static NEXT_PLAYER_INSTANCE: AtomicU64 = AtomicU64::new(1);

struct PlaybackTiming {
    playback_generation: u64,
    video_id: String,
    started_at: Instant,
    expected_mpv_entry_id: Option<i64>,
    queue_commands_sent: bool,
    loaded: bool,
    playing_timing_reported: bool,
}

impl PlaybackTiming {
    fn event(&self, current_revision: u64) -> Option<(u64, String, Duration)> {
        (self.playback_generation == current_revision).then(|| {
            (
                self.playback_generation,
                self.video_id.clone(),
                self.started_at.elapsed(),
            )
        })
    }
}

/// Playback handles share one engine and observe state without polling.
#[derive(Clone)]
pub struct Player {
    inner: Arc<PlayerInner>,
    _lifetime: Arc<PlayerLifetime>,
}

/// Public handles own cancellation because background tasks retain player state.
struct PlayerLifetime {
    inner: Weak<PlayerInner>,
}

impl Drop for PlayerLifetime {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let engine = std::mem::replace(
            &mut *inner.lifecycle.state.lock().expect("player lifecycle"),
            LifecycleState::Stopped,
        );
        drop(engine);
        inner.background_tasks.lock().expect("player tasks").abort_all();
    }
}

struct PlayerInner {
    options: PlayerOptions,
    state: watch::Sender<PlayerState>,
    stream_source: Option<Arc<dyn DynStreamSource>>,
    lifecycle: Lifecycle,
    bookkeeping: Mutex<PlayerBookkeeping>,
    queue_edit_lock: tokio::sync::Mutex<()>,
    current_playback_request_id: Mutex<u64>,
    playback_generation: AtomicU64,
    next_fallback_request_id: AtomicU64,
    playback_generation_changed: tokio::sync::Notify,
    timing_sink: Mutex<Option<Arc<TimingSink>>>,
    timing_enabled: AtomicBool,
    restart_attempted: AtomicBool,
    restart_snapshot: Mutex<Option<PlayerState>>,
    background_tasks: Mutex<JoinSet<()>>,
}

/// Dropping an obsolete request must not clear the latest selection.
struct PendingPlayback<'a> {
    player: &'a PlayerInner,
    playback_generation: u64,
    jump_mpv_entry_id: Option<i64>,
}

enum SelectedStream {
    Ready(Option<Playable>),
    Cancelled,
    Failed,
}

impl Drop for PendingPlayback<'_> {
    fn drop(&mut self) {
        if let Some(mpv_entry_id) = self.jump_mpv_entry_id {
            let mut bookkeeping = self.player.bookkeeping.lock().expect("player bookkeeping state");
            if bookkeeping.pending_jump == Some((mpv_entry_id, self.playback_generation)) {
                bookkeeping.pending_jump = None;
            }
        }
        self.player.state.send_modify(|state| {
            if self.player.playback_generation.load(Ordering::SeqCst) == self.playback_generation {
                state.pending_track = None;
                let bookkeeping = self.player.bookkeeping.lock().expect("player bookkeeping state");
                state.loading = bookkeeping.loading(state);
            }
        });
    }
}

struct MpvSession {
    ipc: Arc<MpvIpc>,
    mpv_process: tokio::process::Child,
}

struct EngineResources {
    socket_path: PathBuf,
    resolver_script_path: PathBuf,
}

impl Drop for EngineResources {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.resolver_script_path);
    }
}

struct RunningEngine {
    engine_generation: u64,
    ipc: Arc<dyn DynIpc>,
    session: Option<MpvSession>,
    _resources: Option<EngineResources>,
    event_reader_task: Option<JoinHandle<()>>,
}

impl Drop for RunningEngine {
    fn drop(&mut self) {
        if let Some(event_reader_task) = self.event_reader_task.take() {
            event_reader_task.abort();
        }
        if let Some(session) = &self.session {
            session.ipc.shutdown_now();
        }
    }
}

struct Closing<'a> {
    lifecycle: &'a Lifecycle,
    engine_generation: u64,
    engine: Option<Box<RunningEngine>>,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        drop(self.engine.take());
        let mut state = self.lifecycle.state.lock().expect("player lifecycle");
        if matches!(
            *state,
            LifecycleState::Closing(engine_generation)
                if engine_generation == self.engine_generation
        ) {
            *state = LifecycleState::Stopped;
        }
        drop(state);
        self.lifecycle.closing_completed.notify_waiters();
    }
}

/// mpv reports insertion and removal separately, while consumers own one logical queue entry.
/// An interrupted replacement retires the engine before those partial edits become public.
struct EntryReplacement {
    player: Arc<PlayerInner>,
    engine_generation: u64,
    old_mpv_entry_id: i64,
    logical_id: Option<QueueEntryId>,
    track: Track,
    inserted_mpv_entry_id: Option<i64>,
    completed: bool,
}

impl EntryReplacement {
    fn complete(mut self, replacement: Option<(i64, &str, usize)>, playlist: Value) {
        let lifecycle = self.player.lifecycle.state.lock().expect("player lifecycle");
        if !matches!(&*lifecycle, LifecycleState::Running(engine) if engine.engine_generation == self.engine_generation)
        {
            drop(lifecycle);
            return;
        }
        let mut bookkeeping = self.player.bookkeeping.lock().expect("player bookkeeping state");
        if let Some((mpv_entry_id, target, index)) = replacement {
            bookkeeping.retired_mpv_entry_ids.insert(self.old_mpv_entry_id);
            bookkeeping.mpv_entries.remove(&self.old_mpv_entry_id);
            bookkeeping
                .mpv_entries
                .insert(mpv_entry_id, (target.to_string(), index));
            bookkeeping
                .logical_entry_ids_by_mpv_id
                .remove(&self.old_mpv_entry_id);
            if let Some(logical_id) = self.logical_id {
                bookkeeping
                    .logical_entry_ids_by_mpv_id
                    .insert(mpv_entry_id, logical_id);
            }
            bookkeeping.tracks_by_mpv_entry_id.remove(&self.old_mpv_entry_id);
            bookkeeping
                .tracks_by_mpv_entry_id
                .insert(mpv_entry_id, self.track.clone());
            if bookkeeping.playing_mpv_entry_id == Some(self.old_mpv_entry_id) {
                bookkeeping.playing_mpv_entry_id = Some(mpv_entry_id);
            }
        }
        if replacement.is_none()
            && let Some(inserted_mpv_entry_id) = self.inserted_mpv_entry_id
        {
            bookkeeping.retired_mpv_entry_ids.insert(inserted_mpv_entry_id);
        }
        bookkeeping.replacing_mpv_entry_id = None;
        self.completed = true;
        drop(bookkeeping);
        self.player
            .handle_event(&json!({"event": "property-change", "name": "playlist", "data": playlist}));
    }
}

impl Drop for EntryReplacement {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut bookkeeping = self.player.bookkeeping.lock().expect("player bookkeeping state");
        if bookkeeping.replacing_mpv_entry_id == Some(self.old_mpv_entry_id) {
            bookkeeping.replacing_mpv_entry_id = None;
        }
        drop(bookkeeping);
        self.player.engine_gone(self.engine_generation);
    }
}

const ENGINE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaybackIntent {
    Play,
    RestorePaused,
}

enum QueueLoad {
    Applied,
    Cancelled,
    ResolutionFailed,
}

struct SwapReservation {
    player: Arc<PlayerInner>,
    video_id: String,
}

impl Drop for SwapReservation {
    fn drop(&mut self) {
        self.player
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .swapping_video_ids
            .remove(&self.video_id);
    }
}

enum LifecycleState {
    Stopped,
    Starting(u64),
    RestartQueued(u64),
    Running(Box<RunningEngine>),
    Closing(u64),
}

struct Lifecycle {
    player_instance_id: u64,
    next_engine_generation: AtomicU64,
    transition_lock: tokio::sync::Mutex<()>,
    closing_completed: tokio::sync::Notify,
    state: Mutex<LifecycleState>,
    launcher: Arc<dyn EngineLauncher>,
}

struct LaunchedEngine {
    ipc: Arc<dyn DynIpc>,
    event_receiver: mpsc::UnboundedReceiver<Value>,
    session: Option<MpvSession>,
}

trait EngineLauncher: Send + Sync {
    fn launch<'a>(
        &'a self,
        options: &'a PlayerOptions,
        resources: &'a EngineResources,
    ) -> BoxFuture<'a, Result<LaunchedEngine>>;
}

struct MpvLauncher;

impl EngineLauncher for MpvLauncher {
    fn launch<'a>(
        &'a self,
        options: &'a PlayerOptions,
        resources: &'a EngineResources,
    ) -> BoxFuture<'a, Result<LaunchedEngine>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&options.socket_dir).await?;
            let _ = tokio::fs::remove_file(&resources.socket_path).await;
            tokio::fs::write(&resources.resolver_script_path, include_bytes!("resolve.lua")).await?;

            let mpv_arguments = mpv_argv_with_script(
                options,
                &resources.socket_path,
                Some(&resources.resolver_script_path),
            );
            let spawn_result = tokio::process::Command::new(&mpv_arguments[0])
                .args(&mpv_arguments[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let mut mpv_process = match spawn_result {
                Ok(mpv_process) => mpv_process,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::MpvMissing);
                }
                Err(error) => return Err(error.into()),
            };

            let (event_sender, event_receiver) = mpsc::unbounded_channel();
            let ipc =
                match MpvIpc::connect(&resources.socket_path, event_sender, Some(&mut mpv_process)).await {
                    Ok(ipc) => Arc::new(ipc),
                    Err(error) => {
                        let _ = mpv_process.kill().await;
                        return Err(error);
                    }
                };
            for property in OBSERVED {
                if let Err(error) = Ipc::observe(ipc.as_ref(), property).await {
                    ipc.close().await;
                    let _ = mpv_process.kill().await;
                    return Err(error);
                }
            }
            Ok(LaunchedEngine {
                ipc: Arc::clone(&ipc) as Arc<dyn DynIpc>,
                event_receiver,
                session: Some(MpvSession { ipc, mpv_process }),
            })
        })
    }
}

struct Starting<'a> {
    lifecycle: &'a Lifecycle,
    engine_generation: u64,
    armed: bool,
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.lifecycle.state.lock().expect("player lifecycle");
        if matches!(
            *state,
            LifecycleState::Starting(engine_generation)
                if engine_generation == self.engine_generation
        ) {
            *state = LifecycleState::Stopped;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoveryStage {
    NativeRetry,
    FinalFallback,
}

/// The part of the player mpv's events read and write that is not in
/// `PlayerState`, kept behind one lock so an event applies as a whole.
struct PlayerBookkeeping {
    /// Stream URL handed to mpv, to the video id it plays. An entry that fails
    /// is traced back to its track through this.
    stream_video_ids_by_target: HashMap<String, String>,
    mpv_entries: HashMap<i64, (String, usize)>,
    replacing_mpv_entry_id: Option<i64>,
    retired_mpv_entry_ids: HashSet<i64>,
    logical_entry_ids_by_mpv_id: HashMap<i64, QueueEntryId>,
    restart_logical_entry_ids: Option<Vec<QueueEntryId>>,
    next_logical_entry_sequence: u64,
    tracks_by_mpv_entry_id: HashMap<i64, Track>,
    known_tracks_by_video_id: HashMap<String, Track>,
    tier_labels_by_video_id: HashMap<String, String>,
    warnings_by_video_id: HashMap<String, String>,
    // Recovery can replace the playlist entry before the selected song loads.
    pending_resume: Option<(u64, String, f64, PlaybackIntent)>,
    /// A stable logical entry id keeps repeated input from cancelling its own request.
    pending_jump: Option<(i64, u64)>,
    /// Ids handed to mpv whose playlist entry has not come back yet. Without
    /// them a prune drops a track while its own loadfile is still in flight.
    queueing_video_ids: HashSet<String>,
    /// Video ids with a swap task running. A second task for the same id would
    /// insert against a playlist mpv has not reported back yet.
    swapping_video_ids: HashSet<String>,
    recovering_mpv_entry_ids: HashSet<i64>,
    recovery_stage_by_mpv_entry_id: HashMap<i64, RecoveryStage>,
    fallback_acknowledgements: HashMap<String, (String, oneshot::Sender<()>)>,
    fallback_cancel_acknowledgements: HashMap<String, (String, oneshot::Sender<()>)>,
    playback_timing: Option<PlaybackTiming>,
    playing_mpv_entry_id: Option<i64>,
    paused_for_cache: bool,
    core_idle: bool,
    loop_file: bool,
    loop_playlist: bool,
}

impl PlayerBookkeeping {
    fn loading(&self, state: &PlayerState) -> bool {
        state.pending_track.is_some()
            || (state.error.is_none()
                && state.index.is_some()
                && !state.paused
                && (self.paused_for_cache || self.core_idle))
    }
}

impl Default for PlayerBookkeeping {
    fn default() -> Self {
        PlayerBookkeeping {
            stream_video_ids_by_target: HashMap::new(),
            mpv_entries: HashMap::new(),
            replacing_mpv_entry_id: None,
            retired_mpv_entry_ids: HashSet::new(),
            logical_entry_ids_by_mpv_id: HashMap::new(),
            restart_logical_entry_ids: None,
            next_logical_entry_sequence: 1,
            tracks_by_mpv_entry_id: HashMap::new(),
            known_tracks_by_video_id: HashMap::new(),
            tier_labels_by_video_id: HashMap::new(),
            warnings_by_video_id: HashMap::new(),
            pending_resume: None,
            pending_jump: None,
            queueing_video_ids: HashSet::new(),
            swapping_video_ids: HashSet::new(),
            recovering_mpv_entry_ids: HashSet::new(),
            recovery_stage_by_mpv_entry_id: HashMap::new(),
            fallback_acknowledgements: HashMap::new(),
            fallback_cancel_acknowledgements: HashMap::new(),
            playback_timing: None,
            playing_mpv_entry_id: None,
            paused_for_cache: false,
            // mpv is idle until it says otherwise, so a queue that never starts
            // reads as loading rather than as playing.
            core_idle: true,
            loop_file: false,
            loop_playlist: false,
        }
    }
}

impl Player {
    pub fn new<S: StreamSource>(options: PlayerOptions, stream_source: Option<S>) -> Player {
        let stream_source = stream_source.map(|source| Arc::new(source) as Arc<dyn DynStreamSource>);
        Self::with_launcher(options, stream_source, Arc::new(MpvLauncher))
    }

    fn with_launcher(
        options: PlayerOptions,
        stream_source: Option<Arc<dyn DynStreamSource>>,
        launcher: Arc<dyn EngineLauncher>,
    ) -> Player {
        let (state, _) = watch::channel(PlayerState {
            volume: options.volume,
            ..PlayerState::default()
        });
        let inner = Arc::new(PlayerInner {
            options,
            state,
            stream_source,
            lifecycle: Lifecycle {
                player_instance_id: NEXT_PLAYER_INSTANCE.fetch_add(1, Ordering::Relaxed),
                next_engine_generation: AtomicU64::new(1),
                transition_lock: tokio::sync::Mutex::new(()),
                closing_completed: tokio::sync::Notify::new(),
                state: Mutex::new(LifecycleState::Stopped),
                launcher,
            },
            bookkeeping: Mutex::new(PlayerBookkeeping::default()),
            queue_edit_lock: tokio::sync::Mutex::new(()),
            current_playback_request_id: Mutex::new(0),
            playback_generation: AtomicU64::new(0),
            next_fallback_request_id: AtomicU64::new(0),
            playback_generation_changed: tokio::sync::Notify::new(),
            timing_sink: Mutex::new(None),
            timing_enabled: AtomicBool::new(false),
            restart_attempted: AtomicBool::new(false),
            restart_snapshot: Mutex::new(None),
            background_tasks: Mutex::new(JoinSet::new()),
        });
        let lifetime = Arc::new(PlayerLifetime {
            inner: Arc::downgrade(&inner),
        });
        Player {
            inner,
            _lifetime: lifetime,
        }
    }

    pub fn options(&self) -> &PlayerOptions {
        &self.inner.options
    }

    /// Latest state, and a receiver that wakes on every change.
    pub fn state(&self) -> watch::Receiver<PlayerState> {
        self.inner.state.subscribe()
    }

    pub fn snapshot(&self) -> PlayerState {
        self.inner.state.borrow().clone()
    }

    /// The playing marker measures mpv activity and does not prove audible output.
    /// Callbacks run on player tasks and must return promptly.
    pub fn set_timing_sink(&self, timing_sink: Option<Arc<TimingSink>>) {
        let timing_enabled = timing_sink.is_some();
        *self
            .inner
            .timing_sink
            .lock()
            .expect("player playback timing sink") = timing_sink;
        self.inner.timing_enabled.store(timing_enabled, Ordering::Relaxed);
        if !timing_enabled {
            self.inner
                .bookkeeping
                .lock()
                .expect("player bookkeeping state")
                .playback_timing = None;
        }
    }

    pub async fn start(&self) -> Result<()> {
        self.inner.start(None).await
    }

    /// Shutdown gets one budget, including a child that does not acknowledge quit.
    pub async fn close(&self) -> Result<()> {
        let inner = &self.inner;
        let closing_admission = {
            let mut state = inner.lifecycle.state.lock().expect("player lifecycle");
            match &*state {
                LifecycleState::Closing(engine_generation) => Err(*engine_generation),
                LifecycleState::Stopped => return Ok(()),
                _ => {
                    let engine_generation = match &*state {
                        LifecycleState::Starting(engine_generation)
                        | LifecycleState::RestartQueued(engine_generation) => *engine_generation,
                        LifecycleState::Running(engine) => engine.engine_generation,
                        _ => unreachable!("retiring engine checked while lifecycle lock held"),
                    };
                    let engine =
                        match std::mem::replace(&mut *state, LifecycleState::Closing(engine_generation)) {
                            LifecycleState::Running(engine) => Some(engine),
                            LifecycleState::Starting(_) | LifecycleState::RestartQueued(_) => None,
                            _ => unreachable!("retiring engine checked while lifecycle lock held"),
                        };
                    Ok(Closing {
                        lifecycle: &inner.lifecycle,
                        engine_generation,
                        engine,
                    })
                }
            }
        };
        let mut closing = match closing_admission {
            Ok(closing) => closing,
            Err(engine_generation) => loop {
                let closing_completed = inner.lifecycle.closing_completed.notified();
                tokio::pin!(closing_completed);
                closing_completed.as_mut().enable();
                if !matches!(
                    *inner.lifecycle.state.lock().expect("player lifecycle"),
                    LifecycleState::Closing(current_generation) if current_generation == engine_generation
                ) {
                    return Ok(());
                }
                closing_completed.await;
            },
        };
        inner.begin_playback();
        {
            let mut bookkeeping = inner.bookkeeping.lock().expect("player bookkeeping state");
            bookkeeping.fallback_acknowledgements.clear();
            bookkeeping.fallback_cancel_acknowledgements.clear();
        }
        let mut background_tasks = std::mem::take(&mut *inner.background_tasks.lock().expect("player tasks"));
        background_tasks.abort_all();
        let shutdown = async {
            let _queue_edit_guard = inner.queue_edit_lock.lock().await;
            let _lifecycle_transition_guard = inner.lifecycle.transition_lock.lock().await;
            let Some(engine) = closing.engine.as_mut() else {
                return;
            };
            let _ = engine.ipc.command(vec![json!("quit")]).await;
            while background_tasks.join_next().await.is_some() {}
            if let Some(event_reader_task) = engine.event_reader_task.take() {
                event_reader_task.abort();
            }
            if let Some(session) = engine.session.as_mut() {
                session.ipc.close().await;
                let _ = session.mpv_process.wait().await;
            }
        };
        let _ = tokio::time::timeout(ENGINE_SHUTDOWN_BUDGET, shutdown).await;
        Ok(())
    }

    pub async fn retry_playback(&self) -> Result<()> {
        if self.snapshot().availability != PlaybackAvailability::Unavailable {
            return Ok(());
        }
        let confirmed_state = self
            .inner
            .restart_snapshot
            .lock()
            .expect("player restart snapshot")
            .clone()
            .unwrap_or_else(|| self.snapshot());
        self.inner.restore_playback(confirmed_state, None).await?;
        self.inner.restart_attempted.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Read mpv's authoritative playlist after an IPC outcome became unclear.
    pub async fn reconcile_queue(&self) -> Result<PlayerState> {
        let playlist = self
            .inner
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        self.inner.handle_event(&json!({
            "event": "property-change",
            "name": "playlist",
            "data": playlist,
        }));
        Ok(self.snapshot())
    }

    /// Replace the queue with `tracks` and start at `index`.
    pub async fn play(&self, tracks: Vec<Track>, index: usize) -> Result<()> {
        if tracks.is_empty() {
            return Ok(());
        }
        let inner = &self.inner;
        let started_at = Instant::now();
        let playback_request_id = inner.reserve_playback();
        let queue_edit_guard = inner.queue_edit_lock.lock().await;
        if !inner.playback_request_is_current(playback_request_id) {
            return Ok(());
        }
        let playlist = inner
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        if !inner.playback_request_is_current(playback_request_id) {
            return Ok(());
        }
        let reuses_queue = playlist.as_array().is_some_and(|entries| {
            let bookkeeping = inner.bookkeeping.lock().expect("player bookkeeping state");
            entries.len() == tracks.len()
                && entries
                    .iter()
                    .zip(&tracks)
                    .all(|(entry, incoming)| track_for(&bookkeeping, entry).video_id == incoming.video_id)
        });
        if reuses_queue {
            let index = index.min(tracks.len() - 1);
            return self
                .jump_from_playlist(index, playback_request_id, queue_edit_guard, playlist, started_at)
                .await;
        }
        drop(queue_edit_guard);
        self.inner
            .play_from_reserved(tracks, index, 0.0, playback_request_id, PlaybackIntent::Play)
            .await
            .map(|_| ())
    }

    /// Resume only after the selected file loads, including a replacement stream after recovery.
    pub async fn play_from(&self, tracks: Vec<Track>, index: usize, seconds: f64) -> Result<()> {
        if tracks.is_empty() {
            return Ok(());
        }
        let playback_request_id = self.inner.reserve_playback();
        self.inner
            .play_from_reserved(tracks, index, seconds, playback_request_id, PlaybackIntent::Play)
            .await
            .map(|_| ())
    }

    pub async fn enqueue(&self, tracks: Vec<Track>, play_next: bool) -> Result<()> {
        let inner = &self.inner;
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        inner.remember(&tracks);
        let flag = if play_next {
            "insert-next-play"
        } else {
            "append-play"
        };
        // Reversed, so several tracks inserted one after the current keep their order.
        let ordered: Vec<&Track> = if play_next {
            tracks.iter().rev().collect()
        } else {
            tracks.iter().collect()
        };
        for track in ordered {
            let queue_entry_target = inner.entry_url(track);
            inner
                .command(vec![json!("loadfile"), json!(queue_entry_target), json!(flag)])
                .await?;
        }
        Ok(())
    }

    /// Replace every entry after the current one without disturbing playback.
    pub async fn replace_upcoming(&self, tracks: Vec<Track>) -> Result<()> {
        let inner = &self.inner;
        let playback_generation = inner.begin_playback();
        {
            let _queue_edit_guard = inner.queue_edit_lock.lock().await;
            if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
                return Ok(());
            }
            inner.state.send_modify(|state| state.error = None);
            inner.command(vec![json!("playlist-clear")]).await?;
            inner.remember(&tracks);
            for track in &tracks {
                let queue_entry_target = inner.entry_url(track);
                inner
                    .command(vec![
                        json!("loadfile"),
                        json!(queue_entry_target),
                        json!("append-play"),
                    ])
                    .await?;
            }
            if inner.state.borrow().shuffle && !tracks.is_empty() {
                inner.command(vec![json!("playlist-shuffle")]).await?;
            }
        }
        inner.prefetch(tracks.into_iter().take(inner.options.prefetch_ahead).collect());
        Ok(())
    }

    pub async fn jump(&self, index: usize) -> Result<()> {
        let inner = &self.inner;
        let started_at = Instant::now();
        let playback_request_id = inner.reserve_playback();
        self.jump_reserved(index, playback_request_id, started_at).await
    }

    async fn jump_reserved(&self, index: usize, playback_request_id: u64, started_at: Instant) -> Result<()> {
        let inner = &self.inner;
        let queue_edit_guard = inner.queue_edit_lock.lock().await;
        if !inner.playback_request_is_current(playback_request_id) {
            return Ok(());
        }
        let mpv_playlist = inner
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        if !inner.playback_request_is_current(playback_request_id) {
            return Ok(());
        }
        self.jump_from_playlist(
            index,
            playback_request_id,
            queue_edit_guard,
            mpv_playlist,
            started_at,
        )
        .await
    }

    async fn jump_from_playlist(
        &self,
        index: usize,
        playback_request_id: u64,
        queue_edit_guard: tokio::sync::MutexGuard<'_, ()>,
        mpv_playlist: Value,
        started_at: Instant,
    ) -> Result<()> {
        let inner = &self.inner;
        let selected_playlist_entry = mpv_playlist
            .as_array()
            .and_then(|entries| entries.get(index))
            .ok_or_else(|| Error::Mpv("selected track is no longer in the queue".to_string()))?;
        let track = {
            let bookkeeping = inner.bookkeeping.lock().expect("player bookkeeping state");
            track_for(&bookkeeping, selected_playlist_entry)
        };
        let mpv_entry_id = selected_playlist_entry
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Mpv("selected track has no playlist identity".to_string()))?;
        if inner
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .pending_jump
            .is_some_and(|(pending_mpv_entry_id, playback_generation)| {
                pending_mpv_entry_id == mpv_entry_id
                    && inner.playback_generation.load(Ordering::SeqCst) == playback_generation
            })
        {
            return Ok(());
        }
        let Some(playback_generation) = inner.begin_reserved_playback(playback_request_id) else {
            return Ok(());
        };
        inner.state.send_modify(|state| state.error = None);
        let _pending_playback = inner.pending_playback(&track, playback_generation, Some(mpv_entry_id));
        inner.start_timing(playback_generation, &track.video_id, "jump.request", started_at);
        inner.emit_timing(
            playback_generation,
            &track.video_id,
            "queue.wait",
            started_at.elapsed(),
        );
        let selected_filename = filename_of(selected_playlist_entry).to_string();
        drop(queue_edit_guard);
        let resolved_from_watch = selected_filename == track.url();
        let resolved = if resolved_from_watch {
            let SelectedStream::Ready(resolved) =
                inner.resolve_selected(&track.video_id, playback_generation).await
            else {
                return Ok(());
            };
            resolved
        } else {
            None
        };
        let fallback = resolved_from_watch
            && resolved
                .as_ref()
                .is_none_or(|playable| playable.target == track.url());
        let queue_wait_started_at = Instant::now();
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
            return Ok(());
        }
        inner.emit_timing(
            playback_generation,
            &track.video_id,
            "queue.wait_after_resolve",
            queue_wait_started_at.elapsed(),
        );
        let queue_commands_started_at = Instant::now();
        inner.timing_stage("queue.commands_start");
        let playlist = inner
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        let still_selected = playlist.as_array().and_then(|entries| entries.get(index));
        if !still_selected.is_some_and(|entry| {
            entry.get("id").and_then(Value::as_i64) == Some(mpv_entry_id)
                && track_for(
                    &inner.bookkeeping.lock().expect("player bookkeeping state"),
                    entry,
                )
                .video_id
                    == track.video_id
        }) {
            return Err(Error::Mpv("selected track is no longer in the queue".to_string()));
        }
        inner.timing_entry(playback_generation, Some(mpv_entry_id));
        let still_youtube = still_selected.is_some_and(|entry| filename_of(entry) == track.url());
        if let Some(playable) = resolved.filter(|_| still_youtube) {
            inner.remember(std::slice::from_ref(&track));
            inner.remember_stream(&track.video_id, &playable);
            let target = playable.target;
            if target != track.url() {
                let replaced = inner
                    .replace_entry(index, mpv_entry_id, &track.url(), &target, true)
                    .await?;
                if replaced.is_none() {
                    return Err(Error::Mpv("selected track is no longer in the queue".to_string()));
                }
            }
        }
        inner.timing_commands_sent(playback_generation);
        if fallback {
            let fallback_path = track.url();
            let fallback_request = inner.allow_ytdl_once(&fallback_path, playback_generation).await?;
            let Some(fallback_request_id) = fallback_request else {
                inner.emit_timing(
                    playback_generation,
                    &track.video_id,
                    "queue.cancelled",
                    queue_commands_started_at.elapsed(),
                );
                return Ok(());
            };
            if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
                let _ = inner.cancel_ytdl_once(&fallback_request_id, &fallback_path).await;
                inner.emit_timing(
                    playback_generation,
                    &track.video_id,
                    "queue.cancelled",
                    queue_commands_started_at.elapsed(),
                );
                return Ok(());
            }
        }
        self.inner
            .command(vec![json!("playlist-play-index"), json!(index)])
            .await?;
        self.inner
            .command(vec![json!("set_property"), json!("pause"), json!(false)])
            .await?;
        inner.emit_timing(
            playback_generation,
            &track.video_id,
            "queue.commands_done",
            queue_commands_started_at.elapsed(),
        );
        Ok(())
    }

    pub async fn remove(&self, index: usize) -> Result<()> {
        let observed_entry = {
            let player_state = self.inner.state.borrow();
            player_state
                .queue_entries
                .get(index)
                .map(|queue_entry| (queue_entry.logical_id, player_state.queue_revision))
        };
        if let Some((logical_entry_id, queue_revision)) = observed_entry {
            self.remove_entry(logical_entry_id, queue_revision).await?;
            return Ok(());
        }
        self.inner.begin_playback();
        let _queue_edit_guard = self.inner.queue_edit_lock.lock().await;
        self.inner
            .command(vec![json!("playlist-remove"), json!(index)])
            .await?;
        Ok(())
    }

    /// Remove the same observed queue entry even when equal tracks sit beside it.
    /// A revision from an older queue is a harmless stale edit.
    pub async fn remove_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
    ) -> Result<QueueEdit> {
        let inner = &self.inner;
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        let Some((index, _)) = inner
            .queue_entry_position(logical_entry_id, queue_revision)
            .await?
        else {
            return Ok(QueueEdit::Stale);
        };
        inner.begin_playback();
        inner
            .command(vec![json!("playlist-remove"), json!(index)])
            .await?;
        Ok(QueueEdit::Applied)
    }

    /// Move the same observed queue entry to a new position. Positions beyond
    /// the current queue move it to the end.
    pub async fn move_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
        destination_index: usize,
    ) -> Result<QueueEdit> {
        let inner = &self.inner;
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        let Some((source_index, queue_length)) = inner
            .queue_entry_position(logical_entry_id, queue_revision)
            .await?
        else {
            return Ok(QueueEdit::Stale);
        };
        let destination_index = destination_index.min(queue_length - 1);
        if source_index == destination_index {
            return Ok(QueueEdit::Applied);
        }
        inner.begin_playback();
        inner
            .command(vec![
                json!("playlist-move"),
                json!(source_index),
                json!(destination_index),
            ])
            .await?;
        Ok(QueueEdit::Applied)
    }

    pub async fn clear(&self) -> Result<()> {
        self.inner.begin_playback();
        let _queue_edit_guard = self.inner.queue_edit_lock.lock().await;
        self.inner.command(vec![json!("playlist-clear")]).await?;
        Ok(())
    }

    pub async fn toggle_pause(&self) -> Result<()> {
        self.inner.state.send_modify(|state| state.error = None);
        let playback_is_idle = {
            let state = self.inner.state.borrow();
            state.track().is_none() && !state.queue.is_empty()
        };
        if playback_is_idle {
            return self.jump(0).await;
        }
        self.inner.command(vec![json!("cycle"), json!("pause")]).await?;
        Ok(())
    }

    pub async fn next(&self) -> Result<()> {
        let inner = &self.inner;
        let started_at = Instant::now();
        let playback_generation = inner.begin_playback();
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        inner.state.send_modify(|state| state.error = None);
        inner.start_skip_timing(playback_generation, true, started_at);
        inner.timing_commands_sent(playback_generation);
        inner
            .command(vec![json!("playlist-next"), json!("force")])
            .await?;
        inner.timing_stage("skip.commands_done");
        Ok(())
    }

    /// Restart the track past three seconds in, else the previous entry.
    pub async fn previous(&self) -> Result<()> {
        let inner = &self.inner;
        let started_at = Instant::now();
        let playback_generation = inner.begin_playback();
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        inner.state.send_modify(|state| state.error = None);
        let position = inner.state.borrow().position;
        if position > RESTART_AFTER_SECONDS {
            let video_id = inner
                .state
                .borrow()
                .track()
                .map(|track| track.video_id.clone())
                .unwrap_or_default();
            inner.start_timing(playback_generation, &video_id, "skip.restart", started_at);
            inner.timing_commands_sent(playback_generation);
            inner
                .command(vec![json!("seek"), json!(0), json!("absolute")])
                .await?;
        } else {
            inner.start_skip_timing(playback_generation, false, started_at);
            inner.timing_commands_sent(playback_generation);
            inner
                .command(vec![json!("playlist-prev"), json!("force")])
                .await?;
        }
        inner.timing_stage("skip.commands_done");
        Ok(())
    }

    pub async fn seek(&self, delta_seconds: f64) -> Result<()> {
        // Nothing loaded yet is not a failure the user should see.
        let _ = self
            .inner
            .command(vec![json!("seek"), json!(delta_seconds), json!("relative")])
            .await;
        Ok(())
    }

    pub async fn seek_to(&self, seconds: f64) -> Result<()> {
        let target = seconds.max(0.0);
        let _ = self
            .inner
            .command(vec![json!("seek"), json!(target), json!("absolute")])
            .await;
        Ok(())
    }

    pub async fn adjust_volume(&self, delta: i16) -> Result<()> {
        let current_volume = i16::from(self.inner.state.borrow().volume);
        self.set_volume(current_volume.saturating_add(delta).clamp(0, 100) as u8)
            .await
    }

    pub async fn set_volume(&self, volume: u8) -> Result<()> {
        self.inner
            .command(vec![
                json!("set_property"),
                json!("volume"),
                json!(volume.min(100)),
            ])
            .await?;
        Ok(())
    }

    pub async fn toggle_mute(&self) -> Result<()> {
        self.inner.command(vec![json!("cycle"), json!("mute")]).await?;
        Ok(())
    }

    /// The whole audio filter chain, as one lavfi graph, or nothing at all.
    /// The chain belongs to the mpv instance and not to the file, so it holds
    /// across a track change and a change per track sends it again.
    pub async fn set_audio_filter(&self, graph: Option<&str>) -> Result<()> {
        self.inner
            .command(vec![
                json!("set_property"),
                json!("af"),
                json!(graph.unwrap_or("")),
            ])
            .await?;
        Ok(())
    }

    pub async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        let file = if mode == Repeat::One { "inf" } else { "no" };
        let playlist = if mode == Repeat::All { "inf" } else { "no" };
        self.inner
            .command(vec![json!("set_property"), json!("loop-file"), json!(file)])
            .await?;
        self.inner
            .command(vec![
                json!("set_property"),
                json!("loop-playlist"),
                json!(playlist),
            ])
            .await?;
        Ok(())
    }

    pub async fn cycle_repeat(&self) -> Result<Repeat> {
        let mode = self.inner.state.borrow().repeat.next();
        self.set_repeat(mode).await?;
        Ok(mode)
    }

    pub async fn set_shuffle(&self, shuffle_enabled: bool) -> Result<()> {
        let inner = &self.inner;
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        let (already_enabled, queue_needs_shuffle) = {
            let state = inner.state.borrow();
            (state.shuffle == shuffle_enabled, state.queue.len() > 1)
        };
        if already_enabled {
            return Ok(());
        }
        inner.state.send_modify(|state| state.shuffle = shuffle_enabled);
        if queue_needs_shuffle {
            let mpv_command_name = if shuffle_enabled {
                "playlist-shuffle"
            } else {
                "playlist-unshuffle"
            };
            inner.command(vec![json!(mpv_command_name)]).await?;
        }
        Ok(())
    }

    pub async fn toggle_shuffle(&self) -> Result<bool> {
        let shuffle_enabled = !self.inner.state.borrow().shuffle;
        self.set_shuffle(shuffle_enabled).await?;
        Ok(shuffle_enabled)
    }

    /// Resolve `tracks` ahead of time and swap their queue entries in place.
    pub fn prefetch(&self, tracks: Vec<Track>) {
        self.inner.prefetch(tracks);
    }
}

/// Like every player: early in a track "previous" restarts it.
const RESTART_AFTER_SECONDS: f64 = 3.0;
const HOOK_RESOLVE_BUDGET: Duration = Duration::from_secs(45);
const FALLBACK_ACK_BUDGET: Duration = Duration::from_secs(2);
const HOOK_FALLBACK_WARNING: &str = "Native resolver failed, using slower yt-dlp fallback";

impl PlayerControl for Player {
    fn state(&self) -> watch::Receiver<PlayerState> {
        Player::state(self)
    }

    fn snapshot(&self) -> PlayerState {
        Player::snapshot(self)
    }

    async fn start(&self) -> Result<()> {
        Player::start(self).await
    }

    async fn close(&self) -> Result<()> {
        Player::close(self).await
    }

    async fn retry_playback(&self) -> Result<()> {
        Player::retry_playback(self).await
    }

    async fn reconcile_queue(&self) -> Result<PlayerState> {
        Player::reconcile_queue(self).await
    }

    async fn play(&self, tracks: Vec<Track>, index: usize) -> Result<()> {
        Player::play(self, tracks, index).await
    }

    async fn play_from(&self, tracks: Vec<Track>, index: usize, seconds: f64) -> Result<()> {
        Player::play_from(self, tracks, index, seconds).await
    }

    async fn enqueue(&self, tracks: Vec<Track>, play_next: bool) -> Result<()> {
        Player::enqueue(self, tracks, play_next).await
    }

    async fn replace_upcoming(&self, tracks: Vec<Track>) -> Result<()> {
        Player::replace_upcoming(self, tracks).await
    }

    async fn jump(&self, index: usize) -> Result<()> {
        Player::jump(self, index).await
    }

    async fn remove(&self, index: usize) -> Result<()> {
        Player::remove(self, index).await
    }

    async fn remove_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
    ) -> Result<QueueEdit> {
        Player::remove_entry(self, logical_entry_id, queue_revision).await
    }

    async fn move_entry(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
        destination_index: usize,
    ) -> Result<QueueEdit> {
        Player::move_entry(self, logical_entry_id, queue_revision, destination_index).await
    }

    async fn clear(&self) -> Result<()> {
        Player::clear(self).await
    }

    async fn toggle_pause(&self) -> Result<()> {
        Player::toggle_pause(self).await
    }

    async fn next(&self) -> Result<()> {
        Player::next(self).await
    }

    async fn previous(&self) -> Result<()> {
        Player::previous(self).await
    }

    async fn seek(&self, delta_seconds: f64) -> Result<()> {
        Player::seek(self, delta_seconds).await
    }

    async fn seek_to(&self, seconds: f64) -> Result<()> {
        Player::seek_to(self, seconds).await
    }

    async fn adjust_volume(&self, delta: i16) -> Result<()> {
        Player::adjust_volume(self, delta).await
    }

    async fn set_volume(&self, volume: u8) -> Result<()> {
        Player::set_volume(self, volume).await
    }

    async fn toggle_mute(&self) -> Result<()> {
        Player::toggle_mute(self).await
    }

    async fn set_audio_filter(&self, graph: Option<&str>) -> Result<()> {
        Player::set_audio_filter(self, graph).await
    }

    async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        Player::set_repeat(self, mode).await
    }

    async fn cycle_repeat(&self) -> Result<Repeat> {
        Player::cycle_repeat(self).await
    }

    async fn set_shuffle(&self, shuffle_enabled: bool) -> Result<()> {
        Player::set_shuffle(self, shuffle_enabled).await
    }

    async fn toggle_shuffle(&self) -> Result<bool> {
        Player::toggle_shuffle(self).await
    }

    fn prefetch(&self, tracks: Vec<Track>) {
        Player::prefetch(self, tracks)
    }
}

impl PlayerInner {
    async fn start(self: &Arc<Self>, expected_restart_generation: Option<u64>) -> Result<()> {
        let inner = self;
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        let _lifecycle_transition_guard = inner.lifecycle.transition_lock.lock().await;
        let engine_generation = {
            let mut lifecycle_state = inner.lifecycle.state.lock().expect("player lifecycle");
            let engine_generation = match (&*lifecycle_state, expected_restart_generation) {
                (LifecycleState::Stopped, None) => inner
                    .lifecycle
                    .next_engine_generation
                    .fetch_add(1, Ordering::Relaxed),
                (LifecycleState::RestartQueued(current_generation), Some(expected_generation))
                    if *current_generation == expected_generation =>
                {
                    expected_generation
                }
                _ => return Ok(()),
            };
            *lifecycle_state = LifecycleState::Starting(engine_generation);
            engine_generation
        };
        inner.invalidate_queue_entries();
        {
            let mut bookkeeping = inner.bookkeeping.lock().expect("player bookkeeping state");
            bookkeeping.replacing_mpv_entry_id = None;
            bookkeeping.retired_mpv_entry_ids.clear();
        }
        let mut starting_guard = Starting {
            lifecycle: &inner.lifecycle,
            engine_generation,
            armed: true,
        };
        let engine_resource_suffix = format!(
            "{}-{}-{engine_generation}",
            std::process::id(),
            inner.lifecycle.player_instance_id
        );
        let resources = EngineResources {
            socket_path: inner
                .options
                .socket_dir
                .join(format!("redwax-mpv-{engine_resource_suffix}.sock")),
            resolver_script_path: inner
                .options
                .socket_dir
                .join(format!("redwax-resolve-{engine_resource_suffix}.lua")),
        };
        let launched_engine = match inner.lifecycle.launcher.launch(&inner.options, &resources).await {
            Ok(launched_engine) => launched_engine,
            Err(error) => {
                let lifecycle_state = inner.lifecycle.state.lock().expect("player lifecycle");
                if matches!(
                    *lifecycle_state,
                    LifecycleState::Starting(current_generation) if current_generation == engine_generation
                ) {
                    inner.state.send_modify(|state| {
                        state.availability = PlaybackAvailability::Unavailable;
                        state.error = Some(format!("Playback unavailable: {error}"));
                    });
                }
                return Err(error);
            }
        };
        let mut lifecycle_state = inner.lifecycle.state.lock().expect("player lifecycle");
        if !matches!(
            *lifecycle_state,
            LifecycleState::Starting(current_generation) if current_generation == engine_generation
        ) {
            return Err(Error::Mpv("mpv startup was cancelled".to_string()));
        }
        let mut event_receiver = launched_engine.event_receiver;
        let weak_player = Arc::downgrade(inner);
        let event_reader_task = tokio::spawn(async move {
            while let Some(ipc_message) = event_receiver.recv().await {
                let Some(player) = weak_player.upgrade() else {
                    break;
                };
                player.handle_event_for(engine_generation, &ipc_message);
            }
            if let Some(player) = weak_player.upgrade() {
                player.engine_gone(engine_generation);
            }
        });
        *lifecycle_state = LifecycleState::Running(Box::new(RunningEngine {
            engine_generation,
            ipc: launched_engine.ipc,
            session: launched_engine.session,
            _resources: Some(resources),
            event_reader_task: Some(event_reader_task),
        }));
        starting_guard.armed = false;
        inner.state.send_modify(|state| {
            state.availability = PlaybackAvailability::Ready;
        });
        drop(lifecycle_state);
        Ok(())
    }

    async fn restore_playback(
        self: &Arc<Self>,
        confirmed_state: PlayerState,
        expected_restart_generation: Option<u64>,
    ) -> Result<()> {
        let playback_request_id = {
            let lifecycle_state = self.lifecycle.state.lock().expect("player lifecycle");
            if let Some(expected_generation) = expected_restart_generation
                && !matches!(*lifecycle_state, LifecycleState::RestartQueued(current_generation) if current_generation == expected_generation)
            {
                return Ok(());
            }
            self.reserve_playback()
        };
        let queue_load = async {
            self.start(expected_restart_generation).await?;
            if !self.playback_request_is_current(playback_request_id) {
                return Ok(QueueLoad::Cancelled);
            }
            if confirmed_state.queue.is_empty() {
                self.command(vec![json!("set_property"), json!("pause"), json!(true)])
                    .await?;
                return Ok(QueueLoad::Applied);
            }
            let resume_queue_index = confirmed_state
                .index
                .unwrap_or(0)
                .min(confirmed_state.queue.len().saturating_sub(1));
            self.bookkeeping
                .lock()
                .expect("player bookkeeping state")
                .restart_logical_entry_ids =
                Some(confirmed_state.queue_entries.iter().map(QueueEntry::id).collect());
            self.play_from_reserved(
                confirmed_state.queue,
                resume_queue_index,
                confirmed_state.position,
                playback_request_id,
                PlaybackIntent::RestorePaused,
            )
            .await
        }
        .await;
        if !self.playback_request_is_current(playback_request_id) {
            return Ok(());
        }
        let restoration_result = match queue_load {
            Ok(QueueLoad::Applied) => {
                self.state.send_modify(|state| {
                    state.paused = true;
                    state.availability = PlaybackAvailability::RestoredPaused;
                    state.error = Some("Playback restored paused".to_string());
                });
                Ok(())
            }
            Ok(QueueLoad::Cancelled) => Ok(()),
            Ok(QueueLoad::ResolutionFailed) => {
                let message = self
                    .state
                    .borrow()
                    .error
                    .clone()
                    .unwrap_or_else(|| "Could not resolve the restored track".to_string());
                Err(Error::Mpv(message))
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &restoration_result {
            self.state.send_modify(|state| {
                state.paused = true;
                state.pending_track = None;
                state.loading = false;
                state.error = Some(format!(
                    "Playback unavailable: {error}. Use Retry playback to try again"
                ));
                state.availability = PlaybackAvailability::Unavailable;
            });
        }
        restoration_result
    }

    async fn play_from_reserved(
        self: &Arc<Self>,
        tracks: Vec<Track>,
        index: usize,
        seconds: f64,
        playback_request_id: u64,
        playback_intent: PlaybackIntent,
    ) -> Result<QueueLoad> {
        let seconds = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        if tracks.is_empty() {
            return Ok(QueueLoad::Cancelled);
        }
        let index = index.min(tracks.len() - 1);
        let inner = self;
        let Some(playback_generation) = inner.begin_reserved_playback(playback_request_id) else {
            return Ok(QueueLoad::Cancelled);
        };
        let _pending = inner.pending_playback(&tracks[index], playback_generation, None);
        inner.start_timing(
            playback_generation,
            &tracks[index].video_id,
            "play.request",
            Instant::now(),
        );
        let resolved = match inner
            .resolve_selected(&tracks[index].video_id, playback_generation)
            .await
        {
            SelectedStream::Ready(resolved) => resolved,
            SelectedStream::Cancelled => return Ok(QueueLoad::Cancelled),
            SelectedStream::Failed => return Ok(QueueLoad::ResolutionFailed),
        };
        let fallback = resolved
            .as_ref()
            .is_none_or(|playable| playable.target == tracks[index].url());
        let queue_wait_started_at = Instant::now();
        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
        inner.emit_timing(
            playback_generation,
            &tracks[index].video_id,
            "queue.wait",
            queue_wait_started_at.elapsed(),
        );
        if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
            inner.emit_timing(
                playback_generation,
                &tracks[index].video_id,
                "queue.cancelled",
                queue_wait_started_at.elapsed(),
            );
            return Ok(QueueLoad::Cancelled);
        }
        inner.state.send_modify(|state| state.error = None);
        let queue_commands_started_at = Instant::now();
        inner.timing_stage("queue.commands_start");

        inner.command(vec![json!("stop")]).await?;
        inner.remember(&tracks);
        if let Some(playable) = &resolved {
            inner.remember_stream(&tracks[index].video_id, playable);
        }
        inner
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .pending_resume = (seconds > 0.0).then(|| {
            (
                playback_generation,
                tracks[index].video_id.clone(),
                seconds,
                playback_intent,
            )
        });
        if seconds > 0.0 || playback_intent == PlaybackIntent::RestorePaused {
            inner
                .command(vec![json!("set_property"), json!("pause"), json!(true)])
                .await?;
        }
        for (queue_index, track) in tracks.iter().enumerate() {
            if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
                inner.emit_timing(
                    playback_generation,
                    &tracks[index].video_id,
                    "queue.cancelled",
                    queue_commands_started_at.elapsed(),
                );
                return Ok(QueueLoad::Cancelled);
            }
            let queue_entry_target = match &resolved {
                Some(playable) if queue_index == index => playable.target.clone(),
                _ => inner.entry_url(track),
            };
            let loaded = inner
                .command(vec![
                    json!("loadfile"),
                    json!(queue_entry_target),
                    json!("append"),
                ])
                .await?;
            if queue_index == index {
                inner.timing_entry(
                    playback_generation,
                    loaded.get("playlist_entry_id").and_then(Value::as_i64),
                );
            }
        }
        if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
            inner.emit_timing(
                playback_generation,
                &tracks[index].video_id,
                "queue.cancelled",
                queue_commands_started_at.elapsed(),
            );
            return Ok(QueueLoad::Cancelled);
        }
        inner.timing_commands_sent(playback_generation);
        if fallback {
            let fallback_path = tracks[index].url();
            let fallback_request = inner.allow_ytdl_once(&fallback_path, playback_generation).await?;
            let Some(fallback_request_id) = fallback_request else {
                inner.emit_timing(
                    playback_generation,
                    &tracks[index].video_id,
                    "queue.cancelled",
                    queue_commands_started_at.elapsed(),
                );
                return Ok(QueueLoad::Cancelled);
            };
            if inner.playback_generation.load(Ordering::SeqCst) != playback_generation {
                let _ = inner.cancel_ytdl_once(&fallback_request_id, &fallback_path).await;
                inner.emit_timing(
                    playback_generation,
                    &tracks[index].video_id,
                    "queue.cancelled",
                    queue_commands_started_at.elapsed(),
                );
                return Ok(QueueLoad::Cancelled);
            }
        }
        inner
            .command(vec![json!("playlist-play-index"), json!(index)])
            .await?;
        if seconds == 0.0 && playback_intent == PlaybackIntent::Play {
            inner
                .command(vec![json!("set_property"), json!("pause"), json!(false)])
                .await?;
        }
        let shuffled = inner.state.borrow().shuffle;
        if shuffled {
            inner.command(vec![json!("playlist-shuffle")]).await?;
        }
        inner.emit_timing(
            playback_generation,
            &tracks[index].video_id,
            "queue.commands_done",
            queue_commands_started_at.elapsed(),
        );

        let tracks_to_prefetch: Vec<Track> = tracks
            .iter()
            .skip(index + 1)
            .take(inner.options.prefetch_ahead)
            .cloned()
            .collect();
        inner.prefetch(tracks_to_prefetch);
        Ok(QueueLoad::Applied)
    }

    fn invalidate_queue_entries(&self) {
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping.mpv_entries.clear();
        bookkeeping.tracks_by_mpv_entry_id.clear();
        bookkeeping.logical_entry_ids_by_mpv_id.clear();
        drop(bookkeeping);
        self.state.send_modify(|state| {
            state.queue_entries.clear();
            state.queue_revision.0 = state
                .queue_revision
                .0
                .checked_add(1)
                .expect("queue revision exhausted");
        });
    }

    async fn queue_entry_position(
        &self,
        logical_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
    ) -> Result<Option<(usize, usize)>> {
        let mpv_entry_id = {
            let state = self.state.borrow();
            if state.queue_revision != queue_revision {
                return Ok(None);
            }
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            bookkeeping
                .logical_entry_ids_by_mpv_id
                .iter()
                .find_map(|(mpv_entry_id, candidate_logical_id)| {
                    (*candidate_logical_id == logical_entry_id).then_some(*mpv_entry_id)
                })
        };
        let Some(mpv_entry_id) = mpv_entry_id else {
            return Ok(None);
        };
        let playlist = self
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        let mpv_playlist_entries = playlist.as_array().map(Vec::as_slice).unwrap_or_default();
        let queue_position = mpv_playlist_entries
            .iter()
            .position(|entry| entry.get("id").and_then(Value::as_i64) == Some(mpv_entry_id));
        let revision_is_current = {
            let state = self.state.borrow();
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            state.queue_revision == queue_revision
                && bookkeeping.logical_entry_ids_by_mpv_id.get(&mpv_entry_id) == Some(&logical_entry_id)
        };
        Ok(queue_position
            .filter(|_| revision_is_current)
            .map(|index| (index, mpv_playlist_entries.len())))
    }

    fn emit_timing(
        &self,
        playback_generation: u64,
        video_id: &str,
        stage: &'static str,
        elapsed_time: Duration,
    ) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        let sink = self
            .timing_sink
            .lock()
            .expect("player playback timing sink")
            .clone();
        if let Some(sink) = sink {
            let video_id = if video_id.starts_with("http://") || video_id.starts_with("https://") {
                "unknown"
            } else {
                video_id
            };
            sink(playback_generation, video_id, stage, elapsed_time);
        }
    }

    fn start_timing(
        &self,
        playback_generation: u64,
        video_id: &str,
        stage: &'static str,
        started_at: Instant,
    ) {
        if !self.timing_enabled.load(Ordering::Relaxed)
            || self.playback_generation.load(Ordering::SeqCst) != playback_generation
        {
            return;
        }
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .playback_timing = Some(PlaybackTiming {
            playback_generation,
            video_id: video_id.to_string(),
            started_at,
            expected_mpv_entry_id: None,
            queue_commands_sent: false,
            loaded: false,
            playing_timing_reported: false,
        });
        self.emit_timing(playback_generation, video_id, stage, Duration::ZERO);
    }

    fn start_skip_timing(&self, playback_generation: u64, forward: bool, started_at: Instant) {
        let state = self.state.borrow().clone();
        let destination_queue_index = state.index.and_then(|current_queue_index| {
            if forward {
                (current_queue_index + 1 < state.queue.len())
                    .then_some(current_queue_index + 1)
                    .or_else(|| (state.repeat == Repeat::All && !state.queue.is_empty()).then_some(0))
            } else {
                current_queue_index.checked_sub(1).or_else(|| {
                    (state.repeat == Repeat::All && !state.queue.is_empty()).then_some(state.queue.len() - 1)
                })
            }
        });
        let Some(destination_queue_index) = destination_queue_index else {
            let video_id = state.track().map_or("", |track| track.video_id.as_str());
            self.start_timing(playback_generation, video_id, "skip.request", started_at);
            self.emit_timing(
                playback_generation,
                video_id,
                "skip.target.none",
                started_at.elapsed(),
            );
            return;
        };
        let Some(track) = state.queue.get(destination_queue_index) else {
            return;
        };
        let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let destination_mpv_entry_id =
            bookkeeping
                .mpv_entries
                .iter()
                .find_map(|(mpv_entry_id, (_, queue_index))| {
                    (*queue_index == destination_queue_index).then_some(*mpv_entry_id)
                });
        let filename = destination_mpv_entry_id.and_then(|mpv_entry_id| {
            bookkeeping
                .mpv_entries
                .get(&mpv_entry_id)
                .map(|(filename, _)| filename.as_str())
        });
        let timing_stage = match filename {
            Some(filename) if bookkeeping.stream_video_ids_by_target.contains_key(filename) => {
                "skip.target.direct"
            }
            Some(filename) if filename == track.url() => "skip.target.youtube",
            _ => "skip.target.unknown",
        };
        drop(bookkeeping);
        self.start_timing(playback_generation, &track.video_id, "skip.request", started_at);
        self.timing_entry(playback_generation, destination_mpv_entry_id);
        self.emit_timing(
            playback_generation,
            &track.video_id,
            timing_stage,
            started_at.elapsed(),
        );
    }

    fn timing_stage(&self, stage: &'static str) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        let event = self
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .playback_timing
            .as_ref()
            .and_then(|playback_timing| {
                playback_timing.event(self.playback_generation.load(Ordering::SeqCst))
            });
        if let Some((playback_generation, video_id, elapsed_time)) = event {
            self.emit_timing(playback_generation, &video_id, stage, elapsed_time);
        }
    }

    fn timing_entry(&self, playback_generation: u64, expected_mpv_entry_id: Option<i64>) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        if let Some(playback_timing) = self
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .playback_timing
            .as_mut()
            && playback_timing.playback_generation == playback_generation
        {
            playback_timing.expected_mpv_entry_id = expected_mpv_entry_id;
            playback_timing.loaded = false;
        }
    }

    fn timing_commands_sent(&self, playback_generation: u64) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        if let Some(playback_timing) = self
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .playback_timing
            .as_mut()
            && playback_timing.playback_generation == playback_generation
        {
            playback_timing.queue_commands_sent = true;
        }
    }

    fn timing_loaded(&self) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        let state = self.state.borrow().clone();
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let playing_mpv_entry_id = bookkeeping.playing_mpv_entry_id;
        let event = bookkeeping.playback_timing.as_mut().and_then(|playback_timing| {
            let timing_matches_entry = match playback_timing.expected_mpv_entry_id {
                Some(expected_mpv_entry_id) => playing_mpv_entry_id == Some(expected_mpv_entry_id),
                None => state
                    .track()
                    .is_some_and(|track| track.video_id == playback_timing.video_id),
            };
            if !timing_matches_entry || !playback_timing.queue_commands_sent || playback_timing.loaded {
                return None;
            }
            let event = playback_timing.event(self.playback_generation.load(Ordering::SeqCst))?;
            playback_timing.loaded = true;
            Some(event)
        });
        drop(bookkeeping);
        drop(state);
        if let Some((playback_generation, video_id, elapsed_time)) = event {
            self.emit_timing(playback_generation, &video_id, "file_loaded", elapsed_time);
        }
    }

    fn timing_playing(&self, message: &Value, previous_position: f64) {
        if !self.timing_enabled.load(Ordering::Relaxed) {
            return;
        }
        let state = self.state.borrow().clone();
        let playback_advanced = match message.get("name").and_then(Value::as_str) {
            Some("time-pos") => message
                .get("data")
                .and_then(Value::as_f64)
                .is_some_and(|position| position > previous_position && position > 0.0),
            Some("core-idle") => message.get("data").and_then(Value::as_bool) == Some(false),
            _ => false,
        };
        if !playback_advanced || state.paused {
            return;
        }
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let event = bookkeeping.playback_timing.as_mut().and_then(|playback_timing| {
            if playback_timing.playing_timing_reported
                || !playback_timing.loaded
                || !state
                    .track()
                    .is_some_and(|track| track.video_id == playback_timing.video_id)
            {
                return None;
            }
            let event = playback_timing.event(self.playback_generation.load(Ordering::SeqCst))?;
            playback_timing.playing_timing_reported = true;
            Some(event)
        });
        drop(bookkeeping);
        drop(state);
        if let Some((playback_generation, video_id, elapsed_time)) = event {
            self.emit_timing(playback_generation, &video_id, "playing_approx", elapsed_time);
        }
    }

    fn begin_playback(&self) -> u64 {
        let mut current_request_id = self.current_playback_request_id.lock().expect("playback request");
        *current_request_id += 1;
        let playback_generation = self.activate_playback();
        drop(current_request_id);
        playback_generation
    }

    fn reserve_playback(&self) -> u64 {
        let mut current_request_id = self.current_playback_request_id.lock().expect("playback request");
        *current_request_id += 1;
        *current_request_id
    }

    fn playback_request_is_current(&self, playback_request_id: u64) -> bool {
        *self.current_playback_request_id.lock().expect("playback request") == playback_request_id
    }

    fn begin_reserved_playback(&self, playback_request_id: u64) -> Option<u64> {
        let current_request_id = self.current_playback_request_id.lock().expect("playback request");
        if *current_request_id != playback_request_id {
            return None;
        }
        let playback_generation = self.activate_playback();
        drop(current_request_id);
        Some(playback_generation)
    }

    fn activate_playback(&self) -> u64 {
        let playback_generation = self.playback_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping.recovering_mpv_entry_ids.clear();
        bookkeeping.recovery_stage_by_mpv_entry_id.clear();
        bookkeeping.pending_jump = None;
        drop(bookkeeping);
        self.state.send_modify(|state| {
            if self.playback_generation.load(Ordering::SeqCst) == playback_generation {
                state.pending_track = None;
                state.error = None;
                let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
                state.loading = bookkeeping.loading(state);
            }
        });
        self.playback_generation_changed.notify_waiters();
        playback_generation
    }

    fn pending_playback(
        &self,
        track: &Track,
        playback_generation: u64,
        jump_mpv_entry_id: Option<i64>,
    ) -> PendingPlayback<'_> {
        if let Some(mpv_entry_id) = jump_mpv_entry_id {
            self.bookkeeping
                .lock()
                .expect("player bookkeeping state")
                .pending_jump = Some((mpv_entry_id, playback_generation));
        }
        self.state.send_modify(|state| {
            if self.playback_generation.load(Ordering::SeqCst) == playback_generation {
                state.pending_track = Some(track.clone());
                state.loading = true;
                state.error = None;
            }
        });
        PendingPlayback {
            player: self,
            playback_generation,
            jump_mpv_entry_id,
        }
    }

    async fn resolve_selected(&self, video_id: &str, playback_generation: u64) -> SelectedStream {
        let started_at = Instant::now();
        let playback_changed = self.playback_generation_changed.notified();
        tokio::pin!(playback_changed);
        playback_changed.as_mut().enable();
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            self.emit_timing(
                playback_generation,
                video_id,
                "resolve.cancelled",
                started_at.elapsed(),
            );
            return SelectedStream::Cancelled;
        }
        let Some(stream_source) = &self.stream_source else {
            self.emit_timing(
                playback_generation,
                video_id,
                "resolve.no_source",
                started_at.elapsed(),
            );
            return SelectedStream::Ready(None);
        };
        if let Some(playable) = stream_source.cached(video_id) {
            self.emit_timing(
                playback_generation,
                video_id,
                "resolve.cache_hit",
                started_at.elapsed(),
            );
            return SelectedStream::Ready(Some(playable));
        }
        self.emit_timing(
            playback_generation,
            video_id,
            "resolve.cache_miss",
            Duration::ZERO,
        );
        let resolution_outcome = tokio::select! {
            biased;
            _ = playback_changed => None,
            playable = stream_source.resolve(video_id) => Some(playable),
        };
        match resolution_outcome {
            None => {
                self.emit_timing(
                    playback_generation,
                    video_id,
                    "resolve.cancelled",
                    started_at.elapsed(),
                );
                SelectedStream::Cancelled
            }
            Some(Err(message)) => {
                self.state.send_modify(|state| {
                    if self.playback_generation.load(Ordering::SeqCst) == playback_generation {
                        state.error = Some(message);
                        state.loading = state.pending_track.is_some();
                    }
                });
                self.emit_timing(
                    playback_generation,
                    video_id,
                    "resolve.failed",
                    started_at.elapsed(),
                );
                SelectedStream::Failed
            }
            Some(Ok(None)) if !stream_source.allow_mpv_fallback() => {
                self.resolution_failed(playback_generation);
                self.emit_timing(
                    playback_generation,
                    video_id,
                    "resolve.failed",
                    started_at.elapsed(),
                );
                SelectedStream::Failed
            }
            Some(Ok(playable)) => {
                let stage = match &playable {
                    None => "resolve.fallback",
                    Some(playable) if playable.warning.is_some() => "resolve.fallback",
                    Some(_) => "resolve.done",
                };
                self.emit_timing(playback_generation, video_id, stage, started_at.elapsed());
                SelectedStream::Ready(playable)
            }
        }
    }

    fn resolution_failed(&self, playback_generation: u64) {
        self.state.send_modify(|state| {
            if self.playback_generation.load(Ordering::SeqCst) == playback_generation {
                state
                    .error
                    .get_or_insert_with(|| "Could not resolve audio. Select the track to retry".to_string());
                state.loading = state.pending_track.is_some();
            }
        });
    }

    fn allows_mpv_fallback(&self) -> bool {
        self.stream_source
            .as_ref()
            .is_none_or(|stream_source| stream_source.allow_mpv_fallback())
    }

    async fn fail_resolve_hook(
        &self,
        hook_request_id: &str,
        original_target: &str,
        playback_generation: u64,
    ) {
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            return;
        }
        self.resolution_failed(playback_generation);
        let _ = self
            .command(vec![
                json!("script-message"),
                json!("redwax-resolution-failed"),
                json!(hook_request_id),
                json!(original_target),
            ])
            .await;
    }

    async fn resume_loaded(self: Arc<Self>) {
        let _queue_edit_guard = self.queue_edit_lock.lock().await;
        let pending_resume = self
            .bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .pending_resume
            .clone();
        let Some((playback_generation, video_id, resume_position_seconds, playback_intent)) = pending_resume
        else {
            return;
        };
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            return;
        }
        let Ok(playlist) = self.command(vec![json!("get_property"), json!("playlist")]).await else {
            return;
        };
        let current_playlist_entry = playlist.as_array().and_then(|mpv_playlist_entries| {
            mpv_playlist_entries
                .iter()
                .find(|entry| entry.get("current").and_then(Value::as_bool) == Some(true))
        });
        let current_track_matches = current_playlist_entry.is_some_and(|entry| {
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            track_for(&bookkeeping, entry).video_id == video_id
        });
        if !current_track_matches {
            return;
        }
        let seek_started_at = Instant::now();
        self.emit_timing(
            playback_generation,
            &video_id,
            "resume.seek_start",
            Duration::ZERO,
        );
        if let Err(error) = self
            .command(vec![
                json!("seek"),
                json!(resume_position_seconds),
                json!("absolute+exact"),
            ])
            .await
        {
            self.emit_timing(
                playback_generation,
                &video_id,
                "resume.seek_failed",
                seek_started_at.elapsed(),
            );
            self.state.send_modify(|state| {
                state.error = Some(format!("Could not resume saved position: {error}"));
            });
            return;
        }
        self.emit_timing(
            playback_generation,
            &video_id,
            "resume.seek_done",
            seek_started_at.elapsed(),
        );
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            return;
        }
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .pending_resume = None;
        if playback_intent == PlaybackIntent::Play
            && let Err(error) = self
                .command(vec![json!("set_property"), json!("pause"), json!(false)])
                .await
        {
            self.state
                .send_modify(|state| state.error = Some(error.to_string()));
        }
    }

    async fn command(&self, command_arguments: Vec<Value>) -> Result<Value> {
        let _lifecycle_transition_guard = self.lifecycle.transition_lock.lock().await;
        let ipc = {
            let state = self.lifecycle.state.lock().expect("player lifecycle");
            match &*state {
                LifecycleState::Running(engine) => Arc::clone(&engine.ipc),
                _ => return Err(Error::Mpv("not connected to mpv".to_string())),
            }
        };
        let command_result = ipc.command(command_arguments).await;
        if command_result.is_err() {
            self.timing_stage("mpv.command_failed");
        }
        command_result
    }

    async fn allow_ytdl_once(&self, path: &str, playback_generation: u64) -> Result<Option<String>> {
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            return Ok(None);
        }
        let playback_changed = self.playback_generation_changed.notified();
        tokio::pin!(playback_changed);
        playback_changed.as_mut().enable();
        let fallback_request_id = self
            .next_fallback_request_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let (acknowledgement_sender, acknowledgement_receiver) = oneshot::channel();
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_acknowledgements
            .insert(
                fallback_request_id.clone(),
                (path.to_string(), acknowledgement_sender),
            );
        let send_result = self
            .command(vec![
                json!("script-message"),
                json!("redwax-fallback"),
                json!(fallback_request_id),
                json!(path),
            ])
            .await;
        if let Err(error) = send_result {
            self.remove_fallback_ack(&fallback_request_id);
            return Err(error);
        }
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            self.remove_fallback_ack(&fallback_request_id);
            let _ = self.cancel_ytdl_once(&fallback_request_id, path).await;
            return Ok(None);
        }
        tokio::select! {
            biased;
            _ = playback_changed => {
                self.remove_fallback_ack(&fallback_request_id);
                let _ = self.cancel_ytdl_once(&fallback_request_id, path).await;
                Ok(None)
            }
            acknowledgement = acknowledgement_receiver => match acknowledgement {
                Ok(()) => Ok(Some(fallback_request_id)),
                Err(_) => Err(Error::Mpv(
                    "mpv closed before confirming yt-dlp fallback".to_string(),
                )),
            },
            _ = tokio::time::sleep(FALLBACK_ACK_BUDGET) => {
                self.remove_fallback_ack(&fallback_request_id);
                let _ = self.cancel_ytdl_once(&fallback_request_id, path).await;
                Err(Error::Mpv("timed out confirming yt-dlp fallback".to_string()))
            }
        }
    }

    async fn cancel_ytdl_once(&self, fallback_request_id: &str, path: &str) -> Result<()> {
        let (acknowledgement_sender, acknowledgement_receiver) = oneshot::channel();
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_cancel_acknowledgements
            .insert(
                fallback_request_id.to_string(),
                (path.to_string(), acknowledgement_sender),
            );
        let send_result = self
            .command(vec![
                json!("script-message"),
                json!("redwax-fallback-cancel"),
                json!(fallback_request_id),
                json!(path),
            ])
            .await;
        if let Err(error) = send_result {
            self.remove_fallback_cancel_ack(fallback_request_id);
            return Err(error);
        }
        match tokio::time::timeout(FALLBACK_ACK_BUDGET, acknowledgement_receiver).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Error::Mpv(
                "mpv closed before confirming yt-dlp fallback cancellation".to_string(),
            )),
            Err(_) => {
                self.remove_fallback_cancel_ack(fallback_request_id);
                Err(Error::Mpv(
                    "timed out confirming yt-dlp fallback cancellation".to_string(),
                ))
            }
        }
    }

    fn remove_fallback_ack(&self, fallback_request_id: &str) {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_acknowledgements
            .remove(fallback_request_id);
    }

    fn remove_fallback_cancel_ack(&self, fallback_request_id: &str) {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_cancel_acknowledgements
            .remove(fallback_request_id);
    }

    async fn resolve_hook(
        self: Arc<Self>,
        hook_request_id: String,
        original_target: String,
        playback_generation: u64,
    ) {
        let video_id = {
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            bookkeeping
                .stream_video_ids_by_target
                .get(&original_target)
                .cloned()
                .or_else(|| video_id_in(&original_target))
        };
        let target = match video_id {
            Some(video_id) => {
                match tokio::time::timeout(
                    HOOK_RESOLVE_BUDGET,
                    self.resolve_selected(&video_id, playback_generation),
                )
                .await
                {
                    Ok(SelectedStream::Ready(Some(playable))) => {
                        self.remember_stream(&video_id, &playable);
                        self.bookkeeping
                            .lock()
                            .expect("player bookkeeping state")
                            .stream_video_ids_by_target
                            .insert(original_target.clone(), video_id.clone());
                        playable.target
                    }
                    Ok(SelectedStream::Ready(None)) => {
                        self.remember_fallback(&video_id);
                        self.watch_url(&video_id)
                    }
                    Ok(SelectedStream::Cancelled) => String::new(),
                    Ok(SelectedStream::Failed) => {
                        self.fail_resolve_hook(&hook_request_id, &original_target, playback_generation)
                            .await;
                        return;
                    }
                    Err(_) => {
                        if !self.allows_mpv_fallback() {
                            self.fail_resolve_hook(&hook_request_id, &original_target, playback_generation)
                                .await;
                            return;
                        }
                        self.remember_fallback(&video_id);
                        self.emit_timing(
                            playback_generation,
                            &video_id,
                            "resolve.timeout",
                            HOOK_RESOLVE_BUDGET,
                        );
                        self.watch_url(&video_id)
                    }
                }
            }
            None => original_target.clone(),
        };
        let _ = self
            .command(vec![
                json!("script-message"),
                json!("redwax-resolved"),
                json!(hook_request_id),
                json!(original_target),
                json!(target),
            ])
            .await;
    }

    fn remember(&self, tracks: &[Track]) {
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        for track in tracks {
            let mut track = track.clone();
            if let Some(known_tracks_by_video_id) = bookkeeping.known_tracks_by_video_id.get(&track.video_id)
            {
                if track.artists.is_empty() {
                    track.artists.clone_from(&known_tracks_by_video_id.artists);
                }
                if track.title == track.video_id || track.title.starts_with("https://") {
                    track.title.clone_from(&known_tracks_by_video_id.title);
                }
                track.thumbnail_url = track
                    .thumbnail_url
                    .or_else(|| known_tracks_by_video_id.thumbnail_url.clone());
                track.album = track.album.or_else(|| known_tracks_by_video_id.album.clone());
                track.album_browse_id = track
                    .album_browse_id
                    .or_else(|| known_tracks_by_video_id.album_browse_id.clone());
                track.duration_seconds = track
                    .duration_seconds
                    .or(known_tracks_by_video_id.duration_seconds);
            }
            bookkeeping
                .known_tracks_by_video_id
                .insert(track.video_id.clone(), track.clone());
            bookkeeping.queueing_video_ids.insert(track.video_id.clone());
        }
    }

    fn watch_url(&self, video_id: &str) -> String {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .known_tracks_by_video_id
            .get(video_id)
            .map_or_else(|| bare_track(video_id, video_id), Clone::clone)
            .url()
    }

    /// The stream URL when it is already known, otherwise the YouTube URL for
    /// mpv's own hook.
    fn entry_url(&self, track: &Track) -> String {
        let Some(stream_source) = &self.stream_source else {
            return track.url();
        };
        let Some(playable) = stream_source.cached(&track.video_id) else {
            return track.url();
        };
        self.remember_stream(&track.video_id, &playable);
        playable.target
    }

    fn remember_stream(&self, video_id: &str, playable: &Playable) {
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let queued: HashSet<String> = bookkeeping
            .mpv_entries
            .values()
            .map(|(filename, _)| filename.clone())
            .collect();
        bookkeeping
            .stream_video_ids_by_target
            .retain(|url, mapped| mapped != video_id || url == &playable.target || queued.contains(url));
        bookkeeping
            .stream_video_ids_by_target
            .insert(playable.target.clone(), video_id.to_string());
        bookkeeping
            .tier_labels_by_video_id
            .insert(video_id.to_string(), playable.tier_label.clone());
        remember_warning(&mut bookkeeping, video_id, playable.warning.as_deref());
    }

    fn remember_fallback(&self, video_id: &str) {
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping.tier_labels_by_video_id.remove(video_id);
        bookkeeping
            .warnings_by_video_id
            .insert(video_id.to_string(), HOOK_FALLBACK_WARNING.to_string());
    }

    fn queued_as_youtube(&self, track: &Track) -> bool {
        let url = track.url();
        let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping
            .mpv_entries
            .values()
            .any(|(filename, _)| *filename == url)
    }

    fn prefetch(self: &Arc<Self>, tracks: Vec<Track>) {
        let Some(stream_source) = self.stream_source.clone() else {
            return;
        };
        let wanted: Vec<Track> = tracks
            .into_iter()
            .filter(|track| {
                (stream_source.cached(&track.video_id).is_none() || self.queued_as_youtube(track))
                    && self.begin_swap(&track.video_id)
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let mut tasks = self.background_tasks.lock().expect("player tasks");
        while tasks.try_join_next().is_some() {}
        for track in wanted {
            let reservation = SwapReservation {
                player: Arc::clone(self),
                video_id: track.video_id.clone(),
            };
            let player = Arc::clone(self);
            tasks.spawn(async move {
                let _reservation = reservation;
                player.swap_entries(&track).await;
            });
        }
    }

    fn begin_swap(&self, video_id: &str) -> bool {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .swapping_video_ids
            .insert(video_id.to_string())
    }

    async fn swap_entries(self: Arc<Self>, track: &Track) {
        let Some(stream_source) = self.stream_source.clone() else {
            return;
        };
        let Some(playable) = stream_source.resolve_prefetch(&track.video_id).await else {
            return;
        };
        let _queue_edit_guard = self.queue_edit_lock.lock().await;
        let playing = self.state.borrow().index;
        let Some(playing) = playing else { return };
        let queued: Vec<(i64, usize, String)> = {
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            bookkeeping
                .mpv_entries
                .iter()
                .filter(|(_, (filename, index))| {
                    *index > playing
                        && (*filename == track.url()
                            || bookkeeping.stream_video_ids_by_target.get(filename) == Some(&track.video_id))
                })
                .map(|(mpv_entry_id, (filename, index))| (*mpv_entry_id, *index, filename.clone()))
                .collect()
        };
        if !queued.is_empty() {
            self.remember(std::slice::from_ref(track));
            self.remember_stream(&track.video_id, &playable);
        }
        for (mpv_entry_id, index, old) in queued {
            if old != playable.target {
                let _ = self
                    .replace_entry(index, mpv_entry_id, &old, &playable.target, false)
                    .await;
            }
        }
    }

    async fn replace_entry(
        self: &Arc<Self>,
        queue_index: usize,
        expected_mpv_entry_id: i64,
        old_target: &str,
        new_target: &str,
        allow_playing: bool,
    ) -> Result<Option<i64>> {
        let playlist = self
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        let selected_entry = playlist
            .as_array()
            .and_then(|mpv_playlist_entries| mpv_playlist_entries.get(queue_index));
        let selected_entry_matches = selected_entry.is_some_and(|mpv_playlist_entry| {
            mpv_playlist_entry.get("id").and_then(Value::as_i64) == Some(expected_mpv_entry_id)
                && filename_of(mpv_playlist_entry) == old_target
                && (allow_playing || mpv_playlist_entry.get("current").and_then(Value::as_bool) != Some(true))
        });
        if !selected_entry_matches {
            return Ok(None);
        }
        let engine_generation = {
            let lifecycle = self.lifecycle.state.lock().expect("player lifecycle");
            match &*lifecycle {
                LifecycleState::Running(engine) => engine.engine_generation,
                _ => return Ok(None),
            }
        };
        let mut replacement = {
            let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            let logical_id = bookkeeping
                .logical_entry_ids_by_mpv_id
                .get(&expected_mpv_entry_id)
                .copied();
            let track = track_for(&bookkeeping, selected_entry.expect("matching selected entry"));
            bookkeeping.replacing_mpv_entry_id = Some(expected_mpv_entry_id);
            EntryReplacement {
                player: Arc::clone(self),
                engine_generation,
                old_mpv_entry_id: expected_mpv_entry_id,
                logical_id,
                track,
                inserted_mpv_entry_id: None,
                completed: false,
            }
        };
        let load_result = self
            .command(vec![
                json!("loadfile"),
                json!(new_target),
                json!("insert-at"),
                json!(queue_index),
            ])
            .await?;
        let inserted_mpv_entry_id = load_result.get("playlist_entry_id").and_then(Value::as_i64);
        replacement.inserted_mpv_entry_id = inserted_mpv_entry_id;
        let playlist = match self.command(vec![json!("get_property"), json!("playlist")]).await {
            Ok(playlist) => playlist,
            Err(error) => {
                self.rollback_inserted(queue_index, inserted_mpv_entry_id, new_target)
                    .await;
                return Err(error);
            }
        };
        let mpv_playlist_entries = playlist.as_array();
        let replacement_entry = mpv_playlist_entries.and_then(|entries| entries.get(queue_index));
        let displaced_entry = mpv_playlist_entries.and_then(|entries| entries.get(queue_index + 1));
        let replacement_mpv_entry_id = replacement_entry
            .filter(|entry| filename_of(entry) == new_target)
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_i64);
        replacement.inserted_mpv_entry_id = replacement_mpv_entry_id.or(inserted_mpv_entry_id);
        let displaced_entry_matches = displaced_entry.is_some_and(|entry| {
            entry.get("id").and_then(Value::as_i64) == Some(expected_mpv_entry_id)
                && filename_of(entry) == old_target
        });
        let displaced_entry_is_playing = displaced_entry
            .and_then(|entry| entry.get("current"))
            .and_then(Value::as_bool)
            == Some(true);
        let Some(replacement_mpv_entry_id) = replacement_mpv_entry_id else {
            if replacement_entry.is_some_and(|entry| filename_of(entry) == new_target) {
                let _ = self
                    .command(vec![json!("playlist-remove"), json!(queue_index)])
                    .await;
            }
            let playlist = self
                .command(vec![json!("get_property"), json!("playlist")])
                .await?;
            replacement.complete(None, playlist);
            return Ok(None);
        };
        if !displaced_entry_matches || (!allow_playing && displaced_entry_is_playing) {
            self.command(vec![json!("playlist-remove"), json!(queue_index)])
                .await?;
            let playlist = self
                .command(vec![json!("get_property"), json!("playlist")])
                .await?;
            replacement.complete(None, playlist);
            return Ok(None);
        }
        if let Err(error) = self
            .command(vec![json!("playlist-remove"), json!(queue_index + 1)])
            .await
        {
            let _ = self
                .command(vec![json!("playlist-remove"), json!(queue_index)])
                .await;
            return Err(error);
        }
        let playlist = self
            .command(vec![json!("get_property"), json!("playlist")])
            .await?;
        replacement.complete(
            Some((replacement_mpv_entry_id, new_target, queue_index)),
            playlist,
        );
        if allow_playing {
            self.timing_entry(
                self.playback_generation.load(Ordering::SeqCst),
                Some(replacement_mpv_entry_id),
            );
        }
        Ok(Some(replacement_mpv_entry_id))
    }

    async fn rollback_inserted(
        &self,
        original_queue_index: usize,
        inserted_mpv_entry_id: Option<i64>,
        filename: &str,
    ) {
        let Ok(playlist) = self.command(vec![json!("get_property"), json!("playlist")]).await else {
            return;
        };
        let rollback_index =
            playlist
                .as_array()
                .and_then(|mpv_playlist_entries| match inserted_mpv_entry_id {
                    Some(inserted_mpv_entry_id) => mpv_playlist_entries.iter().position(|entry| {
                        entry.get("id").and_then(Value::as_i64) == Some(inserted_mpv_entry_id)
                            && filename_of(entry) == filename
                    }),
                    None => mpv_playlist_entries
                        .get(original_queue_index)
                        .filter(|entry| filename_of(entry) == filename)
                        .map(|_| original_queue_index),
                });
        if let Some(rollback_index) = rollback_index {
            let _ = self
                .command(vec![json!("playlist-remove"), json!(rollback_index)])
                .await;
        }
    }

    /// Retry through the native resolver, then try mpv's yt-dlp once.
    async fn recover(
        self: Arc<Self>,
        mpv_entry_id: i64,
        index: usize,
        filename: String,
        playback_generation: u64,
    ) {
        let started_at = Instant::now();
        let Some(stream_source) = self.stream_source.clone() else {
            return;
        };
        let (video_id, track, direct, mut stage) = {
            let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            let direct = bookkeeping.stream_video_ids_by_target.get(&filename).cloned();
            let Some(video_id) = direct.clone().or_else(|| video_id_in(&filename)) else {
                return;
            };
            if !bookkeeping.recovering_mpv_entry_ids.insert(mpv_entry_id) {
                return;
            }
            let stage = match bookkeeping.recovery_stage_by_mpv_entry_id.get(&mpv_entry_id) {
                None => RecoveryStage::NativeRetry,
                Some(RecoveryStage::NativeRetry) => RecoveryStage::FinalFallback,
                Some(RecoveryStage::FinalFallback) => {
                    bookkeeping.recovering_mpv_entry_ids.remove(&mpv_entry_id);
                    return;
                }
            };
            bookkeeping
                .recovery_stage_by_mpv_entry_id
                .insert(mpv_entry_id, stage);
            let track = bookkeeping.known_tracks_by_video_id.get(&video_id).cloned();
            (video_id, track, direct.is_some(), stage)
        };
        self.emit_timing(playback_generation, &video_id, "recovery.start", Duration::ZERO);
        let track = track.unwrap_or_else(|| bare_track(&video_id, &video_id));
        let target = if stage == RecoveryStage::FinalFallback {
            if !stream_source.allow_mpv_fallback() {
                self.resolution_failed(playback_generation);
                self.finish_recovery(mpv_entry_id, None);
                return;
            }
            self.remember_fallback(&video_id);
            track.url()
        } else {
            if direct {
                stream_source.forget(&video_id);
            }
            let resolved = tokio::time::timeout(
                HOOK_RESOLVE_BUDGET,
                self.resolve_selected(&video_id, playback_generation),
            )
            .await;
            let resolved = match resolved {
                Ok(SelectedStream::Ready(resolved)) => Some(resolved),
                Ok(SelectedStream::Cancelled) => None,
                Ok(SelectedStream::Failed) => {
                    self.finish_recovery(mpv_entry_id, None);
                    return;
                }
                Err(_) => {
                    if !stream_source.allow_mpv_fallback() {
                        self.resolution_failed(playback_generation);
                        self.finish_recovery(mpv_entry_id, None);
                        return;
                    }
                    self.remember_fallback(&video_id);
                    self.emit_timing(
                        playback_generation,
                        &video_id,
                        "resolve.timeout",
                        HOOK_RESOLVE_BUDGET,
                    );
                    Some(None)
                }
            };
            let Some(resolved) = resolved else {
                self.finish_recovery(mpv_entry_id, None);
                self.emit_timing(
                    playback_generation,
                    &video_id,
                    "recovery.cancelled",
                    started_at.elapsed(),
                );
                return;
            };
            match resolved {
                Some(playable) if playable.target != track.url() => {
                    self.remember_stream(&video_id, &playable);
                    playable.target
                }
                Some(playable) => {
                    self.remember_stream(&video_id, &playable);
                    stage = RecoveryStage::FinalFallback;
                    self.mark_final_fallback(mpv_entry_id);
                    playable.target
                }
                None => {
                    stage = RecoveryStage::FinalFallback;
                    self.mark_final_fallback(mpv_entry_id);
                    self.remember_fallback(&video_id);
                    track.url()
                }
            }
        };
        let _queue_edit_guard = self.queue_edit_lock.lock().await;
        if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
            self.finish_recovery(mpv_entry_id, None);
            self.emit_timing(
                playback_generation,
                &video_id,
                "recovery.cancelled",
                started_at.elapsed(),
            );
            return;
        }
        let Ok(Some(replacement_id)) = self
            .replace_entry(index, mpv_entry_id, &filename, &target, true)
            .await
        else {
            self.finish_recovery(mpv_entry_id, None);
            self.emit_timing(
                playback_generation,
                &video_id,
                "recovery.skipped",
                started_at.elapsed(),
            );
            return;
        };
        self.finish_recovery(mpv_entry_id, Some((replacement_id, stage)));
        if target == track.url() {
            let permit = match self.allow_ytdl_once(&target, playback_generation).await {
                Ok(permit) => permit,
                Err(error) => {
                    self.state
                        .send_modify(|state| state.error = Some(error.to_string()));
                    self.emit_timing(
                        playback_generation,
                        &video_id,
                        "recovery.failed",
                        started_at.elapsed(),
                    );
                    return;
                }
            };
            let Some(fallback_request_id) = permit else {
                self.emit_timing(
                    playback_generation,
                    &video_id,
                    "recovery.cancelled",
                    started_at.elapsed(),
                );
                return;
            };
            if self.playback_generation.load(Ordering::SeqCst) != playback_generation {
                let _ = self.cancel_ytdl_once(&fallback_request_id, &target).await;
                self.emit_timing(
                    playback_generation,
                    &video_id,
                    "recovery.cancelled",
                    started_at.elapsed(),
                );
                return;
            }
        }
        let recovered = self
            .command(vec![json!("playlist-play-index"), json!(index)])
            .await;
        let stage = if recovered.is_ok() {
            "recovery.done"
        } else {
            "recovery.failed"
        };
        if let Err(error) = &recovered {
            self.state
                .send_modify(|state| state.error = Some(error.to_string()));
        }
        self.emit_timing(playback_generation, &video_id, stage, started_at.elapsed());
    }

    fn finish_recovery(&self, mpv_entry_id: i64, replacement: Option<(i64, RecoveryStage)>) {
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping.recovering_mpv_entry_ids.remove(&mpv_entry_id);
        bookkeeping.recovery_stage_by_mpv_entry_id.remove(&mpv_entry_id);
        if let Some((replacement_id, stage)) = replacement {
            bookkeeping
                .recovery_stage_by_mpv_entry_id
                .insert(replacement_id, stage);
        }
    }

    fn mark_final_fallback(&self, mpv_entry_id: i64) {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .recovery_stage_by_mpv_entry_id
            .insert(mpv_entry_id, RecoveryStage::FinalFallback);
    }

    fn handle_event_for(self: &Arc<Self>, engine_generation: u64, message: &Value) {
        if message.get("event").and_then(Value::as_str) == Some("shutdown") {
            self.engine_gone(engine_generation);
            return;
        }
        let lifecycle = self.lifecycle.state.lock().expect("player lifecycle");
        if !matches!(
            &*lifecycle,
            LifecycleState::Running(engine) if engine.engine_generation == engine_generation
        ) {
            return;
        }
        if message.get("event").and_then(Value::as_str) == Some("property-change")
            && message.get("name").and_then(Value::as_str) == Some("playlist-pos")
        {
            let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
            let refresh =
                bookkeeping.replacing_mpv_entry_id.is_none() && !bookkeeping.retired_mpv_entry_ids.is_empty();
            drop(bookkeeping);
            if refresh {
                let ipc = match &*lifecycle {
                    LifecycleState::Running(engine) => Arc::clone(&engine.ipc),
                    _ => unreachable!("generation checked while lifecycle lock held"),
                };
                let inner = Arc::clone(self);
                self.background_tasks
                    .lock()
                    .expect("player tasks")
                    .spawn(async move {
                        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
                        if let Ok(playlist) =
                            ipc.command(vec![json!("get_property"), json!("playlist")]).await
                        {
                            inner.handle_event_for(
                                engine_generation,
                                &json!({
                                    "event": "property-change", "name": "playlist", "data": playlist,
                                    "current_entry_authoritative": true,
                                }),
                            );
                        }
                    });
                return;
            }
        }
        self.handle_event(message);
    }

    fn handle_event(self: &Arc<Self>, message: &Value) {
        match message.get("event").and_then(Value::as_str).unwrap_or_default() {
            "client-message" => {
                let message_arguments = message.get("args").and_then(Value::as_array);
                let message_name = message_arguments
                    .and_then(|arguments| arguments.first())
                    .and_then(Value::as_str);
                let hook_request_id = message_arguments
                    .and_then(|arguments| arguments.get(1))
                    .and_then(Value::as_str);
                let original_target = message_arguments
                    .and_then(|arguments| arguments.get(2))
                    .and_then(Value::as_str);
                match (message_name, hook_request_id, original_target) {
                    (Some("redwax-fallback-ready"), Some(fallback_request_id), Some(path)) => {
                        let pending_acknowledgement = self
                            .bookkeeping
                            .lock()
                            .expect("player bookkeeping state")
                            .fallback_acknowledgements
                            .remove(fallback_request_id);
                        if let Some((expected_path, acknowledgement_sender)) = pending_acknowledgement
                            && expected_path == path
                        {
                            let _ = acknowledgement_sender.send(());
                        }
                    }
                    (Some("redwax-fallback-cancelled"), Some(fallback_request_id), Some(path)) => {
                        let pending_cancellation = self
                            .bookkeeping
                            .lock()
                            .expect("player bookkeeping state")
                            .fallback_cancel_acknowledgements
                            .remove(fallback_request_id);
                        if let Some((expected_path, acknowledgement_sender)) = pending_cancellation
                            && expected_path == path
                        {
                            let _ = acknowledgement_sender.send(());
                        }
                    }
                    (Some("redwax-resolve"), Some(hook_request_id), Some(original_target)) => {
                        let playback_generation = self.playback_generation.load(Ordering::SeqCst);
                        self.background_tasks.lock().expect("player tasks").spawn(
                            Arc::clone(self).resolve_hook(
                                hook_request_id.to_string(),
                                original_target.to_string(),
                                playback_generation,
                            ),
                        );
                    }
                    _ => {}
                }
            }
            "start-file" => {
                self.bookkeeping
                    .lock()
                    .expect("player bookkeeping state")
                    .playing_mpv_entry_id = message.get("playlist_entry_id").and_then(Value::as_i64);
            }
            "file-loaded" => {
                self.timing_loaded();
                self.background_tasks
                    .lock()
                    .expect("player tasks")
                    .spawn(Arc::clone(self).resume_loaded());
            }
            "property-change" => {
                let mut tracks_to_prefetch = Vec::new();
                let previous_position = self.state.borrow().position;
                self.state.send_modify(|state| {
                    self.apply_property(message, state, &mut tracks_to_prefetch);
                });
                self.prefetch(tracks_to_prefetch);
                self.timing_playing(message, previous_position);
            }
            "end-file" if message.get("reason").and_then(Value::as_str) == Some("error") => {
                self.timing_stage("file_failed");
                let playback_generation = self.playback_generation.load(Ordering::SeqCst);
                if let Some((mpv_entry_id, index, filename)) = self.dead_stream(message) {
                    self.background_tasks
                        .lock()
                        .expect("player tasks")
                        .spawn(Arc::clone(self).recover(mpv_entry_id, index, filename, playback_generation));
                    return;
                }
                let playback_error_detail = message
                    .get("file_error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                let reported_error = format!("Could not play track: {playback_error_detail}");
                self.state.send_modify(|state| state.error = Some(reported_error));
                let inner = Arc::clone(self);
                self.background_tasks
                    .lock()
                    .expect("player tasks")
                    .spawn(async move {
                        let _queue_edit_guard = inner.queue_edit_lock.lock().await;
                        if inner.playback_generation.load(Ordering::SeqCst) != playback_generation
                            || inner.state.borrow().error.is_none()
                        {
                            return;
                        }
                        let _ = inner
                            .command(vec![json!("set_property"), json!("pause"), json!(true)])
                            .await;
                    });
            }
            "end-file" => {
                if let Some(mpv_entry_id) = message.get("playlist_entry_id").and_then(Value::as_i64) {
                    let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
                    bookkeeping.recovering_mpv_entry_ids.remove(&mpv_entry_id);
                    bookkeeping.recovery_stage_by_mpv_entry_id.remove(&mpv_entry_id);
                }
            }
            "shutdown" => {
                let engine_generation = {
                    let lifecycle = self.lifecycle.state.lock().expect("player lifecycle");
                    match &*lifecycle {
                        LifecycleState::Running(engine) => Some(engine.engine_generation),
                        _ => None,
                    }
                };
                if let Some(engine_generation) = engine_generation {
                    self.engine_gone(engine_generation);
                }
            }
            _ => {}
        }
    }

    /// mpv is no longer there. Said once, and only while the player still
    /// believes it owns an engine, so a deliberate close says nothing.
    fn engine_gone(self: &Arc<Self>, engine_generation: u64) {
        let mut lifecycle = self.lifecycle.state.lock().expect("player lifecycle");
        if !matches!(
            &*lifecycle,
            LifecycleState::Running(engine) if engine.engine_generation == engine_generation
        ) {
            return;
        }
        let retry = self
            .restart_attempted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        let restart_generation = self
            .lifecycle
            .next_engine_generation
            .fetch_add(1, Ordering::Relaxed);
        let replacement_state = if retry {
            LifecycleState::RestartQueued(restart_generation)
        } else {
            LifecycleState::Stopped
        };
        let engine = match std::mem::replace(&mut *lifecycle, replacement_state) {
            LifecycleState::Running(engine) => engine,
            _ => unreachable!("generation checked while lifecycle lock held"),
        };
        let confirmed_state = self.state.borrow().clone();
        if !confirmed_state.queue.is_empty() {
            *self.restart_snapshot.lock().expect("player restart snapshot") = Some(confirmed_state.clone());
        }
        self.begin_playback();
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_acknowledgements
            .clear();
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .fallback_cancel_acknowledgements
            .clear();
        drop(engine);
        self.state.send_modify(|state| {
            state.paused = true;
            state.pending_track = None;
            state.loading = false;
            state.error = Some(match retry {
                true => "mpv stopped, reconnecting playback".to_string(),
                false => "playback unavailable, use Retry playback to try again".to_string(),
            });
            state.availability = if retry {
                PlaybackAvailability::Reconnecting
            } else {
                PlaybackAvailability::Unavailable
            };
        });
        if !retry {
            return;
        }
        let inner = Arc::clone(self);
        self.background_tasks
            .lock()
            .expect("player tasks")
            .spawn(async move {
                let _ = inner
                    .restore_playback(confirmed_state, Some(restart_generation))
                    .await;
            });
    }

    fn apply_property(&self, message: &Value, state: &mut PlayerState, tracks_to_prefetch: &mut Vec<Track>) {
        let property_name = message.get("name").and_then(Value::as_str).unwrap_or_default();
        let property_value = message.get("data").unwrap_or(&Value::Null);
        let mut bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        match property_name {
            "time-pos" => state.position = property_value.as_f64().unwrap_or(0.0),
            "duration" => state.duration = property_value.as_f64(),
            "pause" => state.paused = property_value.as_bool().unwrap_or(false),
            "playlist-pos" => {
                // Position notifications have no entry identity, so buffered replacement
                // notifications require an authoritative current-entry refresh.
                if bookkeeping.replacing_mpv_entry_id.is_some()
                    || !bookkeeping.retired_mpv_entry_ids.is_empty()
                {
                    return;
                }
                state.index = match property_value.as_i64() {
                    Some(index) if index >= 0 => Some(index as usize),
                    _ => None,
                };
                tracks_to_prefetch.extend(state.upcoming(self.options.prefetch_ahead));
            }
            "volume" => {
                if let Some(volume) = property_value.as_f64() {
                    state.volume = volume.round().clamp(0.0, 100.0) as u8;
                }
            }
            "playlist" => {
                let mpv_playlist_entries: &[Value] =
                    property_value.as_array().map(Vec::as_slice).unwrap_or_default();
                if bookkeeping.replacing_mpv_entry_id.is_some()
                    || mpv_playlist_entries.iter().any(|entry| {
                        entry
                            .get("id")
                            .and_then(Value::as_i64)
                            .is_some_and(|id| bookkeeping.retired_mpv_entry_ids.contains(&id))
                    })
                {
                    return;
                }
                let current_index = mpv_playlist_entries
                    .iter()
                    .position(|entry| entry.get("current").and_then(Value::as_bool) == Some(true));
                if current_index.is_some()
                    || message
                        .get("current_entry_authoritative")
                        .and_then(Value::as_bool)
                        == Some(true)
                {
                    state.index = current_index;
                }
                let mpv_entry_ids: HashSet<i64> = mpv_playlist_entries
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_i64))
                    .collect();
                bookkeeping
                    .logical_entry_ids_by_mpv_id
                    .retain(|mpv_entry_id, _| mpv_entry_ids.contains(mpv_entry_id));
                for (queue_index, mpv_playlist_entry) in mpv_playlist_entries.iter().enumerate() {
                    let Some(mpv_entry_id) = mpv_playlist_entry.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    if !bookkeeping
                        .logical_entry_ids_by_mpv_id
                        .contains_key(&mpv_entry_id)
                    {
                        let recovered = bookkeeping
                            .restart_logical_entry_ids
                            .as_ref()
                            .and_then(|logical_entry_ids| logical_entry_ids.get(queue_index))
                            .copied();
                        let logical_entry_id = match recovered {
                            Some(logical_entry_id) => logical_entry_id,
                            None => {
                                let logical_entry_id = QueueEntryId {
                                    player_instance_id: self.lifecycle.player_instance_id,
                                    entry_sequence: bookkeeping.next_logical_entry_sequence,
                                };
                                bookkeeping.next_logical_entry_sequence = bookkeeping
                                    .next_logical_entry_sequence
                                    .checked_add(1)
                                    .expect("queue entry identity exhausted");
                                logical_entry_id
                            }
                        };
                        bookkeeping
                            .logical_entry_ids_by_mpv_id
                            .insert(mpv_entry_id, logical_entry_id);
                    }
                }
                if bookkeeping
                    .restart_logical_entry_ids
                    .as_ref()
                    .is_some_and(|logical_entry_ids| mpv_playlist_entries.len() >= logical_entry_ids.len())
                {
                    bookkeeping.restart_logical_entry_ids = None;
                }
                bookkeeping.mpv_entries = mpv_playlist_entries
                    .iter()
                    .enumerate()
                    .filter_map(|(queue_index, mpv_playlist_entry)| {
                        let mpv_entry_id = mpv_playlist_entry.get("id").and_then(Value::as_i64)?;
                        Some((
                            mpv_entry_id,
                            (filename_of(mpv_playlist_entry).to_string(), queue_index),
                        ))
                    })
                    .collect();
                state.queue = mpv_playlist_entries
                    .iter()
                    .map(|mpv_playlist_entry| track_for(&bookkeeping, mpv_playlist_entry))
                    .collect();
                state.queue_entries = mpv_playlist_entries
                    .iter()
                    .zip(&state.queue)
                    .filter_map(|(mpv_playlist_entry, track)| {
                        let mpv_entry_id = mpv_playlist_entry.get("id").and_then(Value::as_i64)?;
                        let logical_id = *bookkeeping.logical_entry_ids_by_mpv_id.get(&mpv_entry_id)?;
                        Some(QueueEntry {
                            logical_id,
                            track: track.clone(),
                        })
                    })
                    .collect();
                state.queue_revision.0 = state
                    .queue_revision
                    .0
                    .checked_add(1)
                    .expect("queue revision exhausted");
                bookkeeping.tracks_by_mpv_entry_id = mpv_playlist_entries
                    .iter()
                    .zip(&state.queue)
                    .filter_map(|(mpv_playlist_entry, track)| {
                        let mpv_entry_id = mpv_playlist_entry.get("id").and_then(Value::as_i64)?;
                        Some((mpv_entry_id, track.clone()))
                    })
                    .collect();
                prune(&mut bookkeeping, &state.queue);
                tracks_to_prefetch.extend(state.upcoming(self.options.prefetch_ahead));
            }
            "mute" => state.muted = property_value.as_bool().unwrap_or(false),
            "loop-file" => bookkeeping.loop_file = loop_on(property_value),
            "loop-playlist" => bookkeeping.loop_playlist = loop_on(property_value),
            "paused-for-cache" => bookkeeping.paused_for_cache = property_value.as_bool().unwrap_or(false),
            "core-idle" => bookkeeping.core_idle = property_value.as_bool().unwrap_or(false),
            _ => {}
        }
        state.repeat = match (bookkeeping.loop_file, bookkeeping.loop_playlist) {
            (true, _) => Repeat::One,
            (false, true) => Repeat::All,
            (false, false) => Repeat::Off,
        };
        state.loading = bookkeeping.loading(state);
        let tier = state
            .track()
            .and_then(|track| bookkeeping.tier_labels_by_video_id.get(&track.video_id))
            .cloned();
        state.tier_label = tier;
        state.warning = state
            .track()
            .and_then(|track| bookkeeping.warnings_by_video_id.get(&track.video_id))
            .cloned();
    }

    #[cfg(test)]
    pub(crate) fn set_ipc(&self, ipc: Arc<dyn DynIpc>) {
        let engine_generation = self
            .lifecycle
            .next_engine_generation
            .fetch_add(1, Ordering::Relaxed);
        *self.lifecycle.state.lock().expect("player lifecycle") =
            LifecycleState::Running(Box::new(RunningEngine {
                engine_generation,
                ipc,
                session: None,
                _resources: None,
                event_reader_task: None,
            }));
    }

    #[cfg(test)]
    pub(crate) fn entry_position(&self, mpv_entry_id: i64) -> Option<usize> {
        self.bookkeeping
            .lock()
            .expect("player bookkeeping state")
            .mpv_entries
            .get(&mpv_entry_id)
            .map(|(_, index)| *index)
    }

    #[cfg(test)]
    pub(crate) fn is_recovering(&self, filename: &str) -> bool {
        let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        bookkeeping
            .mpv_entries
            .iter()
            .any(|(mpv_entry_id, (entry_filename, _))| {
                entry_filename == filename && bookkeeping.recovering_mpv_entry_ids.contains(mpv_entry_id)
            })
    }

    #[cfg(test)]
    pub(crate) fn remembered(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let sorted = |mut video_ids: Vec<String>| {
            video_ids.sort();
            video_ids
        };
        (
            sorted(bookkeeping.known_tracks_by_video_id.keys().cloned().collect()),
            sorted(bookkeeping.tier_labels_by_video_id.keys().cloned().collect()),
            sorted(bookkeeping.stream_video_ids_by_target.values().cloned().collect()),
        )
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_tasks(&self) {
        loop {
            let mut tasks = std::mem::take(&mut *self.background_tasks.lock().expect("player tasks"));
            if tasks.is_empty() {
                return;
            }
            while tasks.join_next().await.is_some() {}
        }
    }

    /// A failed managed network entry that still has a recovery attempt.
    fn dead_stream(&self, message: &Value) -> Option<(i64, usize, String)> {
        self.stream_source.as_ref()?;
        let mpv_entry_id = message.get("playlist_entry_id").and_then(Value::as_i64)?;
        let bookkeeping = self.bookkeeping.lock().expect("player bookkeeping state");
        let (filename, queue_index) = bookkeeping.mpv_entries.get(&mpv_entry_id)?;
        let video_id = bookkeeping
            .stream_video_ids_by_target
            .get(filename)
            .cloned()
            .or_else(|| video_id_in(filename));
        let fallback_is_exhausted = bookkeeping.recovery_stage_by_mpv_entry_id.get(&mpv_entry_id)
            == Some(&RecoveryStage::FinalFallback);
        video_id
            .is_some()
            .then_some(())
            .filter(|_| {
                !bookkeeping.recovering_mpv_entry_ids.contains(&mpv_entry_id) && !fallback_is_exhausted
            })
            .map(|_| (mpv_entry_id, *queue_index, filename.clone()))
    }
}

/// mpv's playlist is the queue, so what it no longer holds is worth keeping
/// only while a load or a prefetch is still on its way there.
fn prune(bookkeeping: &mut PlayerBookkeeping, queue: &[Track]) {
    let mut retained_video_ids: HashSet<String> = queue.iter().map(|track| track.video_id.clone()).collect();
    bookkeeping
        .queueing_video_ids
        .retain(|video_id| !retained_video_ids.contains(video_id));
    retained_video_ids.extend(bookkeeping.queueing_video_ids.iter().cloned());
    retained_video_ids.extend(bookkeeping.swapping_video_ids.iter().cloned());
    bookkeeping
        .known_tracks_by_video_id
        .retain(|video_id, _| retained_video_ids.contains(video_id));
    bookkeeping
        .tier_labels_by_video_id
        .retain(|video_id, _| retained_video_ids.contains(video_id));
    bookkeeping
        .warnings_by_video_id
        .retain(|video_id, _| retained_video_ids.contains(video_id));
    bookkeeping
        .stream_video_ids_by_target
        .retain(|_, video_id| retained_video_ids.contains(video_id));
    let retained_mpv_entry_ids: HashSet<i64> = bookkeeping.mpv_entries.keys().copied().collect();
    bookkeeping
        .recovery_stage_by_mpv_entry_id
        .retain(|mpv_entry_id, _| retained_mpv_entry_ids.contains(mpv_entry_id));
    bookkeeping
        .recovering_mpv_entry_ids
        .retain(|mpv_entry_id| retained_mpv_entry_ids.contains(mpv_entry_id));
}

fn remember_warning(bookkeeping: &mut PlayerBookkeeping, video_id: &str, warning: Option<&str>) {
    match warning {
        Some(warning) => {
            bookkeeping
                .warnings_by_video_id
                .insert(video_id.to_string(), warning.to_string());
        }
        None => {
            bookkeeping.warnings_by_video_id.remove(video_id);
        }
    }
}

fn filename_of(mpv_playlist_entry: &Value) -> &str {
    mpv_playlist_entry
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn track_for(bookkeeping: &PlayerBookkeeping, mpv_playlist_entry: &Value) -> Track {
    let filename = filename_of(mpv_playlist_entry);
    let video_id = bookkeeping
        .stream_video_ids_by_target
        .get(filename)
        .cloned()
        .or_else(|| video_id_in(filename))
        .unwrap_or_else(|| filename.to_string());
    match bookkeeping.known_tracks_by_video_id.get(&video_id) {
        Some(track) => track.clone(),
        None => {
            if let Some(track) = mpv_playlist_entry
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|mpv_entry_id| bookkeeping.tracks_by_mpv_entry_id.get(&mpv_entry_id))
            {
                return track.clone();
            }
            let network_file =
                url::Url::parse(filename).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
            let title = mpv_playlist_entry
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.starts_with("http://") && !title.starts_with("https://"))
                .unwrap_or(if network_file { "Unknown track" } else { filename });
            let video_id = if network_file && video_id == filename {
                match mpv_playlist_entry.get("id").and_then(Value::as_i64) {
                    Some(mpv_entry_id) => format!("mpv-entry:{mpv_entry_id}"),
                    None => "mpv-entry".to_string(),
                }
            } else {
                video_id
            };
            bare_track(&video_id, title)
        }
    }
}

fn video_id_in(filename: &str) -> Option<String> {
    let parsed = url::Url::parse(filename).ok()?;
    if parsed.path() != "/watch"
        || !matches!(
            parsed.host_str(),
            Some("music.youtube.com" | "www.youtube.com" | "youtube.com" | "m.youtube.com")
        )
    {
        return None;
    }
    parsed
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
}

/// All we know about a track mpv is playing that no page ever gave us.
pub fn bare_track(video_id: &str, title: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: Vec::new(),
        album: None,
        album_browse_id: None,
        duration_seconds: None,
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

/// mpv reports its loop properties as `no`, a count, or `inf`.
fn loop_on(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::String(text) => text != "no" && text != "0",
        Value::Number(number) => number.as_f64().is_some_and(|count| count != 0.0),
        _ => true,
    }
}

/// Build the mpv argv without the runtime script that `start` installs.
pub fn mpv_argv(options: &PlayerOptions, socket: &Path) -> Vec<String> {
    mpv_argv_with_script(options, socket, None)
}

fn mpv_argv_with_script(
    options: &PlayerOptions,
    socket: &Path,
    resolver_script: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        "mpv".to_string(),
        "--idle=yes".to_string(),
        "--vid=no".to_string(),
        "--audio-display=no".to_string(),
        "--ytdl=yes".to_string(),
        "--ytdl-format=bestaudio/best".to_string(),
        "--prefetch-playlist=yes".to_string(),
        "--no-terminal".to_string(),
        "--really-quiet".to_string(),
        format!("--volume={}", options.volume),
        format!("--input-ipc-server={}", socket.display()),
    ];
    if let Some(resolver_script) = resolver_script {
        argv.insert(7, format!("--script={}", resolver_script.display()));
    }
    if !options.ytdl_raw_options.is_empty() {
        argv.push(format!("--ytdl-raw-options={}", options.ytdl_raw_options));
    }
    argv.extend(options.mpv_args.iter().cloned());
    argv
}
