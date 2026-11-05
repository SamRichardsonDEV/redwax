mod support;

use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::style::Color;
use redwax::app::{AppEvent, Browser, Level, Tab};
use redwax::config::Config;
use redwax::theme::FOCUS_RED;
use redwax::ui::Hit;
use redwax_player::{PlayerState, Repeat};
use redwax_ytm::{Browsable, CompletePage, Item, Page, Playlist, PlaylistEntry, Shelf, Track};
use support::{FakeSource, Harness, Paged, Setup, album, paged_track, playlist, tracks};
use tokio::sync::Semaphore;

fn configured(name: &str) -> Setup {
    let mut setup = Setup::new(name);
    setup.config.source = setup.dir.join("config.toml");
    setup.config.focus_playlist_id = Some("PL1".to_string());
    setup.config.focus_playlist_title = Some("Late night".to_string());
    setup.genre = Some("Ambient".to_string());
    setup
}

fn song(video_id: &str) -> Track {
    redwax_player::bare_track(video_id, video_id)
}

fn library(app: &mut Harness, playlists: Vec<Playlist>) {
    app.app.library.replace(Page::new(
        "Library",
        vec![Shelf::new(
            "Playlists",
            playlists.into_iter().map(Item::Playlist).collect(),
        )],
    ));
    app.draw();
}

async fn toggle_setting(app: &mut Harness) {
    app.press("7").await;
    app.click(|hit| matches!(hit, Hit::ToggleFocusSetting)).await;
}

async fn activate(app: &mut Harness) {
    library(app, vec![playlist()]);
    toggle_setting(app).await;
    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(app.app.prompt().is_none());
}

fn setting_row(app: &Harness) -> String {
    let area = app.area_of(|hit| matches!(hit, Hit::ToggleFocusSetting));
    app.row(area.y)
}

async fn complete(app: &mut Harness, completions: &[(u64, Track)]) {
    let mut state = app.app.player_state.clone();
    state.playback_sequence = completions
        .last()
        .map_or(state.playback_sequence, |(sequence, _)| *sequence);
    state.completed_tracks.extend(completions.iter().cloned());
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.settle().await;
}

fn open_destination(browser: &mut Browser, page: &Page) {
    browser.show_from(page.clone(), Some(Browsable::Playlist(playlist())));
    let first_track = browser
        .rows()
        .iter()
        .position(|row| matches!(row, redwax::ui::Row::Item(0, 0)));
    browser.select_row(first_track);
}

async fn await_confirmed_save(app: &mut Harness, track: Track) -> AppEvent {
    let mut state = app.app.player_state.clone();
    state.playback_sequence += 1;
    state.completed_tracks.push_back((state.playback_sequence, track));
    app.app.handle_event(AppEvent::PlayerChanged(state));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = app.events.recv().await.expect("the app event channel stays open");
            if matches!(event, AppEvent::FocusSaved { .. }) {
                assert!(matches!(&event, AppEvent::FocusSaved { result: Ok(_), .. }));
                return event;
            }
            app.app.handle_event(event);
        }
    })
    .await
    .expect("the fixture save completes")
}

fn assert_text_color(app: &Harness, bounds: Rect, text: &str, expected: Color) {
    let buffer = app.terminal.backend().buffer();
    let symbols: Vec<String> = text.chars().map(|character| character.to_string()).collect();
    let bounds = bounds.intersection(*buffer.area());
    for row in bounds.y..bounds.bottom() {
        for column in bounds.x..bounds.right().saturating_sub(symbols.len() as u16 - 1) {
            if symbols
                .iter()
                .enumerate()
                .all(|(offset, symbol)| buffer[(column + offset as u16, row)].symbol() == symbol)
            {
                for offset in 0..symbols.len() as u16 {
                    assert_eq!(
                        buffer[(column + offset, row)].fg,
                        expected,
                        "{text:?} has the wrong color at ({}, {row})",
                        column + offset,
                    );
                }
                return;
            }
        }
    }
    panic!("{text:?} is not visible within {bounds:?}");
}

#[tokio::test]
async fn settings_selects_a_playlist_and_starts_focus_without_another_toggle_or_redirect() {
    let mut setup = configured("focus-settings-start");
    setup.config.focus_playlist_id = None;
    setup.config.focus_playlist_title = None;
    let mut app = setup.build();
    library(&mut app, vec![playlist()]);

    app.press_all(&["7", "j", "j", "enter"]).await;

    assert_eq!(app.app.tab, Tab::Settings);
    assert!(!app.app.focus_active());
    assert!(app.app.playlist_picker().unwrap().focus_destination());
    app.press("enter").await;

    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(app.app.prompt().is_none());
    assert!(setting_row(&app).contains("[On]"));
    let playlist_row = app.area_of(|hit| matches!(hit, Hit::ChooseFocusPlaylist));
    assert!(app.row(playlist_row.y).contains("Playlist"));
    assert!(app.row(playlist_row.y).contains("[Late night]"));
    let saved = Config::load(Some(&app.app.deps.config.source)).unwrap();
    assert_eq!(saved.focus_playlist_id.as_deref(), Some("PL1"));
    assert_eq!(saved.focus_playlist_title.as_deref(), Some("Late night"));
    assert!(app.source.playlist_edits().is_empty());
}

#[tokio::test]
async fn cancelling_the_playlist_picker_leaves_focus_off_and_the_saved_destination_unchanged() {
    let mut app = configured("focus-settings-cancel").build();
    library(&mut app, vec![playlist()]);
    app.press("7").await;
    app.click(|hit| matches!(hit, Hit::ChooseFocusPlaylist)).await;

    app.press("escape").await;

    assert!(!app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(setting_row(&app).contains("[Off]"));
    assert_eq!(app.app.deps.config.focus_playlist_id.as_deref(), Some("PL1"));
    assert!(app.source.playlist_edits().is_empty());
}

#[tokio::test]
async fn configuring_the_nested_playlist_row_keeps_focus_off_until_the_mode_is_enabled() {
    let mut app = configured("focus-settings-restart").build();
    activate(&mut app).await;

    toggle_setting(&mut app).await;
    assert!(!app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(app.app.prompt().is_none());
    assert!(setting_row(&app).contains("[Off]"));
    assert!(!app.player_bar().contains("FOCUS ON"));

    let second = Playlist {
        playlist_id: "PLsecond".to_string(),
        title: "Deep work".to_string(),
        subtitle: None,
        thumbnail_url: None,
    };
    library(&mut app, vec![playlist(), second]);
    app.click(|hit| matches!(hit, Hit::ChooseFocusPlaylist)).await;
    app.press_all(&["j", "enter"]).await;

    assert!(!app.app.focus_active());
    assert_eq!(app.app.deps.config.focus_playlist_id.as_deref(), Some("PLsecond"));
    assert_eq!(
        app.app.deps.config.focus_playlist_title.as_deref(),
        Some("Deep work")
    );
    assert!(setting_row(&app).contains("[Off]"));
    toggle_setting(&mut app).await;
    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    complete(&mut app, &[(1, song("new-destination-song"))]).await;
    assert_eq!(
        app.source.playlist_edits(),
        ["add_to_playlist:PLsecond:new-destination-song"]
    );
}

#[tokio::test]
async fn an_unloaded_library_is_loaded_inside_the_same_destination_picker() {
    let mut setup = configured("focus-load-library");
    setup.config.focus_playlist_id = None;
    setup.config.focus_playlist_title = None;
    let mut app = setup.build();
    library(&mut app, Vec::new());

    toggle_setting(&mut app).await;

    assert!(app.source.called("library"));
    assert_eq!(app.app.tab, Tab::Settings);
    assert!(!app.app.focus_active());
    let picker = app
        .app
        .playlist_picker()
        .expect("the destination picker stays open");
    assert!(picker.focus_destination());
    assert!(picker.playlists.iter().any(|entry| entry.playlist_id == "PL1"));

    app.press("enter").await;
    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
}

#[tokio::test]
async fn the_playlist_child_row_is_reached_by_the_next_settings_navigation_step() {
    let mut app = configured("focus-child-navigation").build();
    library(&mut app, vec![playlist()]);

    app.press_all(&["7", "j", "j", "j", "enter"]).await;
    assert!(app.app.playlist_picker().unwrap().focus_destination());
    app.press("escape").await;
    assert!(!app.app.focus_active());
    app.press_all(&["home", "end", "enter"]).await;
    assert!(app.app.playlist_picker().unwrap().focus_destination());
    app.press("enter").await;
    assert!(!app.app.focus_active());

    app.press_all(&["k", "enter"]).await;
    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
}

#[tokio::test]
async fn changing_the_nested_playlist_keeps_the_current_full_play_eligible_and_ignores_completed_history() {
    let mut app = configured("focus-change-active-destination").build();
    app.app.player_state.playback_sequence = 4;
    app.app
        .player_state
        .completed_tracks
        .push_back((3, song("already-finished")));
    app.playing_queue(vec![song("already-playing")], 0);
    app.settle().await;
    activate(&mut app).await;
    let second = Playlist {
        playlist_id: "PLsecond".to_string(),
        title: "Deep work".to_string(),
        subtitle: None,
        thumbnail_url: None,
    };
    library(&mut app, vec![playlist(), second]);

    app.click(|hit| matches!(hit, Hit::ChooseFocusPlaylist)).await;
    assert!(app.app.focus_active());
    app.press_all(&["j", "enter"]).await;

    assert!(app.app.focus_active());
    assert_eq!(app.app.deps.config.focus_playlist_id.as_deref(), Some("PLsecond"));
    assert!(setting_row(&app).contains("[On]"));
    complete(
        &mut app,
        &[
            (3, song("already-finished")),
            (4, song("already-playing")),
            (5, song("new-destination-song")),
        ],
    )
    .await;
    assert_eq!(
        app.source.playlist_edits(),
        [
            "add_to_playlist:PLsecond:already-playing",
            "add_to_playlist:PLsecond:new-destination-song"
        ]
    );
}

#[tokio::test]
async fn cancelling_the_nested_playlist_picker_keeps_the_active_mode_and_destination() {
    let mut app = configured("focus-cancel-active-destination").build();
    activate(&mut app).await;

    app.click(|hit| matches!(hit, Hit::ChooseFocusPlaylist)).await;
    app.press("escape").await;

    assert!(app.app.focus_active());
    assert_eq!(app.app.deps.config.focus_playlist_id.as_deref(), Some("PL1"));
    assert!(setting_row(&app).contains("[On]"));
    complete(&mut app, &[(1, song("same-destination-song"))]).await;
    assert_eq!(
        app.source.playlist_edits(),
        ["add_to_playlist:PL1:same-destination-song"]
    );
}

#[tokio::test]
async fn shift_f_does_not_enable_or_disable_focus_in_the_full_app() {
    let mut app = configured("focus-no-shortcut").build();
    app.draw();
    app.press("F").await;
    assert!(!app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(app.app.prompt().is_none());

    activate(&mut app).await;
    app.press_all(&["1", "F"]).await;
    assert_eq!(app.app.tab, Tab::Home);
    assert!(app.app.focus_active());
    assert!(app.app.playlist_picker().is_none());
    assert!(app.app.prompt().is_none());
}

#[tokio::test]
async fn all_player_details_turn_red_immediately_and_return_to_their_normal_colors_when_disabled() {
    let mut app = configured("focus-details-red").build();
    app.app.player_state.duration = Some(214.0);
    app.playing_queue(tracks(), 0);
    app.at_position(42.0).await;
    activate(&mut app).await;
    app.app.notifications.clear();
    app.app.screen.touch();
    app.draw();

    let buffer = app.terminal.backend().buffer();
    let height = buffer.area().height;
    let player_top = height - app.hint_row() - redwax::ui::player_bar::PLAYER_BAR_HEIGHT;
    let player = Rect::new(
        0,
        player_top,
        buffer.area().width,
        redwax::ui::player_bar::PLAYER_BAR_HEIGHT,
    );
    let sidebar = Rect::new(
        0,
        redwax::app::HEADER_HEIGHT,
        redwax::app::SIDEBAR_WIDTH,
        player_top - redwax::app::HEADER_HEIGHT,
    );
    for text in ["FOCUS ON", "Alpine Glow", "Arnegger", "0:42", "3:34"] {
        assert_text_color(&app, player, text, FOCUS_RED);
    }
    for text in ["Alpine Glow", "Arnegger", "Peaks"] {
        assert_text_color(&app, sidebar, text, FOCUS_RED);
    }
    for key in ["1", "4", "7"] {
        app.press(key).await;
        assert!(app.player_bar().contains("FOCUS ON"));
        assert_text_color(&app, player, "Alpine Glow", FOCUS_RED);
    }

    toggle_setting(&mut app).await;
    app.app.notifications.clear();
    app.app.screen.touch();
    app.draw();
    assert!(!app.player_bar().contains("FOCUS ON"));
    assert_text_color(&app, player, "Alpine Glow", app.app.base_theme.foreground);
    assert_text_color(&app, player, "Arnegger", app.app.base_theme.muted);
    assert_text_color(&app, sidebar, "Alpine Glow", app.app.base_theme.accent);
    assert_text_color(&app, sidebar, "Arnegger", app.app.base_theme.foreground);
    assert_text_color(&app, sidebar, "Peaks", app.app.base_theme.muted);
}

#[tokio::test]
async fn the_focus_indicator_is_visible_even_when_nothing_is_playing() {
    let mut app = configured("focus-idle-indicator").build();
    activate(&mut app).await;
    app.app.notifications.clear();
    app.app.screen.touch();
    app.draw();
    assert!(app.player_bar().contains("FOCUS ON"));
    assert!(app.player_bar().contains("nothing playing"));
}

#[tokio::test]
async fn completed_history_is_ignored_but_the_current_uninterrupted_play_is_saved_once() {
    let mut app = configured("focus-completions").build();
    app.app.player_state.playback_sequence = 4;
    app.app
        .player_state
        .completed_tracks
        .push_back((3, song("already-finished")));
    app.playing_queue(vec![song("already-playing")], 0);
    app.settle().await;
    activate(&mut app).await;

    let mut advanced = app.app.player_state.clone();
    advanced.playback_sequence = 5;
    advanced.position = 0.0;
    advanced.queue = vec![song("next-song")];
    advanced.index = Some(0);
    app.app.handle_event(AppEvent::PlayerChanged(advanced));
    app.settle().await;
    assert!(app.source.playlist_edits().is_empty());

    complete(
        &mut app,
        &[
            (3, song("already-finished")),
            (4, song("already-playing")),
            (5, song("first-full-play")),
        ],
    )
    .await;
    complete(
        &mut app,
        &[(6, song("second-full-play")), (7, song("already-playing"))],
    )
    .await;
    let same_state: PlayerState = app.app.player_state.clone();
    app.app.handle_event(AppEvent::PlayerChanged(same_state));
    app.settle().await;

    assert_eq!(
        app.source.playlist_edits(),
        [
            "add_to_playlist:PL1:already-playing",
            "add_to_playlist:PL1:first-full-play",
            "add_to_playlist:PL1:second-full-play"
        ]
    );
    assert!(app.app.focus_active());
    app.app.notifications.clear();
    app.app.screen.touch();
    app.draw();
    assert!(app.screen().contains("Saved: second-full-play"));
}

#[tokio::test]
async fn coalesced_full_plays_wait_for_the_previous_membership_check_and_write() {
    let mut setup = configured("focus-serialized");
    let gate = Arc::new(Semaphore::new(0));
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    activate(&mut app).await;
    complete(&mut app, &[(1, song("one")), (2, song("two"))]).await;
    assert_eq!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "browse:playlist:PL1")
            .count(),
        1
    );
    assert!(app.source.playlist_edits().is_empty());

    gate.add_permits(1);
    app.settle().await;
    assert_eq!(
        app.source.playlist_edits(),
        ["add_to_playlist:PL1:one", "add_to_playlist:PL1:two"]
    );
}

#[tokio::test]
async fn destination_members_are_exempt_even_when_found_on_a_continuation() {
    for paginated in [false, true] {
        let mut setup = configured(if paginated {
            "focus-existing-tail"
        } else {
            "focus-existing"
        });
        let member = if paginated {
            setup.paged(&[2, 2]);
            paged_track(3)
        } else {
            tracks()[0].clone()
        };
        let mut app = setup.build();
        activate(&mut app).await;
        let title = member.title.clone();
        complete(&mut app, &[(1, member)]).await;
        assert!(app.app.focus_active());
        assert!(app.source.playlist_edits().is_empty());
        app.app.notifications.clear();
        app.app.screen.touch();
        app.draw();
        assert!(app.screen().contains(&format!("Already in playlist: {title}")));
        if paginated {
            assert!(app.source.called("continue:batch:1"));
        }
    }
}

#[tokio::test]
async fn incomplete_destination_membership_stops_focus_without_writing_and_settings_shows_off() {
    let mut setup = configured("focus-incomplete");
    setup.source = Arc::new(FakeSource {
        paged: Some(Paged::new(&[2, 2])),
        continuation_fails: true,
        ..FakeSource::default()
    });
    let mut app = setup.build();
    activate(&mut app).await;
    complete(&mut app, &[(1, song("new-song"))]).await;
    assert!(!app.app.focus_active());
    assert!(setting_row(&app).contains("[Off]"));
    assert!(!app.player_bar().contains("FOCUS ON"));
    assert!(app.source.playlist_edits().is_empty());
    assert!(
        app.app
            .notifications
            .iter()
            .any(|notice| notice.level == Level::Error && notice.text.contains("Focus mode stopped"))
    );
}

#[tokio::test]
async fn a_failed_save_stops_focus_and_discards_later_pending_songs() {
    let mut setup = configured("focus-save-failure");
    setup.failing_edits();
    let mut app = setup.build();
    activate(&mut app).await;
    complete(&mut app, &[(1, song("failed-song")), (2, song("pending-song"))]).await;
    assert!(!app.app.focus_active());
    assert!(setting_row(&app).contains("[Off]"));
    assert_eq!(app.source.playlist_edits(), ["add_to_playlist:PL1:failed-song"]);
    assert!(
        app.app
            .notifications
            .iter()
            .any(|notice| notice.level == Level::Error && notice.text.contains("Could not save failed-song"))
    );
    app.app.notifications.clear();
    app.app.screen.touch();
    app.draw();
    assert!(app.screen().contains("Could not save failed-song"));
}

#[tokio::test]
async fn disabling_focus_from_settings_cancels_a_membership_check_before_it_can_write() {
    let mut setup = configured("focus-cancel-save");
    let gate = Arc::new(Semaphore::new(0));
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    activate(&mut app).await;
    complete(&mut app, &[(1, song("cancelled-song"))]).await;
    assert!(app.source.called("browse:playlist:PL1"));

    toggle_setting(&mut app).await;
    gate.add_permits(1);
    app.settle().await;
    complete(&mut app, &[(2, song("after-stop"))]).await;
    assert!(!app.app.focus_active());
    assert!(app.source.playlist_edits().is_empty());
}

#[tokio::test]
async fn logging_out_stops_focus_and_rejects_later_playback_completions() {
    let mut app = configured("focus-logout").build();
    activate(&mut app).await;
    app.app.handle_event(AppEvent::SessionLoggedOut {
        error: None,
        reauthenticate: false,
    });
    complete(&mut app, &[(1, song("after-logout"))]).await;
    assert!(!app.app.focus_active());
    assert!(setting_row(&app).contains("[Off]"));
    assert!(app.source.playlist_edits().is_empty());
}

#[tokio::test]
async fn repeat_track_stops_collection_and_settings_shows_that_focus_is_off() {
    let mut app = configured("focus-repeat-track").build();
    activate(&mut app).await;
    let mut state = app.app.player_state.clone();
    state.repeat = Repeat::One;
    state.playback_sequence = 1;
    state.completed_tracks.push_back((1, song("looping-song")));
    app.app.handle_event(AppEvent::PlayerChanged(state));
    app.settle().await;
    assert!(!app.app.focus_active());
    assert!(setting_row(&app).contains("[Off]"));
    assert!(app.source.playlist_edits().is_empty());

    toggle_setting(&mut app).await;
    assert!(app.app.playlist_picker().is_none());
    assert!(
        app.app
            .notifications
            .iter()
            .any(|notice| notice.text.contains("Choose repeat off or repeat queue"))
    );
}

#[tokio::test]
async fn confirmed_focus_saves_refresh_open_destination_views_and_cache_with_server_edit_authority() {
    let setup = configured("focus-live-playlist-refresh");
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".to_string(), vec![tracks()[0].clone()]);
    let mut app = setup.build();
    activate(&mut app).await;
    let mut stale_track = tracks()[0].clone();
    stale_track.playlist_entry = Some(PlaylistEntry::synthetic(900));
    let old_page = Page::new(
        "Late night",
        vec![Shelf::new("Tracks", vec![Item::Track(stale_track)])],
    );
    for browser in [&mut app.app.home, &mut app.app.search, &mut app.app.library] {
        open_destination(browser, &old_page);
    }
    let depths = [
        app.app.home.depth(),
        app.app.search.depth(),
        app.app.library.depth(),
    ];
    let cursors = [
        app.app.home.cursor(),
        app.app.search.cursor(),
        app.app.library.cursor(),
    ];
    let unrelated = Page::new("Another cached page", Vec::new());
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(old_page).unwrap());
    app.app.deps.page_cache.put(
        "album:unrelated",
        &CompletePage::try_from(unrelated.clone()).unwrap(),
    );
    app.press("1").await;

    complete(&mut app, &[(1, song("new-focus-song"))]).await;

    assert!(app.app.focus_active());
    assert_eq!(app.app.tab, Tab::Home);
    assert_eq!(
        app.source.playlist_edits(),
        ["add_to_playlist:PL1:new-focus-song"]
    );
    assert_eq!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "browse:playlist:PL1")
            .count(),
        2
    );
    for (index, browser) in [&app.app.home, &app.app.search, &app.app.library]
        .into_iter()
        .enumerate()
    {
        let page = browser.top().unwrap();
        assert_eq!(
            page.tracks()
                .map(|track| track.video_id.as_str())
                .collect::<Vec<_>>(),
            ["v1", "new-focus-song"]
        );
        assert_eq!(browser.depth(), depths[index]);
        assert_eq!(browser.cursor(), cursors[index]);
        assert!(matches!(browser.selected_item(), Some(Item::Track(track)) if track.video_id == "v1"));
        let added = page
            .tracks()
            .find(|track| track.video_id == "new-focus-song")
            .unwrap();
        assert_eq!(added.playlist_entry, Some(PlaylistEntry::synthetic(2)));
        assert_eq!(
            page.tracks().next().unwrap().playlist_entry,
            Some(PlaylistEntry::synthetic(1))
        );
    }
    assert_eq!(app.cached("playlist:PL1"), app.app.home.top().cloned());
    assert_eq!(app.cached("album:unrelated"), Some(unrelated));
    assert!(app.screen().contains("new-focus-song"));

    app.press_all(&["j", "delete"]).await;
    assert!(app.source.called("remove_playlist_entry:PL1:new-focus-song"));
}

#[tokio::test]
async fn an_existing_focus_song_refreshes_the_destination_without_another_playlist_write() {
    let mut app = configured("focus-refresh-existing-member").build();
    activate(&mut app).await;
    let old_page = Page::new("Late night", vec![Shelf::new("Tracks", Vec::new())]);
    open_destination(&mut app.app.home, &old_page);
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(old_page).unwrap());
    app.press("1").await;

    complete(&mut app, &[(1, tracks()[0].clone())]).await;

    assert!(app.app.focus_active());
    assert!(app.source.playlist_edits().is_empty());
    assert_eq!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "browse:playlist:PL1")
            .count(),
        2
    );
    assert_eq!(app.app.home.top().unwrap().tracks().count(), 3);
    assert_eq!(app.cached("playlist:PL1"), app.app.home.top().cloned());
}

#[tokio::test]
async fn a_focus_refresh_updates_history_without_reopening_the_destination_after_navigation() {
    let mut setup = configured("focus-refresh-navigation");
    let gate = Arc::new(Semaphore::new(1));
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".to_string(), vec![tracks()[0].clone()]);
    let mut app = setup.build();
    activate(&mut app).await;
    let old_page = Page::new(
        "Late night",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[0].clone())])],
    );
    open_destination(&mut app.app.home, &old_page);
    app.press("1").await;

    let confirmed_save = await_confirmed_save(&mut app, song("new-focus-song")).await;
    let blocked_refresh = gate
        .clone()
        .try_acquire_owned()
        .expect("the completed membership check released its permit");
    app.app.handle_event(confirmed_save);
    app.settle().await;
    assert_eq!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "browse:playlist:PL1")
            .count(),
        2
    );
    let album_page = Page::new(
        "Peaks",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[1].clone())])],
    );
    app.app
        .home
        .show_from(album_page.clone(), Some(Browsable::Album(album())));
    let depth = app.app.home.depth();
    let cursor = app.app.home.cursor();

    drop(blocked_refresh);
    app.settle().await;

    assert_eq!(app.app.tab, Tab::Home);
    assert_eq!(app.app.home.top(), Some(&album_page));
    assert_eq!(app.app.home.depth(), depth);
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.app.home.pages().rev().nth(1).unwrap().tracks().count(), 2);
    app.press("escape").await;
    assert_eq!(app.app.home.top().unwrap().title, "Late night");
    assert!(
        app.app
            .home
            .top()
            .unwrap()
            .tracks()
            .any(|track| track.video_id == "new-focus-song")
    );
}

#[tokio::test]
async fn a_failed_post_save_refresh_preserves_the_remote_save_and_keeps_focus_active() {
    let mut setup = configured("focus-refresh-failure");
    setup.source = Arc::new(FakeSource {
        fail_playlist_reads_after_edit: true,
        ..FakeSource::default()
    });
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".to_string(), vec![tracks()[0].clone()]);
    let mut app = setup.build();
    activate(&mut app).await;
    let old_page = Page::new(
        "Late night",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[0].clone())])],
    );
    open_destination(&mut app.app.home, &old_page);
    app.app
        .deps
        .page_cache
        .put("playlist:PL1", &CompletePage::try_from(old_page.clone()).unwrap());

    complete(&mut app, &[(1, song("new-focus-song"))]).await;

    assert!(app.app.focus_active());
    assert_eq!(
        app.source.playlist_edits(),
        ["add_to_playlist:PL1:new-focus-song"]
    );
    assert!(
        app.source.playlist_overrides.lock().unwrap()["PL1"]
            .iter()
            .any(|track| track.video_id == "new-focus-song")
    );
    assert_eq!(app.app.home.top(), Some(&old_page));
    assert_eq!(app.cached("playlist:PL1"), None);
    assert!(app.app.notifications.iter().any(|notice| {
        notice.level == Level::Warning && notice.text.contains("Song saved, but could not refresh")
    }));
}

#[tokio::test]
async fn logging_out_cancels_a_pending_focus_refresh_before_it_can_restore_the_old_accounts_playlist() {
    let mut setup = configured("focus-refresh-logout");
    let gate = Arc::new(Semaphore::new(1));
    setup.source = Arc::new(FakeSource {
        browse_gate: Some(gate.clone()),
        ..FakeSource::default()
    });
    setup
        .source
        .playlist_overrides
        .lock()
        .unwrap()
        .insert("PL1".to_string(), vec![tracks()[0].clone()]);
    let mut app = setup.build();
    activate(&mut app).await;
    let old_page = Page::new(
        "Late night",
        vec![Shelf::new("Tracks", vec![Item::Track(tracks()[0].clone())])],
    );
    open_destination(&mut app.app.home, &old_page);
    let confirmed_save = await_confirmed_save(&mut app, song("new-focus-song")).await;
    let blocked_refresh = gate
        .clone()
        .try_acquire_owned()
        .expect("the completed membership check released its permit");
    app.app.handle_event(confirmed_save);
    app.settle().await;

    app.app.handle_event(AppEvent::SessionLoggedOut {
        error: None,
        reauthenticate: false,
    });
    drop(blocked_refresh);
    app.settle().await;

    assert!(!app.app.focus_active());
    assert_eq!(app.cached("playlist:PL1"), None);
    assert!(!app.app.home.pages().any(|page| page.title == "Late night"));
    assert!(!app.app.library.pages().any(|page| page.title == "Late night"));
}
