//! Complete pages cached for immediate display. Missing or corrupt files are cache misses.
//! SHA-1 filenames and item `kind` tags preserve the existing cache format.

use std::path::PathBuf;

use redwax_ytm::{CompletePage, Page};
use sha1::{Digest, Sha1};

pub struct PageCache {
    cache_directory: PathBuf,
}

impl PageCache {
    pub fn new(cache_directory: PathBuf) -> PageCache {
        let _ = crate::private_file::prepare_directory(&cache_directory);
        PageCache { cache_directory }
    }

    pub fn get(&self, cache_key: &str) -> Option<Page> {
        let page_json = std::fs::read_to_string(self.cache_path(cache_key)).ok()?;
        serde_json::from_str(&page_json).ok()
    }

    pub fn put(&self, cache_key: &str, page: &CompletePage) {
        let Ok(page_json) = serde_json::to_string(page.as_page()) else {
            return;
        };
        let _ = crate::private_file::write(&self.cache_path(cache_key), page_json.as_bytes());
    }

    pub(crate) fn restore_track_metadata(&self, queued_tracks: &mut [redwax_ytm::Track]) {
        if !queued_tracks
            .iter()
            .any(|track| track.title.starts_with("https://"))
        {
            return;
        }
        let Ok(cache_entries) = std::fs::read_dir(&self.cache_directory) else {
            return;
        };
        for cache_entry in cache_entries.flatten() {
            let Ok(page_json) = std::fs::read_to_string(cache_entry.path()) else {
                continue;
            };
            let Ok(cached_page) = serde_json::from_str::<Page>(&page_json) else {
                continue;
            };
            for cached_track in cached_page.tracks() {
                if cached_track.title.starts_with("https://") || cached_track.artists.is_empty() {
                    continue;
                }
                for queued_track in queued_tracks.iter_mut() {
                    if queued_track.video_id == cached_track.video_id
                        && queued_track.title.starts_with("https://")
                    {
                        let playlist_entry = queued_track.playlist_entry.clone();
                        let queued_duration = queued_track.duration_seconds;
                        *queued_track = cached_track.clone();
                        queued_track.playlist_entry = playlist_entry;
                        queued_track.duration_seconds = queued_track.duration_seconds.or(queued_duration);
                    }
                }
            }
            if !queued_tracks
                .iter()
                .any(|track| track.title.starts_with("https://"))
            {
                break;
            }
        }
    }

    fn cache_path(&self, cache_key: &str) -> PathBuf {
        let key_digest = Sha1::digest(cache_key.as_bytes());
        self.cache_directory
            .join(format!("{}.json", hex::encode(key_digest)))
    }
}

pub fn search_key(query: &str) -> String {
    format!("search:{}", query.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use redwax_ytm::{Item, Shelf, Track};

    fn temporary_directory(test_name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("redwax-pages-{}-{test_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn test_page_survives_the_round_trip() {
        let page_cache = PageCache::new(temporary_directory("round"));
        let cached_track = Track {
            video_id: "v1".into(),
            title: "T".into(),
            artists: vec!["A".into()],
            album: Some("Al".into()),
            album_browse_id: None,
            duration_seconds: Some(61),
            thumbnail_url: None,
            liked: true,
            playlist_entry: None,
        };
        let cached_page = Page::new("Home", vec![Shelf::new("Songs", vec![Item::Track(cached_track)])]);
        page_cache.put("home", &CompletePage::try_from(cached_page.clone()).unwrap());
        assert_eq!(page_cache.get("home"), Some(cached_page));
        assert_eq!(page_cache.get("missing"), None);
    }

    #[test]
    fn test_corrupt_entry_reads_as_a_miss() {
        let page_cache = PageCache::new(temporary_directory("corrupt"));
        std::fs::create_dir_all(&page_cache.cache_directory).unwrap();
        std::fs::write(page_cache.cache_path("home"), "{not json").unwrap();
        assert_eq!(page_cache.get("home"), None);
    }

    #[test]
    fn a_partial_page_cannot_replace_a_complete_cached_page() {
        let page_cache = PageCache::new(temporary_directory("partial"));
        let complete_page = Page::new("Complete", Vec::new());
        page_cache.put("home", &CompletePage::try_from(complete_page.clone()).unwrap());
        let partial_page = Page {
            more: Some(redwax_ytm::Continuation {
                browse_id: "fixture".into(),
                token: "next".into(),
            }),
            ..Page::new("Partial", Vec::new())
        };

        assert!(CompletePage::try_from(partial_page).is_err());
        assert_eq!(page_cache.get("home"), Some(complete_page));
        std::fs::remove_dir_all(&page_cache.cache_directory).unwrap();
    }
}
