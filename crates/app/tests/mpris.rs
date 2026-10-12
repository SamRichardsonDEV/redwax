//! The two `test_mpris.py` cases plus the bus name fallback. The bus tests
//! need a session bus, which every desktop this runs on has.

use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mpris_server::zbus::zvariant::OwnedValue;
use mpris_server::zbus::{Connection, Proxy};
use mpris_server::{PlaybackStatus, Time};
use redwax::mpris::{Mpris, metadata, playback_status};
use redwax_player::{PlayerState, Repeat, Result};
use redwax_ytm::Track;
use tokio::sync::watch;

const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Both bus tests pick a name by asking the bus which ones are free, so they
/// claim theirs one test at a time rather than racing for the same one.
static BUS_NAMES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq)]
enum Call {
    PlayPause,
    Next,
    Previous,
    Clear,
    Seek(f64),
    SeekTo(f64),
    Volume(u8),
    SetRepeat(Repeat),
    SetShuffle(bool),
    Close,
}

/// Records what MPRIS asked for and hands out the state it was given, the
/// Rust shape of Python's `Recorder`.
#[derive(Clone)]
struct FakePlayer {
    states: Arc<watch::Sender<PlayerState>>,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakePlayer {
    fn new(state: PlayerState) -> FakePlayer {
        FakePlayer {
            states: Arc::new(watch::Sender::new(state)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("the test owns this log").clone()
    }

    fn record(&self, call: Call) -> Result<()> {
        self.calls.lock().expect("the test owns this log").push(call);
        Ok(())
    }
}

impl redwax_player::PlayerControl for FakePlayer {
    fn state(&self) -> watch::Receiver<PlayerState> {
        self.states.subscribe()
    }
    fn snapshot(&self) -> PlayerState {
        self.states.borrow().clone()
    }
    async fn start(&self) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.record(Call::Close)
    }
    async fn retry_playback(&self) -> Result<()> {
        Ok(())
    }
    async fn play(&self, _tracks: Vec<Track>, _index: usize) -> Result<()> {
        Ok(())
    }
    async fn play_from(&self, _tracks: Vec<Track>, _index: usize, _seconds: f64) -> Result<()> {
        Ok(())
    }
    async fn enqueue(&self, _tracks: Vec<Track>, _play_next: bool) -> Result<()> {
        Ok(())
    }
    async fn replace_upcoming(&self, _tracks: Vec<Track>) -> Result<()> {
        Ok(())
    }
    async fn jump(&self, _index: usize) -> Result<()> {
        Ok(())
    }
    async fn remove(&self, _index: usize) -> Result<()> {
        Ok(())
    }
    async fn remove_entry(
        &self,
        _entry: redwax_player::QueueEntryId,
        _revision: redwax_player::QueueRevision,
    ) -> Result<redwax_player::QueueEdit> {
        Ok(redwax_player::QueueEdit::Stale)
    }
    async fn move_entry(
        &self,
        _entry: redwax_player::QueueEntryId,
        _revision: redwax_player::QueueRevision,
        _to: usize,
    ) -> Result<redwax_player::QueueEdit> {
        Ok(redwax_player::QueueEdit::Stale)
    }
    async fn clear(&self) -> Result<()> {
        self.record(Call::Clear)
    }
    async fn toggle_pause(&self) -> Result<()> {
        self.record(Call::PlayPause)
    }
    async fn set_audio_filter(&self, _graph: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn next(&self) -> Result<()> {
        self.record(Call::Next)
    }
    async fn previous(&self) -> Result<()> {
        self.record(Call::Previous)
    }
    async fn seek(&self, delta_seconds: f64) -> Result<()> {
        self.record(Call::Seek(delta_seconds))
    }
    async fn seek_to(&self, seconds: f64) -> Result<()> {
        self.record(Call::SeekTo(seconds))
    }
    async fn adjust_volume(&self, _delta: i16) -> Result<()> {
        Ok(())
    }
    async fn set_volume(&self, volume: u8) -> Result<()> {
        self.record(Call::Volume(volume))
    }
    async fn toggle_mute(&self) -> Result<()> {
        Ok(())
    }
    async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        self.record(Call::SetRepeat(mode))
    }
    async fn cycle_repeat(&self) -> Result<Repeat> {
        Ok(Repeat::Off)
    }
    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.record(Call::SetShuffle(on))
    }
    async fn toggle_shuffle(&self) -> Result<bool> {
        Ok(false)
    }
    fn prefetch(&self, _tracks: Vec<Track>) {}
}

fn track(video_id: &str, title: &str, artists: &[&str]) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: artists.iter().map(|a| a.to_string()).collect(),
        album: None,
        album_browse_id: None,
        duration_seconds: Some(100),
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

#[test]
fn test_metadata_and_status_from_state() {
    let mut song = track("v1", "Song", &["A", "B"]);
    song.album = Some("Album".to_string());
    song.duration_seconds = Some(200);
    song.thumbnail_url = Some("https://img".to_string());
    let state = PlayerState {
        queue: vec![song],
        index: Some(0),
        duration: Some(201.5),
        paused: true,
        ..PlayerState::default()
    };
    assert_eq!(playback_status(&state), PlaybackStatus::Paused);
    let data = metadata(&state);
    assert_eq!(data.title(), Some("Song"));
    assert_eq!(data.artist(), Some(vec!["A".to_string(), "B".to_string()]));
    assert_eq!(data.length(), Some(Time::from_micros(201_500_000)));
    assert_eq!(data.art_url().as_deref(), Some("https://img"));
    assert_eq!(data.album(), Some("Album"));
    assert_eq!(playback_status(&PlayerState::default()), PlaybackStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_playerctl_style_client_can_drive_the_service() {
    let _claim = BUS_NAMES.lock().await;
    let state = PlayerState {
        queue: vec![track("v1", "Song", &["A"])],
        index: Some(0),
        volume: 40,
        ..PlayerState::default()
    };
    let player = FakePlayer::new(state);
    let service = Mpris::start(player.clone())
        .await
        .expect("this desktop has a session bus");

    let Ok(connection) = Connection::session().await else {
        eprintln!("skipped: no session bus");
        return;
    };
    let proxy = Proxy::new(&connection, service.bus_name(), OBJECT_PATH, PLAYER_INTERFACE)
        .await
        .expect("the service is on the bus");

    let status: String = proxy.get_property("PlaybackStatus").await.unwrap();
    assert_eq!(status, "Playing");
    let data: HashMap<String, OwnedValue> = proxy.get_property("Metadata").await.unwrap();
    let title: String = data["xesam:title"].clone().try_into().unwrap();
    assert_eq!(title, "Song");
    let volume: f64 = proxy.get_property("Volume").await.unwrap();
    assert!((volume - 0.4).abs() < 1e-9, "volume was {volume}");

    proxy.call::<_, _, ()>("PlayPause", &()).await.unwrap();
    proxy.call::<_, _, ()>("Next", &()).await.unwrap();
    proxy.call::<_, _, ()>("Seek", &(5_000_000i64)).await.unwrap();
    proxy.set_property("Volume", 0.6f64).await.unwrap();
    proxy.set_property("Shuffle", true).await.unwrap();
    proxy.set_property("LoopStatus", "Track").await.unwrap();

    let calls = player.calls();
    assert_eq!(
        calls[..3],
        [Call::PlayPause, Call::Next, Call::Seek(5.0)],
        "got {calls:?}"
    );
    assert!(calls.contains(&Call::Volume(60)), "got {calls:?}");
    assert!(calls.contains(&Call::SetShuffle(true)), "got {calls:?}");
    assert!(calls.contains(&Call::SetRepeat(Repeat::One)), "got {calls:?}");
    service.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_bus_name_falls_back_to_instance() {
    let _claim = BUS_NAMES.lock().await;
    let Ok(first) = Mpris::start(FakePlayer::new(PlayerState::default())).await else {
        eprintln!("skipped: no session bus");
        return;
    };
    let second = Mpris::start(FakePlayer::new(PlayerState::default()))
        .await
        .expect("this desktop has a session bus");
    assert_ne!(first.bus_name(), second.bus_name());
    let instance = format!("instance{}", std::process::id());
    assert!(
        second.bus_name().contains(&instance),
        "second name was {}",
        second.bus_name()
    );
    second.close();
    first.close();
}

/// Holds the service up for twenty seconds so playerctl can drive it, then
/// prints what arrived. Run with `cargo test -p redwax --test mpris -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live check, needs playerctl driving it from another shell"]
async fn live_service_for_playerctl() {
    let mut song = track("1-Fxb9jsOuI", "Live check", &["redwax"]);
    song.album = Some("Milestone 5".to_string());
    song.thumbnail_url = Some("https://example.invalid/art.jpg".to_string());
    let state = PlayerState {
        queue: vec![song],
        index: Some(0),
        position: 12.0,
        duration: Some(180.0),
        volume: 55,
        ..PlayerState::default()
    };
    let player = FakePlayer::new(state);
    let service = Mpris::start(player.clone())
        .await
        .expect("this desktop has a session bus");
    println!("serving {}", service.bus_name());
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    println!("calls {:?}", player.calls());
    service.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_position_jump_signals_seeked() {
    let _claim = BUS_NAMES.lock().await;
    let state = PlayerState {
        queue: vec![track("v1", "Song", &["A"])],
        index: Some(0),
        position: 1.0,
        ..PlayerState::default()
    };
    let player = FakePlayer::new(state);
    let service = Mpris::start(player.clone())
        .await
        .expect("this desktop has a session bus");
    let Ok(connection) = Connection::session().await else {
        eprintln!("skipped: no session bus");
        return;
    };
    let proxy = Proxy::new(&connection, service.bus_name(), OBJECT_PATH, PLAYER_INTERFACE)
        .await
        .expect("the service is on the bus");
    let mut seeked = proxy
        .receive_signal("Seeked")
        .await
        .expect("the signal is on the interface");

    player.states.send_modify(|state| state.position = 40.0);

    let message = tokio::time::timeout(std::time::Duration::from_secs(5), seeked.next())
        .await
        .expect("Seeked arrives")
        .expect("the signal stream stays open");
    let position: i64 = message.body().deserialize().expect("Seeked carries a time");
    assert_eq!(position, 40_000_000);
    service.close();
}
