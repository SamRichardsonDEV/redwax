//! Draw every tab at many terminal sizes. A panic here is a crash a user can
//! reach by resizing a pane, which no assertion in the other tests covers.

mod support;

use redwax::app::{AppEvent, SIDEBAR_MIN_WIDTH};
use redwax_player::PlayerState;
use redwax_ytm::Lyrics;
use support::{Harness, Setup, long_track, timed_lines};

fn playing(base: &PlayerState) -> PlayerState {
    PlayerState {
        queue: vec![long_track(); 12],
        index: Some(0),
        duration: Some(240.0),
        position: 30.0,
        ..base.clone()
    }
}

#[tokio::test]
async fn every_tab_survives_every_size() {
    let sizes: Vec<(u16, u16)> = (1u16..=8)
        .flat_map(|w| (1u16..=8).map(move |h| (w * 3, h * 3)))
        .chain([
            (1, 1),
            (7, 7),
            (8, 6),
            (8, 7),
            (15, 11),
            (16, 11),
            (23, 11),
            (24, 11),
            (40, 12),
            (41, 12),
            (42, 12),
            (46, 20),
            (56, 11),
            (57, 11),
            (88, 24),
            (89, 24),
            (109, 45),
            (110, 45),
            (111, 45),
            (134, 45),
            (135, 45),
            (200, 60),
            (30, 8),
        ])
        .collect();
    for (width, height) in sizes {
        let mut setup = Setup::new(&format!("sweep-{width}-{height}"));
        setup.width = width;
        setup.height = height;
        let mut harness = setup.build();
        harness.draw();
        harness.app.start_background();
        harness.settle().await;
        let state = playing(&harness.app.player_state);
        harness.app.handle_event(AppEvent::PlayerChanged(state));
        let request = harness.app.lyrics.fetching(&long_track());
        harness.app.handle_event(AppEvent::LyricsReady {
            request,
            track: long_track(),
            lyrics: Some(Lyrics::Timed(timed_lines())),
        });
        let keys = [
            "1",
            "2",
            "3",
            "4",
            "5",
            "6",
            "7",
            "?",
            "e",
            "k",
            "l",
            "e",
            "Q",
            "P",
            "j",
            "enter",
            "P",
            "escape",
            "D",
            "tab",
            "escape",
            "P",
            "tab",
            "G",
            "enter",
            "a",
            "backspace",
            "escape",
            "escape",
        ];
        for key in keys {
            harness.press(key).await;
            harness.draw();
        }
    }
}

#[tokio::test]
async fn a_narrow_pane_survives_a_long_title() {
    for width in [20u16, 34, 47, 60, 96, 110, 111] {
        let mut setup = Setup::new(&format!("sweep-narrow-{width}"));
        setup.width = width;
        setup.height = 24;
        let mut harness: Harness = setup.build();
        harness.draw();
        let state = playing(&harness.app.player_state);
        harness.app.handle_event(AppEvent::PlayerChanged(state));
        harness.draw();
    }
}

/// Click and scroll across the screen on every tab. Stale targets can outlive
/// a refresh between a draw and the next input event.
#[tokio::test]
async fn a_click_anywhere_survives() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use support::mouse;

    for (width, height) in [(140u16, 45u16), (60, 20), (111, 30)] {
        let mut setup = Setup::new(&format!("sweep-click-{width}-{height}"));
        setup.width = width;
        setup.height = height;
        let mut harness: Harness = setup.build();
        harness.draw();
        harness.app.start_background();
        harness.settle().await;
        let state = playing(&harness.app.player_state);
        harness.app.handle_event(AppEvent::PlayerChanged(state));
        let request = harness.app.lyrics.fetching(&long_track());
        harness.app.handle_event(AppEvent::LyricsReady {
            request,
            track: long_track(),
            lyrics: Some(Lyrics::Timed(timed_lines())),
        });
        for tab in ["1", "2", "3", "4", "5", "6", "7"] {
            harness.press(tab).await;
            for y in 0..height {
                for x in (0..width).step_by(3) {
                    for kind in [
                        MouseEventKind::Down(MouseButton::Left),
                        MouseEventKind::Down(MouseButton::Right),
                        MouseEventKind::Down(MouseButton::Middle),
                        MouseEventKind::ScrollDown,
                        MouseEventKind::ScrollUp,
                    ] {
                        harness.app.handle_mouse(mouse(x, y, kind));
                    }
                }
            }
            harness.draw();
        }
    }
}

#[tokio::test]
async fn every_tab_survives_shrinking_and_growing() {
    let mut harness = Harness::start("sweep-resize").await;
    let state = playing(&harness.app.player_state);
    harness.app.handle_event(AppEvent::PlayerChanged(state));
    let request = harness.app.lyrics.fetching(&long_track());
    harness.app.handle_event(AppEvent::LyricsReady {
        request,
        track: long_track(),
        lyrics: Some(Lyrics::Timed(timed_lines())),
    });

    for tab in ["1", "2", "3", "4", "5", "6", "7"] {
        harness.press(tab).await;
        for (width, height) in [
            (140, 45),
            (109, 24),
            (110, 24),
            (SIDEBAR_MIN_WIDTH - 1, 24),
            (SIDEBAR_MIN_WIDTH, 24),
            (60, 12),
            (8, 7),
            (7, 7),
            (140, 6),
            (1, 1),
            (140, 45),
        ] {
            harness.terminal.backend_mut().resize(width, height);
            harness.draw();
            assert_eq!(harness.app.sidebar_visible(), width >= SIDEBAR_MIN_WIDTH);
            if width < 8 || height < 7 {
                assert!(harness.app.screen.hit_map().regions().is_empty());
                assert_eq!(harness.app.screen.visible_rows(), 0);
            } else {
                assert!(
                    harness.screen().chars().any(|cell| !cell.is_whitespace()),
                    "tab {tab} at {width}x{height} must repaint after a blank frame"
                );
                if width >= 40 && height >= 12 {
                    assert!(
                        !harness.app.screen.hit_map().regions().is_empty(),
                        "tab {tab} at {width}x{height} must restore its click targets"
                    );
                }
            }
        }
    }
}
