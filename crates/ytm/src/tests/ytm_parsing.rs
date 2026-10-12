//! The Python `tests/test_ytm_parsing.py` suite, ported name for name.
//!
//! Python asserted against ytmusicapi's parsed dicts. The Rust walker reads the
//! raw response, so each case is written as the InnerTube shape those dicts
//! were parsed from. `test_album_track_inherits_album_context` lives beside the
//! page assembly it tests, in the crate's own test module.

use crate::{Album, Artist, Item, Playlist, Track, walker};
use serde_json::{Value, json};

/// The row art Google serves at 60 pixels and resizes on request.
fn thumbnails() -> Value {
    json!([{ "url": "https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", "width": 60, "height": 60 }])
}

fn column(runs: Value) -> Value {
    json!({ "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": runs } } })
}

fn linked(text: &str, browse_id: &str) -> Value {
    json!({ "text": text, "navigationEndpoint": { "browseEndpoint": { "browseId": browse_id } } })
}

fn dot() -> Value {
    json!({ "text": " \u{2022} " })
}

#[test]
fn test_search_song_becomes_track_with_hires_art() {
    let raw = json!({
        "musicResponsiveListItemRenderer": {
            "flexColumns": [
                column(json!([{
                    "text": "Wonderwall",
                    "navigationEndpoint": { "watchEndpoint": { "videoId": "ZrOKjDZOtkA" } },
                }])),
                column(json!([
                    linked("Oasis", "UC1"),
                    dot(),
                    linked("Morning Glory", "MPREb_9"),
                    dot(),
                    { "text": "4:19" },
                ])),
            ],
            "menu": { "menuRenderer": {
                "topLevelButtons": [{ "likeButtonRenderer": { "likeStatus": "LIKE" } }],
            } },
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": thumbnails() } } },
        }
    });

    let item = walker::item(&raw).expect("a song row");
    assert_eq!(
        item,
        Item::Track(Track {
            video_id: "ZrOKjDZOtkA".to_string(),
            title: "Wonderwall".to_string(),
            artists: vec!["Oasis".to_string()],
            album: Some("Morning Glory".to_string()),
            album_browse_id: Some("MPREb_9".to_string()),
            duration_seconds: Some(259),
            thumbnail_url: Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj".to_string()),
            liked: true,
            playlist_entry: None,
        })
    );
    let track = item.as_track().expect("a track");
    assert_eq!(track.url(), "https://music.youtube.com/watch?v=ZrOKjDZOtkA");
    assert_eq!(track.duration_text(), "4:19");
}

#[test]
fn test_watch_playlist_track_uses_length_text_and_single_thumbnail_key() {
    let raw = json!({
        "playlistPanelVideoRenderer": {
            "videoId": "x",
            "title": { "runs": [{ "text": "T" }] },
            "lengthText": { "runs": [{ "text": "1:02:03" }] },
            "thumbnail": { "thumbnails": thumbnails() },
            "longBylineText": { "runs": [linked("A", "UC3")] },
        }
    });

    let track = walker::item(&raw)
        .and_then(|item| item.as_track().cloned())
        .expect("a queue row");
    assert_eq!(track.title, "T");
    assert_eq!(track.artists, vec!["A".to_string()]);
    assert_eq!(track.duration_seconds, Some(3723));
    assert_eq!(track.duration_text(), "1:02:03");
    assert!(
        track
            .thumbnail_url
            .as_deref()
            .is_some_and(|url| url.ends_with("=w544-h544-l90-rj")),
        "{:?}",
        track.thumbnail_url
    );
}

#[test]
fn test_home_items_are_inferred_without_result_type() {
    let album = walker::item(&json!({
        "musicTwoRowItemRenderer": {
            "title": { "runs": [linked("Sentiment", "MPREb_Q")] },
            "subtitle": { "runs": [{ "text": "Artist Name" }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": {
                "thumbnail": { "thumbnails": thumbnails() },
            } },
        }
    }));
    assert_eq!(
        album,
        Some(Item::Album(Album {
            browse_id: "MPREb_Q".to_string(),
            title: "Sentiment".to_string(),
            artists: vec!["Artist Name".to_string()],
            year: None,
            thumbnail_url: Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj".to_string()),
        }))
    );

    let playlist = walker::item(&json!({
        "musicTwoRowItemRenderer": {
            "title": { "runs": [{
                "text": "Top",
                "navigationEndpoint": { "watchEndpoint": { "playlistId": "PLz7" } },
            }] },
            "subtitle": { "runs": [{ "text": "redditEDM \u{2022} 161 songs" }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": {
                "thumbnail": { "thumbnails": thumbnails() },
            } },
        }
    }));
    assert_eq!(
        playlist,
        Some(Item::Playlist(Playlist {
            playlist_id: "PLz7".to_string(),
            title: "Top".to_string(),
            subtitle: Some("redditEDM \u{2022} 161 songs".to_string()),
            thumbnail_url: Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj".to_string()),
        }))
    );

    let artist = walker::item(&json!({
        "musicTwoRowItemRenderer": {
            "title": { "runs": [linked("Rammstein", "UCabc")] },
            "subtitle": { "runs": [{ "text": "3M subscribers" }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": {
                "thumbnail": { "thumbnails": thumbnails() },
            } },
        }
    }));
    assert_eq!(
        artist,
        Some(Item::Artist(Artist {
            channel_id: "UCabc".to_string(),
            name: "Rammstein".to_string(),
            thumbnail_url: Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj".to_string()),
        }))
    );
}

#[test]
fn test_search_playlist_strips_vl_prefix_and_ignores_podcasts() {
    let playlist = walker::item(&json!({
        "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPL123" } },
            "flexColumns": [
                column(json!([{ "text": "Mix" }])),
                column(json!([
                    { "text": "Playlist" }, dot(), { "text": "YouTube Music" }, dot(),
                    { "text": "40 songs" },
                ])),
            ],
        }
    }));
    assert_eq!(
        playlist,
        Some(Item::Playlist(Playlist {
            playlist_id: "PL123".to_string(),
            title: "Mix".to_string(),
            subtitle: Some("Playlist \u{2022} YouTube Music \u{2022} 40 songs".to_string()),
            thumbnail_url: None,
        }))
    );

    let podcast = walker::item(&json!({
        "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "browseEndpoint": { "browseId": "MPSP1" } },
            "flexColumns": [
                column(json!([{ "text": "Pod" }])),
                column(json!([{ "text": "Podcast" }])),
            ],
        }
    }));
    assert_eq!(podcast, None);
}

#[test]
fn test_library_artist_uses_artist_key_for_name() {
    // The library page prefixes the channel id, and the channel id is what
    // opens the artist page.
    let artist = walker::item(&json!({
        "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "browseEndpoint": { "browseId": "MPLAUC9" } },
            "flexColumns": [
                column(json!([{ "text": "Oasis" }])),
                column(json!([{ "text": "1M subscribers" }])),
            ],
        }
    }));
    assert_eq!(
        artist,
        Some(Item::Artist(Artist {
            channel_id: "UC9".to_string(),
            name: "Oasis".to_string(),
            thumbnail_url: None,
        }))
    );
}

#[test]
fn test_track_without_video_id_is_dropped() {
    let unavailable = walker::item(&json!({
        "musicResponsiveListItemRenderer": {
            "flexColumns": [
                column(json!([{ "text": "Unavailable" }])),
                column(json!([{ "text": "Song" }])),
            ],
        }
    }));
    assert_eq!(unavailable, None);
}
