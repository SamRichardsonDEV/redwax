//! Playback metadata survives a relaunch. Live queue identities and playlist
//! editing authority belong to their current player or account generation.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use redwax_player::{PlayerState, Repeat};
use redwax_ytm::Track;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MAX_QUEUE: usize = 10_000;
const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SavedState {
    #[serde(default, serialize_with = "playback_queue")]
    pub queue: Vec<Track>,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default, deserialize_with = "repeat_mode")]
    pub repeat: Repeat,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub muted: bool,
}

pub struct StateLoad {
    pub state: SavedState,
    pub warning: Option<&'static str>,
}

impl SavedState {
    pub fn from_player(player_state: &PlayerState) -> Self {
        let mut saved_state = Self {
            queue: player_state.queue.clone(),
            index: player_state.index,
            position: player_state.position,
            volume: Some(player_state.volume),
            repeat: player_state.repeat,
            shuffle: player_state.shuffle,
            muted: player_state.muted,
        };
        saved_state.normalize();
        saved_state
    }

    pub fn load(state_path: &Path) -> StateLoad {
        let state_read_result = read_private(state_path).and_then(|serialized_state| {
            serde_json::from_slice::<Self>(&serialized_state).map_err(io::Error::other)
        });
        match state_read_result {
            Ok(mut saved_state) if saved_state.queue.len() <= MAX_QUEUE => {
                let invalid_values_reset = saved_state.normalize();
                StateLoad {
                    state: saved_state,
                    warning: invalid_values_reset
                        .then_some("Saved playback contained invalid values. They were reset."),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => StateLoad {
                state: Self::default(),
                warning: None,
            },
            Ok(_) | Err(_) => StateLoad {
                state: Self::default(),
                warning: Some("Saved playback could not be read. Starting with an empty queue."),
            },
        }
    }

    pub fn read(path: &Path) -> Self {
        Self::load(path).state
    }

    pub fn write(&self, state_path: &Path) -> io::Result<()> {
        if self.queue.len() > MAX_QUEUE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "saved queue exceeds the restore limit",
            ));
        }
        let mut normalized_state = self.clone();
        normalized_state.normalize();
        let serialized_state = serde_json::to_vec(&normalized_state).map_err(io::Error::other)?;
        if serialized_state.len() > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "saved playback exceeds the restore limit",
            ));
        }
        crate::private_file::write(state_path, &serialized_state)
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", state_path.display())))
    }

    fn normalize(&mut self) -> bool {
        let mut invalid_values_reset = false;
        if self
            .index
            .is_some_and(|queue_index| queue_index >= self.queue.len())
        {
            self.index = None;
            invalid_values_reset = true;
        }
        if !self.position.is_finite() || self.position < 0.0 || self.index.is_none() {
            invalid_values_reset |= self.position != 0.0;
            self.position = 0.0;
        }
        if let Some(volume_percent) = &mut self.volume {
            invalid_values_reset |= *volume_percent > 100;
            *volume_percent = (*volume_percent).min(100);
        }
        for track in &mut self.queue {
            track.playlist_entry = None;
        }
        invalid_values_reset
    }
}

/// Serializes durable checkpoints and remembers only successful writes. A failed
/// snapshot stays retryable without sending the player command again.
#[derive(Clone)]
pub struct PlaybackStateStore {
    inner: Arc<Store>,
}

struct Store {
    path: PathBuf,
    state: Mutex<CheckpointState>,
    writer: Mutex<()>,
}

#[derive(Default)]
struct CheckpointState {
    pending: Option<SavedState>,
    written: Option<SavedState>,
}

/// Captured synchronously before handing disk work to a worker. An older
/// worker flushes the newest submitted state and cannot restore an old queue.
pub struct PendingCheckpoint {
    inner: Arc<Store>,
}

impl PlaybackStateStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Store {
                path,
                state: Mutex::new(CheckpointState::default()),
                writer: Mutex::new(()),
            }),
        }
    }

    pub fn submit(&self, player_state: &PlayerState) -> PendingCheckpoint {
        self.submit_saved(SavedState::from_player(player_state))
    }

    pub fn checkpoint(&self, player_state: &PlayerState) -> io::Result<()> {
        self.submit(player_state).write()
    }

    pub fn write(&self, saved_state: &SavedState) -> io::Result<()> {
        let mut normalized_snapshot = saved_state.clone();
        normalized_snapshot.normalize();
        self.submit_saved(normalized_snapshot).write()
    }

    fn submit_saved(&self, normalized_snapshot: SavedState) -> PendingCheckpoint {
        let mut checkpoint_state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        checkpoint_state.pending = Some(normalized_snapshot);
        PendingCheckpoint {
            inner: self.inner.clone(),
        }
    }
}

impl PendingCheckpoint {
    pub fn write(self) -> io::Result<()> {
        let _write_guard = self
            .inner
            .writer
            .lock()
            .map_err(|_| io::Error::other("playback checkpoint is unavailable"))?;
        loop {
            let snapshot_to_write = {
                let checkpoint_state = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("playback checkpoint is unavailable"))?;
                let Some(snapshot_to_write) = checkpoint_state.pending.as_ref() else {
                    return Ok(());
                };
                if checkpoint_state.written.as_ref() == Some(snapshot_to_write) {
                    return Ok(());
                }
                snapshot_to_write.clone()
            };
            snapshot_to_write.write(&self.inner.path)?;
            let mut checkpoint_state = self
                .inner
                .state
                .lock()
                .map_err(|_| io::Error::other("playback checkpoint is unavailable"))?;
            checkpoint_state.written = Some(snapshot_to_write.clone());
            if checkpoint_state.pending.as_ref() == Some(&snapshot_to_write) {
                return Ok(());
            }
        }
    }
}

fn read_private(state_path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;

    let state_file = std::fs::File::open(state_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        state_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut serialized_state = Vec::new();
    std::io::BufReader::new(state_file)
        .take((MAX_STATE_BYTES + 1) as u64)
        .read_to_end(&mut serialized_state)?;
    if serialized_state.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved playback exceeds the restore limit",
        ));
    }
    Ok(serialized_state)
}

fn repeat_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Repeat, D::Error> {
    let persisted_repeat_mode = String::deserialize(deserializer)?;
    match persisted_repeat_mode.as_str() {
        "all" => Ok(Repeat::All),
        "one" => Ok(Repeat::One),
        "off" | "" => Ok(Repeat::Off),
        _ => Err(serde::de::Error::custom("saved repeat mode is invalid")),
    }
}

fn playback_queue<S: Serializer>(tracks: &[Track], serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Metadata<'a> {
        video_id: &'a str,
        title: &'a str,
        artists: &'a [String],
        album: &'a Option<String>,
        album_browse_id: &'a Option<String>,
        duration_seconds: Option<u32>,
        thumbnail_url: &'a Option<String>,
        liked: bool,
    }
    let track_metadata: Vec<_> = tracks
        .iter()
        .map(|track| Metadata {
            video_id: &track.video_id,
            title: &track.title,
            artists: &track.artists,
            album: &track.album,
            album_browse_id: &track.album_browse_id,
            duration_seconds: track.duration_seconds,
            thumbnail_url: &track.thumbnail_url,
            liked: track.liked,
        })
        .collect();
    track_metadata.serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            video_id: "abcdefghijk".to_owned(),
            title: "Fixture".to_owned(),
            artists: vec![],
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: Some(redwax_ytm::PlaylistEntry::synthetic(7)),
        }
    }

    #[test]
    fn legacy_state_decodes_without_preserving_edit_authority() {
        let mut saved: SavedState = serde_json::from_str(r#"{"queue":[{"video_id":"abcdefghijk","title":"Fixture","set_video_id":"old-authority"}],"index":0,"repeat":"all"}"#).unwrap();
        saved.normalize();
        assert_eq!(saved.repeat, Repeat::All);
        assert!(!saved.muted);
        assert!(saved.queue[0].playlist_entry.is_none());
        assert!(!serde_json::to_string(&saved).unwrap().contains("set_video_id"));
    }

    #[test]
    fn snapshot_resets_invalid_runtime_values_and_omits_capabilities() {
        let saved = SavedState::from_player(&PlayerState {
            queue: vec![track()],
            index: Some(8),
            position: f64::NAN,
            volume: 255,
            ..PlayerState::default()
        });
        assert_eq!(saved.index, None);
        assert_eq!(saved.position, 0.0);
        assert_eq!(saved.volume, Some(100));
        assert!(saved.queue[0].playlist_entry.is_none());
        let body = serde_json::to_string(&saved).unwrap();
        assert!(!body.contains("set_video_id"));
        assert!(!body.contains("redwax-fixture"));
    }

    #[test]
    fn missing_and_invalid_state_have_distinct_startup_results() {
        let directory = std::env::temp_dir().join(format!("redwax-state-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.json");
        assert!(SavedState::load(&path).warning.is_none());
        std::fs::write(&path, b"not JSON").unwrap();
        let loaded = SavedState::load(&path);
        assert!(loaded.warning.is_some());
        assert!(loaded.state.queue.is_empty());
        std::fs::write(&path, br#"{"queue":[],"index":8,"position":-2,"volume":255}"#).unwrap();
        let loaded = SavedState::load(&path);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.state.index, None);
        assert_eq!(loaded.state.position, 0.0);
        assert_eq!(loaded.state.volume, Some(100));
        std::fs::write(&path, br#"{"repeat":"unknown"}"#).unwrap();
        assert!(SavedState::load(&path).warning.is_some());
        std::fs::write(&path, br#"{"muted":"yes"}"#).unwrap();
        assert!(SavedState::load(&path).warning.is_some());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reverse_worker_completion_cannot_restore_an_older_queue() {
        let directory = std::env::temp_dir().join(format!("redwax-state-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.json");
        let store = PlaybackStateStore::new(path.clone());
        let old = store.submit(&PlayerState {
            queue: vec![track()],
            index: Some(0),
            ..PlayerState::default()
        });
        let new = store.submit(&PlayerState::default());
        new.write().unwrap();
        old.write().unwrap();
        assert!(SavedState::read(&path).queue.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_checkpoint_keeps_the_same_snapshot_retryable() {
        let path = std::env::temp_dir().join(format!("redwax-state-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        let store = PlaybackStateStore::new(path.clone());
        let state = PlayerState {
            queue: vec![track()],
            index: Some(0),
            muted: true,
            ..PlayerState::default()
        };
        assert!(store.checkpoint(&state).is_err());
        std::fs::remove_dir(&path).unwrap();
        store.checkpoint(&state).unwrap();
        let saved = SavedState::read(&path);
        assert_eq!(saved.queue.len(), 1);
        assert!(saved.muted);
        std::fs::remove_file(path).unwrap();
    }
}
