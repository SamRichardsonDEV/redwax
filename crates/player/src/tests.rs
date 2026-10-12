use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, mpsc};

use crate::ipc::{BoxFuture, DynIpc, Ipc, MpvIpc};
use crate::{
    EngineLauncher, EngineResources, Error, LaunchedEngine, LifecycleState, Playable, PlaybackAvailability,
    Player, PlayerOptions, PlayerState, QueueEdit, Repeat, SelectedStream, StreamSource, bare_track,
    mpv_argv, mpv_argv_with_script,
};

fn track(video_id: &str, title: &str) -> redwax_ytm::Track {
    bare_track(video_id, title)
}

fn options(dir: &str) -> PlayerOptions {
    PlayerOptions {
        socket_dir: temp_dir(dir),
        ..PlayerOptions::default()
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("redwax-player-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test socket directory");
    dir
}

fn quiet_player() -> Player {
    Player::new::<TestSource>(options("quiet"), None)
}

fn change(name: &str, data: Value) -> Value {
    json!({"event": "property-change", "name": name, "data": data})
}

fn playlist_event(entries: &[(i64, String)]) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .map(|(id, filename)| json!({"id": id, "filename": filename}))
        .collect();
    change("playlist", Value::Array(entries))
}

/// Streams that are always known, so the tests never wait on a network.
#[derive(Clone, Default)]
struct TestSource {
    state: Arc<TestSourceState>,
}

#[derive(Default)]
struct TestSourceState {
    urls: Mutex<HashMap<String, Playable>>,
    forgotten: Mutex<Vec<String>>,
}

#[derive(Clone, Default)]
struct SlowSource {
    fast: TestSource,
}

#[derive(Clone)]
struct GatedSource {
    gate: Arc<Semaphore>,
    resolves: Arc<AtomicUsize>,
}

impl Default for GatedSource {
    fn default() -> Self {
        GatedSource {
            gate: Arc::new(Semaphore::new(0)),
            resolves: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl StreamSource for GatedSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    async fn resolve(&self, video_id: &str) -> std::result::Result<Option<Playable>, String> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        self.gate
            .acquire()
            .await
            .expect("gated source remains open")
            .forget();
        Ok(Some(TestSource::stream_for(video_id)))
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

impl StreamSource for SlowSource {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        self.fast.cached(video_id)
    }

    async fn resolve(&self, video_id: &str) -> std::result::Result<Option<Playable>, String> {
        if video_id == "slow" {
            std::future::pending().await
        } else {
            self.fast.resolve(video_id).await
        }
    }

    fn prefetch(&self, video_ids: Vec<String>) {
        self.fast.prefetch(video_ids);
    }

    fn forget(&self, video_id: &str) {
        self.fast.forget(video_id);
    }
}

#[derive(Clone)]
struct WarningSource;

impl StreamSource for WarningSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    async fn resolve(&self, video_id: &str) -> std::result::Result<Option<Playable>, String> {
        Ok(Some(Playable {
            warning: Some("fallback in use".to_string()),
            ..TestSource::stream_for(video_id)
        }))
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

#[derive(Clone, Default)]
struct EmptySource;

impl StreamSource for EmptySource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    async fn resolve(&self, _video_id: &str) -> std::result::Result<Option<Playable>, String> {
        Ok(None)
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

#[derive(Clone)]
struct ExhaustedSource;

impl StreamSource for ExhaustedSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    fn allow_mpv_fallback(&self) -> bool {
        false
    }

    async fn resolve(&self, _video_id: &str) -> std::result::Result<Option<Playable>, String> {
        Ok(None)
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

#[derive(Clone)]
struct ErrorSource;

impl StreamSource for ErrorSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    fn allow_mpv_fallback(&self) -> bool {
        false
    }

    async fn resolve(&self, _video_id: &str) -> std::result::Result<Option<Playable>, String> {
        Err("Unavailable on YouTube Music.".to_string())
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

#[derive(Clone)]
struct GatedErrorSource {
    gate: Arc<Semaphore>,
}

impl StreamSource for GatedErrorSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    async fn resolve(&self, video_id: &str) -> std::result::Result<Option<Playable>, String> {
        if video_id != "old" {
            return Ok(Some(TestSource::stream_for(video_id)));
        }
        self.gate
            .acquire()
            .await
            .expect("gated error source remains open")
            .forget();
        Err("The old selection failed".to_string())
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

#[derive(Clone, Default)]
struct RefreshingSource {
    resolves: Arc<Mutex<usize>>,
}

impl StreamSource for RefreshingSource {
    fn cached(&self, _video_id: &str) -> Option<Playable> {
        None
    }

    fn resolve(
        &self,
        video_id: &str,
    ) -> impl Future<Output = std::result::Result<Option<Playable>, String>> + Send {
        let mut resolves = self.resolves.lock().expect("refreshing source");
        *resolves += 1;
        let target = format!("https://cdn/{video_id}?generation={}", *resolves);
        async move {
            Ok(Some(Playable {
                target,
                tier_label: "fresh".to_string(),
                warning: None,
            }))
        }
    }

    fn prefetch(&self, _video_ids: Vec<String>) {}

    fn forget(&self, _video_id: &str) {}
}

impl TestSource {
    fn stream_for(video_id: &str) -> Playable {
        Playable {
            target: format!("https://cdn/{video_id}?expire=99999999999"),
            tier_label: "774 opus 283k".to_string(),
            warning: None,
        }
    }

    fn forgotten(&self) -> Vec<String> {
        self.state.forgotten.lock().expect("test source").clone()
    }
}

impl StreamSource for TestSource {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        self.state
            .urls
            .lock()
            .expect("test source")
            .get(video_id)
            .cloned()
    }

    // The answer is ready before the future is polled, so the swap tasks run in
    // the order they were spawned, as the Python fakes do.
    fn resolve(
        &self,
        video_id: &str,
    ) -> impl Future<Output = std::result::Result<Option<Playable>, String>> + Send {
        let playable = TestSource::stream_for(video_id);
        self.state
            .urls
            .lock()
            .expect("test source")
            .insert(video_id.to_string(), playable.clone());
        async move { Ok(Some(playable)) }
    }

    fn prefetch(&self, video_ids: Vec<String>) {
        for video_id in video_ids {
            let playable = TestSource::stream_for(&video_id);
            self.state
                .urls
                .lock()
                .expect("test source")
                .insert(video_id, playable);
        }
    }

    fn forget(&self, video_id: &str) {
        self.state.urls.lock().expect("test source").remove(video_id);
        self.state
            .forgotten
            .lock()
            .expect("test source")
            .push(video_id.to_string());
    }
}

/// Records every command and keeps a playlist the swap can be checked against.
type MpvEventSink = dyn Fn(Value) + Send + Sync;

struct FakeMpv {
    calls: Mutex<Vec<Vec<Value>>>,
    playlist: Mutex<Vec<Value>>,
    next_entry_id: Mutex<i64>,
    playlist_gate: Mutex<Option<Arc<Semaphore>>>,
    playlist_gate_used: AtomicBool,
    playlist_requested: Arc<Semaphore>,
    quit_gate: Mutex<Option<Arc<Semaphore>>>,
    quit_requested: Semaphore,
    event_sink: Mutex<Option<Arc<MpvEventSink>>>,
}

impl FakeMpv {
    fn with_playlist(entries: &[(i64, String)]) -> Arc<FakeMpv> {
        let entries = entries
            .iter()
            .map(|(id, filename)| json!({"id": id, "filename": filename}))
            .collect();
        Arc::new(FakeMpv {
            calls: Mutex::new(Vec::new()),
            playlist: Mutex::new(entries),
            next_entry_id: Mutex::new(9),
            playlist_gate: Mutex::new(None),
            playlist_gate_used: AtomicBool::new(false),
            playlist_requested: Arc::new(Semaphore::new(0)),
            quit_gate: Mutex::new(None),
            quit_requested: Semaphore::new(0),
            event_sink: Mutex::new(None),
        })
    }

    fn with_gated_playlist(entries: &[(i64, String)]) -> Arc<FakeMpv> {
        let mpv = Self::with_playlist(entries);
        *mpv.playlist_gate.lock().expect("fake mpv playlist gate") = Some(Arc::new(Semaphore::new(0)));
        mpv
    }

    async fn wait_for_playlist_request(&self) {
        self.playlist_requested
            .acquire()
            .await
            .expect("fake mpv remains open")
            .forget();
    }

    fn release_playlist(&self) {
        if let Some(gate) = self
            .playlist_gate
            .lock()
            .expect("fake mpv playlist gate")
            .as_ref()
        {
            gate.add_permits(1);
        }
    }

    fn calls(&self) -> Vec<Vec<Value>> {
        self.calls.lock().expect("fake mpv").clone()
    }

    fn filenames(&self) -> Vec<String> {
        self.playlist
            .lock()
            .expect("fake mpv")
            .iter()
            .map(|entry| entry["filename"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Ipc for FakeMpv {
    async fn command(&self, args: Vec<Value>) -> crate::Result<Value> {
        self.calls.lock().expect("fake mpv").push(args.clone());
        let name = args.first().and_then(Value::as_str).unwrap_or_default();
        if name == "get_property"
            && args.get(1) == Some(&json!("playlist"))
            && !self.playlist_gate_used.swap(true, Ordering::SeqCst)
        {
            let gate = self.playlist_gate.lock().expect("fake mpv playlist gate").clone();
            if let Some(gate) = gate {
                self.playlist_requested.add_permits(1);
                gate.acquire().await.expect("fake mpv remains open").forget();
            }
        }
        if name == "quit" {
            self.quit_requested.add_permits(1);
            let gate = self.quit_gate.lock().expect("fake mpv quit gate").clone();
            if let Some(gate) = gate {
                gate.acquire()
                    .await
                    .expect("fake mpv quit gate remains open")
                    .forget();
            }
        }
        match name {
            "loadfile" if args.get(2) == Some(&json!("insert-at")) => {
                let at = args[3].as_u64().unwrap_or_default() as usize;
                let id = {
                    let mut next = self.next_entry_id.lock().expect("fake mpv entry id");
                    let id = *next;
                    *next += 1;
                    id
                };
                let mut playlist = self.playlist.lock().expect("fake mpv");
                let at = at.min(playlist.len());
                playlist.insert(at, json!({"id": id, "filename": args[1]}));
                let snapshot = playlist.clone();
                drop(playlist);
                let event_sink = self.event_sink.lock().expect("fake mpv event sink").clone();
                if let Some(event_sink) = event_sink {
                    event_sink(change("playlist", json!(snapshot)));
                    if let Some(index) = snapshot.iter().position(|entry| entry["current"] == json!(true)) {
                        event_sink(change("playlist-pos", json!(index)));
                    }
                }
                Ok(Value::Null)
            }
            "playlist-remove" => {
                let at = args[1].as_u64().unwrap_or_default() as usize;
                let mut playlist = self.playlist.lock().expect("fake mpv");
                if at < playlist.len() {
                    playlist.remove(at);
                }
                let snapshot = playlist.clone();
                drop(playlist);
                let event_sink = self.event_sink.lock().expect("fake mpv event sink").clone();
                if let Some(event_sink) = event_sink {
                    event_sink(change("playlist", json!(snapshot)));
                    if let Some(index) = snapshot.iter().position(|entry| entry["current"] == json!(true)) {
                        event_sink(change("playlist-pos", json!(index)));
                    }
                }
                Ok(Value::Null)
            }
            "playlist-move" => {
                let from = args[1].as_u64().unwrap_or_default() as usize;
                let to = args[2].as_u64().unwrap_or_default() as usize;
                let mut playlist = self.playlist.lock().expect("fake mpv");
                if from < playlist.len() {
                    let entry = playlist.remove(from);
                    let to = to.min(playlist.len());
                    playlist.insert(to, entry);
                }
                Ok(Value::Null)
            }
            "get_property" => Ok(Value::Array(self.playlist.lock().expect("fake mpv").clone())),
            _ => Ok(Value::Null),
        }
    }

    async fn observe(&self, _property: &str) -> crate::Result<()> {
        Ok(())
    }
}

struct FakeLauncher {
    launches: AtomicUsize,
    fail_next: AtomicBool,
    resources: Mutex<Vec<(PathBuf, PathBuf)>>,
    engines: Mutex<Vec<Arc<FakeMpv>>>,
    event_senders: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
    launch_gate: Mutex<Option<Arc<Semaphore>>>,
    launch_started: Semaphore,
}

impl Default for FakeLauncher {
    fn default() -> Self {
        FakeLauncher {
            launches: AtomicUsize::new(0),
            fail_next: AtomicBool::new(false),
            resources: Mutex::new(Vec::new()),
            engines: Mutex::new(Vec::new()),
            event_senders: Mutex::new(Vec::new()),
            launch_gate: Mutex::new(None),
            launch_started: Semaphore::new(0),
        }
    }
}

impl EngineLauncher for FakeLauncher {
    fn launch<'a>(
        &'a self,
        _options: &'a PlayerOptions,
        resources: &'a EngineResources,
    ) -> BoxFuture<'a, crate::Result<LaunchedEngine>> {
        Box::pin(async move {
            self.launches.fetch_add(1, Ordering::SeqCst);
            self.resources.lock().expect("fake launcher resources").push((
                resources.socket_path.clone(),
                resources.resolver_script_path.clone(),
            ));
            std::fs::write(&resources.socket_path, b"fake socket")?;
            std::fs::write(&resources.resolver_script_path, b"fake resolver")?;
            self.launch_started.add_permits(1);
            let gate = self.launch_gate.lock().expect("fake launcher gate").clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("fake launch remains open").forget();
            }
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(Error::Mpv("injected launch failure".to_string()));
            }
            let ipc = FakeMpv::with_playlist(&[]);
            self.engines
                .lock()
                .expect("fake launcher engines")
                .push(Arc::clone(&ipc));
            let (event_sender, event_receiver) = mpsc::unbounded_channel();
            self.event_senders
                .lock()
                .expect("fake launcher event senders")
                .push(event_sender);
            Ok(LaunchedEngine {
                ipc: ipc as Arc<dyn DynIpc>,
                event_receiver,
                session: None,
            })
        })
    }
}

fn player_with_launcher(name: &str, launcher: Arc<FakeLauncher>) -> Player {
    Player::with_launcher(options(name), None, launcher)
}

fn player_with_source_and_launcher(name: &str, launcher: Arc<FakeLauncher>) -> Player {
    Player::with_launcher(options(name), Some(Arc::new(TestSource::default())), launcher)
}

fn resources_for(launcher: &FakeLauncher, index: usize) -> (PathBuf, PathBuf) {
    launcher.resources.lock().expect("fake launcher resources")[index].clone()
}

async fn wait_for(player: &Player, wanted: impl Fn(&PlayerState) -> bool) {
    let mut receiver = player.state();
    let watching = async {
        loop {
            if wanted(&receiver.borrow_and_update()) {
                return;
            }
            receiver.changed().await.expect("player state channel");
        }
    };
    tokio::time::timeout(Duration::from_secs(5), watching)
        .await
        .expect("mpv never reported it");
}

async fn with_fallback_ack<T>(player: &Player, mpv: &FakeMpv, future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::task::yield_now() => {
                if let Some(call) = mpv.calls().iter().rev().find(|call| {
                    call.first() == Some(&json!("script-message"))
                        && call.get(1) == Some(&json!("redwax-fallback"))
                }) {
                    player.inner.handle_event(&json!({
                        "event": "client-message",
                        "args": ["redwax-fallback-ready", call[2], call[3]],
                    }));
                }
            }
        }
    }
}

fn is_fallback_call(call: &[Value], path: &str) -> bool {
    call.first() == Some(&json!("script-message"))
        && call.get(1) == Some(&json!("redwax-fallback"))
        && call.get(3) == Some(&json!(path))
}

#[test]
fn test_state_decodes_mpv_playlist_and_flags() {
    let player = Player::new::<TestSource>(
        PlayerOptions {
            volume: 50,
            ..options("state-decodes")
        },
        None,
    );
    let mut receiver = player.state();
    let tracks = vec![track("a1", "One"), track("b2", "Two")];
    player.inner.remember(&tracks);

    let events = [
        change(
            "playlist",
            json!([
                {"filename": "https://music.youtube.com/watch?v=a1", "current": true},
                {"filename": "https://music.youtube.com/watch?v=b2"},
                {"filename": "https://music.youtube.com/watch?v=zz", "title": "Unknown"},
            ]),
        ),
        change("playlist-pos", json!(1)),
        change("time-pos", json!(12.5)),
        change("duration", json!(200.0)),
        change("volume", json!(42.4)),
        change("core-idle", json!(false)),
    ];
    let mut seen = 0;
    for event in &events {
        player.inner.handle_event(event);
        if receiver.has_changed().expect("player state channel") {
            receiver.borrow_and_update();
            seen += 1;
        }
    }

    let state = player.snapshot();
    let titles: Vec<&str> = state.queue.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["One", "Two", "Unknown"]);
    assert_eq!(state.track(), Some(&tracks[1]));
    assert_eq!(state.upcoming(8)[0].video_id, "zz");
    assert_eq!(
        (state.position, state.duration, state.volume, state.loading),
        (12.5, Some(200.0), 42, false)
    );
    assert_eq!(seen, 6);
}

#[test]
fn test_loading_is_true_while_mpv_resolves_the_stream() {
    let player = quiet_player();
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.handle_event(&change("pause", json!(false)));
    player.inner.handle_event(&change("core-idle", json!(true)));
    assert!(player.snapshot().loading);
    player.inner.handle_event(&change("core-idle", json!(false)));
    assert!(!player.snapshot().loading);
}

#[tokio::test]
async fn playback_error_survives_property_updates_and_pauses_the_queue() {
    let player = quiet_player();
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut receiver = player.state();

    player
        .inner
        .handle_event(&json!({"event": "end-file", "reason": "error", "file_error": "loading failed"}));
    assert!(receiver.has_changed().expect("player state channel"));
    assert_eq!(
        receiver.borrow_and_update().error.as_deref(),
        Some("Could not play track: loading failed")
    );

    player.inner.handle_event(&change("pause", json!(true)));
    assert!(receiver.has_changed().expect("player state channel"));
    assert_eq!(
        receiver.borrow_and_update().error.as_deref(),
        Some("Could not play track: loading failed")
    );
    player.inner.wait_for_tasks().await;
    assert!(
        mpv.calls()
            .contains(&vec![json!("set_property"), json!("pause"), json!(true)])
    );
    player.toggle_pause().await.expect("explicit playback command");
    assert_eq!(player.snapshot().error, None);
}

#[test]
fn test_repeat_follows_mpv_loop_properties() {
    let player = quiet_player();
    player.inner.handle_event(&change("loop-playlist", json!("inf")));
    assert_eq!(player.snapshot().repeat, Repeat::All);
    player.inner.handle_event(&change("loop-file", json!("inf")));
    assert_eq!(player.snapshot().repeat, Repeat::One);
    player.inner.handle_event(&change("loop-file", json!(false)));
    player.inner.handle_event(&change("loop-playlist", json!(false)));
    assert_eq!(player.snapshot().repeat, Repeat::Off);
}

#[tokio::test]
async fn test_play_builds_the_queue_stopped_so_the_first_track_never_flashes() {
    let player = Player::new::<TestSource>(options("play-stopped"), None);
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());

    let tracks = vec![track("a", "One"), track("b", "Two"), track("c", "Three")];
    with_fallback_ack(&player, &mpv, player.play(tracks.clone(), 2))
        .await
        .expect("play");

    let sent = mpv.calls();
    let stopped = sent
        .iter()
        .position(|call| *call == vec![json!("stop")])
        .expect("queue stopped before rebuild");
    let loads: Vec<Vec<Value>> = sent
        .iter()
        .filter(|call| call[0] == json!("loadfile"))
        .cloned()
        .collect();
    let wanted: Vec<Vec<Value>> = tracks
        .iter()
        .map(|t| vec![json!("loadfile"), json!(t.url()), json!("append")])
        .collect();
    assert_eq!(loads, wanted);
    let first_load = sent
        .iter()
        .position(|call| call[0] == json!("loadfile"))
        .expect("queue load");
    assert!(stopped < first_load);

    let jump = vec![json!("playlist-play-index"), json!(2)];
    let last_load = vec![json!("loadfile"), json!(tracks[2].url()), json!("append")];
    assert!(sent.contains(&jump));
    assert!(sent.iter().position(|c| *c == jump) > sent.iter().position(|c| *c == last_load));
}

#[tokio::test]
async fn test_play_past_the_end_resolves_the_track_it_plays() {
    let source = TestSource::default();
    let player = Player::new(options("play-clamped"), Some(source.clone()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());

    let tracks = vec![track("a", "One"), track("b", "Two"), track("c", "Three")];
    player.play(tracks.clone(), 99).await.expect("play");

    assert!(source.cached("c").is_some(), "the last track is the one resolved");
    assert_eq!(source.cached("a"), None);
    let sent = mpv.calls();
    assert!(sent.contains(&vec![json!("playlist-play-index"), json!(2)]));
}

#[tokio::test]
async fn test_a_replaced_queue_leaves_no_track_behind() {
    let source = TestSource::default();
    let player = Player::new(options("prune"), Some(source));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());

    let first = vec![track("a", "One"), track("b", "Two"), track("c", "Three")];
    player.play(first.clone(), 0).await.expect("play");
    let entries: Vec<(i64, String)> = first
        .iter()
        .enumerate()
        .map(|(index, t)| (index as i64, player.inner.entry_url(t)))
        .collect();
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.wait_for_tasks().await;
    let (known_tracks_by_video_id, _, _) = player.inner.remembered();
    assert_eq!(known_tracks_by_video_id, ["a", "b", "c"]);

    let second = vec![track("d", "Four")];
    player.play(second.clone(), 0).await.expect("play");
    let entry = player.inner.entry_url(&second[0]);
    player.inner.handle_event(&playlist_event(&[(4, entry)]));
    player.inner.wait_for_tasks().await;

    let (known_tracks_by_video_id, tier_labels_by_video_id, direct) = player.inner.remembered();
    assert_eq!(
        known_tracks_by_video_id,
        ["d"],
        "a replaced queue keeps only what it holds"
    );
    assert_eq!(tier_labels_by_video_id, ["d"]);
    assert_eq!(direct, ["d"]);
}

#[tokio::test]
async fn replacing_upcoming_keeps_the_current_playback_and_queue_modes() {
    let player = Player::new::<TestSource>(options("replace-upcoming"), None);
    let current = track("a", "One");
    let old_next = track("b", "Old next");
    player.inner.remember(&[current.clone(), old_next.clone()]);
    let entries = vec![(1, current.url()), (2, old_next.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.handle_event(&change("time-pos", json!(42.5)));
    player.inner.handle_event(&change("pause", json!(true)));
    player.inner.handle_event(&change("loop-playlist", json!("inf")));
    player.set_shuffle(true).await.expect("shuffle");
    let calls_before = mpv.calls().len();
    let upcoming = vec![track("c", "New one"), track("d", "New two")];

    player
        .replace_upcoming(upcoming.clone())
        .await
        .expect("replace upcoming");

    let calls = mpv.calls();
    let replaced = &calls[calls_before..];
    assert_eq!(replaced[0], vec![json!("playlist-clear")]);
    assert_eq!(
        replaced
            .iter()
            .filter(|call| call[0] == json!("loadfile"))
            .cloned()
            .collect::<Vec<_>>(),
        upcoming
            .iter()
            .map(|track| vec![json!("loadfile"), json!(track.url()), json!("append-play")])
            .collect::<Vec<_>>()
    );
    assert_eq!(replaced.last(), Some(&vec![json!("playlist-shuffle")]));
    assert!(!replaced.iter().any(|call| call[0] == json!("stop")));
    assert!(
        !replaced
            .iter()
            .any(|call| call[0] == json!("playlist-play-index"))
    );
    assert!(!replaced.iter().any(|call| call[0] == json!("set_property")));
    let state = player.snapshot();
    assert_eq!(state.track(), Some(&current));
    assert_eq!(state.position, 42.5);
    assert!(state.paused);
    assert_eq!(state.repeat, Repeat::All);
    assert!(state.shuffle);
}

#[tokio::test]
async fn test_ipc_round_trip_against_a_fake_mpv() {
    let socket = temp_dir("ipc-round-trip").join("mpv.sock");
    let listener = UnixListener::bind(&socket).expect("fake mpv socket");
    let server = tokio::spawn(fake_mpv_server(listener));

    let (sender, mut events) = mpsc::unbounded_channel();
    let ipc = MpvIpc::connect(&socket, sender, None).await.expect("connect");

    let version = Ipc::command(&ipc, vec![json!("get_property"), json!("mpv-version")])
        .await
        .expect("version");
    assert_eq!(version, json!("0.41.0"));
    Ipc::observe(&ipc, "volume").await.expect("observe");
    assert!(Ipc::command(&ipc, vec![json!("nonsense")]).await.is_err());

    let event = events.recv().await.expect("one event");
    assert_eq!(
        event,
        json!({"event": "property-change", "name": "volume", "data": 80})
    );
    assert!(events.try_recv().is_err());

    ipc.close().await;
    server.abort();
}

async fn fake_mpv_server(listener: UnixListener) {
    let Ok((stream, _)) = listener.accept().await else {
        return;
    };
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let request: Value = serde_json::from_str(&line).expect("fake mpv request");
        let id = request["request_id"].clone();
        let command = request["command"].as_array().cloned().unwrap_or_default();
        let reply = match command.first().and_then(Value::as_str).unwrap_or_default() {
            "get_property" => json!({"request_id": id, "error": "success", "data": "0.41.0"}),
            "observe_property" => {
                let event = json!({"event": "property-change", "name": command[2], "data": 80});
                let _ = write.write_all(format!("{event}\n").as_bytes()).await;
                json!({"request_id": id, "error": "success"})
            }
            _ => json!({"request_id": id, "error": "invalid parameter"}),
        };
        let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
        let _ = write.flush().await;
    }
}

#[tokio::test]
async fn test_real_mpv_starts_and_answers() {
    let options = PlayerOptions {
        volume: 37,
        mpv_args: vec!["--ao=null".to_string()],
        ..options("real-mpv")
    };
    let player = Player::new::<TestSource>(options, None);
    player.start().await.expect("mpv starts");

    let version = player
        .inner
        .command(vec![json!("get_property"), json!("mpv-version")])
        .await
        .expect("mpv answers");
    assert!(version.as_str().unwrap_or_default().starts_with("mpv"));

    wait_for(&player, |state| state.volume == 37).await;
    player.adjust_volume(-7).await.expect("volume down");
    wait_for(&player, |state| state.volume == 30).await;

    player.close().await.expect("close");
}

#[tokio::test]
async fn test_player_hands_mpv_known_streams_and_maps_them_back() {
    let source = TestSource::default();
    let _ = source.resolve("a1").await;
    let player = Player::new(options("known_tracks_by_video_id-stream_source"), Some(source));

    let tracks = vec![track("a1", "One"), track("b2", "Two")];
    player.inner.remember(&tracks);
    let first = player.inner.entry_url(&tracks[0]);
    assert!(first.starts_with("https://cdn/a1"));
    assert_eq!(player.inner.entry_url(&tracks[1]), tracks[1].url());

    player
        .inner
        .handle_event(&playlist_event(&[(7, first), (8, tracks[1].url())]));

    let state = player.snapshot();
    let titles: Vec<&str> = state.queue.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["One", "Two"]);
    assert_eq!(player.inner.entry_position(7), Some(0));
}

#[tokio::test]
async fn test_prefetch_swaps_a_queued_youtube_url_for_its_stream() {
    let player = Player::new(options("prefetch-swap"), Some(TestSource::default()));
    let tracks = vec![track("a1", "One"), track("b2", "Two"), track("c3", "Three")];
    player.inner.remember(&tracks);

    let entries: Vec<(i64, String)> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (i as i64 + 1, t.url()))
        .collect();
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.wait_for_tasks().await;

    let filenames = mpv.filenames();
    assert_eq!(filenames[0], tracks[0].url());
    assert!(filenames[1].starts_with("https://cdn/b2"));
    assert!(filenames[2].starts_with("https://cdn/c3"));

    let calls = mpv.calls();
    assert!(calls.contains(&vec![json!("playlist-remove"), json!(2)]));
    assert!(calls.contains(&vec![json!("playlist-remove"), json!(3)]));
    // The playing entry is never touched.
    assert!(
        !calls
            .iter()
            .any(|call| call[0] == json!("loadfile") && call.get(3) == Some(&json!(0)))
    );
}

#[tokio::test]
async fn prefetch_refreshes_a_queued_direct_url_by_video_identity() {
    let source = TestSource::default();
    let player = Player::new(options("prefetch-refresh-direct"), Some(source));
    let tracks = vec![track("a", "One"), track("b", "Two")];
    player.inner.remember(&tracks);
    let stale = Playable {
        target: "https://old.example/b".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember_stream("b", &stale);
    let entries = vec![(1, tracks[0].url()), (2, stale.target)];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));

    player.inner.wait_for_tasks().await;

    assert_eq!(mpv.filenames()[1], TestSource::stream_for("b").target);
}

#[tokio::test]
async fn load_hook_resolves_a_watch_url_before_mpv_opens_it() {
    let source = TestSource::default();
    let player = Player::new(options("load-hook"), Some(source));
    let track = track("a1", "One");
    player.inner.remember(std::slice::from_ref(&track));
    let mpv = FakeMpv::with_playlist(&[(1, track.url())]);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "17", track.url()],
    }));
    player.inner.wait_for_tasks().await;

    assert!(mpv.calls().contains(&vec![
        json!("script-message"),
        json!("redwax-resolved"),
        json!("17"),
        json!(track.url()),
        json!(TestSource::stream_for("a1").target),
    ]));
}

#[tokio::test]
async fn load_hook_leaves_an_unmanaged_http_url_alone() {
    let source = RefreshingSource::default();
    let player = Player::new(options("load-hook-unmanaged"), Some(source.clone()));
    let original = "https://example.com/audio?v=not-a-video";
    let mpv = FakeMpv::with_playlist(&[(1, original.to_string())]);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "unmanaged", original],
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(*source.resolves.lock().expect("refreshing source"), 0);
    assert!(mpv.calls().contains(&vec![
        json!("script-message"),
        json!("redwax-resolved"),
        json!("unmanaged"),
        json!(original),
        json!(original),
    ]));
}

#[tokio::test]
async fn load_hook_refreshes_a_stale_direct_url() {
    let source = TestSource::default();
    let player = Player::new(options("load-hook-direct"), Some(source));
    let stale = Playable {
        target: "https://old.example/a1".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember_stream("a1", &stale);
    let mpv = FakeMpv::with_playlist(&[(1, stale.target.clone())]);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "18", stale.target],
    }));
    player.inner.wait_for_tasks().await;

    assert!(mpv.calls().iter().any(|call| {
        call.first() == Some(&json!("script-message"))
            && call.get(4) == Some(&json!(TestSource::stream_for("a1").target))
    }));
}

#[tokio::test]
async fn load_hook_redirects_an_unresolved_direct_url_to_the_watch_page() {
    let player = Player::new(options("load-hook-direct-fallback"), Some(EmptySource));
    let track = track("a1", "One");
    let stale = Playable {
        target: "https://old.example/a1".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("a1", &stale);
    let mpv = FakeMpv::with_playlist(&[(1, stale.target.clone())]);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "19", stale.target],
    }));
    player.inner.wait_for_tasks().await;

    assert!(mpv.calls().contains(&vec![
        json!("script-message"),
        json!("redwax-resolved"),
        json!("19"),
        json!("https://old.example/a1"),
        json!(track.url()),
    ]));
}

#[tokio::test]
async fn an_exhausted_source_does_not_start_another_mpv_fallback() {
    let player = Player::new(options("exhausted-play"), Some(ExhaustedSource));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());

    player
        .play(vec![track("a1", "Requested song")], 0)
        .await
        .expect("failure is published in state");

    assert!(
        player
            .snapshot()
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Could not resolve audio"))
    );
    assert!(player.snapshot().pending_track.is_none());
    assert!(!player.snapshot().loading);
    assert_eq!(mpv.calls(), vec![vec![json!("get_property"), json!("playlist")]]);
}

#[tokio::test]
async fn a_source_error_is_the_explicit_playback_error() {
    let player = Player::new(options("source-error-play"), Some(ErrorSource));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |_, _, stage, _| {
        captured.lock().expect("playback_timing events").push(stage);
    })));

    player
        .play(vec![track("a1", "Requested song")], 0)
        .await
        .expect("failure is published in state");

    let state = player.snapshot();
    assert_eq!(state.error.as_deref(), Some("Unavailable on YouTube Music."));
    assert!(state.pending_track.is_none());
    assert!(!state.loading);
    assert!(
        events
            .lock()
            .expect("playback_timing events")
            .contains(&"resolve.failed")
    );
    assert_eq!(mpv.calls(), vec![vec![json!("get_property"), json!("playlist")]]);
}

#[tokio::test]
async fn an_exhausted_load_hook_aborts_instead_of_opening_the_watch_url() {
    let player = Player::new(options("exhausted-hook"), Some(ExhaustedSource));
    let track = track("a1", "Requested song");
    player.inner.remember(std::slice::from_ref(&track));
    let mpv = FakeMpv::with_playlist(&[(1, track.url())]);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "failed", track.url()],
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(
        mpv.calls(),
        vec![vec![
            json!("script-message"),
            json!("redwax-resolution-failed"),
            json!("failed"),
            json!(track.url()),
        ]]
    );
    assert!(player.snapshot().error.is_some());
}

#[tokio::test]
async fn a_source_error_survives_the_load_hook_failure_response() {
    let player = Player::new(options("source-error-hook"), Some(ErrorSource));
    let track = track("a1", "Requested song");
    player.inner.remember(std::slice::from_ref(&track));
    let mpv = FakeMpv::with_playlist(&[(1, track.url())]);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "failed", track.url()],
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(
        player.snapshot().error.as_deref(),
        Some("Unavailable on YouTube Music.")
    );
    assert_eq!(
        mpv.calls(),
        vec![vec![
            json!("script-message"),
            json!("redwax-resolution-failed"),
            json!("failed"),
            json!(track.url()),
        ]]
    );
}

#[tokio::test]
async fn a_cancelled_source_error_cannot_replace_the_latest_selection() {
    let gate = Arc::new(Semaphore::new(0));
    let source = GatedErrorSource {
        gate: Arc::clone(&gate),
    };
    let player = Player::new(options("source-error-cancelled"), Some(source));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut old = Box::pin(player.play(vec![track("old", "Old selection")], 0));
    tokio::select! {
        biased;
        _ = &mut old => panic!("the gated source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    player
        .play(vec![track("new", "Latest selection")], 0)
        .await
        .expect("latest selection");
    gate.add_permits(1);
    old.await.expect("obsolete selection is cancelled");

    assert_eq!(player.snapshot().error, None);
    let loads: Vec<_> = mpv
        .calls()
        .into_iter()
        .filter(|call| call.first() == Some(&json!("loadfile")))
        .collect();
    assert_eq!(loads.len(), 1);
    assert!(
        loads[0][1]
            .as_str()
            .expect("stream target")
            .starts_with("https://cdn/new")
    );
}

#[tokio::test]
async fn exhausted_recovery_keeps_the_failed_entry_without_a_watch_url_retry() {
    let player = Player::new(options("exhausted-recovery"), Some(ExhaustedSource));
    let track = track("a1", "Requested song");
    let stale = Playable {
        target: "https://old.example/a1".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("a1", &stale);
    let entries = [(1, stale.target.clone())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(mpv.filenames(), [stale.target]);
    assert!(!mpv.calls().iter().any(|call| call[0] == json!("loadfile")));
    assert!(player.snapshot().error.is_some());
    player.inner.handle_event(&change("core-idle", json!(true)));
    assert!(!player.snapshot().loading);
}

#[tokio::test]
async fn cancelled_load_hook_does_not_rewrite_the_stream_filename() {
    let player = Player::new(options("load-hook-cancelled"), Some(SlowSource::default()));
    let track = track("slow", "Slow song");
    player.inner.remember(std::slice::from_ref(&track));
    let mpv = FakeMpv::with_playlist(&[(1, track.url())]);
    player.inner.set_ipc(mpv.clone());

    player.inner.handle_event(&json!({
        "event": "client-message",
        "args": ["redwax-resolve", "cancelled", track.url()],
    }));
    tokio::task::yield_now().await;
    player.inner.begin_playback();
    player.inner.wait_for_tasks().await;

    assert!(mpv.calls().contains(&vec![
        json!("script-message"),
        json!("redwax-resolved"),
        json!("cancelled"),
        json!(track.url()),
        json!(""),
    ]));
}

#[tokio::test]
async fn play_marks_a_final_watch_fallback_before_starting_it() {
    let player = Player::new(options("play-fallback"), Some(EmptySource));
    let track = track("a1", "One");
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());

    with_fallback_ack(&player, &mpv, player.play(vec![track.clone()], 0))
        .await
        .expect("play");

    let calls = mpv.calls();
    let play = vec![json!("playlist-play-index"), json!(0)];
    let bypass_at = calls
        .iter()
        .position(|call| is_fallback_call(call, &track.url()))
        .expect("fallback bypass");
    let play_at = calls.iter().position(|call| *call == play).expect("playback");
    assert!(bypass_at < play_at);
}

#[tokio::test]
async fn jump_marks_a_final_watch_fallback_before_starting_it() {
    let player = Player::new(options("jump-fallback"), Some(EmptySource));
    let track = track("a1", "One");
    player.inner.remember(std::slice::from_ref(&track));
    let entries = vec![(1, track.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());

    with_fallback_ack(&player, &mpv, player.jump(0))
        .await
        .expect("jump");

    let calls = mpv.calls();
    let play = vec![json!("playlist-play-index"), json!(0)];
    let bypass_at = calls
        .iter()
        .position(|call| is_fallback_call(call, &track.url()))
        .expect("fallback bypass");
    let play_at = calls.iter().position(|call| *call == play).expect("playback");
    assert!(bypass_at < play_at);
}

#[tokio::test]
async fn selected_stream_publishes_its_warning() {
    let player = Player::new::<TestSource>(options("stream-warning"), None);
    let track = track("a1", "One");
    let playable = Playable {
        target: "https://cdn/a1?expire=99999999999".to_string(),
        tier_label: "251 opus 160k".to_string(),
        warning: Some("fallback in use".to_string()),
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream(&track.video_id, &playable);
    assert_eq!(player.snapshot().warning, None);

    player
        .inner
        .handle_event(&playlist_event(&[(1, playable.target)]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));

    assert_eq!(player.snapshot().warning.as_deref(), Some("fallback in use"));
}

#[tokio::test]
async fn test_expired_stream_recovers_once() {
    let source = TestSource::default();
    let stream = source
        .resolve("a1")
        .await
        .expect("resolve succeeds")
        .expect("a stream")
        .target;
    let player = Player::new(options("expired-stream"), Some(source.clone()));

    let tracks = vec![track("a1", "One")];
    player.inner.remember(&tracks);
    assert_eq!(player.inner.entry_url(&tracks[0]), stream);

    let entries = vec![(1, stream.clone())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));

    let failure = json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
        "file_error": "Failed to open",
    });
    player.inner.handle_event(&failure);
    player.inner.wait_for_tasks().await;

    assert_eq!(mpv.filenames(), vec![stream.clone()]);
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(0)])
    );
    assert_eq!(source.forgotten(), vec!["a1".to_string()]);
    assert_eq!(player.snapshot().error, None);

    // A failed native refresh gets one final yt-dlp attempt.
    player.inner.handle_event(&playlist_event(&[(9, stream.clone())]));
    let retry_failure = json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 9,
        "file_error": "Failed to open",
    });
    player.inner.handle_event(&retry_failure);
    with_fallback_ack(&player, &mpv, player.inner.wait_for_tasks()).await;
    assert_eq!(mpv.filenames(), vec![tracks[0].url()]);
    assert_eq!(source.forgotten(), vec!["a1".to_string()]);
    assert_eq!(player.snapshot().error, None);

    player
        .inner
        .handle_event(&playlist_event(&[(10, tracks[0].url())]));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 10,
        "file_error": "Failed to open",
    }));
    player.inner.wait_for_tasks().await;
    assert_eq!(
        player.snapshot().error.as_deref(),
        Some("Could not play track: Failed to open")
    );
}

#[tokio::test]
async fn jump_resolves_the_selected_youtube_entry() {
    let source = TestSource::default();
    let player = Player::new(options("jump-resolves"), Some(source.clone()));
    let tracks = vec![track("a", "One"), track("b", "Two")];
    player.inner.remember(&tracks);
    let mpv = FakeMpv::with_playlist(&[(1, tracks[0].url()), (2, tracks[1].url())]);
    player.inner.set_ipc(mpv.clone());

    player.jump(1).await.expect("jump to selected track");

    assert_eq!(mpv.filenames()[0], tracks[0].url());
    assert_eq!(
        mpv.filenames()[1],
        source.cached("b").expect("resolved track").target
    );
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(1)])
    );
}

#[tokio::test]
async fn recovery_does_not_override_a_new_playback_request() {
    let source = TestSource::default();
    let stream = source
        .resolve("a")
        .await
        .expect("resolve succeeds")
        .expect("resolved stream")
        .target;
    let player = Player::new(options("recovery-selection"), Some(source));
    let tracks = vec![track("a", "One"), track("b", "Two")];
    player.inner.remember(&tracks);
    player.inner.entry_url(&tracks[0]);
    let entries = vec![(1, stream), (2, tracks[1].url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
        "file_error": "Failed to open",
    }));
    player.jump(1).await.expect("new selected track");
    player.inner.wait_for_tasks().await;

    assert!(
        !mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(0)])
    );
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(1)])
    );
}

#[tokio::test]
async fn automatic_watch_url_failure_gets_a_foreground_resolve() {
    let source = TestSource::default();
    let player = Player::new(options("recover-watch"), Some(source));
    let track = track("a1", "One");
    player.inner.remember(std::slice::from_ref(&track));
    let entries = vec![(1, track.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
        "file_error": "Failed to open",
    }));
    with_fallback_ack(&player, &mpv, player.inner.wait_for_tasks()).await;

    assert_eq!(mpv.filenames(), [TestSource::stream_for("a1").target]);
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(0)])
    );
}

#[tokio::test]
async fn recovery_does_not_loop_when_each_retry_has_a_new_url() {
    let source = RefreshingSource::default();
    let player = Player::new(options("recover-once"), Some(source.clone()));
    let track = track("a1", "One");
    let stale = Playable {
        target: "https://cdn/a1?generation=old".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("a1", &stale);
    let entries = vec![(1, stale.target)];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
    }));
    with_fallback_ack(&player, &mpv, player.inner.wait_for_tasks()).await;
    let refreshed = mpv.filenames()[0].clone();
    player.inner.handle_event(&playlist_event(&[(9, refreshed)]));
    player.inner.handle_event(&json!({
        "event": "start-file",
        "playlist_entry_id": 9,
    }));
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 9,
        "file_error": "Failed to open",
    }));
    with_fallback_ack(&player, &mpv, player.inner.wait_for_tasks()).await;

    assert_eq!(*source.resolves.lock().expect("refreshing source"), 1);
    assert_eq!(mpv.filenames(), [track.url()]);
    assert_eq!(player.snapshot().error, None);

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 10,
        "file_error": "Failed to open",
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(*source.resolves.lock().expect("refreshing source"), 1);
    assert_eq!(
        player.snapshot().error.as_deref(),
        Some("Could not play track: Failed to open")
    );
}

#[tokio::test]
async fn duplicate_video_entries_have_independent_recovery_budgets() {
    let source = RefreshingSource::default();
    let player = Player::new(options("recover-duplicates"), Some(source.clone()));
    let track = track("a1", "One");
    let stale = Playable {
        target: "https://cdn/a1?generation=old".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("a1", &stale);
    let entries = vec![(1, stale.target.clone()), (2, stale.target)];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    for entry_id in [1, 2] {
        player.inner.handle_event(&json!({
            "event": "end-file",
            "reason": "error",
            "playlist_entry_id": entry_id,
        }));
        player.inner.wait_for_tasks().await;
    }

    assert_eq!(*source.resolves.lock().expect("refreshing source"), 2);
    assert!(mpv.filenames()[0].contains("generation=1"));
    assert!(mpv.filenames()[1].contains("generation=2"));
}

#[tokio::test]
async fn a_normal_end_resets_the_entry_recovery_budget() {
    let source = RefreshingSource::default();
    let player = Player::new(options("recover-after-normal-end"), Some(source.clone()));
    let track = track("a1", "One");
    let stale = Playable {
        target: "https://cdn/a1?generation=old".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("a1", &stale);
    let entries = vec![(1, stale.target)];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
    }));
    player.inner.wait_for_tasks().await;

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "eof",
        "playlist_entry_id": 9,
    }));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 9,
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(*source.resolves.lock().expect("refreshing source"), 2);
}

#[test]
fn stream_identity_pruning_keeps_only_urls_still_in_the_queue() {
    let player = Player::new(options("direct-pruning"), Some(TestSource::default()));
    let queued = Playable {
        target: "https://cdn/a1?generation=queued".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    let abandoned = Playable {
        target: "https://cdn/a1?generation=abandoned".to_string(),
        tier_label: "old".to_string(),
        warning: None,
    };
    let fresh = Playable {
        target: "https://cdn/a1?generation=fresh".to_string(),
        tier_label: "fresh".to_string(),
        warning: None,
    };
    player.inner.remember_stream("a1", &queued);
    player
        .inner
        .handle_event(&playlist_event(&[(1, queued.target.clone())]));
    player.inner.remember_stream("a1", &abandoned);
    player.inner.remember_stream("a1", &fresh);

    let bookkeeping = player.inner.bookkeeping.lock().expect("player bookkeeping state");
    assert!(
        bookkeeping
            .stream_video_ids_by_target
            .contains_key(&queued.target)
    );
    assert!(
        !bookkeeping
            .stream_video_ids_by_target
            .contains_key(&abandoned.target)
    );
    assert!(bookkeeping.stream_video_ids_by_target.contains_key(&fresh.target));
}

#[tokio::test]
async fn final_watch_fallback_bypasses_the_next_load_hook() {
    let player = Player::new(options("recover-fallback"), Some(EmptySource));
    let track = track("a1", "One");
    player.inner.remember(std::slice::from_ref(&track));
    let entries = vec![(1, track.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
    }));
    with_fallback_ack(&player, &mpv, player.inner.wait_for_tasks()).await;

    let calls = mpv.calls();
    let play = vec![json!("playlist-play-index"), json!(0)];
    let bypass_at = calls
        .iter()
        .position(|call| is_fallback_call(call, &track.url()))
        .expect("fallback bypass");
    let play_at = calls
        .iter()
        .position(|call| *call == play)
        .expect("fallback playback");
    assert!(bypass_at < play_at);

    player.inner.handle_event(&playlist_event(&[(9, track.url())]));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 9,
        "file_error": "Failed to open",
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(
        mpv.calls()
            .iter()
            .filter(|call| is_fallback_call(call, &track.url()))
            .count(),
        1
    );
    assert_eq!(
        player.snapshot().error.as_deref(),
        Some("Could not play track: Failed to open")
    );
}

#[tokio::test]
async fn selection_change_cancels_an_installed_fallback_marker() {
    let player = Player::new(options("cancel-fallback-marker"), Some(EmptySource));
    let track = track("a1", "One");
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut play = Box::pin(player.play(vec![track.clone()], 0));
    let mut installed = false;
    let mut cancelled = false;

    let result = loop {
        tokio::select! {
            result = &mut play => break result,
            _ = tokio::task::yield_now() => {
                let calls = mpv.calls();
                if !installed
                    && let Some(call) = calls.iter().find(|call| is_fallback_call(call, &track.url()))
                {
                    player.inner.handle_event(&json!({
                        "event": "client-message",
                        "args": ["redwax-fallback-ready", call[2], call[3]],
                    }));
                    player.inner.begin_playback();
                    installed = true;
                }
                if installed
                    && !cancelled
                    && let Some(call) = calls.iter().find(|call| {
                        call.first() == Some(&json!("script-message"))
                            && call.get(1) == Some(&json!("redwax-fallback-cancel"))
                    })
                {
                    player.inner.handle_event(&json!({
                        "event": "client-message",
                        "args": ["redwax-fallback-cancelled", call[2], call[3]],
                    }));
                    cancelled = true;
                }
            }
        }
    };

    result.expect("the superseded play is cancelled cleanly");
    assert!(installed);
    assert!(cancelled);
    assert!(
        !mpv.calls()
            .iter()
            .any(|call| { call.first() == Some(&json!("playlist-play-index")) })
    );
}

#[tokio::test]
async fn a_watch_failure_without_a_native_source_is_reported() {
    let player = quiet_player();
    let track = track("a1", "One");
    let entries = vec![(1, track.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv);
    player.inner.handle_event(&playlist_event(&entries));

    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
        "file_error": "Failed to open",
    }));
    player.inner.wait_for_tasks().await;

    assert_eq!(
        player.snapshot().error.as_deref(),
        Some("Could not play track: Failed to open")
    );
}

#[tokio::test]
async fn cancelled_recovery_releases_its_retry_marker() {
    let source = SlowSource::default();
    let player = Player::new(options("recover-cancelled"), Some(source));
    let track = track("slow", "Slow song");
    let stale = TestSource::stream_for("slow");
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.remember_stream("slow", &stale);
    let entries = vec![(1, stale.target.clone())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv);
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
    }));
    tokio::task::yield_now().await;
    assert!(player.inner.is_recovering(&stale.target));

    player.inner.begin_playback();
    player.inner.wait_for_tasks().await;

    assert!(!player.inner.is_recovering(&stale.target));
}

#[tokio::test]
async fn replacing_a_stale_entry_leaves_the_queue_alone() {
    let player = quiet_player();
    let mpv = FakeMpv::with_playlist(&[(1, "new queue".to_string())]);
    player.inner.set_ipc(mpv.clone());

    let replaced = player
        .inner
        .replace_entry(0, 1, "old queue", "stale stream", false)
        .await
        .expect("stale replacement is harmless");

    assert!(replaced.is_none());
    assert_eq!(mpv.filenames(), ["new queue"]);
    assert!(!mpv.calls().iter().any(|call| call[0] == json!("loadfile")));
}

#[tokio::test]
async fn prefetch_does_not_replace_an_entry_that_has_started_playing() {
    let player = quiet_player();
    let mpv = FakeMpv::with_playlist(&[(1, "playing".to_string())]);
    mpv.playlist.lock().expect("fake mpv")[0]["current"] = json!(true);
    player.inner.set_ipc(mpv.clone());

    let replaced = player
        .inner
        .replace_entry(0, 1, "playing", "prefetched", false)
        .await
        .expect("playing entry is left alone");

    assert!(replaced.is_none());
    assert_eq!(mpv.filenames(), ["playing"]);
}

#[test]
fn test_mpv_argv_matches_python() {
    let options = PlayerOptions {
        volume: 37,
        mpv_args: vec!["--ao=null".to_string()],
        ytdl_raw_options: "cookies=/c.txt".to_string(),
        socket_dir: PathBuf::from("/run/user/1000"),
        prefetch_ahead: 3,
    };
    let argv = mpv_argv_with_script(
        &options,
        &PathBuf::from("/run/user/1000/redwax-mpv-42.sock"),
        Some(&PathBuf::from("/run/user/1000/redwax-resolve-42.lua")),
    );
    assert_eq!(
        argv,
        vec![
            "mpv",
            "--idle=yes",
            "--vid=no",
            "--audio-display=no",
            "--ytdl=yes",
            "--ytdl-format=bestaudio/best",
            "--prefetch-playlist=yes",
            "--script=/run/user/1000/redwax-resolve-42.lua",
            "--no-terminal",
            "--really-quiet",
            "--volume=37",
            "--input-ipc-server=/run/user/1000/redwax-mpv-42.sock",
            "--ytdl-raw-options=cookies=/c.txt",
            "--ao=null",
        ]
    );

    let plain = PlayerOptions {
        ytdl_raw_options: String::new(),
        mpv_args: Vec::new(),
        ..options
    };
    let argv = mpv_argv(&plain, &PathBuf::from("/s.sock"));
    assert!(!argv.iter().any(|arg| arg.starts_with("--ytdl-raw-options")));
    assert_eq!(
        argv.last().map(String::as_str),
        Some("--input-ipc-server=/s.sock")
    );
}

#[tokio::test]
async fn resumed_playback_seeks_after_loading_before_unpausing() {
    let player = Player::new::<TestSource>(options("resume-position"), None);
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let tracks = vec![track("a", "One"), track("b", "Two")];

    with_fallback_ack(&player, &mpv, player.play_from(tracks.clone(), 1, 42.5))
        .await
        .expect("resume saved song");

    let loads: Vec<Vec<Value>> = mpv
        .calls()
        .into_iter()
        .filter(|call| call[0] == json!("loadfile"))
        .collect();
    assert_eq!(
        loads[0],
        vec![json!("loadfile"), json!(tracks[0].url()), json!("append")]
    );
    assert_eq!(
        loads[1],
        vec![json!("loadfile"), json!(tracks[1].url()), json!("append")]
    );
    assert!(!mpv.calls().iter().any(|call| call[0] == json!("seek")));
    assert!(
        !mpv.calls()
            .contains(&vec![json!("set_property"), json!("pause"), json!(false)])
    );

    *mpv.playlist.lock().expect("fake mpv") = vec![
        json!({"id": 1, "filename": tracks[0].url()}),
        json!({"id": 2, "filename": tracks[1].url(), "current": true}),
    ];
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;

    let calls = mpv.calls();
    let seek = vec![json!("seek"), json!(42.5), json!("absolute+exact")];
    let unpause = vec![json!("set_property"), json!("pause"), json!(false)];
    let seek_at = calls.iter().position(|call| *call == seek).expect("saved seek");
    let unpause_at = calls
        .iter()
        .position(|call| *call == unpause)
        .expect("unpause after seek");
    assert!(seek_at < unpause_at);

    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;
    assert_eq!(mpv.calls().iter().filter(|call| **call == seek).count(), 1);
}

#[tokio::test]
async fn recovery_preserves_the_pending_resume_position() {
    let source = TestSource::default();
    let player = Player::new(options("resume-recovery"), Some(source.clone()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let track = track("a", "One");
    player
        .play_from(vec![track.clone()], 0, 42.5)
        .await
        .expect("resume");
    let stream = source.cached("a").expect("resolved stream").target;
    *mpv.playlist.lock().expect("fake mpv") = vec![json!({"id": 1, "filename": stream, "current": true})];
    player.inner.handle_event(&playlist_event(&[(1, stream.clone())]));
    player.inner.handle_event(&json!({
        "event": "end-file",
        "reason": "error",
        "playlist_entry_id": 1,
        "file_error": "Failed to open",
    }));
    player.inner.wait_for_tasks().await;
    assert_eq!(mpv.filenames(), [stream]);

    mpv.playlist.lock().expect("fake mpv")[0]["current"] = json!(true);
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;

    assert!(
        mpv.calls()
            .contains(&vec![json!("seek"), json!(42.5), json!("absolute+exact")])
    );
    assert!(
        mpv.calls()
            .contains(&vec![json!("set_property"), json!("pause"), json!(false)])
    );
}

#[tokio::test]
async fn a_new_selection_discards_the_previous_resume_position() {
    let player = Player::new::<TestSource>(options("resume-selection"), None);
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let tracks = vec![track("a", "One"), track("b", "Two")];
    with_fallback_ack(&player, &mpv, player.play_from(tracks.clone(), 0, 42.5))
        .await
        .expect("resume");
    *mpv.playlist.lock().expect("fake mpv") = vec![
        json!({"id": 1, "filename": tracks[0].url()}),
        json!({"id": 2, "filename": tracks[1].url(), "current": true}),
    ];

    with_fallback_ack(&player, &mpv, player.jump(1))
        .await
        .expect("new selection");
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;

    assert!(!mpv.calls().iter().any(|call| call[0] == json!("seek")));
}

#[tokio::test]
async fn a_new_play_request_supersedes_a_slow_resolution() {
    let player = Player::new(options("supersede-slow-play"), Some(SlowSource::default()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut slow = Box::pin(player.play(vec![track("slow", "Slow song")], 0));
    tokio::select! {
        biased;
        _ = &mut slow => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    player
        .play(vec![track("fast", "Latest song")], 0)
        .await
        .expect("latest selection");
    slow.await
        .expect("obsolete play finishes without changing playback");

    let loads: Vec<_> = mpv
        .calls()
        .into_iter()
        .filter(|call| call[0] == json!("loadfile"))
        .collect();
    assert_eq!(loads.len(), 1);
    assert!(
        loads[0][1]
            .as_str()
            .expect("stream target")
            .starts_with("https://cdn/fast")
    );
}

#[tokio::test]
async fn pending_selection_is_visible_before_resolution_without_replacing_the_queue() {
    let player = Player::new(options("pending-selection"), Some(SlowSource::default()));
    let current = track("current", "Already playing");
    player.inner.remember(std::slice::from_ref(&current));
    player
        .inner
        .set_ipc(FakeMpv::with_playlist(&[(1, current.url())]));
    player.inner.handle_event(&playlist_event(&[(1, current.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    let mut requested = Box::pin(player.play(vec![track("slow", "Requested song")], 0));
    tokio::select! {
        biased;
        _ = &mut requested => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    let state = player.snapshot();
    assert_eq!(
        state.pending_track.as_ref().map(|track| track.title.as_str()),
        Some("Requested song")
    );
    assert_eq!(
        state.track().map(|track| track.title.as_str()),
        Some("Already playing")
    );
    assert!(state.loading);
    player.inner.handle_event(&change("core-idle", json!(false)));
    player.inner.handle_event(&change("pause", json!(true)));
    assert!(player.snapshot().loading);

    drop(requested);
    assert!(player.snapshot().pending_track.is_none());
    assert!(!player.snapshot().loading);
}

#[tokio::test]
async fn an_obsolete_pending_request_cannot_clear_its_replacement() {
    let player = Player::new(options("pending-replacement"), Some(SlowSource::default()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut first = Box::pin(player.play(vec![track("slow", "First request")], 0));
    tokio::select! {
        biased;
        _ = &mut first => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }
    let mut second = Box::pin(player.play(vec![track("slow", "Latest request")], 0));
    tokio::select! {
        biased;
        _ = &mut second => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }
    first.await.expect("the first request was superseded");
    assert_eq!(
        player
            .snapshot()
            .pending_track
            .as_ref()
            .map(|track| track.title.as_str()),
        Some("Latest request")
    );
    assert!(player.snapshot().loading);

    player.clear().await.expect("clear cancels pending playback");
    second.await.expect("the latest request was cancelled");
    assert!(player.snapshot().pending_track.is_none());
    assert!(!player.snapshot().loading);
    assert!(!mpv.calls().iter().any(|call| call[0] == json!("loadfile")));
}

#[tokio::test]
async fn a_queue_command_failure_clears_the_pending_selection() {
    let player = Player::new(options("pending-command-error"), Some(TestSource::default()));
    assert!(
        player
            .play(vec![track("fast", "Requested song")], 0)
            .await
            .is_err()
    );
    assert!(player.snapshot().pending_track.is_none());
    assert!(!player.snapshot().loading);
}

#[tokio::test]
async fn skipping_is_not_blocked_by_an_unresolved_play_request() {
    let player = Player::new(options("skip-slow-play"), Some(SlowSource::default()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let mut slow = Box::pin(player.play(vec![track("slow", "Slow song")], 0));
    tokio::select! {
        biased;
        _ = &mut slow => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    player.next().await.expect("skip remains available");
    slow.await.expect("obsolete resolution cancelled");
    assert!(!mpv.calls().iter().any(|call| call[0] == json!("loadfile")));
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-next"), json!("force")])
    );
}

#[tokio::test]
async fn repeated_jump_to_the_same_entry_keeps_the_first_resolution() {
    let source = GatedSource::default();
    let player = Player::new(options("repeat-slow-jump"), Some(source.clone()));
    let selected = track("slow", "Slow song");
    player.inner.remember(std::slice::from_ref(&selected));
    let mpv = FakeMpv::with_playlist(&[(1, selected.url())]);
    player.inner.set_ipc(mpv.clone());
    let mut first = Box::pin(player.jump(0));
    tokio::select! {
        biased;
        _ = &mut first => panic!("the gated source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    player.jump(0).await.expect("repeated selection is coalesced");
    assert_eq!(source.resolves.load(Ordering::SeqCst), 1);

    source.gate.add_permits(1);
    first.await.expect("the original selection finishes");
    assert_eq!(
        mpv.calls()
            .iter()
            .filter(|call| call.first() == Some(&json!("playlist-play-index")))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_jump_waiting_for_mpv_cannot_supersede_a_newer_skip() {
    let player = Player::new(options("stale-jump-inspection"), Some(TestSource::default()));
    let selected = track("old", "Old selection");
    player.inner.remember(std::slice::from_ref(&selected));
    let mpv = FakeMpv::with_gated_playlist(&[(1, selected.url())]);
    player.inner.set_ipc(mpv.clone());
    let mut old = Box::pin(player.jump(0));
    tokio::select! {
        result = &mut old => panic!("jump passed the playlist gate: {result:?}"),
        _ = mpv.wait_for_playlist_request() => {}
    }

    let mut newer = Box::pin(player.next());
    tokio::select! {
        biased;
        result = &mut newer => panic!("skip passed the queue lock: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    mpv.release_playlist();
    old.await.expect("obsolete jump stops after inspection");
    newer.await.expect("newer skip keeps ownership");

    let calls = mpv.calls();
    assert!(calls.contains(&vec![json!("playlist-next"), json!("force")]));
    assert!(
        !calls
            .iter()
            .any(|call| call.first() == Some(&json!("playlist-play-index")))
    );
}

#[tokio::test]
async fn duplicate_tracks_at_different_entries_remain_distinct_jump_targets() {
    let source = GatedSource::default();
    let player = Player::new(options("duplicate-slow-jump"), Some(source.clone()));
    let duplicated = track("same", "Repeated song");
    player.inner.remember(std::slice::from_ref(&duplicated));
    let entries = vec![(1, duplicated.url()), (2, duplicated.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    let mut first = Box::pin(player.jump(0));
    tokio::select! {
        biased;
        _ = &mut first => panic!("the gated source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    let mut second = Box::pin(player.jump(1));
    tokio::select! {
        biased;
        _ = &mut second => panic!("the gated source cannot finish"),
        _ = std::future::ready(()) => {}
    }
    first.await.expect("the earlier entry is superseded");
    assert_eq!(source.resolves.load(Ordering::SeqCst), 2);

    source.gate.add_permits(1);
    second.await.expect("the distinct entry finishes");
    assert!(
        !mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(0)])
    );
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(1)])
    );
}

#[tokio::test]
async fn a_new_jump_supersedes_an_earlier_slow_selection() {
    let player = Player::new(options("supersede-slow-jump"), Some(SlowSource::default()));
    let tracks = vec![track("slow", "Slow song"), track("fast", "Latest song")];
    player.inner.remember(&tracks);
    let mpv = FakeMpv::with_playlist(&[(1, tracks[0].url()), (2, tracks[1].url())]);
    player.inner.set_ipc(mpv.clone());
    let mut slow = Box::pin(player.jump(0));
    tokio::select! {
        biased;
        _ = &mut slow => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }

    player.jump(1).await.expect("latest selection");
    slow.await.expect("obsolete jump finishes without replaying");

    assert!(
        !mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(0)])
    );
    assert!(
        mpv.calls()
            .contains(&vec![json!("playlist-play-index"), json!(1)])
    );
}

#[tokio::test]
async fn play_reuses_an_identical_mpv_queue() {
    let source = TestSource::default();
    let player = Player::new(options("play-reuses-queue"), Some(source.clone()));
    let tracks = vec![track("a", "One"), track("b", "Two")];
    player.inner.remember(&tracks);
    let mut entries = Vec::new();
    for (entry_id, track) in [1, 2].into_iter().zip(&tracks) {
        let playable = source
            .resolve(&track.video_id)
            .await
            .expect("resolve succeeds")
            .expect("stream");
        player.inner.remember_stream(&track.video_id, &playable);
        entries.push((entry_id, playable.target));
    }
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));

    player.play(tracks, 1).await.expect("reuse existing queue");

    let calls = mpv.calls();
    assert!(calls.contains(&vec![json!("playlist-play-index"), json!(1)]));
    assert!(!calls.iter().any(|call| call.first() == Some(&json!("stop"))));
    assert!(!calls.iter().any(|call| call.first() == Some(&json!("loadfile"))));
}

#[tokio::test]
async fn play_does_not_reuse_observed_state_during_a_queue_change() {
    let source = TestSource::default();
    let player = Player::new(options("play-during-queue-change"), Some(source.clone()));
    let tracks = vec![track("a", "One"), track("b", "Two")];
    player.inner.remember(&tracks);
    let mut entries = Vec::new();
    for (entry_id, track) in [1, 2].into_iter().zip(&tracks) {
        let playable = source
            .resolve(&track.video_id)
            .await
            .expect("resolve succeeds")
            .expect("stream");
        player.inner.remember_stream(&track.video_id, &playable);
        entries.push((entry_id, playable.target));
    }
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    let queue_change = player.inner.queue_edit_lock.lock().await;

    let mut play = Box::pin(player.play(tracks, 1));
    tokio::select! {
        biased;
        result = &mut play => panic!("play passed the queue lock: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    *mpv.playlist.lock().expect("fake mpv") = vec![json!({"id": 3, "filename": track("c", "Changed").url()})];
    drop(queue_change);
    play.await.expect("rebuild after the older queue change");

    let calls = mpv.calls();
    assert!(calls.iter().any(|call| call.first() == Some(&json!("stop"))));
    assert!(calls.iter().any(|call| call.first() == Some(&json!("loadfile"))));
}

#[tokio::test]
async fn repeated_play_reuses_the_queue_while_a_jump_is_inspecting_it() {
    let source = GatedSource::default();
    let player = Player::new(options("play-during-jump-inspection"), Some(source.clone()));
    let selected = track("same", "Same selection");
    player.inner.remember(std::slice::from_ref(&selected));
    let mpv = FakeMpv::with_gated_playlist(&[(1, selected.url())]);
    player.inner.set_ipc(mpv.clone());
    let mut first = Box::pin(player.jump(0));
    tokio::select! {
        result = &mut first => panic!("jump passed the playlist gate: {result:?}"),
        _ = mpv.wait_for_playlist_request() => {}
    }

    let mut repeated = Box::pin(player.play(vec![selected], 0));
    tokio::select! {
        biased;
        result = &mut repeated => panic!("play passed the queue lock: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    mpv.release_playlist();
    first.await.expect("older jump yields to repeated play");
    tokio::select! {
        biased;
        result = &mut repeated => panic!("the gated source cannot finish: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    assert_eq!(source.resolves.load(Ordering::SeqCst), 1);

    source.gate.add_permits(1);
    repeated.await.expect("repeated play jumps within the queue");

    let calls = mpv.calls();
    assert!(calls.contains(&vec![json!("playlist-play-index"), json!(0)]));
    assert!(!calls.iter().any(|call| call.first() == Some(&json!("stop"))));
    assert!(
        !calls.iter().any(|call| {
            call.first() == Some(&json!("loadfile")) && call.get(2) == Some(&json!("append"))
        })
    );
}

#[tokio::test]
async fn prefetch_preserves_metadata_when_the_playlist_entry_is_replaced() {
    let player = Player::new(options("prefetch-metadata"), Some(TestSource::default()));
    let first = track("a", "One");
    let mut second = track("b", "Known song");
    second.thumbnail_url = Some("https://images.example/cover".to_string());
    player.inner.remember(&[first.clone(), second.clone()]);
    let entries = vec![(1, first.url()), (2, second.url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.wait_for_tasks().await;
    let playlist = mpv.playlist.lock().expect("fake mpv").clone();
    player.inner.handle_event(&change("playlist", json!(playlist)));

    assert_eq!(player.snapshot().queue[1], second);
}

#[test]
fn a_hook_filename_change_preserves_the_track_metadata() {
    let player = quiet_player();
    let mut track = track("a", "Known song");
    track.thumbnail_url = Some("https://images.example/cover".to_string());
    player.inner.remember(std::slice::from_ref(&track));
    player.inner.handle_event(&playlist_event(&[(1, track.url())]));
    player.inner.handle_event(&playlist_event(&[(
        1,
        "https://cdn.example/videoplayback?signature=private".to_string(),
    )]));

    assert_eq!(player.snapshot().queue, [track]);
}

#[test]
fn an_unknown_network_entry_does_not_expose_its_signed_url() {
    let player = quiet_player();
    player.inner.handle_event(&playlist_event(&[(
        7,
        "https://cdn.example/videoplayback?signature=private".to_string(),
    )]));

    let state = player.snapshot();
    assert_eq!(state.queue[0].title, "Unknown track");
    assert_eq!(state.queue[0].video_id, "mpv-entry:7");
}

#[tokio::test]
async fn timing_reports_cancelled_requests_without_reassigning_their_identity() {
    let player = Player::new(options("playback_timing-cancelled"), Some(SlowSource::default()));
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |revision, video_id: &str, stage, _| {
        captured
            .lock()
            .expect("playback_timing events")
            .push((revision, video_id.to_string(), stage));
    })));
    let mut slow = Box::pin(player.play(vec![track("slow", "Slow song")], 0));
    tokio::select! {
        biased;
        _ = &mut slow => panic!("the slow source cannot finish"),
        _ = std::future::ready(()) => {}
    }
    player
        .play(vec![track("fast", "Latest song")], 0)
        .await
        .expect("new selection");
    slow.await.expect("obsolete selection cancelled");

    let events = events.lock().expect("playback_timing events");
    assert!(events.contains(&(1, "slow".to_string(), "resolve.cancelled")));
    assert!(events.contains(&(2, "fast".to_string(), "queue.commands_done")));
}

#[tokio::test]
async fn skip_timing_names_the_raw_destination_through_loaded_and_playing() {
    let player = Player::new::<TestSource>(options("playback_timing-skip"), None);
    let tracks = [track("a", "One"), track("b", "Two")];
    let entries = vec![(1, tracks[0].url()), (2, tracks[1].url())];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv);
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |revision, video_id: &str, stage, _| {
        captured
            .lock()
            .expect("playback_timing events")
            .push((revision, video_id.to_string(), stage));
    })));

    player.next().await.expect("next");
    player
        .inner
        .handle_event(&json!({"event": "start-file", "playlist_entry_id": 2}));
    player.inner.handle_event(&change("playlist-pos", json!(1)));
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.handle_event(&change("core-idle", json!(false)));
    player.inner.handle_event(&change("time-pos", json!(1)));

    let events = events.lock().expect("playback_timing events");
    for stage in [
        "skip.request",
        "skip.target.youtube",
        "skip.commands_done",
        "file_loaded",
        "playing_approx",
    ] {
        assert!(
            events.contains(&(1, "b".to_string(), stage)),
            "missing {stage}: {events:?}"
        );
    }
}

#[tokio::test]
async fn skip_timing_distinguishes_a_direct_destination() {
    let player = Player::new::<TestSource>(options("playback_timing-skip-direct"), None);
    let tracks = [track("a", "One"), track("b", "Two")];
    let playable = TestSource::stream_for("b");
    player.inner.remember_stream("b", &playable);
    let entries = vec![(1, tracks[0].url()), (2, playable.target)];
    let mpv = FakeMpv::with_playlist(&entries);
    player.inner.set_ipc(mpv);
    player.inner.handle_event(&playlist_event(&entries));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |_, video_id: &str, stage, _| {
        captured
            .lock()
            .expect("playback_timing events")
            .push((video_id.to_string(), stage));
    })));

    player.next().await.expect("next");

    assert!(
        events
            .lock()
            .expect("playback_timing events")
            .contains(&("b".to_string(), "skip.target.direct"))
    );
}

#[tokio::test]
async fn timing_reports_a_warning_playable_as_fallback() {
    let player = Player::new(options("playback_timing-fallback"), Some(WarningSource));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |_, _, stage, _| {
        captured.lock().expect("playback_timing events").push(stage);
    })));
    let revision = player.inner.begin_playback();

    assert!(matches!(
        player.inner.resolve_selected("a1", revision).await,
        SelectedStream::Ready(Some(_))
    ));
    assert!(
        events
            .lock()
            .expect("playback_timing events")
            .contains(&"resolve.fallback")
    );
}

#[tokio::test]
async fn timing_reports_loading_and_playing_once_for_each_repeated_selection() {
    let player = Player::new::<TestSource>(options("playback_timing-loaded"), None);
    let mpv = FakeMpv::with_playlist(&[]);
    player.inner.set_ipc(mpv.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    player.set_timing_sink(Some(Arc::new(move |revision, video_id: &str, stage, _| {
        captured
            .lock()
            .expect("playback_timing events")
            .push((revision, video_id.to_string(), stage));
    })));
    let track = track("a", "Selected song");
    for revision in 1..=2 {
        with_fallback_ack(&player, &mpv, player.play(vec![track.clone()], 0))
            .await
            .expect("repeated selection");
        *mpv.playlist.lock().expect("fake mpv") = vec![json!({"id": revision, "filename": track.url()})];
        player
            .inner
            .handle_event(&playlist_event(&[(revision, track.url())]));
        player.inner.handle_event(&change("playlist-pos", json!(0)));
        player.inner.handle_event(&change("time-pos", json!(0)));
        player.inner.timing_entry(revision as u64, Some(revision));
        player
            .inner
            .handle_event(&json!({"event": "start-file", "playlist_entry_id": 99}));
        player.inner.handle_event(&json!({"event": "file-loaded"}));
        player
            .inner
            .handle_event(&json!({"event": "start-file", "playlist_entry_id": revision}));
        player.inner.handle_event(&json!({"event": "file-loaded"}));
        player.inner.handle_event(&json!({"event": "file-loaded"}));
        player.inner.handle_event(&change("core-idle", json!(false)));
        player.inner.handle_event(&change("time-pos", json!(1)));
        player.inner.wait_for_tasks().await;
    }

    let events = events.lock().expect("playback_timing events");
    for revision in 1..=2 {
        for stage in ["file_loaded", "playing_approx"] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.0 == revision && event.2 == stage)
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn player_instances_own_distinct_resources_and_close_independently() {
    let launcher = Arc::new(FakeLauncher::default());
    let first = player_with_launcher("lifecycle-instances", Arc::clone(&launcher));
    let second = player_with_launcher("lifecycle-instances", Arc::clone(&launcher));

    first.start().await.expect("first player starts");
    second.start().await.expect("second player starts");

    {
        let resources = launcher.resources.lock().expect("fake launcher resources");
        assert_eq!(resources.len(), 2);
        assert_ne!(resources[0].0, resources[1].0);
        assert_ne!(resources[0].1, resources[1].1);
    }

    first.close().await.expect("first player closes");
    assert!(!resources_for(&launcher, 0).0.exists());
    assert!(resources_for(&launcher, 1).0.exists());
    {
        let engines = launcher.engines.lock().expect("fake launcher engines");
        assert!(engines[0].calls().contains(&vec![json!("quit")]));
        assert!(!engines[1].calls().contains(&vec![json!("quit")]));
    }
    second
        .set_volume(43)
        .await
        .expect("second player remains connected");
}

#[tokio::test]
async fn repeated_start_and_close_share_one_lifecycle_transition() {
    let launcher = Arc::new(FakeLauncher::default());
    let gate = Arc::new(Semaphore::new(0));
    *launcher.launch_gate.lock().expect("fake launcher gate") = Some(Arc::clone(&gate));
    let player = player_with_launcher("lifecycle-idempotent", Arc::clone(&launcher));
    let first = tokio::spawn({
        let player = player.clone();
        async move { player.start().await }
    });
    launcher
        .launch_started
        .acquire()
        .await
        .expect("fake launch remains open")
        .forget();
    let second = tokio::spawn({
        let player = player.clone();
        async move { player.start().await }
    });
    tokio::task::yield_now().await;
    gate.add_permits(1);
    first.await.expect("first task").expect("first start");
    second.await.expect("second task").expect("second start");
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);

    let other = player.clone();
    let (first, second) = tokio::join!(player.close(), other.close());
    first.expect("first close");
    second.expect("second close");
    let engines = launcher.engines.lock().expect("fake launcher engines");
    assert_eq!(
        engines[0]
            .calls()
            .iter()
            .filter(|command| command.first() == Some(&json!("quit")))
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_start_returns_to_stopped_and_retry_uses_fresh_resources() {
    let launcher = Arc::new(FakeLauncher::default());
    launcher.fail_next.store(true, Ordering::SeqCst);
    let player = player_with_launcher("lifecycle-retry", Arc::clone(&launcher));

    assert!(player.start().await.is_err());
    let failed_resources = resources_for(&launcher, 0);
    assert!(!failed_resources.0.exists());
    assert!(!failed_resources.1.exists());
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    player.start().await.expect("retry starts");

    let resources = launcher.resources.lock().expect("fake launcher resources");
    assert_eq!(resources.len(), 2);
    assert_ne!(resources[0].0, resources[1].0);
    assert_ne!(resources[0].1, resources[1].1);
}

#[tokio::test]
async fn an_old_reader_cannot_change_or_retire_a_new_engine() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("lifecycle-generation", launcher);
    player.start().await.expect("first start");
    let old_engine_generation = match &*player.inner.lifecycle.state.lock().expect("player lifecycle") {
        LifecycleState::Running(engine) => engine.engine_generation,
        _ => panic!("player should be running"),
    };
    player.close().await.expect("first close");
    player.start().await.expect("second start");

    player
        .inner
        .handle_event_for(old_engine_generation, &change("pause", json!(true)));
    player.inner.engine_gone(old_engine_generation);

    assert!(!player.snapshot().paused);
    assert!(matches!(
        &*player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Running(engine) if engine.engine_generation != old_engine_generation
    ));
}

#[tokio::test]
async fn unexpected_exit_restores_paused_with_duplicate_entry_identities() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_source_and_launcher("lifecycle-automatic-restart", Arc::clone(&launcher));
    player.start().await.expect("first start");
    let duplicate = track("same", "Same song");
    let queue = vec![duplicate.clone(), duplicate.clone()];
    player.inner.remember(&queue);
    player
        .inner
        .handle_event(&playlist_event(&[(1, duplicate.url()), (2, duplicate.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(1)));
    player.inner.handle_event(&change("time-pos", json!(42.5)));
    let identities: Vec<_> = player
        .snapshot()
        .queue_entries
        .iter()
        .map(|entry| entry.id())
        .collect();

    launcher.event_senders.lock().expect("fake launcher events")[0]
        .send(json!({"event": "shutdown"}))
        .expect("first engine receives shutdown");
    wait_for(&player, |state| {
        state.availability == PlaybackAvailability::RestoredPaused
    })
    .await;
    player
        .inner
        .handle_event(&playlist_event(&[(10, duplicate.url()), (11, duplicate.url())]));

    let restored = player.snapshot();
    assert!(restored.paused);
    assert_eq!(restored.index, Some(1));
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    assert_eq!(
        restored
            .queue_entries
            .iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>(),
        identities
    );
}

#[tokio::test]
async fn failed_automatic_restart_waits_for_explicit_retry() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_source_and_launcher("lifecycle-explicit-retry", Arc::clone(&launcher));
    player.start().await.expect("first start");
    let selected = track("selected", "Selected song");
    player.inner.remember(std::slice::from_ref(&selected));
    player.inner.handle_event(&playlist_event(&[(1, selected.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    launcher.fail_next.store(true, Ordering::SeqCst);

    launcher.event_senders.lock().expect("fake launcher events")[0]
        .send(json!({"event": "shutdown"}))
        .expect("first engine receives shutdown");
    wait_for(&player, |state| {
        state.availability == PlaybackAvailability::Unavailable
    })
    .await;
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);

    player
        .retry_playback()
        .await
        .expect("explicit retry starts playback");
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 3);
    assert_eq!(
        player.snapshot().availability,
        PlaybackAvailability::RestoredPaused
    );
    assert!(player.snapshot().paused);
}

#[tokio::test]
async fn duplicate_tracks_keep_identity_through_external_reorder_and_removal() {
    let player = quiet_player();
    let duplicate = track("same", "Same song");
    player.inner.remember(&[duplicate.clone(), duplicate.clone()]);
    let initial = vec![(1, duplicate.url()), (2, duplicate.url())];
    let mpv = FakeMpv::with_playlist(&initial);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&initial));
    let before = player.snapshot();
    let first = before.queue_entries[0].id();
    let second = before.queue_entries[1].id();
    assert_ne!(first, second);

    let reordered = vec![(2, duplicate.url()), (1, duplicate.url())];
    *mpv.playlist.lock().expect("fake mpv playlist") = reordered
        .iter()
        .map(|(id, filename)| json!({"id": id, "filename": filename}))
        .collect();
    player.inner.handle_event(&playlist_event(&reordered));
    let after = player.snapshot();
    assert_eq!(after.queue_entries[0].id(), second);
    assert_eq!(after.queue_entries[1].id(), first);

    assert_eq!(
        player
            .move_entry(first, after.queue_revision, 0)
            .await
            .expect("move entry"),
        QueueEdit::Applied
    );
    assert_eq!(mpv.playlist.lock().expect("fake mpv playlist")[0]["id"], json!(1));
    assert_eq!(
        player
            .remove_entry(second, after.queue_revision)
            .await
            .expect("remove entry"),
        QueueEdit::Applied
    );
    assert_eq!(mpv.playlist.lock().expect("fake mpv playlist")[0]["id"], json!(1));
    assert_eq!(
        player
            .remove_entry(second, before.queue_revision)
            .await
            .expect("stale edit"),
        QueueEdit::Stale
    );
}

#[tokio::test]
async fn stream_replacement_preserves_the_queue_entry_identity() {
    let player = quiet_player();
    let selected = track("selected", "Selected song");
    player.inner.remember(std::slice::from_ref(&selected));
    let initial = vec![(1, selected.url())];
    let mpv = FakeMpv::with_playlist(&initial);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&initial));
    let identity = player.snapshot().queue_entries[0].id();

    let replacement = "https://stream/selected";
    let replacement_id = player
        .inner
        .replace_entry(0, 1, &selected.url(), replacement, true)
        .await
        .expect("replace entry")
        .expect("replacement remains in queue");
    player
        .inner
        .handle_event(&playlist_event(&[(replacement_id, replacement.to_string())]));

    assert_eq!(player.snapshot().queue_entries[0].id(), identity);
}

#[tokio::test]
async fn automatic_restoration_seeks_after_loading_without_unpausing() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_source_and_launcher("restore-paused-loaded", Arc::clone(&launcher));
    player.start().await.expect("first engine starts");
    let selected = track("selected", "Selected song");
    player.inner.remember(std::slice::from_ref(&selected));
    player.inner.handle_event(&playlist_event(&[(1, selected.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.handle_event(&change("time-pos", json!(42.5)));

    launcher.event_senders.lock().expect("fake launcher events")[0]
        .send(json!({"event": "shutdown"}))
        .expect("first engine receives shutdown");
    wait_for(&player, |state| {
        state.availability == PlaybackAvailability::RestoredPaused
    })
    .await;
    let mpv = launcher.engines.lock().expect("fake launcher engines")[1].clone();
    let target = TestSource::stream_for("selected").target;
    *mpv.playlist.lock().expect("fake mpv playlist") = vec![json!({
        "id": 10, "filename": target, "current": true
    })];
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;

    assert!(
        mpv.calls()
            .contains(&vec![json!("seek"), json!(42.5), json!("absolute+exact")])
    );
    assert!(
        !mpv.calls()
            .contains(&vec![json!("set_property"), json!("pause"), json!(false)])
    );
    assert!(player.snapshot().paused);
}

#[tokio::test]
async fn explicit_restoration_at_zero_never_unpauses() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_source_and_launcher("restore-paused-zero", Arc::clone(&launcher));
    let selected = track("selected", "Selected song");
    player.inner.remember(std::slice::from_ref(&selected));
    player.inner.handle_event(&playlist_event(&[(1, selected.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    player.inner.state.send_modify(|state| {
        state.availability = PlaybackAvailability::Unavailable;
    });

    player
        .retry_playback()
        .await
        .expect("explicit restoration succeeds");
    let mpv = launcher.engines.lock().expect("fake launcher engines")[0].clone();
    player.inner.handle_event(&json!({"event": "file-loaded"}));
    player.inner.wait_for_tasks().await;

    assert!(
        !mpv.calls()
            .contains(&vec![json!("set_property"), json!("pause"), json!(false)])
    );
    assert!(player.snapshot().paused);
    assert_eq!(
        player.snapshot().availability,
        PlaybackAvailability::RestoredPaused
    );
}

#[tokio::test]
async fn cancelled_close_releases_the_engine_and_allows_restart() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-cancelled", Arc::clone(&launcher));
    player.start().await.expect("first engine starts");
    let mpv = launcher.engines.lock().expect("fake launcher engines")[0].clone();
    *mpv.quit_gate.lock().expect("fake mpv quit gate") = Some(Arc::new(Semaphore::new(0)));
    let closing = tokio::spawn({
        let player = player.clone();
        async move { player.close().await }
    });
    mpv.quit_requested
        .acquire()
        .await
        .expect("quit request arrives")
        .forget();
    closing.abort();
    assert!(closing.await.expect_err("close was cancelled").is_cancelled());

    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    assert!(!resources_for(&launcher, 0).0.exists());
    player
        .start()
        .await
        .expect("new engine starts after cancellation");
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn stalled_quit_cannot_exceed_the_shutdown_budget() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-deadline", Arc::clone(&launcher));
    player.start().await.expect("first engine starts");
    let mpv = launcher.engines.lock().expect("fake launcher engines")[0].clone();
    *mpv.quit_gate.lock().expect("fake mpv quit gate") = Some(Arc::new(Semaphore::new(0)));
    let started_at = tokio::time::Instant::now();

    player.close().await.expect("stalled engine is retired");

    assert_eq!(started_at.elapsed(), crate::ENGINE_SHUTDOWN_BUDGET);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    assert!(!resources_for(&launcher, 0).0.exists());
    player.start().await.expect("new engine starts after timeout");
}

#[tokio::test(start_paused = true)]
async fn shutdown_budget_includes_a_blocked_launch_and_prevents_late_publication() {
    let launcher = Arc::new(FakeLauncher::default());
    let launch_gate = Arc::new(Semaphore::new(0));
    *launcher.launch_gate.lock().expect("fake launcher gate") = Some(Arc::clone(&launch_gate));
    let player = player_with_launcher("close-blocked-launch", Arc::clone(&launcher));
    let starting = tokio::spawn({
        let player = player.clone();
        async move { player.start().await }
    });
    launcher
        .launch_started
        .acquire()
        .await
        .expect("launch begins")
        .forget();
    let started_at = tokio::time::Instant::now();

    player.close().await.expect("pending engine is retired");

    assert_eq!(started_at.elapsed(), crate::ENGINE_SHUTDOWN_BUDGET);
    launch_gate.add_permits(1);
    assert!(starting.await.expect("startup task completes").is_err());
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    assert!(!resources_for(&launcher, 0).0.exists());
    *launcher.launch_gate.lock().expect("fake launcher gate") = None;
    player.start().await.expect("fresh launch succeeds");
}

#[tokio::test]
async fn cancelled_prefetch_releases_its_reservation_after_restart() {
    let source = GatedSource::default();
    let launcher = Arc::new(FakeLauncher::default());
    let player = Player::with_launcher(
        options("prefetch-cancelled"),
        Some(Arc::new(source.clone())),
        launcher,
    );
    player.start().await.expect("first engine starts");
    player.prefetch(vec![track("selected", "Selected song")]);
    tokio::time::timeout(Duration::from_secs(1), async {
        while source.resolves.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prefetch begins");

    player.close().await.expect("close cancels prefetch");
    player.start().await.expect("new engine starts");
    player.prefetch(vec![track("selected", "Selected song")]);
    tokio::time::timeout(Duration::from_secs(1), async {
        while source.resolves.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same track can prefetch again");
    player.close().await.expect("close cancels second prefetch");
}

#[tokio::test]
async fn dropping_the_last_handle_cancels_a_permanently_pending_task() {
    let source = GatedSource::default();
    let player = Player::new(options("drop-pending-task"), Some(source.clone()));
    let last_handle = player.clone();
    let weak_player = Arc::downgrade(&player.inner);
    player.prefetch(vec![track("selected", "Selected song")]);
    tokio::time::timeout(Duration::from_secs(1), async {
        while source.resolves.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task is permanently gated");
    drop(player);
    tokio::task::yield_now().await;
    assert!(weak_player.upgrade().is_some());

    drop(last_handle);

    tokio::time::timeout(Duration::from_secs(1), async {
        while weak_player.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("last handle cancels task-held state");
}

#[tokio::test(start_paused = true)]
async fn an_obsolete_failed_launch_cannot_publish_a_playback_error() {
    let launcher = Arc::new(FakeLauncher::default());
    let launch_gate = Arc::new(Semaphore::new(0));
    *launcher.launch_gate.lock().expect("fake launcher gate") = Some(Arc::clone(&launch_gate));
    launcher.fail_next.store(true, Ordering::SeqCst);
    let player = player_with_launcher("close-obsolete-launch-error", Arc::clone(&launcher));
    let starting = tokio::spawn({
        let player = player.clone();
        async move { player.start().await }
    });
    launcher
        .launch_started
        .acquire()
        .await
        .expect("launch begins")
        .forget();

    player.close().await.expect("pending engine is retired");
    let retired_state = player.snapshot();
    launch_gate.add_permits(1);
    assert!(starting.await.expect("startup task completes").is_err());

    assert_eq!(player.snapshot(), retired_state);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
}

#[tokio::test]
async fn failed_restoration_preserves_the_resolution_failure_and_stays_unavailable() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = Player::with_launcher(
        options("restore-source-failed"),
        Some(Arc::new(ErrorSource)),
        launcher.clone(),
    );
    player.start().await.expect("first engine starts");
    let selected = track("selected", "Selected song");
    player.inner.remember(std::slice::from_ref(&selected));
    player.inner.handle_event(&playlist_event(&[(1, selected.url())]));
    player.inner.handle_event(&change("playlist-pos", json!(0)));
    launcher.event_senders.lock().expect("fake launcher events")[0]
        .send(json!({"event": "shutdown"}))
        .expect("first engine receives shutdown");

    wait_for(&player, |state| {
        state.availability == PlaybackAvailability::Unavailable
    })
    .await;

    let state = player.snapshot();
    assert!(
        state
            .error
            .as_deref()
            .unwrap()
            .contains("Unavailable on YouTube Music.")
    );
    assert!(
        !state
            .error
            .as_deref()
            .unwrap()
            .contains("Playback restored paused")
    );
    assert!(!state.loading);
    let mpv = launcher.engines.lock().expect("fake launcher engines")[1].clone();
    assert!(
        !mpv.calls()
            .iter()
            .any(|call| call.first() == Some(&json!("loadfile")))
    );
    assert!(
        !mpv.calls()
            .iter()
            .any(|call| call.get(1) == Some(&json!("redwax-fallback")))
    );
    assert!(player.retry_playback().await.is_err());
    assert_eq!(player.snapshot().availability, PlaybackAvailability::Unavailable);
}

#[tokio::test]
async fn close_claims_the_engine_before_a_queue_wait_can_admit_recovery() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-recovery-race", launcher.clone());
    player.start().await.expect("first engine starts");
    let engine_generation = match &*player.inner.lifecycle.state.lock().expect("player lifecycle") {
        LifecycleState::Running(engine) => engine.engine_generation,
        _ => panic!("first engine is running"),
    };
    let queue_guard = player.inner.queue_edit_lock.lock().await;
    let mut closing = Box::pin(player.close());
    tokio::select! {
        biased;
        _ = &mut closing => panic!("shutdown waits for the queue owner"),
        _ = std::future::ready(()) => {}
    }

    player.inner.engine_gone(engine_generation);
    drop(queue_guard);
    closing.await.expect("close completes");
    player.inner.wait_for_tasks().await;

    assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    assert!(!resources_for(&launcher, 0).0.exists());
}

#[tokio::test]
async fn cancelled_close_while_waiting_for_queue_ownership_releases_the_engine() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-cancelled-queue-wait", launcher.clone());
    player.start().await.expect("first engine starts");
    let queue_guard = player.inner.queue_edit_lock.lock().await;
    let mut closing = Box::pin(player.close());
    tokio::select! {
        biased;
        _ = &mut closing => panic!("shutdown waits for the queue owner"),
        _ = std::future::ready(()) => {}
    }

    drop(closing);
    drop(queue_guard);

    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("player lifecycle"),
        LifecycleState::Stopped
    ));
    assert!(!resources_for(&launcher, 0).0.exists());
    player
        .start()
        .await
        .expect("new engine starts after cancelled lock wait");
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_second_close_joins_cleanup_before_a_following_start() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-join", launcher.clone());
    player.start().await.expect("first engine starts");
    let engine = launcher.engines.lock().expect("fake engines")[0].clone();
    let quit_gate = Arc::new(Semaphore::new(0));
    *engine.quit_gate.lock().expect("fake quit gate") = Some(quit_gate.clone());
    let first = tokio::spawn({
        let player = player.clone();
        async move { player.close().await }
    });
    engine
        .quit_requested
        .acquire()
        .await
        .expect("quit requested")
        .forget();
    let mut second = Box::pin(player.close());
    tokio::select! {
        biased;
        _ = &mut second => panic!("the second close must join active shutdown"),
        _ = std::future::ready(()) => {}
    }
    quit_gate.add_permits(1);
    second.await.expect("joined cleanup");
    player.start().await.expect("start after close runs");
    first.await.expect("first task").expect("first close");
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("lifecycle"),
        LifecycleState::Running(_)
    ));
}

#[tokio::test]
async fn close_cancels_an_admitted_recovery_before_it_can_launch() {
    let launcher = Arc::new(FakeLauncher::default());
    let player = player_with_launcher("close-queued-recovery", launcher.clone());
    player.start().await.expect("first engine starts");
    let generation = match &*player.inner.lifecycle.state.lock().expect("lifecycle") {
        LifecycleState::Running(engine) => engine.engine_generation,
        _ => panic!("engine runs"),
    };
    let queue_guard = player.inner.queue_edit_lock.lock().await;
    player.inner.engine_gone(generation);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("lifecycle"),
        LifecycleState::RestartQueued(_)
    ));
    let mut closing = Box::pin(player.close());
    tokio::select! {
        biased;
        _ = &mut closing => panic!("shutdown waits for queue ownership"),
        _ = std::future::ready(()) => {}
    }
    drop(queue_guard);
    closing.await.expect("queued recovery closes");
    player.inner.wait_for_tasks().await;
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
    assert!(matches!(
        *player.inner.lifecycle.state.lock().expect("lifecycle"),
        LifecycleState::Stopped
    ));
}

#[tokio::test]
async fn replacement_events_never_publish_duplicate_or_changed_logical_entries() {
    let player = quiet_player();
    let selected = track("selected", "Selected song");
    let current = track("current", "Current song");
    player.inner.remember(&[selected.clone(), current.clone()]);
    let initial = vec![(1, selected.url()), (2, current.url())];
    let mpv = FakeMpv::with_playlist(&initial);
    mpv.playlist.lock().expect("fake playlist")[1]["current"] = json!(true);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&initial));
    player.inner.handle_event(&change("playlist-pos", json!(1)));
    let before = player.snapshot();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let inner = Arc::downgrade(&player.inner);
    let generation = match &*player.inner.lifecycle.state.lock().expect("lifecycle") {
        LifecycleState::Running(engine) => engine.engine_generation,
        _ => panic!("test engine runs"),
    };
    *mpv.event_sink.lock().expect("fake event sink") = Some(Arc::new({
        let observations = observations.clone();
        let events = events.clone();
        move |event| {
            let inner = inner.upgrade().expect("player remains alive");
            inner.handle_event_for(generation, &event);
            observations
                .lock()
                .expect("observations")
                .push(inner.state.borrow().clone());
            events.lock().expect("events").push(event);
        }
    }));
    let replacement = "https://stream/selected";
    player
        .inner
        .replace_entry(0, 1, &selected.url(), replacement, false)
        .await
        .expect("replacement succeeds")
        .expect("replacement installed");
    let stale_insert = events.lock().expect("events")[0].clone();
    player.inner.handle_event_for(generation, &stale_insert);
    player
        .inner
        .handle_event_for(generation, &change("playlist-pos", json!(2)));
    player.inner.wait_for_tasks().await;
    observations.lock().expect("observations").push(player.snapshot());
    for state in observations.lock().expect("observations").iter() {
        assert_eq!(state.queue_entries, before.queue_entries);
        assert_eq!(state.queue, before.queue);
        assert_eq!(state.index, before.index);
    }
    assert_eq!(player.inner.entry_position(9), Some(0));
    assert_eq!(player.snapshot().queue_entries, before.queue_entries);
}

#[tokio::test]
async fn a_rolled_back_replacement_cannot_reappear_in_buffered_snapshots() {
    let player = quiet_player();
    let selected = track("selected", "Selected song");
    let current = track("current", "Current song");
    player.inner.remember(&[selected.clone(), current.clone()]);
    let initial = vec![(1, selected.url()), (2, current.url())];
    let mpv = FakeMpv::with_playlist(&initial);
    player.inner.set_ipc(mpv.clone());
    player.inner.handle_event(&playlist_event(&initial));
    let before = player.snapshot();
    let events = Arc::new(Mutex::new(Vec::new()));
    let inner = Arc::downgrade(&player.inner);
    let weak_mpv = Arc::downgrade(&mpv);
    *mpv.event_sink.lock().expect("fake event sink") = Some(Arc::new({
        let events = events.clone();
        move |event| {
            let inner = inner.upgrade().expect("player remains alive");
            inner.handle_event(&event);
            if event["name"] == json!("playlist")
                && event["data"].as_array().is_some_and(|entries| entries.len() == 3)
            {
                let mpv = weak_mpv.upgrade().expect("mpv remains alive");
                let mut playlist = mpv.playlist.lock().expect("fake playlist");
                let selected = playlist
                    .iter_mut()
                    .find(|entry| entry["id"] == json!(1))
                    .expect("old selected entry remains");
                selected["current"] = json!(true);
            }
            events.lock().expect("events").push(event);
        }
    }));
    let replaced = player
        .inner
        .replace_entry(0, 1, &selected.url(), "https://stream/selected", false)
        .await
        .expect("playing race rolls back cleanly");
    assert!(replaced.is_none());
    assert_eq!(player.snapshot().queue_entries, before.queue_entries);
    assert_eq!(player.inner.entry_position(1), Some(0));
    assert_eq!(player.inner.entry_position(9), None);
    let settled = player.snapshot();
    player
        .inner
        .handle_event(&events.lock().expect("events")[0].clone());
    assert_eq!(player.snapshot().queue_entries, settled.queue_entries);
    assert_eq!(player.snapshot().queue_revision, settled.queue_revision);
    assert_eq!(player.snapshot().index, Some(0));
}
