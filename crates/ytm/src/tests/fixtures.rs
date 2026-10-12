//! Every page type parsed from a captured response, never from the network.
//!
//! The facts asserted here are the ones the UI shows, so a layout change that
//! the walker survives structurally but reads wrongly still fails a test.

use crate::test_common::{fixture, tracks_in};
use crate::{Item, walker};

#[test]
fn home_gives_titled_shelves_of_items() {
    let shelves: Vec<_> = walker::shelves(&fixture("home.json"), "For you")
        .into_iter()
        .filter(|shelf| !shelf.items.is_empty())
        .collect();
    assert!(shelves.len() >= 3, "{} shelves", shelves.len());
    for shelf in &shelves {
        assert!(!shelf.title.is_empty(), "untitled shelf");
        assert!(!shelf.items.is_empty());
    }
    let titles: Vec<&str> = shelves.iter().map(|shelf| shelf.title.as_str()).collect();
    assert!(titles.contains(&"Listen again"), "{titles:?}");
    assert!(titles.contains(&"Covers and remixes"), "{titles:?}");
}

#[test]
fn home_continuation_adds_more_rows() {
    let shelves: Vec<_> = walker::shelves(&fixture("home_continuation.json"), "For you")
        .into_iter()
        .filter(|shelf| !shelf.items.is_empty())
        .collect();
    assert!(shelves.len() >= 3, "{} shelves", shelves.len());
    assert!(shelves.iter().all(|shelf| !shelf.title.is_empty()));
}

#[test]
fn search_finds_wonderwall_by_oasis_with_hires_art() {
    let items = walker::items(&fixture("search.json"));
    let track = items
        .iter()
        .filter_map(Item::as_track)
        .find(|track| track.video_id == "hpSrLjc5SMs")
        .expect("the Wonderwall song row");
    assert_eq!(track.title, "Wonderwall");
    assert_eq!(track.artists, vec!["Oasis".to_string()]);
    let art = track.thumbnail_url.as_deref().expect("row art");
    assert!(art.contains("=w544-h544"), "{art}");

    // Podcasts, episodes and profiles never reach the UI.
    assert!(
        items
            .iter()
            .all(|item| !item.title().contains("Wonderwall review clip"))
    );
    assert!(items.iter().all(|item| item.title() != "wonderwall oasis"));
}

#[test]
fn search_songs_is_only_tracks_and_carries_the_album() {
    let items = walker::items(&fixture("search_songs.json"));
    assert_eq!(items.len(), 20);
    assert!(
        items.iter().all(|item| item.as_track().is_some()),
        "not all tracks"
    );

    let first = items[0].as_track().expect("first is a track");
    assert_eq!(first.title, "Wonderwall");
    assert_eq!(first.artists, vec!["Oasis".to_string()]);
    assert_eq!(first.album.as_deref(), Some("(What's The Story) Morning Glory?"));
    assert_eq!(first.album_browse_id.as_deref(), Some("MPREb_PITqkpE6ExP"));
    assert_eq!(first.duration_seconds, Some(259));
    assert!(
        first
            .thumbnail_url
            .as_deref()
            .is_some_and(|url| url.contains("=w544-h544")),
        "{:?}",
        first.thumbnail_url
    );
}

#[test]
fn playlist_gives_its_tracks() {
    let tracks = tracks_in("playlist.json");
    assert!(tracks.len() >= 20, "{} tracks", tracks.len());
    assert_eq!(tracks[0].title, "Anvil");
    assert_eq!(tracks[0].artists, vec!["Lorn".to_string()]);
    assert_eq!(tracks[0].duration_seconds, Some(210));
}

/// A removal needs opaque edit authority for every track a playlist page serves.
#[test]
fn playlist_rows_carry_redacted_edit_authority() {
    let tracks = tracks_in("playlist.json");
    let with_ids = tracks
        .iter()
        .filter(|track| track.playlist_entry.is_some())
        .count();
    assert_eq!(
        with_ids,
        tracks.len(),
        "{with_ids} of {} rows carry edit authority",
        tracks.len()
    );
    assert_eq!(
        format!(
            "{:?}",
            tracks[0].playlist_entry.as_ref().expect("the first row has one")
        ),
        "PlaylistEntry([redacted])"
    );
}

#[test]
fn artist_gives_several_shelves() {
    let shelves: Vec<_> = walker::shelves(&fixture("artist.json"), "Songs")
        .into_iter()
        .filter(|shelf| !shelf.items.is_empty())
        .collect();
    assert!(shelves.len() >= 4, "{} shelves", shelves.len());
    assert_eq!(shelves[0].title, "Top songs");
    assert!(shelves[0].tracks().next().is_some());
    let albums = shelves
        .iter()
        .find(|shelf| shelf.title == "Albums")
        .expect("an Albums shelf");
    assert!(albums.items.iter().all(|item| matches!(item, Item::Album(_))));
}

#[test]
fn library_pages_parse_to_the_kinds_they_hold() {
    let playlists = walker::items(&fixture("library_playlists.json"));
    assert!(!playlists.is_empty());
    assert!(playlists.iter().all(|item| matches!(item, Item::Playlist(_))));
    let liked = playlists[0].as_browsable().expect("a browsable playlist");
    assert_eq!(
        liked.key(),
        format!("playlist:{}", crate::LIKED_SONGS_PLAYLIST_ID)
    );

    let artists = walker::items(&fixture("library_artists.json"));
    assert_eq!(artists.len(), 25);
    assert!(artists.iter().all(|item| matches!(item, Item::Artist(_))));
    assert_eq!(artists[0].title(), "akiaura");

    // An empty library page carries a message, not items.
    assert!(walker::items(&fixture("library_albums.json")).is_empty());
}

#[test]
fn history_is_a_shelf_of_tracks_per_period() {
    let shelves: Vec<_> = walker::shelves(&fixture("history.json"), "History")
        .into_iter()
        .filter(|shelf| !shelf.items.is_empty())
        .collect();
    assert_eq!(shelves.len(), 1);
    assert_eq!(shelves[0].title, "Today");
    assert_eq!(shelves[0].items.len(), 10);
    assert!(shelves[0].items.iter().all(|item| item.as_track().is_some()));
}

#[test]
fn watch_playlist_starts_with_the_seed_track() {
    let tracks: Vec<_> = walker::items(&fixture("watch_playlist.json"))
        .into_iter()
        .filter(|item| item.as_track().is_some())
        .collect();
    assert!(tracks.len() >= 20, "{} tracks", tracks.len());
    let first = tracks[0].as_track().expect("filtered to tracks");
    assert_eq!(first.video_id, "1-Fxb9jsOuI");
    assert_eq!(first.title, "Suffocation");
    assert_eq!(first.artists, vec!["Crystal Castles".to_string()]);
    assert_eq!(first.duration_seconds, Some(241));
}

#[test]
fn account_menu_gives_the_signed_in_identity() {
    let accounts = crate::accounts_in(&fixture("account_menu.json"), 1);
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "Test Account");
    assert_eq!(accounts[0].handle.as_deref(), Some("@testaccount"));
    assert_eq!(accounts[0].authuser, 1);
    assert_eq!(accounts[0].label(), "Test Account @testaccount");
    assert!(accounts[0].matches("testaccount"));
    assert!(accounts[0].matches("Test Account"));
}
