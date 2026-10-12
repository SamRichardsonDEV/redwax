//! The alarm for a YouTube layout change, run by `cargo test -- --ignored`.
//!
//! Every test here asserts a minimum count, because the walker's failure mode
//! is silence. They read the session `redwax login` saved and touch the real
//! account, so they never run in a normal `cargo test`.

use std::path::PathBuf;
use std::time::Instant;

use crate::{
    Album, Browsable, Client, ClientOptions, Item, Lyrics, MusicSource, PagePagination, Playlist, Session,
    Track,
};

fn client() -> Client {
    let home = std::env::var("HOME").expect("HOME is set");
    let path = PathBuf::from(&home).join(".config/redwax/browser.json");
    let session = Session::read(&path).expect("browser.json is readable");
    assert!(
        session.is_some(),
        "no session at {}, run `redwax login`",
        path.display()
    );
    Client::new(ClientOptions {
        session,
        dump_dir: PathBuf::from(&home).join(".cache/redwax/dumps"),
        home_rows: 6,
    })
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_whoami_names_the_account() {
    let who = client().whoami().await.expect("identity request succeeds");
    assert!(who.is_some(), "whoami came back empty");
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_home_has_rows() {
    let client = client();
    let started = Instant::now();
    let page = client.home().await.expect("home");
    let elapsed = started.elapsed();
    println!(
        "home(): {:.3} s, {} shelves",
        elapsed.as_secs_f64(),
        page.shelves.len()
    );
    assert!(page.shelves.len() >= 4, "{} shelves", page.shelves.len());
    for shelf in &page.shelves {
        assert!(!shelf.title.is_empty(), "untitled shelf");
        assert!(!shelf.items.is_empty(), "empty shelf {}", shelf.title);
    }
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_search_finds_songs() {
    let page = client().search("oasis wonderwall").await.expect("search");
    assert_eq!(page.title, "Search: oasis wonderwall");
    assert!(page.shelves.len() >= 2, "{} shelves", page.shelves.len());
    assert!(page.tracks().count() >= 3, "{} tracks", page.tracks().count());
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_album_has_its_tracks() {
    let album = Album {
        browse_id: "MPREb_PITqkpE6ExP".to_string(),
        title: "(What's The Story) Morning Glory?".to_string(),
        artists: vec!["Oasis".to_string()],
        year: None,
        thumbnail_url: None,
    };
    let page = client().browse(&Browsable::Album(album)).await.expect("album");
    let tracks: Vec<_> = page.tracks().collect();
    assert!(tracks.len() >= 12, "{} tracks", tracks.len());
    assert!(tracks.iter().all(|track| track.album_browse_id.is_some()));
    assert!(tracks.iter().all(|track| track.thumbnail_url.is_some()));
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_playlist_has_its_tracks() {
    let playlist = Playlist {
        playlist_id: "RDCLAK5uy_nvRGXHpFzon8qbPTZhuoSq3WO4ccYvmHM".to_string(),
        title: "Electronic Focus".to_string(),
        subtitle: None,
        thumbnail_url: None,
    };
    let page = client()
        .browse(&Browsable::Playlist(playlist))
        .await
        .expect("playlist");
    assert!(page.tracks().count() >= 20, "{} tracks", page.tracks().count());
}

/// Use a public playlist with more than 200 tracks to exercise continuation requests.
#[tokio::test]
#[ignore = "touches the live account"]
async fn live_playlist_pages_past_the_first_response() {
    let playlist = Playlist {
        playlist_id: std::env::var("REDWAX_TEST_PLAYLIST")
            .expect("set REDWAX_TEST_PLAYLIST to a public playlist with more than 200 tracks"),
        title: "Pagination test".to_string(),
        subtitle: None,
        thumbnail_url: None,
    };
    let client = client();
    let started = Instant::now();
    let page = client
        .browse(&Browsable::Playlist(playlist))
        .await
        .expect("playlist");
    let first = page.tracks().count();
    println!(
        "browse(): {:.3} s, {first} tracks",
        started.elapsed().as_secs_f64()
    );
    assert!(page.more.is_some(), "the test playlist has no continuation");

    let mut total = first;
    let mut pagination = PagePagination::new(page.more.clone());
    let mut batches = 0;
    while let Some(continuation) = pagination.next().cloned() {
        let (items, next) = client.continue_page(&continuation).await.expect("continuation");
        pagination
            .advance(&items, next)
            .expect("the continuation makes valid progress");
        total += items.iter().filter_map(Item::as_track).count();
        batches += 1;
        assert!(batches < 20, "the follow did not end, {total} tracks");
    }
    println!(
        "continue_page(): {batches} batches, {total} tracks in {:.3} s",
        started.elapsed().as_secs_f64()
    );
    assert!(total > 200, "{total} tracks");
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_library_lists_the_account() {
    let page = client().library().await.expect("library");
    assert_eq!(page.subtitle, None, "an endpoint failed");
    assert!(page.shelves.len() >= 3, "{} shelves", page.shelves.len());
    assert_eq!(page.shelves[0].title, "Collections");
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_radio_and_lyrics_answer_for_a_known_track() {
    let client = client();
    let track = Track {
        video_id: "1-Fxb9jsOuI".to_string(),
        title: "Suffocation".to_string(),
        artists: vec!["Crystal Castles".to_string()],
        album: None,
        album_browse_id: None,
        duration_seconds: None,
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    };
    let radio = client.radio(&track).await.expect("radio");
    assert!(radio.len() >= 10, "{} tracks", radio.len());
    assert_eq!(radio[0].video_id, track.video_id);

    let lyrics = client.lyrics(&track).await.expect("lyrics");
    assert!(lyrics.is_some_and(|text| !text.trim().is_empty()), "no lyrics");
}

/// A track with only its id, which is all the lyrics path reads.
fn track(video_id: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: String::new(),
        artists: vec![],
        album: None,
        album_browse_id: None,
        duration_seconds: None,
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_timed_lyrics() {
    let client = client();
    let found = client
        .timed_lyrics(&track("1-Fxb9jsOuI"))
        .await
        .expect("timed lyrics");
    let Some(Lyrics::Timed(lines)) = found else {
        panic!("no timings, got {found:?}");
    };
    println!(
        "live_timed_lyrics: {} lines, first {:?}",
        lines.len(),
        lines.first()
    );
    assert!(lines.len() >= 20, "{} lines", lines.len());
    assert!(lines.iter().all(|line| line.end_ms >= line.start_ms));
    assert!(lines.iter().any(|line| !line.text.trim().is_empty()));
    assert!(lines.windows(2).all(|pair| pair[0].start_ms <= pair[1].start_ms));

    let none = client
        .timed_lyrics(&track("EPMql3dljqU"))
        .await
        .expect("no lyrics tab");
    assert_eq!(none, None, "a track with no lyrics answered");
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_accounts_lists_an_identity() {
    let accounts = client().accounts().await.expect("accounts");
    assert!(!accounts.is_empty(), "no identity");
    assert!(accounts.iter().all(|account| !account.name.is_empty()));
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_signed_out_search_still_answers() {
    // The signed out path needs the visitor id YouTube stamps into its own page.
    let client = Client::new(ClientOptions {
        session: None,
        dump_dir: std::env::temp_dir(),
        home_rows: 6,
    });
    assert!(!client.signed_in());
    assert_eq!(
        client.whoami().await.expect("signed out is ordinary absence"),
        None
    );
    let page = client.search("oasis wonderwall").await.expect("search");
    assert!(page.tracks().count() >= 3, "{} tracks", page.tracks().count());
}

/// A playlist of its own for the edit tests, deleted at the end of each one.
/// Nothing here ever names a playlist the account already had.
async fn scratch_playlist(client: &Client) -> String {
    let response = client
        .transport
        .post(
            "playlist/create",
            serde_json::json!({
                "title": format!("redwax scratch {}", std::process::id()),
                "description": "made and deleted by the redwax live tests",
                "privacyStatus": "PRIVATE",
            }),
        )
        .await
        .expect("the account can make a playlist");
    response
        .get("playlistId")
        .and_then(serde_json::Value::as_str)
        .expect("the create answered with an id")
        .to_string()
}

async fn delete_playlist(client: &Client, playlist_id: &str) {
    let _ = client
        .transport
        .post(
            "playlist/delete",
            serde_json::json!({ "playlistId": playlist_id }),
        )
        .await;
}

#[tokio::test]
#[ignore = "touches the live account"]
async fn live_a_track_is_added_to_a_playlist_and_taken_out_again() {
    let client = client();
    let playlist_id = scratch_playlist(&client).await;
    let video_id = "1-Fxb9jsOuI".to_string();

    let added = client
        .add_to_playlist(&playlist_id, std::slice::from_ref(&video_id))
        .await;
    if added.is_err() {
        delete_playlist(&client, &playlist_id).await;
    }
    added.expect("the add was accepted");

    let opened = Browsable::Playlist(Playlist {
        playlist_id: playlist_id.clone(),
        title: String::new(),
        subtitle: None,
        thumbnail_url: None,
    });
    let page = client.browse(&opened).await.expect("the playlist reads back");
    let row = page.tracks().find(|track| track.video_id == video_id).cloned();
    let Some(row) = row else {
        delete_playlist(&client, &playlist_id).await;
        panic!("the track was not in the playlist after the add");
    };
    let entry = row
        .playlist_entry
        .clone()
        .expect("an owned playlist row carries edit authority");

    let removed = client
        .remove_playlist_entry(&playlist_id, &video_id, &entry)
        .await;
    let after = client.browse(&opened).await.map(|page| page.tracks().count());
    delete_playlist(&client, &playlist_id).await;

    removed.expect("the removal was accepted");
    assert_eq!(
        after.expect("the playlist reads back"),
        0,
        "the row is still there"
    );
}
