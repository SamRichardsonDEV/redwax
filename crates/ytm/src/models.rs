//! Plain data the UI and player speak. Nothing here knows about YouTube's JSON.
//!
//! One for one with the Python `models.py`, including the JSON shape the page
//! cache writes (`kind` tag per item), so a cache written by either version
//! reads in the other.

use serde::{Deserialize, Serialize};

/// Opaque authority to edit one occurrence inside a playlist.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistEntry(String);

impl PlaylistEntry {
    pub(crate) fn new(entry_token: String) -> PlaylistEntry {
        PlaylistEntry(entry_token)
    }

    pub(crate) fn token(&self) -> &str {
        &self.0
    }

    /// Construct an opaque capability for an in-memory `MusicSource` fixture.
    #[doc(hidden)]
    pub fn synthetic(identity: u64) -> PlaylistEntry {
        PlaylistEntry(format!("redwax-fixture-{identity}"))
    }
}

impl std::fmt::Debug for PlaylistEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PlaylistEntry([redacted])")
    }
}

pub fn format_duration(seconds: Option<u32>) -> String {
    let Some(total_seconds) = seconds else {
        return "--:--".to_string();
    };
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let remaining_seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{remaining_seconds:02}")
    } else {
        format!("{minutes}:{remaining_seconds:02}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_browse_id: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u32>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub liked: bool,
    /// Authority to edit this exact playlist occurrence.
    /// Set only on a playlist the account may edit.
    #[serde(default, rename = "set_video_id")]
    pub playlist_entry: Option<PlaylistEntry>,
}

impl Track {
    pub fn artist_line(&self) -> String {
        if self.artists.is_empty() {
            "Unknown artist".to_string()
        } else {
            self.artists.join(", ")
        }
    }

    pub fn url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
    }

    pub fn duration_text(&self) -> String {
        format_duration(self.duration_seconds)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Album {
    pub browse_id: String,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Playlist {
    pub playlist_id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Artist {
    pub channel_id: String,
    pub name: String,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// One line of lyrics and the window it is sung in, as the mobile client
/// serves it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Timed when YouTube has the line timings, plain when it has the text only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lyrics {
    Plain(String),
    Timed(Vec<LyricLine>),
}

/// Anything that opens as a page of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Browsable {
    Album(Album),
    Playlist(Playlist),
    Artist(Artist),
}

impl Browsable {
    /// Stable key for the page cache and navigation, unique per page.
    pub fn key(&self) -> String {
        match self {
            Browsable::Album(album) => format!("album:{}", album.browse_id),
            Browsable::Playlist(playlist) => format!("playlist:{}", playlist.playlist_id),
            Browsable::Artist(artist) => format!("artist:{}", artist.channel_id),
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Browsable::Album(album) => &album.title,
            Browsable::Playlist(playlist) => &playlist.title,
            Browsable::Artist(artist) => &artist.name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Item {
    Track(Track),
    Album(Album),
    Playlist(Playlist),
    Artist(Artist),
}

impl Item {
    pub fn as_track(&self) -> Option<&Track> {
        match self {
            Item::Track(track) => Some(track),
            _ => None,
        }
    }

    pub fn as_browsable(&self) -> Option<Browsable> {
        match self {
            Item::Track(_) => None,
            Item::Album(album) => Some(Browsable::Album(album.clone())),
            Item::Playlist(playlist) => Some(Browsable::Playlist(playlist.clone())),
            Item::Artist(artist) => Some(Browsable::Artist(artist.clone())),
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Item::Track(track) => &track.title,
            Item::Album(album) => &album.title,
            Item::Playlist(playlist) => &playlist.title,
            Item::Artist(artist) => &artist.name,
        }
    }

    pub fn thumbnail_url(&self) -> Option<&str> {
        match self {
            Item::Track(track) => track.thumbnail_url.as_deref(),
            Item::Album(album) => album.thumbnail_url.as_deref(),
            Item::Playlist(playlist) => playlist.thumbnail_url.as_deref(),
            Item::Artist(artist) => artist.thumbnail_url.as_deref(),
        }
    }
}

/// A titled row of items, the unit YouTube Music lays pages out in.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Shelf {
    pub title: String,
    #[serde(default)]
    pub items: Vec<Item>,
}

impl Shelf {
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Self {
        Shelf {
            title: title.into(),
            items,
        }
    }

    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.items.iter().filter_map(Item::as_track)
    }
}

/// Where a partial page carries on, as one browse request repeats.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Continuation {
    pub browse_id: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Page {
    pub title: String,
    #[serde(default)]
    pub shelves: Vec<Shelf>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Set while items are still to come. Skipped by serde because a live
    /// continuation must not cross the cache boundary.
    #[serde(skip)]
    pub more: Option<Continuation>,
}

impl Page {
    pub fn new(title: impl Into<String>, shelves: Vec<Shelf>) -> Self {
        Page {
            title: title.into(),
            shelves,
            ..Page::default()
        }
    }

    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.shelves.iter().flat_map(Shelf::tracks)
    }

    /// Without an entry identity, removal applies to every occurrence of the video.
    pub fn remove_playlist_track(&mut self, video_id: &str, playlist_entry: Option<&PlaylistEntry>) -> bool {
        let mut removed = false;
        for shelf in &mut self.shelves {
            let previous_count = shelf.items.len();
            shelf.items.retain(|item| {
                !item.as_track().is_some_and(|track| {
                    track.video_id == video_id
                        && playlist_entry.is_none_or(|entry| track.playlist_entry.as_ref() == Some(entry))
                })
            });
            removed |= shelf.items.len() != previous_count;
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.shelves.iter().all(|shelf| shelf.items.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_format_like_python() {
        assert_eq!(format_duration(None), "--:--");
        assert_eq!(format_duration(Some(259)), "4:19");
        assert_eq!(format_duration(Some(3723)), "1:02:03");
    }

    #[test]
    fn items_round_trip_with_kind_tags() {
        let item = Item::Track(Track {
            video_id: "x".into(),
            title: "T".into(),
            artists: vec!["A".into()],
            ..Track {
                video_id: String::new(),
                title: String::new(),
                artists: vec![],
                album: None,
                album_browse_id: None,
                duration_seconds: None,
                thumbnail_url: None,
                liked: false,
                playlist_entry: None,
            }
        });
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"kind\":\"track\""));
        assert_eq!(serde_json::from_str::<Item>(&json).unwrap(), item);
    }

    #[test]
    fn playlist_entry_keeps_the_legacy_cache_key_and_redacts_debug() {
        let entry = PlaylistEntry::new("private-row-token".to_owned());
        let track = Track {
            video_id: "x".into(),
            title: "Track".into(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: Some(entry),
        };

        let value = serde_json::to_value(&track).expect("the track serializes");
        assert_eq!(value["set_video_id"], "private-row-token");
        assert!(value.get("playlist_entry").is_none());
        assert!(!format!("{track:?}").contains("private-row-token"));
    }

    #[test]
    fn page_removal_distinguishes_occurrences_and_keeps_other_videos() {
        let occurrence = |video_id: &str, identity: u64| {
            Item::Track(Track {
                video_id: video_id.to_owned(),
                title: video_id.to_owned(),
                artists: Vec::new(),
                album: None,
                album_browse_id: None,
                duration_seconds: None,
                thumbnail_url: None,
                liked: false,
                playlist_entry: Some(PlaylistEntry::synthetic(identity)),
            })
        };
        let mut page = Page::new(
            "Playlist",
            vec![
                Shelf::new("First", vec![occurrence("one", 1), occurrence("one", 2)]),
                Shelf::new("Second", vec![occurrence("two", 3), occurrence("one", 4)]),
            ],
        );
        assert!(page.remove_playlist_track("one", Some(&PlaylistEntry::synthetic(2))));
        assert_eq!(
            page.tracks()
                .map(|track| track.playlist_entry.clone().unwrap())
                .collect::<Vec<_>>(),
            [
                PlaylistEntry::synthetic(1),
                PlaylistEntry::synthetic(3),
                PlaylistEntry::synthetic(4),
            ]
        );
        assert!(!page.remove_playlist_track("one", Some(&PlaylistEntry::synthetic(2))));
        assert!(page.remove_playlist_track("one", None));
        assert_eq!(
            page.tracks()
                .map(|track| track.video_id.as_str())
                .collect::<Vec<_>>(),
            ["two"]
        );
    }
}
