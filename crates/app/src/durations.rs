//! Track lengths remembered on disk, so a row YouTube sent no duration for
//! shows its time on the next visit without waiting for the network.
//!
//! One flat file beside the page cache. It is a convenience, never an
//! authority: a missing or corrupt file reads as "nothing remembered".

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use redwax_ytm::{Item, Page, Track};

use crate::config::Config;

/// The shortest gap between two writes of the file. A page of forty rows lands
/// in chunks, and each chunk would otherwise cost a rename.
const WRITE_INTERVAL: Duration = Duration::from_secs(1);

/// Seconds per video id, and how far behind the file is.
#[derive(Default)]
struct KnownDurations {
    duration_seconds_by_video_id: HashMap<String, u32>,
    /// Ids YouTube has already been asked about this run, answer or none. Not
    /// written to the file: a track with no length at all is worth one request
    /// per run, and re-asking it every tick is worth none.
    requested_video_ids: HashSet<String>,
    /// Entries the file does not hold yet.
    has_unsaved_changes: bool,
    last_write_time: Option<Instant>,
}

pub struct Durations {
    cache_path: PathBuf,
    known_durations: RwLock<KnownDurations>,
}

impl Durations {
    /// Read the file. Cheap enough for the launch path: one small object.
    pub fn new(cache_path: PathBuf) -> Durations {
        if let Some(cache_directory) = cache_path.parent() {
            let _ = crate::private_file::prepare_directory(cache_directory);
        }
        let duration_seconds_by_video_id = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Durations {
            cache_path,
            known_durations: RwLock::new(KnownDurations {
                duration_seconds_by_video_id,
                requested_video_ids: HashSet::new(),
                has_unsaved_changes: false,
                last_write_time: None,
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.read().duration_seconds_by_video_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, video_id: &str) -> Option<u32> {
        self.read().duration_seconds_by_video_id.get(video_id).copied()
    }

    /// Stamp every track of `page` that has no duration, and say whether any
    /// row changed. Rows move nowhere, so the cursor is untouched.
    pub fn fill(&self, page: &mut Page) -> bool {
        let mut page_changed = false;
        for shelf in &mut page.shelves {
            page_changed |= self.fill_items(&mut shelf.items);
        }
        page_changed
    }

    pub fn fill_items(&self, page_items: &mut [Item]) -> bool {
        let known_durations = self.read();
        let mut items_changed = false;
        for page_item in page_items {
            if let Item::Track(track) = page_item {
                items_changed |= fill_track_duration(&known_durations.duration_seconds_by_video_id, track);
            }
        }
        items_changed
    }

    pub fn fill_tracks(&self, tracks: &mut [Track]) -> bool {
        let known_durations = self.read();
        tracks.iter_mut().fold(false, |tracks_changed, track| {
            tracks_changed | fill_track_duration(&known_durations.duration_seconds_by_video_id, track)
        })
    }

    /// The ids of `wanted` that have not been asked about yet, marked as
    /// asked. The caller is about to send exactly these.
    pub fn take_unasked(&self, wanted_video_ids: Vec<String>) -> Vec<String> {
        let mut known_durations = self.write_lock();
        wanted_video_ids
            .into_iter()
            .filter(|video_id| known_durations.requested_video_ids.insert(video_id.clone()))
            .collect()
    }

    /// Remember what the network answered. True when anything was new, which
    /// is the only case worth repainting for.
    pub fn learn(&self, fetched_durations: &[(String, u32)]) -> bool {
        let mut known_durations = self.write_lock();
        let mut learned_any_duration = false;
        for (video_id, duration_seconds) in fetched_durations {
            if known_durations.duration_seconds_by_video_id.get(video_id) != Some(duration_seconds) {
                known_durations
                    .duration_seconds_by_video_id
                    .insert(video_id.clone(), *duration_seconds);
                learned_any_duration = true;
            }
        }
        known_durations.has_unsaved_changes |= learned_any_duration;
        learned_any_duration
    }

    /// Write the file when it is behind and the last write is old enough. A
    /// temporary file plus a rename, so a kill never leaves half an object.
    pub fn write_if_due(&self) {
        let mut known_durations = self.write_lock();
        if !known_durations.has_unsaved_changes {
            return;
        }
        let current_time = Instant::now();
        if known_durations
            .last_write_time
            .is_some_and(|last_write_time| current_time.duration_since(last_write_time) < WRITE_INTERVAL)
        {
            return;
        }
        if write_durations(&self.cache_path, &known_durations.duration_seconds_by_video_id) {
            known_durations.has_unsaved_changes = false;
            known_durations.last_write_time = Some(current_time);
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, KnownDurations> {
        self.known_durations
            .read()
            .expect("lock is never held across a panic")
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, KnownDurations> {
        self.known_durations
            .write()
            .expect("lock is never held across a panic")
    }
}

/// The cache file, beside the page cache under `~/.cache/redwax`.
pub fn path_for(config: &Config) -> PathBuf {
    match config.page_cache_dir.parent() {
        Some(cache_directory) => cache_directory.join("durations.json"),
        None => PathBuf::from("durations.json"),
    }
}

fn fill_track_duration(duration_seconds_by_video_id: &HashMap<String, u32>, track: &mut Track) -> bool {
    if track.duration_seconds.is_some() {
        return false;
    }
    match duration_seconds_by_video_id.get(&track.video_id) {
        None => false,
        Some(duration_seconds) => {
            track.duration_seconds = Some(*duration_seconds);
            true
        }
    }
}

fn write_durations(
    cache_path: &std::path::Path,
    duration_seconds_by_video_id: &HashMap<String, u32>,
) -> bool {
    let Ok(cache_json) = serde_json::to_string(duration_seconds_by_video_id) else {
        return false;
    };
    crate::private_file::write(cache_path, cache_json.as_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use redwax_ytm::Shelf;

    fn temporary_directory(test_name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("redwax-durations-{}-{test_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the temporary directory is writable");
        directory
    }

    fn track(video_id: &str, duration_seconds: Option<u32>) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: video_id.to_string(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds,
            thumbnail_url: None,
            liked: false,
            playlist_entry: None,
        }
    }

    #[test]
    fn test_learned_lengths_survive_the_round_trip() {
        let cache_path = temporary_directory("round").join("durations.json");
        let durations = Durations::new(cache_path.clone());
        assert!(durations.is_empty());
        assert!(durations.learn(&[("v1".to_string(), 196)]));
        assert!(!durations.learn(&[("v1".to_string(), 196)]));
        durations.write_if_due();

        let reread = Durations::new(cache_path);
        assert_eq!(reread.get("v1"), Some(196));
    }

    #[test]
    fn test_fill_leaves_a_known_duration_alone() {
        let durations = Durations::new(temporary_directory("fill").join("durations.json"));
        durations.learn(&[("v1".to_string(), 196), ("v2".to_string(), 90)]);
        let mut page = Page::new(
            "Home",
            vec![Shelf::new(
                "Listen again",
                vec![
                    Item::Track(track("v1", None)),
                    Item::Track(track("v2", Some(1))),
                    Item::Track(track("v3", None)),
                ],
            )],
        );
        assert!(durations.fill(&mut page));
        let track_durations: Vec<Option<u32>> = page.tracks().map(|track| track.duration_seconds).collect();
        assert_eq!(track_durations, vec![Some(196), Some(1), None]);
        assert!(!durations.fill(&mut page), "nothing left to stamp");
    }

    #[test]
    fn test_an_id_is_asked_about_once_a_run() {
        let durations = Durations::new(temporary_directory("asked").join("durations.json"));
        let wanted_video_ids = vec!["v1".to_string(), "v2".to_string()];
        assert_eq!(durations.take_unasked(wanted_video_ids.clone()), wanted_video_ids);
        assert!(durations.take_unasked(wanted_video_ids).is_empty());
    }

    #[test]
    fn test_a_corrupt_file_reads_as_nothing_remembered() {
        let cache_path = temporary_directory("corrupt").join("durations.json");
        std::fs::write(&cache_path, "{not json").expect("the temporary directory is writable");
        assert!(Durations::new(cache_path).is_empty());
    }
}
