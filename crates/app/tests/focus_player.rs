mod support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use redwax::app::{App, AppEvent, Deps};
use redwax::theme::FOCUS_RED;
use redwax_player::{Playable, Player, PlayerOptions, PlayerState, StreamSource};
use redwax_ytm::{Browsable, CompletePage, MusicSource};
use tokio::sync::{mpsc, watch};

use support::{FakeSource, config_in, fake_genre, parse_key, playlist, scratch};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct LocalAudio(HashMap<String, PathBuf>);

impl StreamSource for LocalAudio {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        self.0.get(video_id).map(|path| Playable {
            target: path.to_string_lossy().into_owned(),
            tier_label: "local wav".to_string(),
            warning: None,
        })
    }

    fn allow_mpv_fallback(&self) -> bool {
        false
    }

    async fn resolve(&self, video_id: &str) -> Result<Option<Playable>, String> {
        Ok(self.cached(video_id))
    }

    fn prefetch(&self, _: Vec<String>) {}

    fn forget(&self, _: &str) {}
}

struct PlayingApp {
    app: App<FakeSource, Player>,
    player: Player,
    source: Arc<FakeSource>,
    events: mpsc::UnboundedReceiver<AppEvent>,
    updates: watch::Receiver<PlayerState>,
    terminal: Terminal<TestBackend>,
    phases: Arc<Mutex<Vec<String>>>,
}

impl PlayingApp {
    async fn new(directory: &std::path::Path) -> Result<Self, String> {
        let mut config = config_in(directory);
        config.source = directory.join("config.toml");
        config.focus_playlist_id = Some("PL1".to_string());
        config.focus_playlist_title = Some("Late night".to_string());
        let audio = ["focus-natural", "focus-seeked"]
            .into_iter()
            .map(|id| (id.to_string(), directory.join(format!("{id}.wav"))))
            .collect::<HashMap<_, _>>();
        for path in audio.values() {
            let sample_rate = 8_000_u32;
            let data_length = sample_rate * 2 * 2;
            let mut wav = Vec::with_capacity(data_length as usize + 44);
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(data_length + 36).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16_u32.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&sample_rate.to_le_bytes());
            wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
            wav.extend_from_slice(&2_u16.to_le_bytes());
            wav.extend_from_slice(&16_u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&data_length.to_le_bytes());
            wav.resize(data_length as usize + 44, 0);
            std::fs::write(path, wav).map_err(|error| error.to_string())?;
        }
        let player = Player::new(
            PlayerOptions {
                socket_dir: directory.join("ipc"),
                mpv_args: vec![
                    "--ao=null".to_string(),
                    "--vo=null".to_string(),
                    "--no-config".to_string(),
                ],
                ..PlayerOptions::default()
            },
            Some(LocalAudio(audio)),
        );
        let phases = Arc::new(Mutex::new(Vec::new()));
        player.set_timing_sink(Some(Arc::new({
            let phases = phases.clone();
            move |_, video_id, phase, _| {
                phases.lock().unwrap().push(format!("{video_id}:{phase}"));
            }
        })));
        let source = Arc::new(FakeSource::default());
        source
            .playlist_overrides
            .lock()
            .unwrap()
            .insert("PL1".to_string(), Vec::new());
        let deps = Deps {
            icons: redwax::icons::TEXT,
            theme: redwax::theme::redwax_theme(),
            page_cache: redwax::page_cache::PageCache::new(config.page_cache_dir.clone()),
            art_cache: Arc::new(redwax::art::ArtCache::new(config.art_cache_dir.clone())),
            art_setup: redwax::terminal::detect(&config.art, Some(false), None),
            saved: redwax::state::SavedState::default(),
            state_store: redwax::state::PlaybackStateStore::new(config.state_path.clone()),
            session_refresh: None,
            session_logout: None,
            session_generation: Default::default(),
            account_access: None,
            config,
        };
        let (sender, events) = mpsc::unbounded_channel();
        let mut app = App::new(source.clone(), player.clone(), deps, sender);
        app.set_genre_lookup(fake_genre("Ambient"));
        app.library
            .replace(source.library().await.map_err(|error| error.to_string())?);
        let initial = source
            .browse(&Browsable::Playlist(playlist()))
            .await
            .map_err(|error| error.to_string())?;
        app.deps.page_cache.put(
            "playlist:PL1",
            &CompletePage::try_from(initial.clone()).map_err(|error| error.to_string())?,
        );
        app.home.show_from(initial, Some(Browsable::Playlist(playlist())));
        let terminal = Terminal::new(TestBackend::new(140, 45)).map_err(|error| error.to_string())?;
        let updates = player.state();
        Ok(Self {
            app,
            player,
            source,
            events,
            updates,
            terminal,
            phases,
        })
    }

    fn draw(&mut self) -> Result<(), String> {
        self.terminal
            .draw(|frame| self.app.draw(frame))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn until(&mut self, description: &str, ready: impl Fn(&Self) -> bool) -> Result<(), String> {
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Ok(event) = self.events.try_recv() {
                    self.app.handle_event(event);
                }
                if self.updates.has_changed().unwrap_or(false) {
                    let state = self.updates.borrow_and_update().clone();
                    self.app.handle_event(AppEvent::PlayerChanged(state));
                }
                self.draw()?;
                if ready(self) {
                    return Ok(());
                }
                tokio::select! {
                    event = self.events.recv() => {
                        self.app.handle_event(event.ok_or("app event channel closed")?);
                    }
                    update = self.updates.changed() => {
                        update.map_err(|_| "player state channel closed")?;
                        let state = self.updates.borrow_and_update().clone();
                        self.app.handle_event(AppEvent::PlayerChanged(state));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {
                        self.app.handle_event(AppEvent::Tick);
                    }
                }
            }
        })
        .await;
        outcome.map_err(|_| {
            format!(
                "timed out waiting for {description}: {:?}",
                self.phases.lock().unwrap()
            )
        })?
    }

    async fn exercise(&mut self) -> Result<(), String> {
        self.player.start().await.map_err(|error| error.to_string())?;
        self.app.handle_key(parse_key("7"));
        self.app.handle_key(parse_key("j"));
        self.app.handle_key(parse_key("j"));
        self.app.handle_key(parse_key("enter"));
        if !self.app.focus_active() {
            return Err("Settings did not enable focus mode".to_string());
        }
        self.app.handle_key(parse_key("1"));
        self.draw()?;
        let buffer = self.terminal.backend().buffer();
        let focus_label = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "F" && cell.fg == FOCUS_RED);
        if focus_label.is_none() {
            return Err("focused player did not render the red warning".to_string());
        }

        let natural = redwax_player::bare_track("focus-natural", "Natural local song");
        self.player
            .play(vec![natural.clone()], 0)
            .await
            .map_err(|error| error.to_string())?;
        self.until(
            "a natural full play, fake save, and playlist refresh",
            |harness| {
                harness.source.called("add_to_playlist:PL1:focus-natural")
                    && harness.app.home.top().is_some_and(|page| {
                        page.tracks()
                            .any(|track| track.video_id == "focus-natural" && track.playlist_entry.is_some())
                    })
                    && harness
                        .app
                        .deps
                        .page_cache
                        .get("playlist:PL1")
                        .is_some_and(|page| {
                            page.tracks().any(|track| {
                                track.video_id == "focus-natural" && track.playlist_entry.is_some()
                            })
                        })
            },
        )
        .await?;
        let rendered: String = self
            .terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        if !rendered.contains("focus-natural") {
            return Err("the saved track was absent from the rendered playlist".to_string());
        }

        let sequence = self.player.snapshot().playback_sequence;
        self.player
            .play(vec![natural], 0)
            .await
            .map_err(|error| error.to_string())?;
        self.until(
            "the same song to finish again without a duplicate append",
            |harness| {
                harness
                    .player
                    .snapshot()
                    .completed_tracks
                    .back()
                    .is_some_and(|(completed, _)| *completed > sequence)
            },
        )
        .await?;
        if self.source.playlist_edits() != ["add_to_playlist:PL1:focus-natural"] {
            return Err(format!(
                "unexpected playlist writes: {:?}",
                self.source.playlist_edits()
            ));
        }

        self.player
            .play(
                vec![redwax_player::bare_track("focus-seeked", "Seeked local song")],
                0,
            )
            .await
            .map_err(|error| error.to_string())?;
        self.until("the seeked fixture to load", |harness| {
            let state = harness.player.snapshot();
            state
                .track()
                .is_some_and(|track| track.video_id == "focus-seeked")
                && state.duration.is_some()
        })
        .await?;
        self.player
            .seek_to(1.0)
            .await
            .map_err(|error| error.to_string())?;
        self.until("the seeked track to reach actual EOF", |harness| {
            harness.player.snapshot().index.is_none()
        })
        .await?;
        let state = self.player.snapshot();
        if let Some(error) = state.error {
            return Err(format!("the seeked audio did not finish normally: {error}"));
        }
        if state
            .completed_tracks
            .iter()
            .any(|(_, track)| track.video_id == "focus-seeked")
        {
            return Err("the seeked track was reported as a full play".to_string());
        }
        if self.source.playlist_edits() != ["add_to_playlist:PL1:focus-natural"] {
            return Err("a seeked song was appended".to_string());
        }
        eprintln!(
            "real mpv focus evidence: {:?}, writes: {:?}, completions: {}",
            self.phases.lock().unwrap(),
            self.source.playlist_edits(),
            state.completed_tracks.len(),
        );
        Ok(())
    }
}

#[tokio::test]
async fn real_mpv_full_plays_save_once_refresh_the_playlist_and_exclude_seeked_tracks() {
    match std::process::Command::new("mpv").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: mpv is not installed");
            return;
        }
        result => assert!(result.expect("mpv can run").status.success()),
    }
    let directory = Scratch(scratch("focus-ipc"));
    let mut harness = PlayingApp::new(&directory.0)
        .await
        .expect("offline focus fixtures");
    let result = harness.exercise().await;
    let shutdown = harness.app.shutdown().await;
    result.expect("real mpv focus collection");
    shutdown.expect("the isolated player shuts down and saves its state");
}
