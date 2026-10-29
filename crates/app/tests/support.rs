//! Fakes and a harness for the ported `test_app.py` scenarios.
//!
//! `FakeSource` and `FakePlayer` are the Python `conftest.py` fakes: the same
//! canned pages, the same recorded call strings. `Harness` renders into a
//! `TestBackend` and drains the event channel, so a test asserts on buffer text
//! the way the Python tests assert on widget contents.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use redwax::app::{App, AppEvent, Deps};
use redwax::config::Config;
use redwax::page_cache::PageCache;
use redwax::state::SavedState;
use redwax::ui::Hit;
use redwax_player::PlayerControl;
use redwax_player::{
    Error as PlayerError, PlayerState, QueueEdit, QueueEntry, QueueEntryId, QueueRevision, Repeat,
    Result as PlayerResult,
};
use redwax_ytm::{
    Account, Album, Artist, Browsable, Continuation, Item, LyricLine, Lyrics, MusicSource, Page, Playlist,
    PlaylistAdd, PlaylistEntry, PlaylistRemoval, RadioContinuation, Result, Shelf, Track,
};
use tokio::sync::{Semaphore, mpsc, watch};

pub const ART_URL: &str = "https://img/1";

/// A picker that draws pixels, at the cell size `Picker::halfblocks` assumes.
/// The tests have no terminal to query, and half blocks are the one protocol
/// the mascot declines.
pub fn pixel_picker() -> Picker {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

pub fn tracks() -> Vec<Track> {
    vec![
        track("v1", "Alpine Glow", &["Arnegger"], 214, true),
        track("v2", "Snowline", &["Arnegger", "Guest"], 187, false),
        track("v3", "Foehn Wind", &["Someone Else"], 302, false),
    ]
}

/// Timed lyrics for `v1`: forty eight lines, four seconds each, so a panel of
/// any test height has more rows than it can show and the follow has to scroll.
pub fn timed_lines() -> Vec<LyricLine> {
    const WORDS: [&str; 6] = [
        "La la la",
        "Snow on the ridge",
        "Wind at the col",
        "Down to the valley",
        "And home again",
        "\u{266a}",
    ];
    (0..48)
        .map(|index: u64| LyricLine {
            start_ms: index * 4000,
            end_ms: (index + 1) * 4000,
            text: format!("{} {index}", WORDS[index as usize % WORDS.len()]),
        })
        .collect()
}

/// The untimed words `v2` answers with, for the plain path.
pub const PLAIN_LYRICS: &str = "Plain and untimed\nsecond line";

fn track(id: &str, title: &str, artists: &[&str], seconds: u32, liked: bool) -> Track {
    let third = id == "v3";
    Track {
        video_id: id.to_string(),
        title: title.to_string(),
        artists: artists.iter().map(|a| a.to_string()).collect(),
        album: Some(match third {
            true => "Valleys".to_string(),
            false => "Peaks".to_string(),
        }),
        album_browse_id: Some(match third {
            true => "MPREb_2".to_string(),
            false => "MPREb_1".to_string(),
        }),
        duration_seconds: Some(seconds),
        thumbnail_url: Some(format!("https://img/{}", &id[1..])),
        liked,
        playlist_entry: None,
    }
}

pub fn album() -> Album {
    Album {
        browse_id: "MPREb_1".to_string(),
        title: "Peaks".to_string(),
        artists: vec!["Arnegger".to_string()],
        year: Some("2024".to_string()),
        thumbnail_url: Some("https://img/album".to_string()),
    }
}

pub fn playlist() -> Playlist {
    Playlist {
        playlist_id: "PL1".to_string(),
        title: "Late night".to_string(),
        subtitle: Some("12 songs".to_string()),
        thumbnail_url: Some("https://img/pl".to_string()),
    }
}

pub fn artist() -> Artist {
    Artist {
        channel_id: "UC1".to_string(),
        name: "Arnegger".to_string(),
        thumbnail_url: Some("https://img/artist".to_string()),
    }
}

/// A playlist longer than one browse response, for the paging tests.
pub struct Paged {
    /// Tracks in each batch, the first page first.
    pub batches: Vec<usize>,
    /// Held closed, every continuation waits on it, so a test sees the page
    /// part loaded.
    pub gate: Option<Arc<Semaphore>>,
}

impl Paged {
    pub fn new(batches: &[usize]) -> Paged {
        Paged {
            batches: batches.to_vec(),
            gate: None,
        }
    }

    pub fn gated(batches: &[usize]) -> (Paged, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let paged = Paged {
            batches: batches.to_vec(),
            gate: Some(gate.clone()),
        };
        (paged, gate)
    }

    pub fn total(&self) -> usize {
        self.batches.iter().sum()
    }

    fn tracks(&self, batch: usize) -> Vec<Track> {
        let start: usize = self.batches[..batch].iter().sum();
        (start..start + self.batches[batch]).map(paged_track).collect()
    }

    fn more(&self, batch: usize) -> Option<Continuation> {
        (batch + 1 < self.batches.len()).then(|| Continuation {
            browse_id: "VLPL1".to_string(),
            token: format!("batch:{}", batch + 1),
        })
    }
}

/// One row of a paged playlist, numbered so a test can name the last one.
pub fn paged_track(index: usize) -> Track {
    Track {
        video_id: format!("p{index}"),
        title: format!("Paged {index}"),
        artists: vec!["Arnegger".to_string()],
        album: Some("Peaks".to_string()),
        album_browse_id: None,
        duration_seconds: Some(180),
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

/// What `track_length` answers, and what it was asked. YouTube sends no
/// duration for home rows, so the tests need a source that behaves the same.
pub struct Lengths {
    pub seconds: u32,
    /// Held closed, every length request waits on it.
    pub gate: Option<Arc<Semaphore>>,
    pub asked: Mutex<Vec<String>>,
}

impl Lengths {
    pub fn answering(seconds: u32) -> Lengths {
        Lengths {
            seconds,
            gate: None,
            asked: Mutex::new(Vec::new()),
        }
    }

    pub fn gated(seconds: u32) -> (Lengths, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let lengths = Lengths {
            seconds,
            gate: Some(gate.clone()),
            asked: Mutex::new(Vec::new()),
        };
        (lengths, gate)
    }
}

/// Stands in for `MusicSource` with canned pages and no network.
pub struct FakeSource {
    pub calls: Mutex<Vec<String>>,
    pub liked: Mutex<HashMap<String, bool>>,
    pub handle: Option<String>,
    /// Held closed, `home` waits on it. The Python test used a threading event.
    pub home_gate: Option<Arc<Semaphore>>,
    pub browse_gate: Option<Arc<Semaphore>>,
    /// Set when the playlist is served a batch at a time.
    pub paged: Option<Paged>,
    pub radio_paged: Option<Paged>,
    pub radio_batch: AtomicUsize,
    /// Serve every page the way YouTube serves a home row, with no duration.
    pub blank_durations: bool,
    /// Set when `track_length` answers instead of the trait default.
    pub lengths: Option<Lengths>,
    /// Refuse every playlist edit, for the path that puts the row back.
    pub edits_fail: bool,
    pub continuation_fails: bool,
    pub continuation_rejected: bool,
    pub continuation_repeats: bool,
}

impl Default for FakeSource {
    fn default() -> Self {
        FakeSource {
            calls: Mutex::new(Vec::new()),
            liked: Mutex::new(HashMap::new()),
            handle: Some("@sam".to_string()),
            home_gate: None,
            browse_gate: None,
            paged: None,
            radio_paged: None,
            radio_batch: AtomicUsize::new(1),
            blank_durations: false,
            lengths: None,
            edits_fail: false,
            continuation_fails: false,
            continuation_rejected: false,
            continuation_repeats: false,
        }
    }
}

impl FakeSource {
    pub fn gated() -> (Arc<FakeSource>, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let source = FakeSource {
            home_gate: Some(gate.clone()),
            ..FakeSource::default()
        };
        (Arc::new(source), gate)
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("no test holds this lock").clone()
    }

    pub fn called(&self, what: &str) -> bool {
        self.calls().iter().any(|call| call == what)
    }

    /// Every playlist edit the app asked for, in the order it asked.
    pub fn playlist_edits(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|call| {
                call.starts_with("add_to_playlist:")
                    || call.starts_with("remove_playlist_entry:")
                    || call.starts_with("remove_all_from_playlist:")
                    || call.starts_with("create_playlist:")
            })
            .collect()
    }

    pub fn liked(&self) -> HashMap<String, bool> {
        self.liked.lock().expect("no test holds this lock").clone()
    }

    /// Every video id `track_length` was asked about, in the order it landed.
    pub fn lengths_asked(&self) -> Vec<String> {
        match &self.lengths {
            None => Vec::new(),
            Some(lengths) => lengths.asked.lock().expect("no test holds this lock").clone(),
        }
    }

    fn record(&self, what: impl Into<String>) {
        self.calls
            .lock()
            .expect("no test holds this lock")
            .push(what.into());
    }

    /// The page as YouTube would serve it, which for most shelves is without
    /// a duration on any row.
    fn served(&self, mut page: Page) -> Page {
        if self.blank_durations {
            for shelf in &mut page.shelves {
                for item in &mut shelf.items {
                    if let Item::Track(track) = item {
                        track.duration_seconds = None;
                    }
                }
            }
        }
        page
    }
}

impl MusicSource for FakeSource {
    fn signed_in(&self) -> bool {
        true
    }

    async fn whoami(&self) -> Result<Option<String>> {
        Ok(self.handle.clone())
    }

    async fn home(&self) -> Result<Page> {
        if let Some(gate) = &self.home_gate {
            let _ = gate.acquire().await;
        }
        self.record("home");
        Ok(self.served(Page::new(
            "Home",
            vec![
                Shelf::new(
                    "Listen again",
                    vec![Item::Track(tracks()[0].clone()), Item::Album(album())],
                ),
                Shelf::new(
                    "Mixed for you",
                    vec![Item::Playlist(playlist()), Item::Artist(artist())],
                ),
            ],
        )))
    }

    async fn search(&self, query: &str) -> Result<Page> {
        self.record(format!("search:{query}"));
        Ok(self.served(Page::new(
            format!("Search: {query}"),
            vec![
                Shelf::new("Songs", tracks().into_iter().map(Item::Track).collect()),
                Shelf::new("Albums", vec![Item::Album(album())]),
            ],
        )))
    }

    async fn library(&self) -> Result<Page> {
        self.record("library");
        Ok(Page::new(
            "Library",
            vec![Shelf::new("Playlists", vec![Item::Playlist(playlist())])],
        ))
    }

    async fn browse(&self, item: &Browsable) -> Result<Page> {
        self.record(format!("browse:{}", item.key()));
        if let Some(gate) = &self.browse_gate {
            let _ = gate.acquire().await;
        }
        if let (Some(paged), Browsable::Playlist(_)) = (&self.paged, item) {
            return Ok(self.served(Page {
                more: paged.more(0),
                ..Page::new(
                    item.title(),
                    vec![Shelf::new(
                        "Tracks",
                        paged.tracks(0).into_iter().map(Item::Track).collect(),
                    )],
                )
            }));
        }
        // YouTube serves a set video id on the rows of a playlist the account
        // may edit, and on nothing else.
        let owned = matches!(item, Browsable::Playlist(playlist) if playlist.playlist_id != "LM");
        let rows = tracks().into_iter().enumerate().map(|(index, track)| {
            let playlist_entry = owned.then(|| PlaylistEntry::synthetic(index as u64 + 1));
            Item::Track(Track {
                playlist_entry,
                ..track
            })
        });
        Ok(self.served(Page::new(
            item.title(),
            vec![Shelf::new("Tracks", rows.collect())],
        )))
    }

    async fn continue_page(&self, continuation: &Continuation) -> Result<(Vec<Item>, Option<Continuation>)> {
        self.record(format!("continue:{}", continuation.token));
        let Some(paged) = &self.paged else {
            return Ok((Vec::new(), None));
        };
        if let Some(gate) = &paged.gate {
            let _ = gate.acquire().await;
        }
        if self.continuation_fails {
            return Err(redwax_ytm::Error::Network("fixture continuation failed".into()));
        }
        if self.continuation_rejected {
            return Err(redwax_ytm::Error::Rejected);
        }
        let batch: usize = continuation
            .token
            .trim_start_matches("batch:")
            .parse()
            .expect("the fake writes its own tokens");
        let items = paged.tracks(batch).into_iter().map(Item::Track).collect();
        let more = match self.continuation_repeats {
            true => Some(continuation.clone()),
            false => paged.more(batch),
        };
        Ok((items, more))
    }

    async fn radio(&self, seed: &Track) -> Result<Vec<Track>> {
        self.record(format!("radio:{}", seed.video_id));
        let mut radio = tracks();
        radio.retain(|track| track.video_id != seed.video_id);
        radio.insert(0, seed.clone());
        Ok(radio)
    }

    async fn start_radio(&self, seed: &Track) -> Result<(Vec<Track>, Option<RadioContinuation>)> {
        let Some(paged) = &self.radio_paged else {
            return self.radio(seed).await.map(|tracks| (tracks, None));
        };
        self.record(format!("radio:{}", seed.video_id));
        self.radio_batch.store(1, Ordering::SeqCst);
        let tracks = std::iter::once(seed.clone()).chain(paged.tracks(0)).collect();
        let more = (paged.batches.len() > 1).then(|| RadioContinuation::new(seed, "page-1".to_string()));
        Ok((tracks, more))
    }

    async fn continue_radio(&self, _: &RadioContinuation) -> Result<(Vec<Track>, Option<RadioContinuation>)> {
        let Some(paged) = &self.radio_paged else {
            return Ok((Vec::new(), None));
        };
        let batch = self.radio_batch.fetch_add(1, Ordering::SeqCst);
        self.record(format!("radio_continue:{batch}"));
        if let Some(gate) = &paged.gate {
            let _ = gate.acquire().await;
        }
        if batch >= paged.batches.len() {
            return Ok((Vec::new(), None));
        }
        let more = (batch + 1 < paged.batches.len())
            .then(|| RadioContinuation::new(&tracks()[0], format!("page-{}", batch + 1)));
        Ok((paged.tracks(batch), more))
    }

    async fn lyrics(&self, track: &Track) -> Result<Option<String>> {
        self.record(format!("lyrics:{}", track.video_id));
        Ok(match track.video_id.as_str() {
            "v1" => Some(timed_lines()[0].text.clone()),
            "v2" => Some(PLAIN_LYRICS.to_string()),
            _ => None,
        })
    }

    async fn timed_lyrics(&self, track: &Track) -> Result<Option<Lyrics>> {
        self.record(format!("timed_lyrics:{}", track.video_id));
        Ok(match track.video_id.as_str() {
            "v1" => Some(Lyrics::Timed(timed_lines())),
            "v2" => Some(Lyrics::Plain(PLAIN_LYRICS.to_string())),
            _ => None,
        })
    }

    async fn set_liked(&self, track: &Track, liked: bool) -> Result<()> {
        self.liked
            .lock()
            .expect("no test holds this lock")
            .insert(track.video_id.clone(), liked);
        Ok(())
    }

    async fn add_to_playlist(&self, playlist_id: &str, video_ids: &[String]) -> Result<PlaylistAdd> {
        self.record(format!("add_to_playlist:{playlist_id}:{}", video_ids.join(",")));
        match self.edits_fail {
            true => Err(redwax_ytm::Error::Other("the edit was refused".to_string())),
            false => Ok(PlaylistAdd::Added),
        }
    }

    async fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        video_id: &str,
        _entry: &PlaylistEntry,
    ) -> Result<()> {
        self.record(format!("remove_playlist_entry:{playlist_id}:{video_id}"));
        match self.edits_fail {
            true => Err(redwax_ytm::Error::Other("the edit was refused".to_string())),
            false => Ok(()),
        }
    }

    async fn remove_all_from_playlist(&self, playlist: &Playlist, video_id: &str) -> Result<PlaylistRemoval> {
        self.record(format!(
            "remove_all_from_playlist:{}:{video_id}",
            playlist.playlist_id
        ));
        match self.edits_fail {
            true => Err(redwax_ytm::Error::Other("the edit was refused".to_string())),
            false => Ok(PlaylistRemoval::Removed { occurrences: 1 }),
        }
    }

    async fn create_playlist(&self, title: &str, video_ids: &[String]) -> Result<String> {
        self.record(format!("create_playlist:{title}:{}", video_ids.join(",")));
        match self.edits_fail {
            true => Err(redwax_ytm::Error::Other("the playlist was refused".to_string())),
            false => Ok("PLnew".to_string()),
        }
    }

    async fn track_length(&self, video_id: &str) -> Result<Option<u32>> {
        let Some(lengths) = &self.lengths else {
            return Ok(None);
        };
        if let Some(gate) = &lengths.gate {
            let _ = gate.acquire().await;
        }
        lengths
            .asked
            .lock()
            .expect("no test holds this lock")
            .push(video_id.to_string());
        Ok(Some(lengths.seconds))
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        Ok(Vec::new())
    }
}

/// A source that must never be asked anything, for the first frame test.
pub struct PanicSource;

impl MusicSource for PanicSource {
    fn signed_in(&self) -> bool {
        panic!("the first frame asked the network")
    }
    async fn whoami(&self) -> Result<Option<String>> {
        panic!("the first frame asked the network")
    }
    async fn home(&self) -> Result<Page> {
        panic!("the first frame asked the network")
    }
    async fn search(&self, _query: &str) -> Result<Page> {
        panic!("the first frame asked the network")
    }
    async fn library(&self) -> Result<Page> {
        panic!("the first frame asked the network")
    }
    async fn browse(&self, _item: &Browsable) -> Result<Page> {
        panic!("the first frame asked the network")
    }
    async fn radio(&self, _track: &Track) -> Result<Vec<Track>> {
        panic!("the first frame asked the network")
    }
    async fn lyrics(&self, _track: &Track) -> Result<Option<String>> {
        panic!("the first frame asked the network")
    }
    async fn timed_lyrics(&self, _track: &Track) -> Result<Option<Lyrics>> {
        panic!("the first frame asked the network")
    }
    async fn set_liked(&self, _track: &Track, _liked: bool) -> Result<()> {
        panic!("the first frame asked the network")
    }
    async fn accounts(&self) -> Result<Vec<Account>> {
        panic!("the first frame asked the network")
    }
}

struct PlayerRecord {
    state: watch::Sender<PlayerState>,
    calls: Mutex<Vec<String>>,
    prefetched: Mutex<Vec<String>>,
    started: AtomicBool,
    closed: AtomicBool,
    controls_fail: AtomicBool,
    next_entry: AtomicU64,
    next_revision: AtomicU64,
    play_gate: Mutex<Option<Arc<Semaphore>>>,
}

/// Keeps a queue in memory and reports state the way the mpv backed player does.
#[derive(Clone)]
pub struct FakePlayer {
    inner: Arc<PlayerRecord>,
}

impl FakePlayer {
    pub fn new(volume: u8) -> FakePlayer {
        let (state, _) = watch::channel(PlayerState {
            volume,
            ..PlayerState::default()
        });
        FakePlayer {
            inner: Arc::new(PlayerRecord {
                state,
                calls: Mutex::new(Vec::new()),
                prefetched: Mutex::new(Vec::new()),
                started: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                controls_fail: AtomicBool::new(false),
                next_entry: AtomicU64::new(1),
                next_revision: AtomicU64::new(1),
                play_gate: Mutex::new(None),
            }),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.inner.calls.lock().expect("no test holds this lock").clone()
    }

    /// Every audio filter graph mpv was given, the empty ones included.
    pub fn filters(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| call.strip_prefix("af:").map(str::to_string))
            .collect()
    }

    pub fn prefetched(&self) -> Vec<String> {
        self.inner
            .prefetched
            .lock()
            .expect("no test holds this lock")
            .clone()
    }

    pub fn clear_prefetched(&self) {
        self.inner
            .prefetched
            .lock()
            .expect("no test holds this lock")
            .clear();
    }

    pub fn started(&self) -> bool {
        self.inner.started.load(Ordering::SeqCst)
    }

    pub fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub fn fail_controls(&self) {
        self.inner.controls_fail.store(true, Ordering::SeqCst);
    }

    pub fn gate_play(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.inner.play_gate.lock().expect("no test holds this lock") = Some(gate.clone());
        gate
    }

    fn record(&self, what: impl Into<String>) {
        self.inner
            .calls
            .lock()
            .expect("no test holds this lock")
            .push(what.into());
    }

    fn change(&self, edit: impl FnOnce(&mut PlayerState)) {
        self.inner.state.send_modify(edit);
    }

    fn entries_for(&self, tracks: &[Track]) -> Vec<QueueEntry> {
        tracks
            .iter()
            .cloned()
            .map(|track| QueueEntry::synthetic(self.inner.next_entry.fetch_add(1, Ordering::SeqCst), track))
            .collect()
    }

    fn next_revision(&self) -> QueueRevision {
        QueueRevision::synthetic(self.inner.next_revision.fetch_add(1, Ordering::SeqCst))
    }

    pub fn identify(&self, state: &mut PlayerState) {
        state.queue_entries = self.entries_for(&state.queue);
        state.queue_revision = self.next_revision();
    }

    pub fn external_revision(&self) {
        let revision = self.next_revision();
        self.change(|state| state.queue_revision = revision);
    }
}

impl PlayerControl for FakePlayer {
    fn state(&self) -> watch::Receiver<PlayerState> {
        self.inner.state.subscribe()
    }

    fn snapshot(&self) -> PlayerState {
        self.inner.state.borrow().clone()
    }

    async fn start(&self) -> PlayerResult<()> {
        self.inner.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> PlayerResult<()> {
        self.inner.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn retry_playback(&self) -> PlayerResult<()> {
        self.change(|state| {
            state.availability = redwax_player::PlaybackAvailability::RestoredPaused;
            state.paused = true;
        });
        Ok(())
    }

    async fn play(&self, tracks: Vec<Track>, index: usize) -> PlayerResult<()> {
        self.play_from(tracks, index, 0.0).await
    }

    async fn play_from(&self, tracks: Vec<Track>, index: usize, seconds: f64) -> PlayerResult<()> {
        self.record(format!("play:{index}"));
        let gate = self
            .inner
            .play_gate
            .lock()
            .expect("no test holds this lock")
            .clone();
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
        let entries = self.entries_for(&tracks);
        let revision = self.next_revision();
        self.change(|state| {
            state.duration = tracks.get(index).and_then(|t| t.duration_seconds).map(f64::from);
            state.queue = tracks;
            state.queue_entries = entries;
            state.queue_revision = revision;
            state.index = Some(index);
            state.position = seconds;
            state.paused = false;
        });
        Ok(())
    }

    async fn enqueue(&self, tracks: Vec<Track>, play_next: bool) -> PlayerResult<()> {
        self.record(format!("enqueue:{}:{play_next}", tracks.len()));
        let entries = self.entries_for(&tracks);
        let revision = self.next_revision();
        self.change(|state| {
            match (play_next, state.index) {
                (true, Some(at)) => {
                    let at = at + 1;
                    for (offset, (track, entry)) in tracks.into_iter().zip(entries).enumerate() {
                        state.queue.insert(at + offset, track);
                        state.queue_entries.insert(at + offset, entry);
                    }
                }
                _ => {
                    state.queue.extend(tracks);
                    state.queue_entries.extend(entries);
                }
            }
            state.queue_revision = revision;
            if state.index.is_none() {
                state.index = Some(0);
                state.duration = state.queue[0].duration_seconds.map(f64::from);
            }
        });
        Ok(())
    }

    async fn replace_upcoming(&self, tracks: Vec<Track>) -> PlayerResult<()> {
        self.record(format!("replace_upcoming:{}", tracks.len()));
        let upcoming = self.entries_for(&tracks);
        let revision = self.next_revision();
        self.change(|state| {
            let current = state.track().cloned();
            let current_entry = state
                .index
                .and_then(|index| state.queue_entries.get(index))
                .cloned();
            state.index = current.as_ref().map(|_| 0);
            state.queue = current.into_iter().chain(tracks).collect();
            state.queue_entries = current_entry.into_iter().chain(upcoming).collect();
            state.queue_revision = revision;
        });
        Ok(())
    }

    async fn jump(&self, index: usize) -> PlayerResult<()> {
        self.record(format!("jump:{index}"));
        self.change(|state| state.index = Some(index));
        Ok(())
    }

    async fn remove(&self, index: usize) -> PlayerResult<()> {
        self.record(format!("remove:{index}"));
        let revision = self.next_revision();
        self.change(|state| {
            if index < state.queue.len() {
                state.queue.remove(index);
                if index < state.queue_entries.len() {
                    state.queue_entries.remove(index);
                }
                state.queue_revision = revision;
            }
        });
        Ok(())
    }

    async fn remove_entry(&self, entry: QueueEntryId, revision: QueueRevision) -> PlayerResult<QueueEdit> {
        let state = self.snapshot();
        if state.queue_revision != revision {
            return Ok(QueueEdit::Stale);
        }
        let Some(index) = state
            .queue_entries
            .iter()
            .position(|current| current.id() == entry)
        else {
            return Ok(QueueEdit::Stale);
        };
        self.remove(index).await?;
        Ok(QueueEdit::Applied)
    }

    async fn move_entry(
        &self,
        entry: QueueEntryId,
        revision: QueueRevision,
        to: usize,
    ) -> PlayerResult<QueueEdit> {
        let state = self.snapshot();
        if state.queue_revision != revision {
            return Ok(QueueEdit::Stale);
        }
        let Some(from) = state
            .queue_entries
            .iter()
            .position(|current| current.id() == entry)
        else {
            return Ok(QueueEdit::Stale);
        };
        let to = to.min(state.queue.len().saturating_sub(1));
        self.record(format!("move:{from}:{to}"));
        let next = self.next_revision();
        self.change(|state| {
            let track = state.queue.remove(from);
            let entry = state.queue_entries.remove(from);
            state.queue.insert(to, track);
            state.queue_entries.insert(to, entry);
            state.queue_revision = next;
            if let Some(index) = state.index {
                state.index = if index == from {
                    Some(to)
                } else if from < index && index <= to {
                    Some(index - 1)
                } else if to <= index && index < from {
                    Some(index + 1)
                } else {
                    Some(index)
                };
            }
        });
        Ok(QueueEdit::Applied)
    }

    async fn clear(&self) -> PlayerResult<()> {
        self.record("clear");
        let revision = self.next_revision();
        self.change(|state| {
            let current = state.track().cloned();
            let current_entry = state
                .index
                .and_then(|index| state.queue_entries.get(index))
                .cloned();
            state.index = current.as_ref().map(|_| 0);
            state.queue = current.into_iter().collect();
            state.queue_entries = current_entry.into_iter().collect();
            state.queue_revision = revision;
        });
        Ok(())
    }

    async fn toggle_pause(&self) -> PlayerResult<()> {
        self.record("toggle_pause");
        self.change(|state| state.paused = !state.paused);
        Ok(())
    }

    async fn next(&self) -> PlayerResult<()> {
        self.record("next");
        self.change(|state| {
            if let Some(index) = state.index
                && index + 1 < state.queue.len()
            {
                state.index = Some(index + 1);
            }
        });
        Ok(())
    }

    async fn previous(&self) -> PlayerResult<()> {
        self.record("previous");
        Ok(())
    }

    async fn seek(&self, delta_seconds: f64) -> PlayerResult<()> {
        self.record(format!("seek:{delta_seconds}"));
        self.change(|state| state.position = (state.position + delta_seconds).max(0.0));
        Ok(())
    }

    async fn seek_to(&self, seconds: f64) -> PlayerResult<()> {
        self.record(format!("seek_to:{seconds}"));
        self.change(|state| state.position = seconds);
        Ok(())
    }

    async fn adjust_volume(&self, delta: i16) -> PlayerResult<()> {
        self.record(format!("volume:{delta}"));
        self.change(|state| {
            state.volume = (i16::from(state.volume) + delta).clamp(0, 100) as u8;
        });
        Ok(())
    }

    async fn set_volume(&self, volume: u8) -> PlayerResult<()> {
        self.change(|state| state.volume = volume.min(100));
        Ok(())
    }

    async fn toggle_mute(&self) -> PlayerResult<()> {
        self.record("toggle_mute");
        self.change(|state| state.muted = !state.muted);
        Ok(())
    }

    async fn set_audio_filter(&self, graph: Option<&str>) -> PlayerResult<()> {
        self.record(format!("af:{}", graph.unwrap_or_default()));
        Ok(())
    }

    async fn set_repeat(&self, mode: Repeat) -> PlayerResult<()> {
        self.change(|state| state.repeat = mode);
        Ok(())
    }

    async fn cycle_repeat(&self) -> PlayerResult<Repeat> {
        self.record("cycle_repeat");
        if self.inner.controls_fail.load(Ordering::SeqCst) {
            return Err(PlayerError::Mpv("control failed".to_string()));
        }
        let mut next = Repeat::Off;
        self.change(|state| {
            state.repeat = state.repeat.next();
            next = state.repeat;
        });
        Ok(next)
    }

    async fn set_shuffle(&self, on: bool) -> PlayerResult<()> {
        self.change(|state| state.shuffle = on);
        Ok(())
    }

    async fn toggle_shuffle(&self) -> PlayerResult<bool> {
        self.record("toggle_shuffle");
        if self.inner.controls_fail.load(Ordering::SeqCst) {
            return Err(PlayerError::Mpv("control failed".to_string()));
        }
        let mut on = false;
        self.change(|state| {
            state.shuffle = !state.shuffle;
            on = state.shuffle;
        });
        Ok(on)
    }

    fn prefetch(&self, tracks: Vec<Track>) {
        self.inner
            .prefetched
            .lock()
            .expect("no test holds this lock")
            .extend(tracks.into_iter().map(|t| t.video_id));
    }
}

/// A directory of this test's own, so page and art caches never collide.
pub fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("redwax-app-{}-{name}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temporary directory is writable");
    dir
}

pub fn config_in(dir: &Path) -> Config {
    Config {
        theme: "redwax".to_string(),
        art: "unicode".to_string(),
        auth_path: dir.join("browser.json"),
        cookies_path: dir.join("cookies.txt"),
        session_path: dir.join("session.json"),
        art_cache_dir: dir.join("art"),
        page_cache_dir: dir.join("pages"),
        state_path: dir.join("state.json"),
        ..Config::default()
    }
}

/// Put a small image where `ArtCache` looks on disk, so the art path runs
/// without the network. The Python tests used a fake cache for the same reason.
pub fn seed_art(dir: &Path, url: &str) {
    let art = dir.join("art");
    std::fs::create_dir_all(&art).expect("the temporary directory is writable");
    let digest = hex::encode(<sha1::Sha1 as sha1::Digest>::digest(url.as_bytes()));
    let image = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 40, 40]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("png encodes");
    std::fs::write(art.join(format!("{digest}.img")), bytes.into_inner())
        .expect("the temporary directory is writable");
}

pub struct Harness {
    pub app: App<FakeSource, FakePlayer>,
    pub terminal: Terminal<TestBackend>,
    pub events: mpsc::UnboundedReceiver<AppEvent>,
    pub player_state: watch::Receiver<PlayerState>,
    pub source: Arc<FakeSource>,
    pub player: FakePlayer,
    pub dir: PathBuf,
}

pub struct Setup {
    pub source: Arc<FakeSource>,
    pub config: Config,
    pub saved: SavedState,
    pub width: u16,
    pub height: u16,
    pub dir: PathBuf,
    /// What the genre source answers for every track. `None` leaves the real
    /// one in place, which no test asks for.
    pub genre: Option<String>,
}

impl Setup {
    pub fn new(name: &str) -> Setup {
        let dir = scratch(name);
        Setup {
            source: Arc::new(FakeSource::default()),
            config: config_in(&dir),
            saved: SavedState::default(),
            width: 140,
            height: 45,
            dir,
            genre: None,
        }
    }

    /// Refuse every playlist edit, so the removal has to put its row back.
    pub fn failing_edits(&mut self) {
        self.source = Arc::new(FakeSource {
            edits_fail: true,
            ..FakeSource::default()
        });
    }

    /// Serve the home playlist a batch at a time, in the sizes given.
    pub fn paged(&mut self, batches: &[usize]) {
        self.source = Arc::new(FakeSource {
            paged: Some(Paged::new(batches)),
            ..FakeSource::default()
        });
    }

    /// Serve every page the way YouTube serves a home row, with no duration
    /// on any track, and answer `seconds` when asked for one.
    pub fn blank_durations(&mut self, seconds: u32) {
        self.source = Arc::new(FakeSource {
            blank_durations: true,
            lengths: Some(Lengths::answering(seconds)),
            ..FakeSource::default()
        });
    }

    /// The same, with every length request held until the test opens the gate.
    pub fn blank_durations_gated(&mut self, seconds: u32) -> Arc<Semaphore> {
        let (lengths, gate) = Lengths::gated(seconds);
        self.source = Arc::new(FakeSource {
            blank_durations: true,
            lengths: Some(lengths),
            ..FakeSource::default()
        });
        gate
    }

    /// The same, held closed until the test opens the gate.
    pub fn paged_gated(&mut self, batches: &[usize]) -> Arc<Semaphore> {
        let (paged, gate) = Paged::gated(batches);
        self.source = Arc::new(FakeSource {
            paged: Some(paged),
            ..FakeSource::default()
        });
        gate
    }

    pub fn build(self) -> Harness {
        let player = FakePlayer::new(self.config.volume);
        let deps = Deps {
            icons: redwax::icons::icon_set(&self.config.icons),
            theme: redwax::theme::redwax_theme(),
            page_cache: PageCache::new(self.config.page_cache_dir.clone()),
            art_cache: Arc::new(redwax::art::ArtCache::new(self.config.art_cache_dir.clone())),
            art_setup: redwax::terminal::detect(&self.config.art, Some(false), None),
            saved: self.saved,
            state_store: redwax::state::PlaybackStateStore::new(self.config.state_path.clone()),
            session_refresh: None,
            session_generation: Default::default(),
            session_logout: None,
            account_access: None,
            config: self.config,
        };
        let deps = match deps.config.transparent {
            true => Deps {
                theme: deps.theme.clone().transparent(),
                ..deps
            },
            false => deps,
        };
        let (sender, events) = mpsc::unbounded_channel();
        let mut app = App::new(self.source.clone(), player.clone(), deps, sender);
        if let Some(genre) = self.genre.clone() {
            app.set_genre_lookup(fake_genre(&genre));
        }
        Harness {
            player_state: player.state(),
            terminal: Terminal::new(TestBackend::new(self.width, self.height))
                .expect("the test backend never fails"),
            app,
            events,
            source: self.source,
            player,
            dir: self.dir,
        }
    }
}

impl Harness {
    /// Build, draw the first frame, start the background work and let it land.
    pub async fn start(name: &str) -> Harness {
        let mut harness = Setup::new(name).build();
        harness.draw();
        harness.app.start_background();
        harness.settle().await;
        harness
    }

    pub fn draw(&mut self) {
        let app = &mut self.app;
        self.terminal
            .draw(|frame| app.draw(frame))
            .expect("the test backend never fails");
    }

    /// Run every task that is ready, feed what it produced to the app, then
    /// redraw. The short sleeps are for the tasks that reach the blocking pool,
    /// which a yield alone does not wait for.
    pub async fn settle(&mut self) {
        for round in 0..48 {
            match round % 8 {
                0 => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
                _ => tokio::task::yield_now().await,
            }
            while let Ok(event) = self.events.try_recv() {
                self.app.handle_event(event);
            }
            if self.player_state.has_changed().unwrap_or(false) {
                let state = self.player_state.borrow_and_update().clone();
                self.app.handle_event(AppEvent::PlayerChanged(state));
            }
        }
        self.draw();
    }

    pub async fn press(&mut self, key: &str) {
        self.app.handle_key(parse_key(key));
        self.settle().await;
    }

    pub async fn press_all(&mut self, keys: &[&str]) {
        for key in keys {
            self.press(key).await;
        }
    }

    pub async fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.app
                .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        self.settle().await;
    }

    /// Let the cursor rest long enough for the prefetch, then run the tick that
    /// acts on it. The wait is real, because the rest is measured on a clock
    /// that starts at the process, not at the test.
    pub async fn rest(&mut self) {
        let wait = redwax::app::PREFETCH_REST_MILLIS + 50;
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        self.app.handle_event(AppEvent::Tick);
        self.settle().await;
    }

    /// What the page cache holds for `key`, which is where a finished page
    /// lands and a partial one must not.
    pub fn cached(&self, key: &str) -> Option<Page> {
        PageCache::new(self.dir.join("pages")).get(key)
    }

    pub fn screen(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// The symbol in one cell. The header row carries an image payload once
    /// the mascot is up, which leaves the joined row unreadable as text.
    pub fn symbol(&self, x: u16, y: u16) -> String {
        self.terminal
            .backend()
            .buffer()
            .cell((x, y))
            .map(|cell| cell.symbol().to_string())
            .unwrap_or_default()
    }

    pub fn row(&self, y: u16) -> String {
        self.screen()
            .lines()
            .nth(y as usize)
            .unwrap_or_default()
            .to_string()
    }

    /// Every row of the transport, joined, so a test asserts on the bar
    /// without caring which of the three rows a part landed on.
    pub fn player_bar(&self) -> String {
        self.player_bar_rows().join("\n")
    }

    pub fn player_bar_rows(&self) -> Vec<String> {
        let height = self.terminal.backend().buffer().area().height;
        let bottom = height - self.hint_row();
        let top = bottom - redwax::ui::player_bar::PLAYER_BAR_HEIGHT;
        (top..bottom).map(|y| self.row(y)).collect()
    }

    /// One where the bottom keys bar is drawn, none where the sidebar's own
    /// keys box holds the hints instead.
    pub fn hint_row(&self) -> u16 {
        u16::from(!self.app.sidebar_visible())
    }

    /// The sidebar row the now playing title is drawn on: the header, the
    /// sidebar padding, the art at its full height, then one blank row.
    pub fn sidebar_title(&self) -> String {
        self.row(redwax::app::HEADER_HEIGHT + 1 + redwax::app::ART_MAX_HEIGHT + 1)
    }

    /// The last row of the body, which is where the queue box's bottom border
    /// sits when the box is anchored.
    pub fn body_bottom(&self) -> u16 {
        let area = self.terminal.backend().buffer().area();
        let chrome = redwax::app::CHROME_HEIGHT - redwax::app::HEADER_HEIGHT - 1 + self.hint_row();
        area.height - chrome - 1
    }

    /// The header row the wordmark and the tabs are drawn on.
    pub fn header_row(&self) -> String {
        self.row(0)
    }

    /// Every cell style on a row, for the rules the terminal draws rather than
    /// the ones written into cells.
    pub fn styles_at(&self, y: u16) -> Vec<ratatui::style::Style> {
        let buffer = self.terminal.backend().buffer();
        (0..buffer.area().width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.style()))
            .collect()
    }

    /// The number of the first row that carries `text`.
    pub fn row_of(&self, text: &str) -> u16 {
        self.screen()
            .lines()
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("no row carries {text:?}")) as u16
    }

    /// The first row of the list that carries `text`.
    pub fn list_row(&self, text: &str) -> String {
        self.screen()
            .lines()
            .find(|line| line.contains(text))
            .unwrap_or_default()
            .to_string()
    }

    /// The left edge of the content pane, which is where the lyrics panel is
    /// drawn when the sidebar has room and the whole body when it has not.
    fn content_x(&self) -> u16 {
        match self.app.sidebar_visible() {
            true => redwax::app::SIDEBAR_WIDTH,
            false => 0,
        }
    }

    /// Row `y` of the content pane, built from the cells so a wide glyph in the
    /// sidebar cannot shift the columns.
    pub fn pane_row(&self, y: u16) -> String {
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area();
        (self.content_x()..area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The lyrics panel's progress rule, the row the marker moves along.
    pub fn lyrics_marker(&self) -> String {
        self.pane_row(redwax::app::HEADER_HEIGHT + 1)
    }

    /// The row of the lit lyric line. Only the current line carries a rule at
    /// the left edge of the panel, so that cell is the whole search. The
    /// transport draws a rule of its own, so the search stops above it.
    fn lit_row(&self) -> Option<u16> {
        let buffer = self.terminal.backend().buffer();
        let rule_x = self.content_x() + 2;
        let bottom = self.body_bottom() + 1;
        (redwax::app::HEADER_HEIGHT + 2..bottom).find(|y| {
            buffer
                .cell((rule_x, *y))
                .is_some_and(|cell| cell.symbol() == "\u{258e}")
        })
    }

    pub fn current_lyric(&self) -> String {
        self.lit_row()
            .map(|y| self.pane_row(y).trim().to_string())
            .unwrap_or_default()
    }

    /// The panel rows either side of the lit line, for a report that shows the
    /// highlight moving.
    pub fn lyric_window(&self, either_side: u16) -> Vec<String> {
        let height = self.terminal.backend().buffer().area().height;
        let Some(y) = self.lit_row() else {
            return Vec::new();
        };
        (y.saturating_sub(either_side)..(y + either_side + 1).min(height))
            .map(|row| self.pane_row(row))
            .collect()
    }

    /// Where the last frame put that hit. A test clicks by name, never by a
    /// coordinate it guessed.
    pub fn area_of(&self, want: impl Fn(&Hit) -> bool) -> Rect {
        self.app
            .screen
            .hit_map()
            .area_of(want)
            .expect("the last frame drew that region")
    }

    pub async fn click_at(&mut self, x: u16, y: u16, button: MouseButton) {
        self.app.handle_mouse(mouse(x, y, MouseEventKind::Down(button)));
        self.app.handle_mouse(mouse(x, y, MouseEventKind::Up(button)));
        self.settle().await;
    }

    /// Click the middle of the region that hit names.
    pub async fn click(&mut self, want: impl Fn(&Hit) -> bool) {
        let area = self.area_of(want);
        self.click_at(area.x + area.width / 2, area.y, MouseButton::Left)
            .await;
    }

    pub async fn right_click(&mut self, want: impl Fn(&Hit) -> bool) {
        let area = self.area_of(want);
        self.click_at(area.x + area.width / 2, area.y, MouseButton::Right)
            .await;
    }

    /// Press at one column, drag through the rest, release at the last. The
    /// draw between moves is what a real terminal does, so a test can read the
    /// bar back mid-drag.
    pub async fn drag(&mut self, y: u16, columns: &[u16]) {
        let (first, rest) = columns.split_first().expect("a drag starts somewhere");
        self.app
            .handle_mouse(mouse(*first, y, MouseEventKind::Down(MouseButton::Left)));
        for x in rest {
            self.app
                .handle_mouse(mouse(*x, y, MouseEventKind::Drag(MouseButton::Left)));
            self.draw();
        }
        let last = *columns.last().expect("a drag ends somewhere");
        self.app
            .handle_mouse(mouse(last, y, MouseEventKind::Up(MouseButton::Left)));
        self.settle().await;
    }

    pub async fn wheel(&mut self, x: u16, y: u16, up: bool) {
        let kind = match up {
            true => MouseEventKind::ScrollUp,
            false => MouseEventKind::ScrollDown,
        };
        self.app.handle_mouse(mouse(x, y, kind));
        self.settle().await;
    }

    /// Move the player's position, the way mpv's own state changes reach the
    /// app, then repaint.
    pub async fn at_position(&mut self, seconds: f64) {
        self.player.change(|state| state.position = seconds);
        let state = PlayerState {
            position: seconds,
            ..self.app.player_state.clone()
        };
        self.app.handle_event(AppEvent::PlayerChanged(state));
        self.draw();
    }

    /// Drive the animation clock, then repaint.
    /// Start the first track on the player without going through a page, then
    /// draw, so the caller starts from a screen with nothing left to repaint.
    pub fn playing(&mut self) {
        self.playing_queue(tracks(), 0);
    }

    /// The same, from `queue` at entry `index`, so what plays can differ from
    /// the row the cursor starts on.
    pub fn playing_queue(&mut self, queue: Vec<Track>, index: usize) {
        let mut state = self.app.player_state.clone();
        state.queue = queue;
        state.index = Some(index);
        state.paused = false;
        state.loading = false;
        self.player.identify(&mut state);
        let player_state = state.clone();
        self.player.change(|state| *state = player_state);
        self.app.handle_event(AppEvent::PlayerChanged(state));
        self.draw();
    }

    pub fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.app.handle_event(AppEvent::Tick);
        }
        self.draw();
    }
}

/// A track whose title cannot fit any sidebar, for the marquee.
pub fn long_track() -> Track {
    Track {
        video_id: "long1".to_string(),
        title: "A Very Long Title That No Sidebar Column Will Ever Hold In One Go".to_string(),
        artists: vec!["Arnegger".to_string()],
        album: Some("Peaks".to_string()),
        album_browse_id: None,
        duration_seconds: Some(214),
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

/// A genre source that answers the same for every track, so no test reaches
/// the iTunes API.
pub fn fake_genre(genre: &str) -> redwax::genre::Lookup {
    let answer = genre.to_string();
    Arc::new(move |_artist, _title| {
        let answer = answer.clone();
        Box::pin(async move { Some(answer) })
    })
}

pub fn mouse(x: u16, y: u16, kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

pub fn parse_key(key: &str) -> KeyEvent {
    let (modifiers, name) = match key.strip_prefix("ctrl+") {
        Some(rest) => (KeyModifiers::CONTROL, rest),
        None => (KeyModifiers::NONE, key),
    };
    let code = match name {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        other => KeyCode::Char(other.chars().next().expect("a key name is never empty")),
    };
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}
