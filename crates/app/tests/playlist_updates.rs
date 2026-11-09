mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use redwax::app::{AppEvent, Browser};
use redwax_ytm::{Browsable, CompletePage, Item, Page, PlaylistEntry, Shelf, Track};
use support::{FakeSource, Harness, Setup, album, playlist, tracks};
use tokio::sync::Semaphore;

fn playlist_page() -> Page {
    Page::new(
        "Late night",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[0].clone())])],
    )
}

fn open_destination(browser: &mut Browser) {
    browser.show_from(playlist_page(), Some(Browsable::Playlist(playlist())));
    let row = browser
        .rows()
        .iter()
        .position(|row| matches!(row, redwax::ui::Row::Item(0, 0)));
    browser.select_row(row);
}

fn confirm_addition(app: &mut Harness, tracks: Vec<Track>) {
    let generation = app.app.deps.session_generation.load(Ordering::SeqCst);
    app.app.handle_event(AppEvent::PlaylistTracksAdded {
        generation,
        playlist: playlist(),
        tracks,
    });
}

async fn next_refresh(app: &mut Harness) -> AppEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = app.events.recv().await.expect("the event channel stays open");
            if matches!(event, AppEvent::PlaylistRefreshed { .. }) {
                return event;
            }
            app.app.handle_event(event);
        }
    })
    .await
    .expect("the synthetic playlist read completes")
}

#[tokio::test]
async fn picker_additions_refresh_the_open_playlist_and_cache_without_moving_the_cursor() {
    let mut setup = Setup::new("playlist-picker-refresh");
    setup.genre = Some("Ambient".into());
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".into(), vec![tracks()[0].clone()]);
    let mut app = setup.build();
    app.app.start_background();
    app.settle().await;
    app.playing_queue(tracks(), 1);
    open_destination(&mut app.app.home);
    let depth = app.app.home.depth();
    let cursor = app.app.home.cursor();
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(playlist_page()).unwrap());

    app.press_all(&["P", "enter"]).await;

    assert!(app.source.called("add_to_playlist:PL1:v2"));
    let visible = app.app.home.top().unwrap();
    assert_eq!(
        visible
            .tracks()
            .map(|track| track.video_id.as_str())
            .collect::<Vec<_>>(),
        ["v1", "v2"]
    );
    assert_eq!(app.app.home.depth(), depth);
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.cached("playlist:PL1"), Some(visible.clone()));
    assert_eq!(
        visible.tracks().nth(1).unwrap().playlist_entry,
        Some(PlaylistEntry::synthetic(2))
    );
}

#[tokio::test]
async fn confirmed_additions_appear_before_the_server_read_and_never_borrow_another_playlists_entry_id() {
    let mut setup = Setup::new("playlist-confirmed-immediate");
    let gate = Arc::new(Semaphore::new(0));
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    open_destination(&mut app.app.home);
    open_destination(&mut app.app.search);
    app.app.search.show_from(
        Page::new("Unrelated album", Vec::new()),
        Some(Browsable::Album(album())),
    );
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(playlist_page()).unwrap());
    let depth = app.app.home.depth();
    let cursor = app.app.home.cursor();
    let mut added = tracks()[1].clone();
    added.playlist_entry = Some(PlaylistEntry::synthetic(900));

    confirm_addition(&mut app, vec![added]);

    let visible = app.app.home.top().unwrap();
    assert_eq!(visible.tracks().count(), 2);
    assert_eq!(visible.tracks().nth(1).unwrap().playlist_entry, None);
    assert_eq!(app.cached("playlist:PL1"), Some(visible.clone()));
    assert_eq!(app.app.home.depth(), depth);
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.app.search.top().unwrap().title, "Unrelated album");
    assert_eq!(app.app.search.pages().rev().nth(1).unwrap().tracks().count(), 2);

    app.settle().await;
    assert_eq!(app.app.home.top().unwrap().tracks().count(), 2);
    gate.add_permits(1);
    app.settle().await;
    assert_eq!(
        app.app
            .home
            .top()
            .unwrap()
            .tracks()
            .nth(1)
            .unwrap()
            .playlist_entry,
        Some(PlaylistEntry::synthetic(2))
    );
}

#[tokio::test]
async fn an_older_refresh_cannot_drop_a_second_confirmed_addition() {
    let setup = Setup::new("playlist-refresh-order");
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".into(), tracks()[..2].to_vec());
    let mut app = setup.build();
    open_destination(&mut app.app.home);
    confirm_addition(&mut app, vec![tracks()[1].clone()]);
    let older_refresh = next_refresh(&mut app).await;

    confirm_addition(&mut app, vec![tracks()[2].clone()]);
    app.app.handle_event(older_refresh);

    assert_eq!(app.app.home.top().unwrap().tracks().count(), 3);
    assert_eq!(
        app.app.home.top().unwrap().tracks().last().unwrap().video_id,
        "v3"
    );
}

#[tokio::test]
async fn failed_writes_leave_the_playlist_and_cache_unchanged() {
    let mut setup = Setup::new("playlist-failed-addition");
    setup.failing_edits();
    setup.genre = Some("Ambient".into());
    let mut app = setup.build();
    app.app.start_background();
    app.settle().await;
    app.playing_queue(tracks(), 1);
    open_destination(&mut app.app.home);
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(playlist_page()).unwrap());

    app.press_all(&["P", "enter"]).await;

    assert!(app.source.called("add_to_playlist:PL1:v2"));
    assert_eq!(app.app.home.top(), Some(&playlist_page()));
    assert_eq!(app.cached("playlist:PL1"), Some(playlist_page()));
    assert!(!app.source.called("browse:playlist:PL1"));
}

#[tokio::test]
async fn album_additions_refresh_every_saved_song() {
    let setup = Setup::new("playlist-album-addition");
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".into(), Vec::new());
    let mut app = setup.build();
    app.app.start_background();
    app.settle().await;
    open_destination(&mut app.app.search);
    app.app.home.replace(Page::new(
        "Albums",
        vec![Shelf::new("Albums", vec![Item::Album(album())])],
    ));

    app.press_all(&["ctrl+p", "enter"]).await;

    assert!(app.source.called("add_to_playlist:PL1:v1,v2,v3"));
    assert_eq!(app.app.search.top().unwrap().tracks().count(), 3);
    assert_eq!(app.cached("playlist:PL1"), app.app.search.top().cloned());
    assert_eq!(app.app.home.top().unwrap().title, "Albums");
}

#[tokio::test]
async fn late_additions_from_the_previous_account_cannot_restore_its_playlist() {
    let mut app = Setup::new("playlist-addition-account-scope").build();
    open_destination(&mut app.app.home);
    let generation = app.app.deps.session_generation.load(Ordering::SeqCst);
    app.app.handle_event(AppEvent::SessionLoggedOut {
        error: None,
        reauthenticate: false,
    });

    app.app.handle_event(AppEvent::PlaylistTracksAdded {
        generation,
        playlist: playlist(),
        tracks: vec![tracks()[1].clone()],
    });
    app.settle().await;

    assert!(app.cached("playlist:PL1").is_none());
    assert!(!app.app.home.pages().any(|page| page.title == "Late night"));
    assert!(!app.source.called("browse:playlist:PL1"));
}
