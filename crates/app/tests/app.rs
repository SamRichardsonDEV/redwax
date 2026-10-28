//! The Python `test_app.py` scenarios, ported by name. Each one drives the app
//! through keys and events and asserts on the rendered buffer, the recorded
//! calls on the fakes, or the state the screen is painted from.

mod support;

/// The header row the wordmark and the tabs sit on. The bar is one line.
const HEADER_TEXT_ROW: u16 = 0;

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use redwax::app::{AppEvent, Level, Prompt, Tab, queue_page};

/// The digits in the volume field, or nothing when it is not open.
fn volume_entry(app: &Harness) -> Option<String> {
    match app.app.prompt() {
        Some(Prompt::Volume(entry)) => Some(entry.clone()),
        _ => None,
    }
}
use redwax::keys::Action;
use redwax::palette::Palette;
use redwax::state::SavedState;
use redwax::theme::{redwax_theme, tokyo_night};
use redwax::ui::Hit;
use redwax_player::PlayerControl;
use redwax_player::{PlayerState, Repeat};
use redwax_ytm::{CompletePage, Item, Lyrics, Page, Shelf, Track};
use support::{
    FakePlayer, FakeSource, Harness, PanicSource, Setup, long_track, paged_track, seed_art, timed_lines,
    tracks,
};

/// Rows down the home feed to the playlist under "Mixed for you".
const TO_PLAYLIST: [&str; 3] = ["j", "j", "enter"];
const PLAYLIST_KEY: &str = "playlist:PL1";

#[tokio::test]
async fn test_partial_page_loads_the_rest_in_the_background() {
    let mut setup = Setup::new("partial-page");
    let gate = setup.paged_gated(&[100, 100, 19]);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&TO_PLAYLIST).await;
    let page = app.app.home.top().expect("the playlist page");
    assert_eq!(page.tracks().count(), 100);
    assert_eq!(app.app.pages_loading(), 1);
    // Half a playlist is never remembered as the whole one.
    assert_eq!(app.cached(PLAYLIST_KEY), None);
    let cursor = app.app.home.cursor();

    gate.add_permits(1);
    app.settle().await;

    let page = app.app.home.top().expect("the playlist page");
    assert_eq!(page.tracks().count(), 219);
    assert_eq!(page.shelves.len(), 1, "the batches joined the one shelf");
    assert_eq!(app.app.home.cursor(), cursor, "the cursor moved");
    assert_eq!(app.app.pages_loading(), 0);
    assert_eq!(
        app.cached(PLAYLIST_KEY).map(|page| page.tracks().count()),
        Some(219)
    );
}

#[tokio::test]
async fn durations_after_a_failed_continuation_do_not_cache_the_prefix_as_complete() {
    let mut setup = Setup::new("partial-failure-durations");
    let (paged, gate) = support::Paged::gated(&[2, 2]);
    setup.source = Arc::new(FakeSource {
        paged: Some(paged),
        continuation_fails: true,
        blank_durations: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&TO_PLAYLIST).await;

    gate.add_permits(1);
    app.settle().await;
    assert!(app.screen().contains("Paged 0"));
    assert_eq!(app.app.home.top().unwrap().tracks().count(), 2);
    assert!(CompletePage::try_from(app.app.home.top().unwrap().clone()).is_err());
    assert!(
        app.app
            .notifications
            .iter()
            .any(|notice| notice.text.contains("Only part of this page loaded"))
    );

    app.app
        .handle_event(AppEvent::DurationsReady(vec![("p0".into(), 301)]));
    app.settle().await;

    assert_eq!(
        app.app
            .home
            .top()
            .unwrap()
            .tracks()
            .next()
            .unwrap()
            .duration_seconds,
        Some(301)
    );
    assert_eq!(app.cached(PLAYLIST_KEY), None);
}

#[tokio::test]
async fn malformed_continuations_leave_the_page_incomplete() {
    for (name, batches, repeated) in [
        ("empty-final-page", vec![2, 0], false),
        ("repeated-page", vec![2, 2], true),
    ] {
        let mut setup = Setup::new(name);
        setup.source = Arc::new(FakeSource {
            paged: Some(support::Paged::new(&batches)),
            continuation_repeats: repeated,
            ..FakeSource::default()
        });
        let mut app = setup.build();
        app.draw();
        app.app.start_background();
        app.settle().await;
        app.press_all(&TO_PLAYLIST).await;

        assert_eq!(app.app.home.top().unwrap().tracks().count(), 2, "{name}");
        assert!(
            CompletePage::try_from(app.app.home.top().unwrap().clone()).is_err(),
            "{name}"
        );
        assert_eq!(app.cached(PLAYLIST_KEY), None, "{name}");
        assert_eq!(
            app.source
                .calls()
                .iter()
                .filter(|call| call.starts_with("continue:"))
                .count(),
            1,
            "{name} continued requesting malformed data"
        );
    }
}

#[tokio::test]
async fn queueing_an_incomplete_playlist_refuses_to_enqueue_a_prefix() {
    let mut setup = Setup::new("queue-incomplete-whole");
    setup.source = Arc::new(FakeSource {
        paged: Some(support::Paged::new(&[2, 2])),
        continuation_fails: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&["j", "j"]).await;
    let selected_item = app
        .app
        .home
        .selected_item()
        .cloned()
        .expect("highlighted playlist");
    app.right_click(|hit| matches!(hit, Hit::Row(row) if row.item == selected_item))
        .await;

    assert!(app.source.called("continue:batch:1"));
    assert!(app.app.player_state.queue.is_empty());
    assert!(
        app.app
            .notifications
            .iter()
            .any(|notice| notice.text.contains("network request failed"))
    );
}

#[tokio::test]
async fn playlist_addition_and_creation_do_not_submit_an_incomplete_collection() {
    for create in [false, true] {
        let mut setup = Setup::new(if create {
            "create-incomplete"
        } else {
            "add-incomplete"
        });
        setup.source = Arc::new(FakeSource {
            paged: Some(support::Paged::new(&[2, 2])),
            continuation_fails: true,
            ..FakeSource::default()
        });
        let mut app = setup.build();
        app.draw();
        app.app.start_background();
        app.settle().await;
        app.press_all(&["j", "j", "P"]).await;
        if create {
            app.press_all(&["j", "enter"]).await;
            app.type_text("Complete only").await;
        }
        app.press("enter").await;

        assert!(app.source.called("continue:batch:1"));
        assert!(app.source.playlist_edits().is_empty());
        assert!(
            app.app
                .notifications
                .iter()
                .any(|notice| notice.text.contains("network request failed"))
        );
    }
}

#[tokio::test]
async fn playlist_removal_delegates_complete_membership_to_the_source() {
    let mut setup = Setup::new("remove-incomplete");
    setup.source = Arc::new(FakeSource {
        paged: Some(support::Paged::new(&[2, 2])),
        continuation_fails: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.playing_queue(vec![paged_track(0)], 0);
    app.press_all(&["D", "enter"]).await;

    assert!(!app.source.called("continue:batch:1"));
    assert_eq!(
        app.source.playlist_edits(),
        vec!["remove_all_from_playlist:PL1:p0".to_string()],
        "the app tried to construct a membership snapshot itself"
    );
}

#[tokio::test]
async fn a_completed_page_remains_cacheable_after_durations_arrive() {
    let mut setup = Setup::new("complete-page-durations");
    setup.source = Arc::new(FakeSource {
        paged: Some(support::Paged::new(&[2, 2])),
        blank_durations: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&TO_PLAYLIST).await;
    app.app
        .handle_event(AppEvent::DurationsReady(vec![("p0".into(), 301)]));
    app.settle().await;

    let cached = app.cached(PLAYLIST_KEY).expect("the complete page is cached");
    assert_eq!(cached.tracks().count(), 4);
    assert_eq!(cached.tracks().next().unwrap().duration_seconds, Some(301));
}

#[tokio::test]
async fn leaving_a_page_does_not_cache_a_late_continuation_as_complete() {
    let mut setup = Setup::new("leave-pending-continuation");
    let (paged, gate) = support::Paged::gated(&[2, 2]);
    setup.source = Arc::new(FakeSource {
        paged: Some(paged),
        blank_durations: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&TO_PLAYLIST).await;
    app.press("escape").await;

    gate.add_permits(1);
    app.app
        .handle_event(AppEvent::DurationsReady(vec![("p0".into(), 301)]));
    app.settle().await;

    assert_eq!(app.app.home.top().unwrap().title, "Home");
    assert_eq!(app.cached(PLAYLIST_KEY), None);
    assert!(app.app.player_state.queue.is_empty());
}

#[tokio::test]
async fn test_playing_a_partial_playlist_queues_the_rest_as_it_arrives() {
    let mut setup = Setup::new("partial-plays");
    let gate = setup.paged_gated(&[100, 100, 19]);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&TO_PLAYLIST).await;
    app.press("enter").await;
    assert_eq!(app.app.player_state.queue.len(), 100);

    gate.add_permits(1);
    app.settle().await;
    assert_eq!(app.app.player_state.queue.len(), 219);
    assert_eq!(
        app.app.player_state.queue.last().map(|t| t.video_id.as_str()),
        Some("p218")
    );
    assert_eq!(app.app.player_state.index, Some(0), "the play position moved");
}

#[tokio::test]
async fn cancelling_page_play_does_not_leave_an_automatic_queue_producer() {
    let player = FakePlayer::new(80);
    let _gate = player.gate_play();
    let playback = redwax::playback::Playback::new(player.clone(), PlayerState::default());
    let pending_playback = playback.clone();
    let request = tokio::spawn(async move {
        pending_playback
            .play_page_tail(tracks(), 0, Some(PLAYLIST_KEY.to_owned()))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !player.calls().iter().any(|call| call == "play:0") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queue command reaches the playback gate");
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(!playback.append_page_tail(PLAYLIST_KEY, tracks()).await.unwrap());
    assert!(player.snapshot().queue.is_empty());
}

#[tokio::test]
async fn playlist_removal_cancels_a_speculative_cache_fill() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut setup = Setup::new("removed-during-prefetch");
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&["j", "j"]).await;
    app.rest().await;
    assert!(app.source.called("browse:playlist:PL1"));

    app.app.handle_event(AppEvent::PlaylistTrackRemoved {
        playlist_id: "PL1".to_owned(),
        video_id: "v1".to_owned(),
        playlist_entry: None,
    });
    gate.add_permits(1);
    app.settle().await;
    assert_eq!(app.cached(PLAYLIST_KEY), None);
}

#[tokio::test]
async fn a_speculative_cache_fill_cannot_overwrite_a_newer_cached_page() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut setup = Setup::new("newer-during-prefetch");
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&["j", "j"]).await;
    app.rest().await;
    assert!(app.source.called("browse:playlist:PL1"));
    let newer_page = Page::new(
        "Newer",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[2].clone())])],
    );
    app.app
        .deps
        .page_cache
        .put(PLAYLIST_KEY, &CompletePage::try_from(newer_page.clone()).unwrap());
    gate.add_permits(1);
    app.settle().await;
    assert_eq!(app.cached(PLAYLIST_KEY), Some(newer_page));
}

#[tokio::test]
async fn signing_out_preserves_request_identity_against_a_queued_old_page() {
    let (source, gate) = FakeSource::gated();
    let mut setup = Setup::new("logout-stale-page");
    setup.source = source;
    let mut app = setup.build();
    app.app.start_background();
    gate.add_permits(1);
    let stale_page = loop {
        let event = app.events.recv().await.expect("background request completes");
        if let AppEvent::PageReady {
            target: redwax::app::Target::Home,
            request,
            ..
        } = event
        {
            break AppEvent::PageReady {
                target: redwax::app::Target::Home,
                request,
                page: Page::new("Old account page", Vec::new()),
            };
        }
        app.app.handle_event(event);
    };
    gate.try_acquire()
        .expect("first page released the source gate")
        .forget();
    app.app.handle_event(AppEvent::SessionLoggedOut { error: None });
    app.app
        .load_page(redwax::app::Target::Home, Some("home".to_owned()), false);
    app.app.handle_event(stale_page);
    assert_eq!(app.app.home.top().unwrap().title, "Home");
    assert!(app.app.home.busy());
    gate.add_permits(1);
    app.settle().await;
    assert_eq!(app.app.home.top().unwrap().title, "Home");
    assert!(!app.app.home.busy());
}

#[tokio::test]
async fn signed_out_reload_does_not_restore_account_pages_from_the_cache() {
    let (source, _gate) = FakeSource::gated();
    let mut setup = Setup::new("logout-cached-page");
    setup.source = source;
    let cached = Page::new(
        "Old account page",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[0].clone())])],
    );
    redwax::page_cache::PageCache::new(setup.config.page_cache_dir.clone())
        .put("home", &CompletePage::try_from(cached).unwrap());
    let mut app = setup.build();
    app.app.handle_event(AppEvent::SessionLoggedOut { error: None });
    app.app
        .load_page(redwax::app::Target::Home, Some("home".to_owned()), false);
    assert_eq!(app.app.home.top().unwrap().title, "Home");
    assert!(app.app.home.top().unwrap().is_empty());
    assert!(app.app.home.busy());
}

#[tokio::test]
async fn test_scrolling_near_the_end_asks_for_more() {
    let mut setup = Setup::new("scroll-loads");
    setup.paged(&[300, 19]);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&TO_PLAYLIST).await;
    // Past the eager count, so the rest waits for a reason to be wanted.
    assert_eq!(app.app.home.top().expect("the page").tracks().count(), 300);
    assert!(!app.source.called("continue:batch:1"), "{:?}", app.source.calls());

    app.press("G").await;
    assert!(app.source.called("continue:batch:1"));
    assert_eq!(app.app.home.top().expect("the page").tracks().count(), 319);
}

#[tokio::test]
async fn test_leaving_the_page_stops_its_loader() {
    let mut setup = Setup::new("leaving-stops");
    setup.paged(&[300, 19]);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&TO_PLAYLIST).await;
    assert_eq!(app.app.pages_loading(), 1);

    app.press("escape").await;
    assert_eq!(app.app.pages_loading(), 0);
    app.press("G").await;
    assert!(!app.source.called("continue:batch:1"), "{:?}", app.source.calls());
}

#[tokio::test]
async fn test_home_loads_and_enter_plays_a_track() {
    let mut setup = Setup::new("home-plays");
    setup.config.accent_from_art = true;
    seed_art(&setup.dir, support::ART_URL);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    assert_eq!(app.app.home.top().map(|p| p.title.as_str()), Some("Home"));
    assert!(app.source.called("home"));
    assert_eq!(app.app.account, "@sam");

    app.press("enter").await;
    assert_eq!(app.player.calls(), vec!["play:0".to_string()]);
    assert_eq!(
        app.app.player_state.track().map(|t| t.title.as_str()),
        Some("Alpine Glow")
    );
    assert!(app.player_bar().contains("3:34"));
    assert_eq!(app.app.theme().name, "art");
}

#[tokio::test]
async fn opening_a_queue_tracks_album_leaves_the_queue_browser_intact() {
    let mut setup = Setup::new("queue-open-album");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(0),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();

    app.press("b").await;
    app.settle().await;

    assert_eq!(app.app.tab, Tab::Home);
    assert_eq!(app.app.queue.top().map(|page| page.title.as_str()), Some("Queue"));
    assert!(app.source.called("browse:album:MPREb_1"));
}

#[tokio::test]
async fn modal_help_blocks_transport_clicks_behind_it() {
    let mut app = Harness::start("help-blocks-player").await;
    app.press("enter").await;
    app.draw();
    let next_button = app.area_of(|hit| matches!(hit, Hit::Next));
    let calls_before_help = app.player.calls();

    app.app.handle_key(support::parse_key("?"));
    app.click_at(next_button.x, next_button.y, MouseButton::Left)
        .await;

    assert_eq!(app.player.calls(), calls_before_help);
    assert!(app.app.help_visible());

    app.click_at(next_button.x, next_button.y, MouseButton::Left)
        .await;
    assert_eq!(app.player.calls(), calls_before_help);
    assert!(app.app.help_visible());
}

#[tokio::test]
async fn test_opening_an_album_pushes_a_page_and_back_pops_it() {
    let mut app = Harness::start("album-page").await;
    app.press_all(&["j", "enter"]).await;
    let titles: Vec<&str> = app.app.home.pages().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["Home", "Peaks"]);
    assert!(app.source.called("browse:album:MPREb_1"));

    app.press("escape").await;
    let titles: Vec<&str> = app.app.home.pages().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["Home"]);
}

#[tokio::test]
async fn test_search_flow_and_queueing() {
    let mut app = Harness::start("search-flow").await;
    app.press("2").await;
    assert!(!app.app.search_focused(), "arriving on the tab took the keyboard");
    assert_eq!(app.app.tab, Tab::Search);
    app.press("enter").await;
    assert!(app.app.search_focused(), "enter did not reach the box");

    app.type_text("snowline").await;
    app.press("enter").await;
    assert!(app.source.called("search:snowline"));
    assert!(!app.app.search_focused());
    assert!(app.screen().contains("/ search again"));

    app.right_click(|hit| matches!(hit, Hit::Row(_))).await;
    assert_eq!(app.player.calls(), vec!["enqueue:1:false".to_string()]);

    app.press("4").await;
    // The column names take a row of their own over the one track.
    assert_eq!(app.app.queue.rows().len(), 2);
    assert_eq!(
        app.app.queue.top().and_then(|p| p.subtitle.as_deref()),
        Some("1 track loaded \u{b7} playing 1 of 1 \u{b7} 3:34 remaining")
    );
}

#[test]
fn queue_subtitle_distinguishes_known_and_unknown_remaining_time() {
    let mut state = PlayerState {
        queue: tracks(),
        index: Some(1),
        position: 7.0,
        ..PlayerState::default()
    };

    assert_eq!(
        queue_page(&state).subtitle.as_deref(),
        Some("3 tracks loaded \u{b7} playing 2 of 3 \u{b7} 8:02 remaining")
    );

    state.queue[2].duration_seconds = None;
    assert_eq!(
        queue_page(&state).subtitle.as_deref(),
        Some("3 tracks loaded \u{b7} playing 2 of 3 \u{b7} ? remaining")
    );

    state.index = Some(0);
    state.position = 0.0;
    state.queue[0].duration_seconds = Some(u32::MAX);
    state.queue[1].duration_seconds = Some(1);
    state.queue[2].duration_seconds = Some(0);
    assert!(
        queue_page(&state)
            .subtitle
            .as_deref()
            .is_some_and(|subtitle| subtitle.ends_with("? remaining"))
    );
}

#[tokio::test]
async fn empty_search_submission_returns_the_keyboard_to_page_controls() {
    let mut app = Harness::start("empty-search-submit").await;
    app.press_all(&["2", "enter"]).await;
    assert!(app.app.search_focused());

    app.press("enter").await;

    assert!(!app.app.search_focused());
    assert!(app.screen().contains("/ search youtube music"));
}

#[tokio::test]
async fn test_lyrics_tab_fetches_for_the_playing_track() {
    let app = lyrics_tab("lyrics-tab").await;
    assert!(app.source.called("timed_lyrics:v1"));
    assert!(
        !app.app.lyrics.wants(&support::tracks()[0]),
        "v1 lyrics are not the shown ones"
    );
    assert!(matches!(app.app.lyrics.text(), Some(Lyrics::Timed(_))));
    assert!(app.screen().contains("La la la"));
    assert!(app.lyrics_marker().contains("0:00"));
}

#[tokio::test]
async fn test_timed_lyrics_highlight_follows_the_position() {
    let mut app = lyrics_tab("lyrics-follow").await;
    app.at_position(10.0).await;
    assert_eq!(app.current_lyric(), "\u{258e} Wind at the col 2");

    app.at_position(90.0).await;
    assert_eq!(app.current_lyric(), "\u{258e} And home again 22");
    // The panel followed the line down rather than leaving it off the bottom.
    assert!(app.app.lyrics.scroll() > 0);
}

#[tokio::test]
async fn test_lyrics_marker_moves_with_the_track() {
    let mut app = lyrics_tab("lyrics-marker").await;
    app.at_position(10.0).await;
    let early = app.lyrics_marker();
    app.at_position(90.0).await;
    let later = app.lyrics_marker();

    assert!(early.starts_with("  0:10"), "{early}");
    assert!(later.starts_with("  1:30"), "{later}");
    assert!(early.ends_with("3:34") && later.ends_with("3:34"));
    let head = |row: &str| row.find('\u{25cf}').expect("the marker has a head");
    assert!(head(&later) > head(&early));
}

#[tokio::test]
async fn test_manual_scroll_pauses_auto_scroll() {
    let mut app = lyrics_tab("lyrics-scroll").await;
    app.at_position(60.0).await;
    let followed = app.app.lyrics.scroll();
    assert!(followed > 0);

    app.press("k").await;
    assert_eq!(app.app.lyrics.scroll(), followed - 1);
    // A later position must not drag the panel back while the pause holds.
    app.at_position(108.0).await;
    assert_eq!(app.app.lyrics.scroll(), followed - 1);

    app.app.lyrics.follow_now();
    app.at_position(108.0).await;
    assert!(app.app.lyrics.scroll() > followed);
}

#[tokio::test]
async fn test_click_on_a_lyric_line_seeks() {
    let mut app = lyrics_tab("lyrics-click").await;
    app.click(|hit| matches!(hit, Hit::LyricLine(3))).await;
    assert_eq!(timed_lines()[3].start_ms, 12_000);
    assert!(app.player.calls().contains(&"seek_to:12".to_string()));
}

#[tokio::test]
async fn test_plain_lyrics_still_render() {
    let mut app = Harness::start("lyrics-plain").await;
    app.player
        .play(support::tracks(), 1)
        .await
        .expect("the fake player never fails");
    app.settle().await;
    app.press("5").await;

    assert!(app.source.called("timed_lyrics:v2"));
    assert!(matches!(app.app.lyrics.text(), Some(Lyrics::Plain(_))));
    assert!(app.screen().contains("Plain and untimed"));
    // The marker is drawn over untimed words, but no line is lit under it.
    assert!(app.lyrics_marker().contains("3:07"));
    assert_eq!(app.current_lyric(), "");
}

#[tokio::test]
async fn test_like_toggles_and_is_sent_to_the_source() {
    let mut app = Harness::start("like-toggle").await;
    app.playing();
    app.press("+").await;
    assert_eq!(app.source.liked().get("v1"), Some(&false));
    app.press("+").await;
    assert_eq!(app.source.liked().get("v1"), Some(&true));
}

#[tokio::test]
async fn test_quit_closes_the_player() {
    let mut app = Harness::start("quit-closes").await;
    app.press("Q").await;
    assert!(app.app.quit);
    app.app.shutdown().await.expect("app shuts down");
    assert!(app.player.closed());
}

#[tokio::test]
async fn test_vim_and_arrow_keys_move_between_tabs() {
    let mut app = Harness::start("tab-keys").await;
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Search);
    app.press_all(&["right", "right"]).await;
    assert_eq!(app.app.tab, Tab::Queue);
    app.press_all(&["h", "left", "left"]).await;
    assert_eq!(app.app.tab, Tab::Home);
    app.press("h").await;
    assert_eq!(app.app.tab, Tab::Home);
    assert!(app.app.account_focused());
    app.press("h").await;
    assert_eq!(app.app.tab, Tab::Settings);
    assert!(!app.app.account_focused());
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Settings);
    assert!(app.app.account_focused());
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Home);
    app.press_all(&["h", "h", "h"]).await;
    assert_eq!(app.app.tab, Tab::Themes);
    app.press("h").await;
    assert_eq!(app.app.tab, Tab::Lyrics);
    app.press("3").await;
    assert_eq!(app.app.tab, Tab::Library);
}

#[tokio::test]
async fn test_sidebar_hides_on_narrow_terminals() {
    let mut setup = Setup::new("narrow");
    setup.width = 80;
    setup.height = 30;
    let mut app = setup.build();
    app.draw();
    assert!(!app.app.sidebar_visible());
    assert!(
        app.header_row().contains("1/7") && app.header_row().contains("h l views"),
        "the compact view navigation was clipped at 80 columns: {:?}",
        app.header_row()
    );

    let mut wide = Setup::new("wide").build();
    wide.draw();
    assert!(wide.app.sidebar_visible());
    // Narrow keeps only the player bar line, wide adds the sidebar one.
    assert_eq!(app.screen().matches("nothing playing").count(), 1);
    assert_eq!(wide.screen().matches("nothing playing").count(), 2);
}

#[tokio::test]
async fn test_theme_follows_the_terminal_palette_while_running() {
    let mut setup = Setup::new("palette-follow");
    setup.config.theme = "terminal".to_string();
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    assert_eq!(app.app.theme().name, "redwax");
    assert_eq!(app.app.theme().background, redwax_theme().background);

    let mut ansi = tokyo_night().ansi;
    ansi[4] = "#7daea3".to_string();
    ansi[0] = "#1d2021".to_string();
    let palette = Palette {
        source: None,
        background: "#282828".to_string(),
        foreground: "#d4be98".to_string(),
        ansi,
        selection_background: Some("#3c3836".to_string()),
    };
    let mut controller = redwax::themes::ThemeController::new("terminal");
    let request = controller
        .activate(redwax::themes::ThemeId::Terminal)
        .expect("terminal is live");
    app.app
        .handle_event(AppEvent::PaletteChanged { request, palette });
    app.settle().await;

    assert_eq!(app.app.theme().name, "terminal");
    assert_eq!(
        app.app.theme().background,
        ratatui::style::Color::Rgb(0x28, 0x28, 0x28)
    );
    assert_eq!(
        app.app.theme().accent,
        ratatui::style::Color::Rgb(0x7d, 0xae, 0xa3)
    );
}

#[tokio::test]
async fn test_shuffle_repeat_and_mute_show_in_the_bar() {
    let mut app = Harness::start("modes-in-bar").await;
    app.press("enter").await;
    let icons = redwax::icons::icon_set("nerd");
    assert!(app.player_bar().contains(icons.shuffle_off));
    assert!(app.player_bar().contains(icons.repeat_off));

    app.press_all(&["s", "R"]).await;
    assert!(app.app.player_state.shuffle);
    assert_eq!(app.app.player_state.repeat, Repeat::All);
    assert!(app.player_bar().contains(icons.shuffle));
    assert!(app.player_bar().contains(icons.repeat));

    app.press_all(&["R", "m"]).await;
    assert_eq!(app.app.player_state.repeat, Repeat::One);
    assert!(app.app.player_state.muted);
    assert!(app.player_bar().contains(icons.repeat_one));
    assert!(app.player_bar().contains("muted"));
    // The first fixture track is liked.
    assert!(app.player_bar().contains(icons.liked));
}

#[tokio::test]
async fn test_transparent_config_marks_the_screen() {
    let mut setup = Setup::new("transparent");
    setup.config.transparent = true;
    let mut app = setup.build();
    app.draw();
    assert!(app.app.theme().transparent);
    assert_eq!(app.app.theme().background, ratatui::style::Color::Reset);
    assert_eq!(app.app.theme().surface, ratatui::style::Color::Reset);
    // The highlight keeps a solid colour, because a translucent row would
    // blend towards black rather than towards the terminal.
    assert_ne!(app.app.theme().panel, ratatui::style::Color::Reset);

    let buffer = app.terminal.backend().buffer();
    let header = buffer.cell((1, 0)).expect("the header row is drawn");
    assert_eq!(header.bg, ratatui::style::Color::Reset);
}

#[tokio::test]
async fn test_art_box_keeps_the_placeholder_until_art_arrives_then_follows_the_image() {
    let setup = Setup::new("art-box");
    seed_art(&setup.dir, support::ART_URL);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    assert!(app.app.art.render_protocol.is_none());
    assert_eq!(app.app.art.cover_height, redwax::app::ART_MAX_HEIGHT);

    app.press("enter").await;
    assert!(app.app.art.render_protocol.is_some());
    assert!(app.app.art.cover_height > 0 && app.app.art.cover_height <= redwax::app::ART_MAX_HEIGHT);
}

#[tokio::test]
async fn test_remembered_home_shows_before_the_network_answers() {
    let (source, gate) = FakeSource::gated();
    let mut setup = Setup::new("remembered-home");
    setup.source = source;
    let remembered = Page::new(
        "Home",
        vec![Shelf::new(
            "Remembered",
            vec![Item::Track(Track {
                video_id: "old1".to_string(),
                title: "From last time".to_string(),
                artists: Vec::new(),
                album: None,
                album_browse_id: None,
                duration_seconds: None,
                thumbnail_url: None,
                liked: false,
                playlist_entry: None,
            })],
        )],
    );
    redwax::page_cache::PageCache::new(setup.config.page_cache_dir.clone())
        .put("home", &CompletePage::try_from(remembered).unwrap());

    let mut app = setup.build();
    app.draw();
    assert_eq!(
        app.app.home.top().map(|p| p.shelves[0].title.as_str()),
        Some("Remembered")
    );
    assert!(app.screen().contains("From last time"));

    app.app.start_background();
    app.settle().await;
    assert_eq!(app.player.prefetched().first().map(String::as_str), Some("old1"));

    gate.add_permits(1);
    app.settle().await;
    assert_eq!(
        app.app.home.top().map(|p| p.shelves[0].title.as_str()),
        Some("Listen again")
    );
    assert_eq!(app.app.home.depth(), 1);
}

#[tokio::test]
async fn test_resting_on_a_track_prefetches_its_stream() {
    let mut app = Harness::start("resting-prefetch").await;
    app.player.clear_prefetched();
    // Down onto the album, back up onto the track: only the row the cursor
    // rests on is looked up.
    app.press_all(&["j", "k"]).await;
    app.rest().await;
    assert_eq!(app.player.prefetched(), vec!["v1".to_string()]);

    // Resting on the album fetches its page into the cache, so opening it is
    // instant.
    app.press("j").await;
    app.rest().await;
    let cache = redwax::page_cache::PageCache::new(app.app.deps.config.page_cache_dir.clone());
    assert!(cache.get("album:MPREb_1").is_some());
    assert!(app.source.called("browse:album:MPREb_1"));
}

#[tokio::test]
async fn a_later_duplicate_track_keeps_its_place_in_a_mixed_shelf() {
    let mut app = Harness::start("later-duplicate-track").await;
    let tracks = support::tracks();
    app.app.home.show(Page::new(
        "Repeated songs",
        vec![Shelf::new(
            "Songs",
            vec![
                Item::Track(tracks[0].clone()),
                Item::Album(support::album()),
                Item::Track(tracks[1].clone()),
                Item::Track(tracks[0].clone()),
                Item::Track(tracks[2].clone()),
            ],
        )],
    ));
    app.press_all(&["j", "j"]).await;
    assert_eq!(app.app.home.selected(), Some((0, 3)));

    app.player.clear_prefetched();
    app.rest().await;
    assert_eq!(
        app.player.prefetched(),
        vec![tracks[0].video_id.clone(), tracks[2].video_id.clone()]
    );

    app.press("enter").await;
    assert_eq!(app.player.calls(), vec!["play:2".to_string()]);
    assert_eq!(app.app.player_state.index, Some(2));
}

#[tokio::test]
async fn test_state_file_restores_the_queue() {
    let mut setup = Setup::new("state-restore");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(1),
        position: 42.0,
        volume: Some(55),
        repeat: Repeat::All,
        shuffle: true,
        muted: false,
    };
    let mut app = setup.build();
    app.draw();

    assert_eq!(app.app.player_state.queue.len(), 3);
    assert_eq!(app.app.player_state.index, Some(1));
    assert_eq!(app.app.player_state.volume, 55);
    assert_eq!(app.app.player_state.repeat, Repeat::All);
    assert!(app.app.player_state.shuffle);

    app.press("4").await;
    assert_eq!(app.app.queue.rows().len(), 4);
    assert!(app.screen().contains("Snowline"));
    assert_eq!(
        app.app.queue.top().and_then(|p| p.subtitle.as_deref()),
        queue_page(&app.app.player_state).subtitle.as_deref()
    );
    assert_eq!(app.app.saved_state().index, Some(1));
}

#[tokio::test]
async fn test_first_frame_needs_no_network() {
    let dir = support::scratch("first-frame");
    let config = support::config_in(&dir);
    let player = FakePlayer::new(config.volume);
    let deps = redwax::app::Deps {
        icons: redwax::icons::icon_set(&config.icons),
        theme: redwax_theme(),
        page_cache: redwax::page_cache::PageCache::new(config.page_cache_dir.clone()),
        art_cache: Arc::new(redwax::art::ArtCache::new(config.art_cache_dir.clone())),
        art_setup: redwax::terminal::detect(&config.art, Some(false), None),
        saved: SavedState::default(),
        state_store: redwax::state::PlaybackStateStore::new(config.state_path.clone()),
        session_refresh: None,
        session_logout: None,
        account_access: None,
        config,
    };
    let (sender, _events) = tokio::sync::mpsc::unbounded_channel();
    let mut app = redwax::app::App::new(Arc::new(PanicSource), player, deps, sender);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40))
        .expect("the test backend never fails");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("the test backend never fails");

    let buffer = terminal.backend().buffer();
    let first = (0..buffer.area().width)
        .filter_map(|x| {
            buffer
                .cell((x, HEADER_TEXT_ROW))
                .map(|cell| cell.symbol().to_string())
        })
        .collect::<String>();
    assert!(first.contains("redwax"));
    assert!(first.contains("home"));
}

#[tokio::test]
async fn test_player_bar_has_three_rows() {
    let mut app = Harness::start("bar-rows").await;
    app.press("enter").await;
    let rows = app.player_bar_rows();
    assert_eq!(rows.len(), 3);

    let icons = redwax::icons::icon_set("nerd");
    // What plays, then the seek bar between its two times, then the modes.
    assert!(rows[0].contains("Alpine Glow"));
    assert!(rows[0].contains("Arnegger"));
    assert!(rows[1].contains("0:00") && rows[1].contains("3:34"));
    assert!(rows[1].contains('\u{25cf}'));
    assert!(rows[2].contains("shuffle"));
    assert!(rows[2].contains("off"));
    assert!(rows[2].contains(&format!("{}%", app.app.player_state.volume)));
    assert!(rows[2].contains(icons.volume));
    // The hints are in the sidebar's keys box here, so the transport keeps the
    // last row rather than leaving it to a bar.
    let height = app.terminal.backend().buffer().area().height;
    assert!(app.app.sidebar_visible());
    assert_eq!(app.row(height - 1), rows[2]);
}

#[tokio::test]
async fn test_columns_carry_separators() {
    let app = Harness::start("column-seams").await;
    let row = app.list_row("Alpine Glow");
    assert!(row.contains("Arnegger"));
    assert!(row.contains("Peaks"));
    // One hairline after each of the title, the artist and the album.
    assert_eq!(row.matches('\u{2502}').count(), 3);
    // Every cell of the row is still inside the pane.
    assert_eq!(row.chars().count(), app.app.screen.width() as usize);

    // The column names sit on the same boundaries as the row under them.
    let names = app.list_row("TITLE");
    assert!(names.contains("ARTIST") && names.contains("ALBUM") && names.contains("TIME"));
    assert_eq!(names.matches('\u{2502}').count(), 3);
    for (name, column) in [
        ("TITLE", "Alpine Glow"),
        ("ARTIST", "Arnegger"),
        ("ALBUM", "Peaks"),
    ] {
        assert_eq!(names.find(name), row.find(column));
    }
}

#[tokio::test]
async fn test_marquee_scrolls_a_long_title() {
    let mut setup = Setup::new("marquee");
    // The marquee is the plain title line, which the pixel title replaces.
    setup.saved = SavedState {
        queue: vec![long_track()],
        index: Some(0),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.playing_queue(vec![long_track()], 0);
    let resting = app.sidebar_title();
    assert!(resting.trim_start().starts_with("A Very Long Title"));

    // The marquee rests two seconds, then steps one cell every third tick.
    app.ticks(40);
    let scrolled = app.sidebar_title();
    assert_ne!(resting, scrolled);
    assert!(!scrolled.trim_start().starts_with("A Very Long Title"));
    // It scrolled inside the title rather than reflowing it.
    assert!(long_track().title.contains(scrolled.trim()));
}

#[tokio::test]
async fn test_animations_off_keeps_everything_still() {
    let mut setup = Setup::new("no-animation");
    setup.config.animations = false;
    setup.saved = SavedState {
        queue: vec![long_track()],
        index: Some(0),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.playing_queue(vec![long_track()], 0);
    let before = app.screen();
    let title = app.sidebar_title();
    assert!(title.trim_end().ends_with('\u{2026}'));

    app.ticks(40);
    assert_eq!(app.screen(), before);
    // The equaliser holds its one still frame in the list and in the bar.
    assert!(app.player_bar_rows()[0].contains("\u{2583}\u{2585}\u{2583}\u{2585}"));
}

#[tokio::test]
async fn test_left_click_plays_and_right_click_queues() {
    let mut app = Harness::start("mouse-plays").await;
    app.click(|hit| matches!(hit, Hit::Row(row) if row.item_index == 0))
        .await;
    assert_eq!(app.player.calls(), vec!["play:0".to_string()]);
    assert_eq!(
        app.app.player_state.track().map(|t| t.title.as_str()),
        Some("Alpine Glow")
    );

    app.right_click(|hit| matches!(hit, Hit::Row(row) if row.item_index == 1))
        .await;
    assert!(app.source.called("browse:album:MPREb_1"));
    assert!(app.player.calls().contains(&"enqueue:3:false".to_string()));
    assert!(app.screen().contains("3 tracks added to queue"));
}

#[tokio::test]
async fn test_click_on_tab_switches() {
    let mut app = Harness::start("mouse-tabs").await;
    app.click(|hit| matches!(hit, Hit::Tab(Tab::Library))).await;
    assert_eq!(app.app.tab, Tab::Library);
    app.click(|hit| matches!(hit, Hit::Tab(Tab::Queue))).await;
    assert_eq!(app.app.tab, Tab::Queue);
}

#[tokio::test]
async fn test_click_on_seek_bar_seeks() {
    let mut app = Harness::start("mouse-seek").await;
    app.press("enter").await;
    let bar = app.area_of(|hit| matches!(hit, Hit::SeekBar(_)));
    let offset = bar.width / 2;
    app.click_at(bar.x + offset, bar.y, MouseButton::Left).await;

    let duration = app.app.player_state.duration.expect("the track has a duration");
    let wanted = duration * f64::from(offset) / f64::from(bar.width);
    assert!((app.app.player_state.position - wanted).abs() < 1.0);
}

#[tokio::test]
async fn test_wheel_moves_the_cursor() {
    let mut setup = Setup::new("mouse-wheel");
    let queue: Vec<Track> = support::tracks().into_iter().cycle().take(8).collect();
    setup.saved = SavedState {
        queue,
        index: Some(0),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    app.press("4").await;
    // Row zero is the column names, so the first track is row one.
    assert_eq!(app.app.queue.cursor(), Some(1));

    let list = app.area_of(|hit| matches!(hit, Hit::List(_)));
    let (x, y) = (list.x + 2, list.y + 1);
    app.wheel(x, y, false).await;
    assert_eq!(app.app.queue.cursor(), Some(4));
    app.wheel(x, y, true).await;
    assert_eq!(app.app.queue.cursor(), Some(1));
}

#[tokio::test]
async fn test_q_jumps_to_the_queue_and_the_playing_track() {
    let mut setup = Setup::new("queue-jump");
    let queue: Vec<Track> = support::tracks().into_iter().cycle().take(9).collect();
    setup.saved = SavedState {
        queue,
        index: Some(4),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    app.press("1").await;
    assert_eq!(app.app.tab, Tab::Home);

    app.press("q").await;
    assert_eq!(app.app.tab, Tab::Queue);
    // Row zero is the column names, so the fifth track is row five.
    assert_eq!(app.app.queue.cursor(), Some(5));

    // The cursor is on a real row, so enter picks it straight away.
    let picked = app.app.queue.selected().expect("the cursor landed on a track");
    assert_eq!(picked.1, 4);
}

/// The library page holds one playlist, so entering it opens that playlist.
async fn on_the_playlist_page(name: &str) -> Harness {
    let mut app = Harness::start(name).await;
    app.press("3").await;
    app.press("enter").await;
    assert!(
        app.source.called("browse:playlist:PL1"),
        "{:?}",
        app.source.calls()
    );
    app
}

#[tokio::test]
async fn test_d_removes_a_track_from_a_playlist_page() {
    let mut app = on_the_playlist_page("playlist-remove").await;
    let before = app.app.library.top().expect("a page").tracks().count();
    app.press("j").await;

    app.press("D").await;
    assert_eq!(
        app.source.playlist_edits(),
        vec!["remove_playlist_entry:PL1:v2".to_string()],
        "the set video id names the row, not the video id alone"
    );

    let page = app.app.library.top().expect("a page");
    assert_eq!(page.tracks().count(), before - 1);
    assert!(
        page.tracks().all(|track| track.video_id != "v2"),
        "the row is still in the page"
    );
    // The toast names the track it removed, so the list is what is read here.
    let list = app.list_row("Foehn Wind");
    assert!(!list.is_empty(), "the rest of the list is gone");
    assert_eq!(app.app.library.cursor(), Some(2), "the cursor kept its row");
}

#[tokio::test]
async fn test_a_refused_removal_puts_the_row_back() {
    let mut setup = Setup::new("playlist-remove-refused");
    setup.failing_edits();
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press("3").await;
    app.press("enter").await;

    let before = app.app.library.top().expect("a page").tracks().count();
    app.press("D").await;
    let page = app.app.library.top().expect("a page");
    assert_eq!(page.tracks().count(), before, "the row did not come back");
    assert!(page.tracks().any(|track| track.video_id == "v1"));
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text.contains("refused")),
        "no toast: {:?}",
        app.app.notifications
    );
}

#[tokio::test]
async fn test_d_off_a_playlist_asks_which_playlist_first() {
    let mut app = Harness::start("playlist-remove-off").await;
    app.press("D").await;
    assert!(
        app.source.playlist_edits().is_empty(),
        "home answered D with an edit"
    );
    let picker = app.app.playlist_picker().expect("D on home opened no picker");
    assert!(picker.remove_mode(), "D opened the add picker");
    assert!(app.screen().contains("REMOVE FROM PLAYLIST"));
    assert!(
        !app.screen().contains("New playlist"),
        "removing offered a new playlist"
    );
    app.press("escape").await;

    // An album page is not a playlist, whatever its rows look like.
    app.press_all(&["j", "enter"]).await;
    assert!(app.source.called("browse:album:MPREb_1"));
    let before = app.app.home.top().expect("a page").tracks().count();
    app.press("D").await;
    assert!(
        app.source.playlist_edits().is_empty(),
        "an album answered D with an edit"
    );
    assert!(
        app.app.playlist_picker().is_some(),
        "D on an album opened no picker"
    );
    assert_eq!(app.app.home.top().expect("a page").tracks().count(), before);
}

#[tokio::test]
async fn test_p_opens_the_picker_and_adds() {
    let mut app = Harness::start("playlist-add").await;
    app.press("P").await;
    let picker = app.app.playlist_picker().expect("the picker opened");
    assert_eq!(
        picker
            .playlists
            .iter()
            .map(|p| p.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Late night"],
        "the picker lists the library playlists"
    );
    let screen = app.screen();
    for text in ["ADD TO PLAYLIST", "Late night", "escape cancel"] {
        assert!(screen.contains(text), "no {text} in the picker: {screen}");
    }

    app.press("enter").await;
    assert!(app.app.playlist_picker().is_none(), "the picker stayed open");
    assert_eq!(
        app.source.playlist_edits(),
        vec!["add_to_playlist:PL1:v1".to_string()]
    );
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text.contains("added to Late night")),
        "no toast: {:?}",
        app.app.notifications
    );
}

#[tokio::test]
async fn test_the_picker_cancels_on_escape() {
    let mut app = Harness::start("playlist-add-cancel").await;
    app.press("P").await;
    assert!(app.app.playlist_picker().is_some());

    // j moves inside the picker rather than the list under it.
    let cursor = app.app.home.cursor();
    app.press("j").await;
    assert_eq!(app.app.home.cursor(), cursor, "the key reached the page");

    app.press("escape").await;
    assert!(app.app.playlist_picker().is_none());
    assert!(app.source.playlist_edits().is_empty(), "cancel added a track");
    assert!(!app.screen().contains("ADD TO PLAYLIST"));
}

#[tokio::test]
async fn test_plus_likes_the_highlighted_row_while_a_different_track_plays() {
    let mut app = Harness::start("like-highlighted").await;
    app.playing_queue(tracks(), 1);
    app.press("+").await;
    assert_eq!(app.source.liked().get("v1"), Some(&false));
    assert_eq!(app.source.liked().get("v2"), None);
}

#[tokio::test]
async fn test_p_adds_what_plays_and_tab_swaps_in_the_row() {
    let mut app = Harness::start("playlist-add-playing").await;
    app.playing_queue(tracks(), 1);
    app.press("P").await;
    let picker = app.app.playlist_picker().expect("the picker opened");
    assert!(
        picker.primary_item_label().contains("Snowline"),
        "{}",
        picker.primary_item_label()
    );
    let screen = app.screen();
    assert!(
        screen.contains("tab  Alpine Glow"),
        "the row is not offered: {screen}"
    );
    app.press("enter").await;
    assert_eq!(
        app.source.playlist_edits(),
        vec!["add_to_playlist:PL1:v2".to_string()]
    );

    app.press_all(&["P", "tab", "enter"]).await;
    assert_eq!(
        app.source.playlist_edits(),
        vec![
            "add_to_playlist:PL1:v2".to_string(),
            "add_to_playlist:PL1:v1".to_string()
        ],
        "tab did not swap in the highlighted row"
    );
}

#[tokio::test]
async fn test_the_picker_makes_a_new_playlist() {
    let mut app = Harness::start("playlist-new").await;
    app.press_all(&["P", "j"]).await;
    assert!(app.screen().contains("+ New playlist"));
    app.press("enter").await;
    assert!(
        app.app
            .playlist_picker()
            .as_ref()
            .is_some_and(|picker| picker.new_playlist_name_input.is_some()),
        "enter on the last row did not ask for a name"
    );
    let fetched = app
        .source
        .calls()
        .iter()
        .filter(|call| *call == "library")
        .count();

    app.type_text("Road trip").await;
    app.press("enter").await;
    assert!(app.app.playlist_picker().is_none(), "the picker stayed open");
    assert!(
        app.source.called("create_playlist:Road trip:v1"),
        "{:?}",
        app.source.calls()
    );
    assert!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "library")
            .count()
            > fetched,
        "the library was not fetched again"
    );
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text.contains("Made Road trip")),
        "no toast: {:?}",
        app.app.notifications
    );
}

#[tokio::test]
async fn test_escape_leaves_the_name_for_the_list() {
    let mut app = Harness::start("playlist-new-cancel").await;
    app.press_all(&["P", "j", "enter"]).await;
    // Keys the picker would act on belong to the name while it is open.
    app.type_text("qPD").await;
    let picker = app.app.playlist_picker().expect("a typed q closed the picker");
    assert_eq!(picker.new_playlist_name_input.as_deref(), Some("qPD"));

    app.press("escape").await;
    let picker = app.app.playlist_picker().expect("escape closed the whole picker");
    assert!(picker.new_playlist_name_input.is_none());
    app.press("escape").await;
    assert!(app.app.playlist_picker().is_none());
    assert!(
        app.source.playlist_edits().is_empty(),
        "cancelling made a playlist"
    );
}

#[tokio::test]
async fn test_d_takes_what_plays_out_of_the_chosen_playlist() {
    let mut app = Harness::start("playlist-remove-playing").await;
    app.playing_queue(tracks(), 0);
    app.press_all(&["D", "enter"]).await;
    assert_eq!(
        app.source.playlist_edits(),
        vec!["remove_all_from_playlist:PL1:v1".to_string()],
        "the row was not named by the set video id the playlist serves"
    );
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text.contains("removed from Late night")),
        "no toast: {:?}",
        app.app.notifications
    );
}

#[tokio::test]
async fn test_removing_a_track_not_loaded_in_the_playlist_uses_complete_removal() {
    let mut app = Harness::start("playlist-remove-absent").await;
    app.playing_queue(vec![paged_track(0)], 0);
    app.press_all(&["D", "enter"]).await;
    assert_eq!(
        app.source.playlist_edits(),
        vec!["remove_all_from_playlist:PL1:p0".to_string()],
        "the app relied on its incomplete local page"
    );
}

#[tokio::test]
async fn test_key_hint_click_acts() {
    let mut setup = Setup::new("mouse-hints");
    setup.height = 55;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.click(|hit| matches!(hit, Hit::KeyHint(Action::ToggleShuffle)))
        .await;
    assert!(app.app.player_state.shuffle);

    app.click(|hit| matches!(hit, Hit::KeyHint(Action::ToggleHelp)))
        .await;
    assert!(app.app.help_visible());
}

#[tokio::test]
async fn repeat_and_shuffle_failures_are_visible_as_playback_errors() {
    let mut app = Harness::start("control-errors").await;
    app.player.fail_controls();

    app.press_all(&["s", "R"]).await;

    assert_eq!(
        app.app
            .notifications
            .iter()
            .filter(|note| note.title.as_deref() == Some("Playback") && note.level == Level::Error)
            .count(),
        2
    );
}

#[tokio::test]
async fn test_volume_is_a_bar() {
    let mut app = Harness::start("volume-bar").await;
    let modes = app.player_bar_rows()[2].clone();
    assert!(modes.contains("\u{2501}") && modes.contains("\u{25cf}"));
    assert!(modes.contains(&format!("{}%", app.app.player_state.volume)));
    // The block meter is gone, bars and a head in its place.
    assert!(!modes.contains("\u{2587}"));
    // Both mode groups read as a word rather than as one glyph.
    assert!(modes.contains("shuffle") && modes.contains("off"));

    let bar = app.area_of(|hit| matches!(hit, Hit::Volume(_)));
    app.click_at(bar.x + bar.width - 1, bar.y, MouseButton::Left)
        .await;
    assert_eq!(app.app.player_state.volume, 100);
}

#[tokio::test]
async fn test_crumbs_are_upper_case_with_accent_separators() {
    let mut app = Harness::start("crumbs").await;
    app.press_all(&["j", "enter"]).await;
    let crumbs = app.list_row("HOME");
    assert!(crumbs.contains("HOME  \u{203a}  PEAKS"));

    let accent = app.app.theme().accent;
    let buffer = app.terminal.backend().buffer();
    let area = buffer.area();
    let found = (0..area.height)
        .flat_map(|y| (0..area.width).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            buffer
                .cell((x, y))
                .map(|cell| (cell.symbol().to_string(), cell.fg))
        })
        .find(|(symbol, _)| symbol == "\u{203a}")
        .expect("the crumbs row carries a separator");
    assert_eq!(found.1, accent);

    // A crumb is a way back, not only a label.
    app.click(|hit| matches!(hit, Hit::Crumb(0))).await;
    let titles: Vec<&str> = app.app.home.pages().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["Home"]);
}

/// Play the first track, open the lyrics tab and let the timed words land.
async fn lyrics_tab(name: &str) -> Harness {
    let mut app = Harness::start(name).await;
    app.press("enter").await;
    app.press("5").await;
    app
}

// YouTube sends no duration for home rows, artist song shelves or most search
// results, so the app asks for each one and remembers the answer.

#[tokio::test]
async fn test_home_rows_get_durations_filled_in_the_background() {
    let mut setup = Setup::new("home-durations");
    let gate = setup.blank_durations_gated(196);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    let cursor = app.app.home.cursor();
    // The blank the user reported: YouTube served the row with no duration.
    assert!(
        !app.list_row("Alpine Glow").contains(':'),
        "{}",
        app.list_row("Alpine Glow")
    );

    // The fill runs on its own, with nothing on screen saying so.
    gate.add_permits(64);
    app.settle().await;
    assert!(
        app.list_row("Alpine Glow").contains("3:16"),
        "{}",
        app.list_row("Alpine Glow")
    );
    assert_eq!(app.app.home.cursor(), cursor, "the cursor moved");
    assert_eq!(app.source.lengths_asked(), vec!["v1".to_string()]);
    assert_eq!(app.app.durations().get("v1"), Some(196));
}

#[tokio::test]
async fn test_known_durations_show_before_the_network_answers() {
    let (source, gate) = FakeSource::gated();
    let mut setup = Setup::new("known-durations");
    setup.source = source;
    let remembered = Page::new(
        "Home",
        vec![Shelf::new(
            "Remembered",
            vec![Item::Track(Track {
                video_id: "old1".to_string(),
                title: "From last time".to_string(),
                artists: Vec::new(),
                album: None,
                album_browse_id: None,
                duration_seconds: None,
                thumbnail_url: None,
                liked: false,
                playlist_entry: None,
            })],
        )],
    );
    redwax::page_cache::PageCache::new(setup.config.page_cache_dir.clone())
        .put("home", &CompletePage::try_from(remembered).unwrap());
    std::fs::write(redwax::durations::path_for(&setup.config), r#"{"old1":245}"#)
        .expect("the temporary directory is writable");

    let mut app = setup.build();
    app.draw();
    assert!(
        app.list_row("From last time").contains("4:05"),
        "{}",
        app.list_row("From last time")
    );
    gate.add_permits(1);
    app.settle().await;
}

#[tokio::test]
async fn test_leaving_the_page_stops_the_fill() {
    let mut setup = Setup::new("leave-fill");
    let gate = setup.blank_durations_gated(196);
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&TO_PLAYLIST).await;
    assert_eq!(app.app.pages_filling(), 1);

    app.press("escape").await;
    assert_eq!(app.app.pages_filling(), 0, "the fill outlived its page");

    gate.add_permits(64);
    app.settle().await;
    assert!(
        app.source.lengths_asked().is_empty(),
        "{:?}",
        app.source.lengths_asked()
    );
}

#[tokio::test]
async fn test_the_queue_box_fills_the_room_the_sidebar_leaves() {
    let mut setup = Setup::new("queue-box");
    // Tall enough for the whole keys box above it and a queue under that.
    setup.height = 55;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    let queue: Vec<Track> = (0..14)
        .map(|index| Track {
            title: format!("Track {index}"),
            ..long_track()
        })
        .collect();
    let mut state = app.app.player_state.clone();
    state.queue = queue;
    state.index = Some(0);
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.draw();

    let screen = app.screen();
    assert!(screen.contains("\u{250c}\u{2500} UP NEXT "), "no box heading");

    // The count is read off the screen rather than written down, so the test
    // still means something after a change to the rows above the box.
    let shown = (1..=14)
        .take_while(|index| screen.contains(&format!("Track {index}")))
        .count();
    assert!(shown >= 5, "the box showed only {shown} tracks");
    assert!(
        shown <= redwax::ui::sidebar::QUEUE_ROWS,
        "the box showed {shown} tracks, past its cap"
    );
    assert!(
        !screen.contains(&format!("Track {}", shown + 1)),
        "the box ran past the room the sidebar left it"
    );
    let bottom = app.row(app.body_bottom());
    assert!(
        bottom.contains("\u{2514}"),
        "the box does not sit on the sidebar's bottom edge: {bottom:?}"
    );
}

#[tokio::test]
async fn keys_box_keeps_two_columns_without_section_titles() {
    let mut setup = Setup::new("keys-box");
    setup.height = 70;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    let queue: Vec<Track> = (0..14)
        .map(|index| Track {
            title: format!("Track {index}"),
            ..long_track()
        })
        .collect();
    let mut state = app.app.player_state.clone();
    state.queue = queue;
    state.index = Some(0);
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.draw();

    let screen = app.screen();
    assert!(
        screen.contains("\u{250c}\u{2500} KEYS "),
        "no box heading: {screen}"
    );
    assert!(screen.contains("\u{250c}\u{2500} UP NEXT "), "no up next box");

    let keys_top = app.row_of("\u{250c}\u{2500} KEYS ");
    let queue_top = app.row_of("\u{250c}\u{2500} UP NEXT ");
    assert!(keys_top < queue_top, "the keys box is not above up next");

    // Both boxes are inside the sidebar, which is the left of the body.
    for y in [keys_top, queue_top] {
        let row: Vec<char> = app.row(y).chars().collect();
        let left = row.iter().position(|c| *c == '\u{250c}').expect("a left corner");
        let right = row.iter().position(|c| *c == '\u{2510}').expect("a right corner");
        assert!(left < right);
        assert!(
            right < redwax::app::SIDEBAR_WIDTH as usize,
            "the box ran past the sidebar at row {y}"
        );
    }

    assert!(app.row(keys_top + 1).contains("hjkl / arrows"));
    assert!(!screen.contains("Songs & playlists"));
    let queue_hint = app.row_of("q queue");
    assert!(app.row(queue_hint).contains("+ like"));
    assert!(screen.contains("b album"));
}

#[tokio::test]
async fn test_the_keys_bar_returns_when_the_sidebar_hides() {
    let mut setup = Setup::new("keys-bar-narrow");
    setup.width = redwax::app::SIDEBAR_MIN_WIDTH - 1;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    assert!(!app.app.sidebar_visible());

    let height = app.terminal.backend().buffer().area().height;
    let bar = app.row(height - 1);
    assert!(
        bar.contains("hjkl") && !bar.contains("Navigation"),
        "no hint row: {bar:?}"
    );
    assert!(
        !app.screen().contains("KEYS"),
        "the box was drawn with no sidebar"
    );

    // The hint still acts on a click, wherever it is drawn.
    app.click(|hit| matches!(hit, Hit::KeyHint(Action::NextTab)))
        .await;
    assert_eq!(app.app.tab, Tab::Search);
}

#[tokio::test]
async fn test_the_bar_is_one_line_and_carries_everything() {
    let app = Harness::start("one-line").await;
    assert_eq!(redwax::app::HEADER_HEIGHT, 1);

    let bar = app.header_row();
    assert!(bar.contains("redwax"), "no wordmark: {bar:?}");
    for tab in [
        "home", "search", "library", "queue", "lyrics", "themes", "settings",
    ] {
        assert!(bar.contains(tab), "no {tab} tab: {bar:?}");
    }

    // The seam is an underline the terminal draws, so it costs no row: the
    // line under the bar is already the first row of the body.
    assert!(
        !app.row(1).contains('\u{2500}'),
        "a seam row survived under the bar: {:?}",
        app.row(1)
    );
    let styles = app.styles_at(0);
    assert!(
        styles
            .iter()
            .any(|style| style.underline_color == Some(redwax_theme().accent)),
        "the active tab carries no accent underline"
    );
}

#[tokio::test]
async fn test_the_eq_panel_opens_and_sends_a_filter() {
    let mut app = Harness::start("eq-panel").await;
    app.press("e").await;
    assert!(app.app.eq.visible());

    let screen = app.screen();
    for text in ["EQUALISER", "65", "20k", "preset", "flat"] {
        assert!(screen.contains(text), "no {text} in the panel");
    }

    // One press of the gain key is heard at once, so the graph is on its way
    // to mpv before the panel closes.
    app.press("k").await;
    let sent = app.player.filters();
    let last = sent.last().expect("a graph reached mpv");
    assert!(last.contains("superequalizer"), "{last}");
    assert!(last.starts_with("lavfi=[superequalizer=1b=1.10:"), "{last}");
    assert!(
        app.app.eq.settings().enabled,
        "a raised band switches the equaliser on"
    );
    assert_eq!(app.app.eq.settings().preset_name(), "custom");

    // The band keys walk the row, and the panel says which band they are on.
    app.press_all(&["l", "l"]).await;
    assert_eq!(app.app.eq.band(), 2);
    assert!(app.screen().contains("131 Hz"), "{}", app.screen());

    app.press("escape").await;
    assert!(!app.app.eq.visible());
}

#[tokio::test]
async fn test_a_flat_eq_sends_no_filter() {
    let mut app = Harness::start("eq-flat").await;
    assert!(
        app.player.filters().is_empty(),
        "a flat equaliser costs no filter at launch"
    );

    app.press_all(&["e", "space"]).await;
    assert!(app.app.eq.settings().enabled);
    assert!(
        app.player.filters().iter().all(|graph| graph.is_empty()),
        "{:?}",
        app.player.filters()
    );

    // A band up and back down leaves the filter cleared rather than flat.
    app.press_all(&["k", "j"]).await;
    assert!(app.app.eq.settings().is_flat());
    assert_eq!(app.player.filters().last().map(String::as_str), Some(""));
}

#[tokio::test]
async fn test_genre_match_applies_a_preset() {
    let mut setup = Setup::new("eq-genre");
    setup.config.eq_genre_match = true;
    setup.genre = Some("Electronic".to_string());
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.playing();
    app.settle().await;
    assert_eq!(app.app.eq.genre(), Some("Electronic"));
    assert_eq!(app.app.eq.settings().preset_name(), "bass");
    assert!(app.app.eq.settings().enabled);
    let sent = app.player.filters();
    assert!(
        sent.last().is_some_and(|graph| graph.contains("superequalizer")),
        "{sent:?}"
    );

    // Looked up once ever: the answer is on disk before the next run asks.
    assert_eq!(app.app.genres().get("v1"), Some("Electronic".to_string()));

    app.press("e").await;
    let screen = app.screen();
    assert!(screen.contains("Electronic"), "{screen}");
    assert!(screen.contains("bass"), "{screen}");
}

#[tokio::test]
async fn test_a_manual_change_stops_genre_matching() {
    let mut setup = Setup::new("eq-manual");
    setup.config.eq_genre_match = true;
    setup.genre = Some("Electronic".to_string());
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.playing();
    app.settle().await;
    assert!(app.app.eq.genre_match());

    app.press_all(&["e", "k"]).await;
    assert!(!app.app.eq.genre_match(), "a band of the user's own stops it");
    let set = app.app.eq.settings().get(0);

    // The next track leaves what the user set exactly where it is.
    let mut state = app.app.player_state.clone();
    state.index = Some(1);
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.settle().await;
    assert_eq!(app.app.eq.settings().get(0), set);
    assert_eq!(app.app.eq.genre(), None);
    assert!(app.screen().contains("off"), "the panel says it is off");
}

#[tokio::test]
async fn test_the_eq_survives_a_restart() {
    let mut first = Setup::new("eq-restart");
    let dir = first.dir.clone();
    first.config = support::config_in(&dir);
    let mut app = first.build();
    app.draw();

    // `p` walks off flat onto the first real preset.
    app.press_all(&["e", "p"]).await;
    assert_eq!(app.app.eq.settings().preset_name(), "bass");
    app.ticks(1);
    drop(app);

    let mut second = Setup::new("eq-restart-again");
    second.config = support::config_in(&dir);
    second.dir = dir;
    let again = second.build();
    assert_eq!(again.app.eq.settings().preset_name(), "bass");
    assert!(again.app.eq.settings().enabled);
}

#[tokio::test]
async fn test_a_click_and_the_wheel_reach_a_band() {
    let mut app = Harness::start("eq-mouse").await;
    app.press("e").await;

    let column = app.area_of(|hit| matches!(hit, Hit::EqBand(4, _)));
    // The top row of a column is the tallest gain the panel draws.
    app.click_at(column.x, column.y, MouseButton::Left).await;
    assert_eq!(app.app.eq.band(), 4);
    assert!(
        app.app.eq.settings().get(4) > 3.0,
        "{}",
        app.app.eq.settings().get(4)
    );

    let lowered = app.app.eq.settings().get(4);
    app.wheel(column.x, column.y + 1, false).await;
    assert!(app.app.eq.settings().get(4) < lowered);
    app.wheel(column.x, column.y + 1, true).await;
    assert_eq!(app.app.eq.settings().get(4), lowered);
}

#[tokio::test]
async fn test_the_eq_is_a_panel_on_e_again() {
    let mut app = Harness::start("eq-panel-again").await;
    assert!(
        Tab::ALL.iter().all(|tab| tab.label() != "eq"),
        "the equaliser is a tab again"
    );
    assert!(
        !app.header_row().contains(" eq"),
        "an eq tab is still in the bar: {:?}",
        app.header_row()
    );

    app.press("e").await;
    assert!(app.app.eq.visible());
    assert_eq!(app.app.tab, Tab::Home, "the panel left the tab where it was");
    assert!(app.screen().contains("EQUALISER"), "no panel: {}", app.screen());

    // A band raised here reaches mpv, which is what the panel is for.
    app.press("k").await;
    let graph = app
        .player
        .calls()
        .into_iter()
        .find(|call| call.contains("superequalizer"));
    assert!(graph.is_some(), "no filter: {:?}", app.player.calls());

    app.press("e").await;
    assert!(!app.app.eq.visible(), "e did not close the panel");
    assert!(!app.screen().contains("EQUALISER"), "the panel is still painted");
}

#[tokio::test]
async fn test_the_eq_is_a_side_pane_with_a_rule() {
    let mut app = Harness::start("eq-side").await;
    app.press("e").await;

    let screen = app.screen();
    assert!(screen.contains("EQUALISER"), "no pane: {screen:?}");

    // The rule stands between the pane and the page, and the pane stops where
    // its content stops rather than running the height of the screen.
    let rows: Vec<String> = screen.lines().map(str::to_string).collect();
    let with_rule: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains('\u{2502}'))
        .map(|(y, _)| y)
        .collect();
    assert!(!with_rule.is_empty(), "the pane has no rule");
    let last = *with_rule.last().expect("checked above");
    assert!(
        last + 2 < rows.len(),
        "the rule reaches the bottom, so the pane is not a side pane"
    );
    assert!(
        rows[last + 2].trim().is_empty() || !rows[last + 2].contains("EQUALISER"),
        "the pane runs past its content"
    );
}

/// The removed resume shortcut must not feed the restored queue.
#[tokio::test]
async fn removed_x_binding_leaves_the_restored_queue_paused() {
    let mut setup = Setup::new("restored-play");
    let queue: Vec<Track> = support::tracks().into_iter().cycle().take(9).collect();
    setup.saved = SavedState {
        queue,
        index: Some(4),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    assert!(app.player.calls().is_empty(), "mpv was told before a key");

    app.press("x").await;
    assert!(app.player.calls().is_empty());
    assert_eq!(app.app.player_state.index, Some(4));
}

/// The same restore, reached with the pause key, because that is the one a
/// user presses first.
#[tokio::test]
async fn test_space_starts_the_restored_queue_as_well() {
    let mut setup = Setup::new("restored-space");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(1),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    assert!(app.app.player_state.paused);
    assert_eq!(app.app.player_state.position, 42.5);
    assert!(!app.app.motion().playback_moving);

    app.press("space").await;
    assert_eq!(app.player.calls(), vec!["play:1".to_string()]);
    assert_eq!(app.app.player_state.position, 42.5);
    assert_eq!(app.app.saved_state().position, 42.5);
    assert!(!app.app.player_state.paused);
}

#[tokio::test]
async fn restored_playback_stays_paused_through_startup_events() {
    let mut setup = Setup::new("restored-paused-startup");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(1),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    let frame = app.app.screen.animation_frame();

    app.app
        .handle_event(AppEvent::PlayerChanged(redwax_player::PlayerState {
            paused: false,
            loading: true,
            volume: 67,
            ..redwax_player::PlayerState::default()
        }));
    app.ticks(20);

    assert_eq!(app.app.player_state.index, Some(1));
    assert_eq!(app.app.player_state.position, 42.5);
    assert_eq!(app.app.saved_state().position, 42.5);
    assert_eq!(app.app.player_state.volume, 67);
    assert!(app.app.player_state.paused);
    assert!(!app.app.player_state.loading);
    assert!(!app.app.motion().playback_moving);
    assert_eq!(app.app.screen.animation_frame(), frame);
    assert!(app.player.calls().is_empty());
}

#[tokio::test]
async fn pending_native_resolution_keeps_the_restored_queue_and_navigation_available() {
    let mut setup = Setup::new("restored-pending-playback");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(1),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    let restored = app.app.player_state.queue.clone();
    app.app
        .handle_event(AppEvent::PlayerChanged(redwax_player::PlayerState {
            pending_track: Some(redwax_player::bare_track("pending", "Requested song")),
            loading: true,
            ..redwax_player::PlayerState::default()
        }));
    app.draw();

    assert_eq!(app.app.player_state.queue, restored);
    assert_eq!(app.app.player_state.index, Some(1));
    assert_eq!(app.app.saved_state().position, 42.5);
    assert!(app.app.player_state.loading);
    assert!(app.screen().contains("Requested song"));
    assert!(app.screen().contains("Resolving audio"));

    app.press("6").await;
    assert_eq!(app.app.tab, Tab::Themes);
    assert!(app.screen().contains("Resolving audio"));
    assert_eq!(app.app.player_state.queue, restored);

    app.app
        .handle_event(AppEvent::PlayerChanged(redwax_player::PlayerState {
            error: Some("Could not resolve audio. Select the track to retry".to_string()),
            ..redwax_player::PlayerState::default()
        }));
    app.draw();
    assert_eq!(app.app.player_state.queue, restored);
    assert_eq!(app.app.saved_state().position, 42.5);
    assert!(!app.app.player_state.loading);
    assert!(app.app.playback.restored_pending());

    app.app
        .handle_event(AppEvent::PlayerChanged(redwax_player::PlayerState {
            error: Some("Could not resolve audio. Select the track to retry".to_string()),
            ..redwax_player::PlayerState::default()
        }));
    assert_eq!(app.app.player_state.queue, restored);
    assert!(app.app.playback.restored_pending());
}

#[tokio::test]
async fn a_new_playback_warning_becomes_a_toast() {
    fn warning_count(app: &Harness) -> usize {
        app.app
            .notifications
            .iter()
            .filter(|note| note.level == Level::Warning && note.title.as_deref() == Some("Playback"))
            .count()
    }

    let mut app = Setup::new("playback-warning").build();
    let mut state = app.app.player_state.clone();
    state.queue = tracks();
    state.index = Some(0);
    state.warning = Some("Using slower yt-dlp fallback".to_string());

    app.app.handle_event(AppEvent::PlayerChanged(state.clone()));
    assert_eq!(warning_count(&app), 1);

    app.app.handle_event(AppEvent::PlayerChanged(state.clone()));
    assert_eq!(
        warning_count(&app),
        1,
        "the same player update repeated the warning"
    );

    state.index = Some(1);
    app.app.handle_event(AppEvent::PlayerChanged(state));
    assert_eq!(warning_count(&app), 2, "the next selected track did not warn");
}

/// Space resumes the current track even when the list cursor has moved.
#[tokio::test]
async fn space_resumes_current_playback_and_x_does_nothing() {
    let mut app = Harness::start("play-not-cursor").await;
    app.press("enter").await;
    let after_open = app.player.calls();
    assert!(after_open.iter().any(|call| call.starts_with("play:")));

    app.press_all(&["j", "j"]).await;
    app.press("x").await;
    assert_eq!(app.player.calls(), after_open, "play followed the cursor");

    // The cursor does not choose which track Space resumes.
    app.press("space").await;
    assert!(app.app.player_state.paused);
    app.press("x").await;
    assert!(app.app.player_state.paused);
    app.press("space").await;
    assert_eq!(
        app.player.calls().last().map(String::as_str),
        Some("toggle_pause")
    );
    assert!(!app.app.player_state.paused);
}

/// `g` is a prefix, so one press waits and any other key drops it, and both
/// edge keys stay inside the section the cursor is in.
///
/// The fake home page is two shelves: rows 2 and 3 are the first, rows 6 and 7
/// the second.
#[tokio::test]
async fn test_gg_and_capital_g_stay_inside_the_section() {
    let mut app = Harness::start("gg-keys").await;
    let cursor = |app: &Harness| app.app.active_browser().and_then(|b| b.cursor());
    assert_eq!(
        cursor(&app),
        Some(2),
        "the cursor did not start on the first item"
    );

    app.press("j").await;
    assert_eq!(cursor(&app), Some(3));
    app.press("g").await;
    assert_eq!(cursor(&app), Some(3), "one g moved the cursor on its own");
    app.press("g").await;
    assert_eq!(cursor(&app), Some(2), "gg did not reach the top of the section");

    app.press("G").await;
    assert_eq!(cursor(&app), Some(3), "G left the section for the page bottom");

    // A key between the two g's drops the prefix.
    app.press_all(&["g", "j", "g"]).await;
    assert_eq!(cursor(&app), Some(6), "a broken g g still jumped");
}

/// `{` and `}` step between the page's sections, and neither is a dead key at
/// the ends of the run.
#[tokio::test]
async fn test_braces_step_between_sections() {
    let mut app = Harness::start("sections").await;
    let cursor = |app: &Harness| app.app.active_browser().and_then(|b| b.cursor());

    app.press("}").await;
    assert_eq!(
        cursor(&app),
        Some(6),
        "the closing brace did not reach the second section"
    );
    app.press("G").await;
    assert_eq!(cursor(&app), Some(7), "G did not reach the end of that section");

    // Past the last section, `}` runs to its end rather than doing nothing.
    app.press("}").await;
    assert_eq!(cursor(&app), Some(7));

    app.press("{").await;
    assert_eq!(
        cursor(&app),
        Some(2),
        "the opening brace did not reach the first section"
    );
    app.press("{").await;
    assert_eq!(
        cursor(&app),
        Some(2),
        "the opening brace ran off the first section"
    );
}

/// `/` finds on the open page. It moves the cursor as it is typed, enter keeps
/// the row it reached and escape gives the old one back.
#[tokio::test]
async fn test_slash_finds_on_the_open_page() {
    let mut app = Harness::start("find-page").await;
    let cursor = |app: &Harness| app.app.active_browser().and_then(|b| b.cursor());
    let start = cursor(&app);

    app.press("/").await;
    assert!(
        matches!(app.app.prompt(), Some(Prompt::Find(_))),
        "the find bar did not open"
    );
    assert_eq!(app.app.tab, Tab::Home, "the find left the page");

    app.type_text("late night").await;
    app.draw();
    assert!(app.screen().contains("/late night"), "{}", app.screen());
    assert_eq!(cursor(&app), Some(6), "the find did not cross into the playlist");

    app.press("escape").await;
    assert!(
        !matches!(app.app.prompt(), Some(Prompt::Find(_))),
        "the bar stayed open"
    );
    assert_eq!(cursor(&app), start, "escape did not give the row back");

    // Enter keeps what the find reached.
    app.press("/").await;
    app.type_text("late night").await;
    app.press("enter").await;
    assert!(!matches!(app.app.prompt(), Some(Prompt::Find(_))));
    assert_eq!(cursor(&app), Some(6));

    // A query nothing holds says so and leaves the cursor where it was.
    app.press("/").await;
    app.type_text("zzzz").await;
    app.draw();
    assert!(app.screen().contains("no match"), "{}", app.screen());
    app.press("escape").await;
}

#[tokio::test]
async fn control_n_and_p_cycle_find_matches_without_editing_the_query() {
    let mut app = Harness::start("find-cycle").await;

    app.press("/").await;
    app.type_text("peaks").await;
    assert!(matches!(
        app.app.prompt(),
        Some(Prompt::Find(find)) if find.query == "peaks" && find.match_position == Some((1, 2))
    ));

    app.press("ctrl+n").await;
    assert!(matches!(
        app.app.prompt(),
        Some(Prompt::Find(find)) if find.query == "peaks" && find.match_position == Some((2, 2))
    ));

    app.press("ctrl+p").await;
    assert!(matches!(
        app.app.prompt(),
        Some(Prompt::Find(find)) if find.query == "peaks" && find.match_position == Some((1, 2))
    ));
}

/// The scrollbar says where in a long list the view is, in a weight the eye
/// can find against its track.
#[tokio::test]
async fn test_the_scrollbar_marks_where_the_list_is() {
    let mut setup = Setup::new("scrollbar");
    setup.height = 30;
    let queue: Vec<Track> = support::tracks().into_iter().cycle().take(60).collect();
    setup.saved = SavedState {
        queue,
        index: Some(0),
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    app.press("q").await;
    app.press_all(&["g", "g"]).await;
    app.draw();

    let thumb_rows = |app: &Harness| -> Vec<u16> {
        (0..app.terminal.backend().buffer().area().height)
            .filter(|y| app.row(*y).contains('\u{2503}'))
            .collect()
    };
    let top = thumb_rows(&app);
    assert!(!top.is_empty(), "no thumb: {}", app.screen());
    assert!(app.screen().contains('\u{2502}'), "no track behind the thumb");

    app.press("G").await;
    app.draw();
    let bottom = thumb_rows(&app);
    assert!(
        bottom.first() > top.first(),
        "the thumb did not move: {top:?} then {bottom:?}"
    );
}

/// `r` puts the panel back to the state it ships in: every band flat and the
/// equaliser off, with the filter cleared out of mpv.
#[tokio::test]
async fn test_r_resets_the_eq_to_default() {
    let mut app = Harness::start("eq-reset").await;
    app.press("e").await;
    app.press("p").await;
    assert!(app.app.eq.settings().enabled);
    assert!(!app.app.eq.settings().is_flat(), "a preset left the bands flat");
    assert_ne!(app.app.eq.settings().preset_name(), "flat");

    app.press("r").await;
    assert!(app.app.eq.settings().is_flat(), "r left a band up");
    assert!(!app.app.eq.settings().enabled, "r left the equaliser on");
    assert_eq!(app.player.filters().last().map(String::as_str), Some(""));
    assert!(app.app.eq.visible(), "r closed the panel");
    assert!(app.screen().contains("r reset"), "{}", app.screen());
}

/// The seek head follows the pointer while the button is held, and mpv is
/// asked once, on release, rather than once per column crossed.
#[tokio::test]
async fn test_the_seek_bar_is_draggable() {
    let mut app = Harness::start("seek-drag").await;
    app.press("enter").await;
    app.at_position(0.0).await;
    let bar = app.area_of(|hit| matches!(hit, Hit::SeekBar(_)));

    let before = app.player.calls().len();
    let quarter = bar.x + bar.width / 4;
    let middle = bar.x + bar.width / 2;
    app.app.handle_mouse(support::mouse(
        bar.x,
        bar.y,
        MouseEventKind::Down(MouseButton::Left),
    ));
    app.app.handle_mouse(support::mouse(
        quarter,
        bar.y,
        MouseEventKind::Drag(MouseButton::Left),
    ));
    app.draw();
    assert_eq!(
        app.player.calls().len(),
        before,
        "the drag reached mpv before the release"
    );
    assert!(app.app.scrub.is_some(), "the bar was not taken hold of");

    // The head is drawn where the pointer is, ahead of the player's position.
    let row = app.row(bar.y);
    let head = row.find('\u{25cf}').expect("a head on the bar");
    assert!(head > bar.x as usize, "the head did not follow: {row:?}");

    app.app.handle_mouse(support::mouse(
        middle,
        bar.y,
        MouseEventKind::Up(MouseButton::Left),
    ));
    app.settle().await;
    assert!(app.app.scrub.is_none(), "the bar was still held after release");
    let seeked = app.player.calls().last().cloned().unwrap_or_default();
    assert!(seeked.starts_with("seek_to:"), "no seek: {seeked}");
    let seconds: f64 = seeked["seek_to:".len()..].parse().expect("a number");
    let duration = app.app.player_state.duration.expect("a duration");
    assert!(
        (seconds - duration / 2.0).abs() < duration / 10.0,
        "released at the middle but seeked to {seconds} of {duration}"
    );
}

/// A drag past the end of the bar parks the head at the end rather than
/// running off it.
#[tokio::test]
async fn test_a_drag_past_the_bar_stops_at_the_end() {
    let mut app = Harness::start("seek-drag-past").await;
    app.press("enter").await;
    app.at_position(0.0).await;
    let bar = app.area_of(|hit| matches!(hit, Hit::SeekBar(_)));

    app.drag(bar.y, &[bar.x, bar.x + bar.width / 2, bar.x + bar.width * 3])
        .await;
    let seeked = app.player.calls().last().cloned().unwrap_or_default();
    let seconds: f64 = seeked["seek_to:".len()..].parse().expect("a number");
    let duration = app.app.player_state.duration.expect("a duration");
    assert!(
        seconds <= duration,
        "seeked past the end: {seconds} of {duration}"
    );
    assert!(
        seconds > duration * 0.8,
        "the head did not reach the end: {seconds}"
    );
}

#[tokio::test]
async fn resizing_during_a_seek_drag_cancels_the_stale_slider() {
    let mut app = Harness::start("seek-drag-resize").await;
    app.press("enter").await;
    app.at_position(0.0).await;
    let bar = app.area_of(|hit| matches!(hit, Hit::SeekBar(_)));
    let before = app.player.calls().len();

    app.app.handle_mouse(support::mouse(
        bar.x,
        bar.y,
        MouseEventKind::Down(MouseButton::Left),
    ));
    assert!(app.app.scrub.is_some());

    app.terminal.backend_mut().resize(100, 45);
    app.draw();
    app.app.handle_mouse(support::mouse(
        bar.x + bar.width - 1,
        bar.y,
        MouseEventKind::Up(MouseButton::Left),
    ));
    app.settle().await;

    assert!(app.app.scrub.is_none());
    assert_eq!(
        app.player.calls().len(),
        before,
        "a release against the old layout reached mpv"
    );
}

#[tokio::test]
async fn playback_buttons_only_appear_in_the_bottom_player_bar() {
    let mut setup = Setup::new("bottom-transport-only");
    setup.height = 55;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press("enter").await;
    let body_bottom = app.terminal.backend().buffer().area().height - redwax::ui::layout::PLAYER_HEIGHT;
    for (area, hit) in app.app.screen.hit_map().regions() {
        if matches!(
            hit,
            Hit::PlayPause | Hit::Previous | Hit::Next | Hit::Shuffle | Hit::Repeat
        ) {
            assert!(area.y >= body_bottom);
        }
    }
    app.click(|hit| matches!(hit, Hit::PlayPause)).await;
    assert!(app.player.calls().iter().any(|call| call == "toggle_pause"));
}

/// Repeating the queue means the tracks at the top play after the last one, so
/// up next has to wrap rather than count down to nothing.
#[tokio::test]
async fn test_up_next_wraps_when_the_queue_repeats() {
    let mut setup = Setup::new("upnext-wrap");
    setup.height = 55;
    let queue: Vec<Track> = (0..12)
        .map(|index| Track {
            title: format!("Track {index}"),
            video_id: format!("v{index}"),
            ..long_track()
        })
        .collect();
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    let show = |app: &mut Harness, index: usize, repeat: Repeat| {
        let mut state = app.app.player_state.clone();
        state.queue = queue.clone();
        state.index = Some(index);
        state.repeat = repeat;
        app.app.handle_event(AppEvent::PlayerChanged(state));
        app.draw();
    };

    // Two from the end, with repeat off, only what is left is coming.
    show(&mut app, 10, Repeat::Off);
    assert_eq!(app.app.player_state.upcoming(20).len(), 1);
    let screen = app.screen();
    assert!(screen.contains("Track 11"), "the last track is not listed");
    assert!(!screen.contains("Track 0"), "the queue wrapped with repeat off");

    // The same place with repeat on: the top of the queue is what plays next.
    show(&mut app, 10, Repeat::All);
    let upcoming = app.app.player_state.upcoming(20);
    assert_eq!(upcoming.len(), 11, "the wrap did not fill the list");
    assert_eq!(upcoming[0].video_id, "v11");
    assert_eq!(upcoming[1].video_id, "v0", "the wrap did not reach the top");
    assert_ne!(
        upcoming.iter().position(|t| t.video_id == "v10"),
        Some(0),
        "the playing track came back round too early"
    );
    let screen = app.screen();
    assert!(screen.contains("Track 0"), "the box did not wrap: {screen}");

    // The list stays the same length as the index walks, which is the bug.
    for index in [11, 0, 1] {
        show(&mut app, index, Repeat::All);
        assert_eq!(
            app.app.player_state.upcoming(20).len(),
            11,
            "up next shrank at index {index}"
        );
    }
}

/// Tabbing into search must not swallow the next key. It used to focus the box
/// on arrival, which left `l` and `h` typing into it with no way back out.
#[tokio::test]
async fn test_tabbing_into_search_does_not_trap_the_keyboard() {
    let mut app = Harness::start("search-trap").await;
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Search);
    assert!(!app.app.search_focused(), "the box took the keyboard on arrival");

    // The tab keys still work, which is the whole of the bug.
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Library);
    app.press("h").await;
    assert_eq!(app.app.tab, Tab::Search);

    // Enter on a page with nothing to open is enter on the bar. Vim editing
    // starts ready to insert, then escape changes mode before it leaves.
    app.press("enter").await;
    assert!(app.app.search_focused());
    assert!(app.screen().contains("INSERT"));
    app.type_text("snow").await;
    assert_eq!(app.app.search_query, "snow");
    app.press("escape").await;
    assert!(app.app.search_focused(), "insert escape left the box");
    assert!(app.screen().contains("NORMAL"));
    app.press("escape").await;
    assert!(!app.app.search_focused(), "escape did not leave the box");
    app.press("l").await;
    assert_eq!(app.app.tab, Tab::Library, "the keys did not come back");

    // `/` reaches the box from anywhere on the search tab, results or not.
    app.press("h").await;
    app.press("/").await;
    assert!(app.app.search_focused(), "slash did not reach the box");
    assert!(
        !matches!(app.app.prompt(), Some(Prompt::Find(_))),
        "slash opened the find bar over the box"
    );
    app.press("enter").await;
    assert!(app.source.called("search:snow"));
    assert!(
        !app.app.search_focused(),
        "the box kept the keyboard after a search"
    );

    // With results on the page, enter plays a row rather than the box, and `/`
    // is the way back to the query.
    app.press("enter").await;
    assert!(
        app.player.calls().iter().any(|call| call.starts_with("play:")),
        "{:?}",
        app.player.calls()
    );
    app.press("/").await;
    assert!(app.app.search_focused());
}

#[tokio::test]
async fn vim_search_edits_the_query_and_submits_from_insert_mode() {
    let mut app = Harness::start("search-vim-editing").await;
    app.press_all(&["2", "enter"]).await;
    assert!(app.screen().contains("INSERT"));

    app.type_text("snowline").await;
    app.press_all(&["escape", "0", "x", "A"]).await;
    assert_eq!(app.app.search_query, "nowline");
    assert!(app.app.search_focused());
    assert!(app.screen().contains("INSERT"));

    app.type_text("s").await;
    app.press("enter").await;
    assert!(app.source.called("search:nowlines"));
    assert!(!app.app.search_focused());
}

#[tokio::test]
async fn a_long_search_query_scrolls_with_the_vim_cursor() {
    let mut setup = Setup::new("search-cursor-scroll");
    setup.width = 48;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    app.press_all(&["2", "enter"]).await;

    let query = format!("START{}END", "界".repeat(24));
    app.type_text(&query).await;
    app.press_all(&["escape", "0"]).await;
    let at_start = app.row(redwax::app::HEADER_HEIGHT);
    assert!(at_start.contains("NORMAL"), "the mode is missing: {at_start:?}");
    assert!(
        at_start.contains("START"),
        "the query start is hidden: {at_start:?}"
    );
    assert!(
        !at_start.contains("END"),
        "both ends fit without scrolling: {at_start:?}"
    );

    app.press("$").await;
    let at_end = app.row(redwax::app::HEADER_HEIGHT);
    assert!(at_end.contains("NORMAL"), "the mode is missing: {at_end:?}");
    assert!(at_end.contains("END"), "the query end is hidden: {at_end:?}");
    assert!(
        !at_end.contains("START"),
        "the cursor did not scroll the query: {at_end:?}"
    );
}

#[tokio::test]
async fn standard_search_escape_immediately_returns_to_page_controls() {
    let mut setup = Setup::new("search-standard-editing");
    setup.config.search_vim_mode = false;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press_all(&["2", "enter"]).await;
    assert!(app.screen().contains("TYPE"));
    app.type_text("snow").await;
    app.press("escape").await;

    assert_eq!(app.app.search_query, "snow");
    assert!(!app.app.search_focused());
}

#[tokio::test]
async fn search_navigation_keys_return_control_to_the_results() {
    let mut app = Harness::start("search-navigation-leaves-editor").await;
    app.press_all(&["2", "enter"]).await;
    app.type_text("snow").await;
    app.press("enter").await;
    assert_eq!(app.app.search.selected(), Some((0, 0)));

    app.press("/").await;
    app.press("down").await;
    assert!(!app.app.search_focused());
    assert_eq!(app.app.search.selected(), Some((0, 1)));

    app.press("/").await;
    app.press("up").await;
    assert!(!app.app.search_focused());
    assert_eq!(app.app.search.selected(), Some((0, 0)));

    app.press("/").await;
    app.press("tab").await;
    assert!(!app.app.search_focused());
    assert_eq!(app.app.tab, Tab::Search);
}

/// The box is the first row of the pane, above the crumbs.
#[tokio::test]
async fn test_the_search_box_is_the_top_row() {
    let mut app = Harness::start("search-top").await;
    app.press("2").await;
    app.draw();
    let box_area = app.area_of(|hit| matches!(hit, Hit::Search));
    let crumbs = app.row_of("SEARCH");
    assert!(app.screen().contains("/ search youtube music"));
    assert!(
        box_area.y < crumbs,
        "the box is under the crumbs: {} then {crumbs}",
        box_area.y
    );
    // Nothing but the header sits above it.
    assert_eq!(
        box_area.y,
        redwax::app::HEADER_HEIGHT,
        "the box is not the first row of the pane"
    );

    app.click(|hit| matches!(hit, Hit::Search)).await;
    assert!(app.app.search_focused());
    assert!(app.screen().contains("INSERT"));
}

/// The home feed puts songs, playlists and artists in one shelf. Grouping them
/// by kind keeps the columns meaning one thing at a time.
#[tokio::test]
async fn test_the_top_home_shelf_groups_its_kinds() {
    let (source, gate) = FakeSource::gated();
    let mut setup = Setup::new("home-grouped");
    setup.source = source;
    // Deliberately interleaved, which is what YouTube sends.
    let mixed = Page::new(
        "Home",
        vec![
            Shelf::new(
                "Listen again",
                vec![
                    Item::Artist(support::artist()),
                    Item::Track(support::tracks()[0].clone()),
                    Item::Playlist(support::playlist()),
                    Item::Track(support::tracks()[1].clone()),
                    Item::Album(support::album()),
                ],
            ),
            // A second shelf keeps the order it arrived in.
            Shelf::new(
                "Mixed for you",
                vec![
                    Item::Artist(support::artist()),
                    Item::Track(support::tracks()[2].clone()),
                ],
            ),
        ],
    );
    redwax::page_cache::PageCache::new(setup.config.page_cache_dir.clone())
        .put("home", &CompletePage::try_from(mixed).unwrap());

    let mut app = setup.build();
    app.draw();
    let screen = app.screen();
    drop(gate);

    let row_of = |text: &str| {
        screen
            .lines()
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("no row for {text} in {screen}"))
    };
    // Songs first, then the album, then the playlist, then the artist.
    let alpine = row_of("Alpine Glow");
    let snowline = row_of("Snowline");
    let album = row_of("Album \u{b7}");
    let playlist = row_of("Playlist");
    assert!(alpine < snowline, "the songs lost their own order");
    assert!(snowline < album, "the album came before a song");
    assert!(album < playlist, "the playlist came before the album");

    // The second shelf is untouched: its artist still leads.
    let second = row_of("MIXED FOR YOU");
    let foehn = row_of("Foehn Wind");
    assert!(second < foehn);
    let artist_rows: Vec<usize> = screen
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("Arnegger") && line.contains("Artist"))
        .map(|(index, _)| index)
        .collect();
    assert!(
        artist_rows.iter().any(|row| *row > second && *row < foehn),
        "the second shelf was grouped too: {artist_rows:?}"
    );
}

/// A playlist row and a track row share one set of columns, so the hairlines
/// line up and nothing runs under the next heading.
#[tokio::test]
async fn test_browsable_rows_sit_on_the_track_columns() {
    let mut app = Harness::start("row-columns").await;
    app.draw();
    let screen = app.screen();
    let seams = |needle: &str| -> Vec<usize> {
        let line = screen
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle} in {screen}"));
        line.chars()
            .enumerate()
            .filter(|(_, c)| *c == '\u{2502}')
            .map(|(index, _)| index)
            .collect()
    };
    let header = seams("TITLE");
    let track = seams("Alpine Glow");
    let album = seams("Album \u{b7}");
    let playlist = seams("Late night");
    assert!(!header.is_empty(), "no column header to line up with");
    assert_eq!(track, header, "a track row is off the header's columns");
    assert_eq!(album, header, "an album row is off the header's columns");
    assert_eq!(playlist, header, "a playlist row is off the header's columns");

    // Each column says one thing: the kind lives under ALBUM, not in the name.
    let line = screen
        .lines()
        .find(|line| line.contains("Late night"))
        .expect("a playlist row");
    let start = line.find("Playlist").expect("the kind is named");
    let column = line[..start].chars().count();
    assert!(
        column > playlist[1] && column < playlist[2],
        "the kind is not in the last column: {line:?}, seams {playlist:?}, at {column}"
    );
}

/// The queue is a window that moves. Five played tracks stay above the playing
/// one, so it is the sixth row and a skip pops one off the top.
#[tokio::test]
async fn test_the_queue_is_a_window_that_moves() {
    let mut setup = Setup::new("queue-window");
    setup.height = 55;
    let queue: Vec<Track> = (0..30)
        .map(|index| Track {
            title: format!("Track {index}"),
            video_id: format!("v{index}"),
            ..long_track()
        })
        .collect();
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    let at = |app: &mut Harness, index: usize| {
        let mut state = app.app.player_state.clone();
        state.queue = queue.clone();
        state.index = Some(index);
        app.app.handle_event(AppEvent::PlayerChanged(state));
        app.app.handle_key(support::parse_key("4"));
        app.draw();
    };
    let items = |app: &Harness| -> Vec<String> {
        app.app.queue.top().expect("a queue page").shelves[0]
            .items
            .iter()
            .map(|item| match item {
                Item::Track(track) => track.title.clone(),
                _ => String::new(),
            })
            .collect()
    };

    // Near the top there is nothing to leave behind, so the list starts there.
    at(&mut app, 2);
    assert_eq!(items(&app)[0], "Track 0");
    assert_eq!(app.app.first_queue_index, 0);

    // Past the fifth track the playing one settles at row six and stays there.
    at(&mut app, 12);
    assert_eq!(app.app.first_queue_index, 7);
    let shown = items(&app);
    assert_eq!(shown[0], "Track 7", "the window did not move up");
    assert_eq!(shown[5], "Track 12", "the playing track is not the sixth row");
    assert_eq!(shown.last().map(String::as_str), Some("Track 29"));

    // What has played is drawn flat, so the eye finds the playing row without
    // reading the list.
    let colour_of = |app: &Harness, title: &str| {
        let buffer = app.terminal.backend().buffer();
        let y = app.row_of(title);
        let x = (0..buffer.area().width)
            .find(|x| buffer.cell((*x, y)).is_some_and(|c| c.symbol() == "T"))
            .expect("the title starts somewhere");
        buffer.cell((x, y)).expect("a cell").fg
    };
    let played = colour_of(&app, "Track 9 ");
    let playing = colour_of(&app, "Track 12 ");
    let coming = colour_of(&app, "Track 20 ");
    assert_ne!(played, coming, "a played row looks like one still to come");
    assert_ne!(playing, coming, "the playing row looks like the rest");
    assert_ne!(playing, played, "the playing row looks like a played one");

    // A skip pops the top and adds one at the bottom.
    at(&mut app, 13);
    let after = items(&app);
    assert_eq!(after[0], "Track 8", "the top did not pop");
    assert_eq!(after[5], "Track 13", "the playing track moved off row six");
    assert_eq!(after.len() + 1, shown.len(), "the list did not lose a row");
}

/// A queue can hold the same track twice, and only the copy that is playing
/// carries the marker. Marking by video id lit every copy of it.
#[tokio::test]
async fn test_only_the_playing_copy_is_marked() {
    let mut setup = Setup::new("queue-duplicates");
    setup.height = 55;
    let one = Track {
        title: "Twice Over".to_string(),
        video_id: "dup".to_string(),
        ..long_track()
    };
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;
    let mut state = app.app.player_state.clone();
    state.queue = vec![one.clone(), one.clone(), one.clone()];
    state.index = Some(1);
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.press("4").await;
    app.draw();

    let icon = redwax::icons::icon_set("nerd").play;
    // A paused transport shares this icon, so count only the table rows.
    let marked = app
        .screen()
        .lines()
        .filter(|line| line.contains('│') && line.contains(icon) && line.contains("Twice Over"))
        .count();
    assert_eq!(marked, 1, "{}", app.screen());

    // Playing the second copy names entry one, not entry zero.
    app.app.queue.select_row(Some(2));
    app.press("enter").await;
    let last = app.player.calls().last().cloned().unwrap_or_default();
    assert!(
        last == "jump:1" || last == "play:1",
        "the second copy did not name entry one: {last}"
    );
}

#[tokio::test]
async fn moving_a_duplicate_queue_entry_keeps_its_identity_and_playback_progress() {
    let mut app = Harness::start("queue-move-duplicate").await;
    let duplicate = Track {
        title: "Twice Over".to_string(),
        video_id: "dup".to_string(),
        ..long_track()
    };
    app.playing_queue(vec![duplicate.clone(), duplicate, tracks()[0].clone()], 0);
    app.at_position(42.5).await;
    app.press("4").await;
    app.app.queue.select_row(Some(2));
    let selected = app.app.player_state.queue_entries[1].id();

    app.press("K").await;

    assert_eq!(app.app.player_state.queue_entries[0].id(), selected);
    assert_eq!(app.app.player_state.position, 42.5);
    assert_eq!(app.app.player_state.index, Some(1));
    assert!(app.player.calls().contains(&"move:1:0".to_string()));
}

#[tokio::test]
async fn removing_the_selected_last_queue_entry_selects_its_predecessor() {
    let mut app = Harness::start("queue-remove-selected-last").await;
    app.playing_queue(tracks(), 0);
    app.press("4").await;
    app.app.queue.select_row(Some(3));
    assert_eq!(app.app.queue.selected(), Some((0, 2)));

    let mut state = app.app.player_state.clone();
    state.queue.pop();
    state.queue_entries.pop();
    app.app.handle_event(AppEvent::PlayerChanged(state));

    assert_eq!(app.app.queue.selected(), Some((0, 1)));
}

#[tokio::test]
async fn a_stale_queue_move_does_not_redirect_to_the_new_row() {
    let mut app = Harness::start("queue-move-stale").await;
    app.playing_queue(tracks(), 0);
    app.press("4").await;
    app.app.queue.select_row(Some(2));

    app.app.handle_key(support::parse_key("K"));
    app.player.external_revision();
    app.settle().await;

    assert!(!app.player.calls().iter().any(|call| call.starts_with("move:")));
    assert!(app.screen().contains("no longer present"));
}

/// `v` opens a field on the volume readout. Enter sets what was typed, escape
/// leaves the level alone, and nothing reaches mpv in between.
#[tokio::test]
async fn test_v_types_a_volume() {
    let mut app = Harness::start("volume-typed").await;
    let before = app.app.player_state.volume;

    app.press("v").await;
    assert_eq!(
        volume_entry(&app).as_deref(),
        Some(before.to_string().as_str()),
        "the field did not start at the current level"
    );
    app.draw();
    assert!(app.screen().contains("enter, escape"), "{}", app.screen());

    // Escape leaves it where it was.
    app.press("escape").await;
    assert!(app.app.prompt().is_none());
    assert_eq!(app.app.player_state.volume, before);

    app.press("v").await;
    app.press_all(&["backspace", "backspace", "backspace"]).await;
    app.press_all(&["3", "5"]).await;
    assert_eq!(volume_entry(&app).as_deref(), Some("35"));
    // Letters are not volumes, and neither is a fourth digit.
    app.press_all(&["z", "9", "9"]).await;
    assert_eq!(volume_entry(&app).as_deref(), Some("359"));
    app.press("enter").await;
    assert!(app.app.prompt().is_none());
    // Over a hundred is a hundred.
    assert_eq!(app.app.player_state.volume, 100);
}

#[tokio::test]
async fn closing_a_modal_restores_the_search_editor_focus() {
    let mut app = Harness::start("modal-restores-search-focus").await;
    app.press_all(&["2", "enter"]).await;
    assert!(app.app.search_focused());
    app.app.handle_event(AppEvent::Notify {
        text: "A message".to_string(),
        title: Some("Test".to_string()),
        level: Level::Info,
    });
    app.draw();

    app.click(|hit| matches!(hit, Hit::Notification(_))).await;
    assert!(!app.app.search_focused());

    app.press("escape").await;
    assert!(app.app.search_focused());
}

/// The volume bar is the same held component as the seek bar: the head follows
/// the pointer and mpv hears about it once, on release.
#[tokio::test]
async fn test_the_volume_bar_is_draggable() {
    let mut app = Harness::start("volume-drag").await;
    app.draw();
    let bar = app.area_of(|hit| matches!(hit, Hit::Volume(_)));
    let before = app.player.calls().len();

    app.app.handle_mouse(support::mouse(
        bar.x,
        bar.y,
        MouseEventKind::Down(MouseButton::Left),
    ));
    app.app.handle_mouse(support::mouse(
        bar.x + bar.width - 1,
        bar.y,
        MouseEventKind::Drag(MouseButton::Left),
    ));
    app.draw();
    assert_eq!(app.player.calls().len(), before, "the drag reached mpv early");
    assert!(
        app.screen().contains("100%"),
        "the bar did not follow: {}",
        app.screen()
    );

    app.app.handle_mouse(support::mouse(
        bar.x + bar.width - 1,
        bar.y,
        MouseEventKind::Up(MouseButton::Left),
    ));
    app.settle().await;
    assert!(app.app.scrub.is_none());
    assert_eq!(app.app.player_state.volume, 100, "{:?}", app.player.calls());
}

/// mpv can report a position past the end of its own playlist: a removal
/// shortens the queue before the new position arrives. The window has to
/// survive that, because a start past the end is a panic and not a bad page.
#[tokio::test]
async fn test_the_queue_survives_a_position_past_the_end() {
    let mut setup = Setup::new("queue-past-end");
    setup.height = 40;
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    for (queue, index) in [(3usize, 15usize), (1, 99), (3, 3), (2, 0)] {
        let mut state = app.app.player_state.clone();
        state.queue = support::tracks().into_iter().cycle().take(queue).collect();
        state.index = Some(index);
        app.app.handle_event(AppEvent::PlayerChanged(state));
        app.press("4").await;
        app.draw();
        assert!(
            app.app.first_queue_index < queue,
            "the window started past the queue: from {} of {queue}",
            app.app.first_queue_index
        );
    }
}

/// The themes tab shows what this machine can offer, previews on the cursor,
/// and keeps a choice only when asked.
#[tokio::test]
async fn test_the_themes_tab_previews_and_keeps() {
    let mut setup = Setup::new("themes-tab");
    setup.height = 44;
    setup.config.theme = "tokyo-night".to_string();
    setup.config.source = setup.dir.join("config.toml");
    let mut app = setup.build();
    app.draw();
    app.app.start_background();
    app.settle().await;

    app.press("6").await;
    assert_eq!(app.app.tab, Tab::Themes);
    app.draw();
    let screen = app.screen();
    for text in [
        "terminal",
        "nord",
        "gruvbox-dark",
        "built in",
        "follows the terminal",
    ] {
        assert!(screen.contains(text), "no {text} in the tab: {screen}");
    }

    // The cursor starts on what the config asks for.
    let pane = app.app.themes.as_ref().expect("the tab opened a pane");
    assert_eq!(
        pane.selected().map(|theme_id| theme_id.name()),
        Some("tokyo-night")
    );

    // Moving repaints in the theme under the cursor without keeping it.
    app.press("j").await;
    let previewed = app.app.base_theme.name.clone();
    assert_ne!(previewed, "tokyo-night", "moving the cursor did not preview");
    assert_eq!(
        app.app.deps.config.theme, "tokyo-night",
        "a look around was written down"
    );

    // Leaving without choosing puts back what the config asks for.
    app.press("1").await;
    assert_eq!(app.app.tab, Tab::Home);
    assert_eq!(
        app.app.base_theme.name, "tokyo-night",
        "leaving did not put the chosen theme back"
    );

    // Enter keeps it, in the config file as well as on screen.
    app.press("6").await;
    app.press_all(&["j", "j"]).await;
    let selected_theme = app
        .app
        .themes
        .as_ref()
        .and_then(|pane| pane.selected())
        .expect("a theme under the cursor");
    let wanted_name = selected_theme.name().to_owned();
    let wanted = selected_theme.persisted();
    app.press("enter").await;
    assert_eq!(app.app.deps.config.theme, wanted);
    let written = std::fs::read_to_string(&app.app.deps.config.source).expect("the config was written");
    assert!(
        written.contains(&format!("theme = \"{wanted}\"")),
        "the choice is not in the file: {written}"
    );

    // And now leaving keeps it, because it was chosen.
    app.press("1").await;
    assert_eq!(app.app.base_theme.name, wanted_name, "a kept theme was put back");
}

#[tokio::test]
async fn settings_toggle_changes_search_editing_and_preserves_the_live_config() {
    let mut setup = Setup::new("settings-search-mode");
    setup.config.source = setup.dir.join("config.toml");
    std::fs::write(
        &setup.config.source,
        "volume = 23\n\n[mpv]\nargs = [\"--quiet\"]\n",
    )
    .expect("the config is writable");
    let mut app = setup.build();
    app.draw();

    app.press("7").await;
    assert_eq!(app.app.tab, Tab::Settings);
    assert!(app.screen().contains("Vim"));

    app.press("enter").await;
    assert!(!app.app.deps.config.search_vim_mode);
    let written = std::fs::read_to_string(&app.app.deps.config.source).expect("the config was written");
    assert!(
        written.contains("volume = 23"),
        "the live config was replaced: {written}"
    );
    assert!(
        written.contains("args = [\"--quiet\"]"),
        "the mpv table was replaced: {written}"
    );
    assert!(
        written.contains("search_vim_mode = false"),
        "the setting was not written: {written}"
    );
    assert!(
        app.app.notifications.iter().any(|note| {
            note.title.as_deref() == Some("Settings") && note.text == "Search editing: Standard"
        }),
        "no success notification: {:?}",
        app.app.notifications
    );

    app.press("space").await;
    assert!(app.app.deps.config.search_vim_mode);
    let written = std::fs::read_to_string(&app.app.deps.config.source).expect("the config was written");
    assert!(
        written.contains("search_vim_mode = true"),
        "the setting was not updated: {written}"
    );
}

#[tokio::test]
async fn settings_search_toggle_works_with_the_mouse() {
    let mut setup = Setup::new("settings-search-mode-mouse");
    setup.config.source = setup.dir.join("config.toml");
    let mut app = setup.build();
    app.draw();

    app.press("7").await;
    app.click(|hit| matches!(hit, Hit::ToggleSearchVim)).await;

    assert!(!app.app.deps.config.search_vim_mode);
    let written = std::fs::read_to_string(&app.app.deps.config.source).expect("the config was written");
    assert!(written.contains("search_vim_mode = false"));
}

#[tokio::test]
async fn a_failed_settings_write_keeps_the_active_search_mode() {
    let mut setup = Setup::new("settings-search-mode-error");
    setup.config.source = setup.dir.clone();
    let mut app = setup.build();
    app.draw();

    app.press_all(&["7", "enter"]).await;

    assert!(app.app.deps.config.search_vim_mode);
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| { note.title.as_deref() == Some("Settings") && note.level == Level::Error }),
        "no write error notification: {:?}",
        app.app.notifications
    );
}

#[tokio::test]
async fn test_enter_on_the_restored_queue_starts_the_saved_track() {
    let mut setup = Setup::new("restored-enter");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(1),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    assert_eq!(app.app.tab, Tab::Queue);
    app.press("enter").await;
    assert_eq!(app.player.calls(), vec!["play:1".to_string()]);
    assert_eq!(app.app.player_state.position, 42.5);
}

#[tokio::test]
async fn selecting_another_restored_queue_track_starts_at_the_beginning() {
    let mut setup = Setup::new("restored-other-enter");
    setup.saved = SavedState {
        queue: support::tracks(),
        index: Some(0),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.press_all(&["j", "enter"]).await;

    assert_eq!(app.player.calls(), vec!["play:1".to_string()]);
    assert_eq!(app.app.player_state.position, 0.0);
}

#[tokio::test]
async fn test_radio_tracks_can_be_selected_and_saved_to_a_playlist() {
    let mut app = Harness::start("radio-playlist").await;
    app.press("r").await;
    assert_eq!(app.app.tab, Tab::Home);
    app.press_all(&["q", "j", "ctrl+p", "enter"]).await;
    assert_eq!(
        app.source.playlist_edits(),
        vec!["add_to_playlist:PL1:v2".to_string()]
    );
}

#[tokio::test]
async fn radio_from_the_playing_track_replaces_only_what_comes_next() {
    let mut app = Harness::start("radio-keeps-playing").await;
    app.playing_queue(tracks(), 1);
    app.at_position(42.5).await;
    app.press("q").await;

    app.app
        .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text == "Starting radio from Snowline · Arnegger, Guest")
    );
    app.settle().await;

    assert_eq!(app.player.calls(), vec!["replace_upcoming:2"]);
    assert_eq!(
        app.app.player_state.track().map(|track| track.video_id.as_str()),
        Some("v2")
    );
    assert_eq!(app.app.player_state.position, 42.5);
    assert!(!app.app.player_state.paused);
    assert_eq!(
        app.app
            .player_state
            .queue
            .iter()
            .map(|track| track.video_id.as_str())
            .collect::<Vec<_>>(),
        ["v2", "v1", "v3"]
    );
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text == "Radio from Snowline · Arnegger, Guest")
    );
}

#[tokio::test]
async fn a_radio_result_does_not_revive_a_seed_that_stopped_playing() {
    let mut app = Harness::start("stale-radio").await;
    let radio = tracks();
    app.playing_queue(radio.clone(), 0);
    app.app
        .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    app.playing_queue(radio, 1);
    app.settle().await;

    assert!(app.player.calls().is_empty());
    assert!(
        app.app
            .notifications
            .iter()
            .any(|note| note.text == "Radio cancelled because playback changed")
    );
}

#[tokio::test]
async fn radio_from_another_highlighted_track_starts_that_track() {
    let mut app = Harness::start("radio-highlighted").await;
    app.playing_queue(tracks(), 1);

    app.press("r").await;

    assert_eq!(app.player.calls(), vec!["play:0"]);
    assert_eq!(
        app.app.player_state.track().map(|track| track.video_id.as_str()),
        Some("v1")
    );
}

#[tokio::test]
async fn highlighted_playlist_binding_does_not_add_the_playing_song() {
    let mut app = Harness::start("highlighted-playlist-binding").await;
    app.playing_queue(tracks(), 1);
    app.press_all(&["ctrl+p", "enter"]).await;
    assert_eq!(app.source.playlist_edits(), vec!["add_to_playlist:PL1:v1"]);
}

#[tokio::test]
async fn delete_removes_the_playlist_row_and_updates_its_cache() {
    let mut app = on_the_playlist_page("delete-playlist-row").await;
    app.press_all(&["j", "delete"]).await;
    assert_eq!(app.source.playlist_edits(), vec!["remove_playlist_entry:PL1:v2"]);
    let page = app.app.library.top().expect("the open playlist");
    assert!(page.tracks().all(|track| track.video_id != "v2"));
    let cached = app
        .app
        .deps
        .page_cache
        .get(PLAYLIST_KEY)
        .expect("the cached playlist");
    assert!(cached.tracks().all(|track| track.video_id != "v2"));
}

#[tokio::test]
async fn paused_transport_displays_show_play_buttons() {
    let mut setup = Setup::new("paused-transport-buttons");
    setup.saved = SavedState {
        queue: tracks(),
        index: Some(1),
        position: 42.5,
        ..SavedState::default()
    };
    let mut app = setup.build();
    app.draw();
    let icons = redwax::icons::icon_set("nerd");
    assert!(app.player_bar().contains(icons.play));
    assert!(!app.screen().contains(icons.pause));
    app.press("space").await;
    assert_eq!(app.app.player_state.position, 42.5);
    app.press("space").await;
    assert!(app.player_bar().contains(icons.play));
    assert!(!app.screen().contains(icons.pause));
}

#[tokio::test]
async fn artwork_loads_when_metadata_arrives_for_the_same_song() {
    let setup = Setup::new("late-art-metadata");
    seed_art(&setup.dir, support::ART_URL);
    let mut app = setup.build();
    let mut incomplete = tracks()[0].clone();
    incomplete.thumbnail_url = None;
    app.playing_queue(vec![incomplete], 0);
    assert!(app.app.art.render_protocol.is_none());

    let mut state = app.app.player_state.clone();
    state.queue[0].thumbnail_url = Some(support::ART_URL.to_string());
    app.playing_queue(state.queue, state.index.expect("the playing track"));
    app.settle().await;
    assert!(app.app.art.render_protocol.is_some());
}

#[tokio::test]
async fn restored_url_titles_recover_metadata_without_losing_progress_or_row_identity() {
    let mut setup = Setup::new("repair-restored-metadata");
    let mut damaged = tracks()[0].clone();
    damaged.title = "https://stream.invalid/videoplayback?signature=synthetic".to_string();
    damaged.artists.clear();
    damaged.thumbnail_url = None;
    damaged.playlist_entry = Some(redwax_ytm::PlaylistEntry::synthetic(41));
    setup.saved = SavedState {
        queue: vec![damaged],
        index: Some(0),
        position: 42.5,
        ..SavedState::default()
    };
    let cached = Page::new(
        "Cached album",
        vec![Shelf::new("Songs", vec![Item::Track(tracks()[0].clone())])],
    );
    redwax::page_cache::PageCache::new(setup.config.page_cache_dir.clone())
        .put("album:cached", &CompletePage::try_from(cached).unwrap());
    let app = setup.build();
    let restored = app.app.player_state.track().expect("the restored track");
    assert_eq!(restored.title, "Alpine Glow");
    assert_eq!(restored.artists, vec!["Arnegger"]);
    assert_eq!(restored.thumbnail_url.as_deref(), Some(support::ART_URL));
    assert_eq!(
        restored.playlist_entry,
        Some(redwax_ytm::PlaylistEntry::synthetic(41))
    );
    assert_eq!(app.app.player_state.position, 42.5);
    assert!(app.app.player_state.paused);
}

#[tokio::test]
async fn reversed_page_completions_preserve_the_latest_intent_and_busy_state() {
    use redwax::app::Target;
    for same_query in [false, true] {
        let mut app = Setup::new(if same_query {
            "same-request-identity"
        } else {
            "reversed-request-identity"
        })
        .build();
        app.app.search_query = "first".to_owned();
        app.app
            .load_page(Target::Search, Some("search:first".to_owned()), true);
        let AppEvent::PageReady {
            target,
            request: old,
            mut page,
        } = app.events.recv().await.expect("the first fetch responds")
        else {
            panic!("the source returns a page");
        };
        page.title = "Obsolete page".to_owned();
        app.app.search_query = if same_query { "first" } else { "second" }.to_owned();
        app.app
            .load_page(Target::Search, Some("search:current".to_owned()), true);
        app.app.handle_event(AppEvent::PageFailed {
            target,
            request: old,
            error: "Obsolete failure".to_owned(),
            rejected: false,
        });
        assert!(
            app.app.search.busy(),
            "an obsolete failure cannot end the current request"
        );
        assert!(app.app.notifications.is_empty(), "obsolete failures are silent");
        let current = app.events.recv().await.expect("the newest fetch responds");
        app.app.handle_event(current);
        let expected = app.app.search.top().cloned();
        app.app.handle_event(AppEvent::PageReady {
            target,
            request: old,
            page,
        });
        assert_eq!(app.app.search.top(), expected.as_ref());
        assert!(!app.app.search.busy());
        assert_ne!(
            app.app.search.top().expect("the newest page is shown").title,
            "Obsolete page"
        );
    }
}

#[tokio::test]
async fn a_saved_login_without_an_identity_is_not_labelled_signed_out() {
    let mut setup = Setup::new("login-unconfirmed");
    setup.source = Arc::new(FakeSource {
        handle: None,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.draw();
    assert!(app.header_row().contains("Checking login"));
    app.app.start_background();
    app.settle().await;
    assert!(app.header_row().contains("Login unconfirmed"));
    assert!(!app.header_row().contains("Sign in"));
}

#[tokio::test]
async fn an_empty_saved_identity_refreshes_and_restores_the_account() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut app = Setup::new("login-empty-recovery").build();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let calls = refreshes.clone();
    app.app.deps.session_refresh = Some(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    app.app.start_background();
    loop {
        match app.events.recv().await.expect("the identity check responds") {
            AppEvent::AccountReady { request, .. } => {
                app.app.handle_event(AppEvent::AccountReady {
                    request,
                    result: Ok(None),
                });
                break;
            }
            event => app.app.handle_event(event),
        }
    }
    app.settle().await;

    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(app.app.account, "@sam");
    assert!(app.header_row().contains("@sam"));
    assert!(app.source.called("library"));
}

#[tokio::test]
async fn an_empty_identity_after_refresh_does_not_repeat_the_import() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut setup = Setup::new("login-empty-after-refresh");
    setup.source = Arc::new(FakeSource {
        handle: None,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let calls = refreshes.clone();
    app.app.deps.session_refresh = Some(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    app.app.start_background();
    app.settle().await;

    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert!(app.header_row().contains("Login unconfirmed"));
    assert!(!app.header_row().contains("Sign in"));
}

#[tokio::test]
async fn a_temporary_identity_failure_keeps_the_confirmed_account_visible() {
    let mut app = Harness::start("login-temporary-failure").await;
    let identity = app.app.account.clone();
    assert!(!identity.is_empty());
    app.app.handle_event(AppEvent::SessionRefreshed {
        error: None,
        retry: None,
    });
    loop {
        match app.events.recv().await.expect("the identity check responds") {
            AppEvent::AccountReady { request, .. } => {
                app.app.handle_event(AppEvent::AccountReady {
                    request,
                    result: Err(redwax_ytm::Error::Network("temporary outage".to_owned())),
                });
                break;
            }
            event => app.app.handle_event(event),
        }
    }
    app.draw();
    assert_eq!(app.app.account, identity);
    assert!(app.header_row().contains(&identity));
    assert!(!app.header_row().contains("Sign in"));
    app.app.handle_event(AppEvent::SessionLoggedOut { error: None });
    app.draw();
    assert!(app.header_row().contains("Sign in"));
    assert!(app.app.account.is_empty());
}

#[tokio::test]
async fn account_box_is_reachable_with_arrows_and_enter() {
    let mut app = Harness::start("account-header-arrows").await;
    let cursor = app.app.home.cursor();
    app.press("left").await;
    assert_eq!(app.app.tab, Tab::Home);
    assert!(app.app.account_focused());
    app.press("enter").await;
    assert!(app.app.account_picker_visible());
    assert_eq!(app.app.home.cursor(), cursor);
    app.press("escape").await;
    assert!(!app.app.account_picker_visible());
    assert_eq!(app.app.tab, Tab::Home);
}

#[tokio::test]
async fn tab_focuses_the_same_account_control_on_each_page() {
    let mut app = Harness::start("account-header-tab").await;
    for (key, view) in [
        ("1", Tab::Home),
        ("3", Tab::Library),
        ("4", Tab::Queue),
        ("5", Tab::Lyrics),
        ("6", Tab::Themes),
        ("7", Tab::Settings),
    ] {
        app.press(key).await;
        assert!(!app.app.account_focused());
        app.press("tab").await;
        assert!(app.app.account_focused());
        assert_eq!(app.app.tab, view);
        app.press("tab").await;
        assert!(!app.app.account_focused());
        assert_eq!(app.app.tab, view);
    }
}

#[tokio::test]
async fn account_box_click_opens_a_picker_with_a_retry_when_login_is_unavailable() {
    let mut app = Harness::start("account-header-click").await;
    app.click(|hit| matches!(hit, Hit::Account)).await;
    assert!(app.app.account_picker_visible());
    assert!(
        app.app
            .screen
            .hit_map()
            .regions()
            .iter()
            .any(|(_, hit)| matches!(hit, Hit::AccountRow(0)))
    );
    app.press("escape").await;
    assert!(!app.app.account_picker_visible());
}

#[tokio::test]
async fn vim_edges_move_inside_the_account_picker_without_reaching_the_page() {
    use ratatui::style::Modifier;

    let mut app = Harness::start("account-picker-vim-edges").await;
    let page_cursor = app.app.home.cursor();
    app.press_all(&["tab", "enter"]).await;
    let selected = |app: &Harness, row| {
        let area = app.area_of(|hit| matches!(hit, Hit::AccountRow(index) if *index == row));
        app.terminal
            .backend()
            .buffer()
            .cell((area.x, area.y))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::BOLD)
    };
    assert!(selected(&app, 0));
    app.press("G").await;
    assert!(selected(&app, 1));
    app.press("g").await;
    assert!(selected(&app, 1));
    app.press("g").await;
    assert!(selected(&app, 0));
    assert_eq!(app.app.home.cursor(), page_cursor);
    app.press("escape").await;
    assert!(!app.app.account_picker_visible());
}

#[tokio::test]
async fn keys_panel_keeps_the_same_text_and_click_actions_on_every_tab() {
    let mut app = Harness::start("universal-key-panel").await;
    let panel = |app: &Harness| {
        app.app
            .screen
            .hit_map()
            .regions()
            .iter()
            .filter_map(|(area, hit)| {
                if let Hit::KeyHint(action) = hit {
                    let buffer = app.terminal.backend().buffer();
                    let text = (area.x..area.right())
                        .map(|x| buffer.cell((x, area.y)).unwrap().symbol())
                        .collect::<String>();
                    Some((*area, *action, text))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let expected = panel(&app);
    assert!(!expected.is_empty());
    for tab in ["2", "3", "4", "5", "6", "7", "1"] {
        app.press(tab).await;
        assert_eq!(panel(&app), expected);
    }
}

#[tokio::test]
async fn backspace_does_not_navigate_and_escape_goes_back() {
    let mut app = Harness::start("escape-only-navigation").await;
    app.press_all(&["j", "enter"]).await;
    assert!(app.app.home.depth() > 1);
    app.press("backspace").await;
    assert!(app.app.home.depth() > 1);
    app.press("escape").await;
    assert_eq!(app.app.home.depth(), 1);
}
