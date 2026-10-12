mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use redwax::app::{AppEvent, Level};
use redwax::keys::BINDINGS;
use redwax::ui::Hit;
use support::{Harness, Setup};

fn sized(name: &str, width: u16, height: u16) -> Harness {
    let mut setup = Setup::new(name);
    setup.width = width;
    setup.height = height;
    setup.build()
}

fn notify(app: &mut Harness, text: &str, title: &str) {
    app.app.handle_event(AppEvent::Notify {
        text: text.to_string(),
        title: Some(title.to_string()),
        level: Level::Error,
    });
    app.draw();
}

fn key(app: &mut Harness, code: KeyCode) {
    app.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    app.draw();
}

fn resize(app: &mut Harness, width: u16, height: u16) {
    app.terminal.backend_mut().resize(width, height);
    app.draw();
}

fn long_message() -> String {
    (0..80)
        .map(|line| format!("Message detail {line:03}\n"))
        .collect::<String>()
        + "FINAL_MESSAGE_SENTINEL"
}

#[tokio::test]
async fn roomy_notifications_show_every_explicit_line() {
    let mut app = sized("popup-multiline", 140, 45);
    notify(
        &mut app,
        "First diagnostic line\nSecond diagnostic line\nLast diagnostic line",
        "Playback failed",
    );

    let frame = app.screen();
    assert!(frame.contains("Playback failed"));
    for line in [
        "First diagnostic line",
        "Second diagnostic line",
        "Last diagnostic line",
    ] {
        assert!(frame.contains(line), "the notification lost {line:?}");
    }
}

#[tokio::test]
async fn a_short_narrow_terminal_still_shows_the_error() {
    let mut app = sized("popup-narrow", 40, 12);
    notify(&mut app, "Audio resolution failed", "Playback failed");

    assert!(app.screen().contains("Audio resolution failed"));
    assert!(
        app.app
            .screen
            .hit_map()
            .regions()
            .iter()
            .any(|(_, hit)| matches!(hit, Hit::Notification(_)))
    );
}

#[tokio::test]
async fn unbroken_wide_unicode_words_keep_every_character_when_wrapped() {
    let mut app = sized("popup-unicode", 140, 45);
    let word = "界🦀".repeat(24);
    notify(&mut app, &word, "Unicode diagnostic");

    let frame = app.screen();
    assert_eq!(frame.chars().filter(|character| *character == '界').count(), 24);
    assert_eq!(frame.chars().filter(|character| *character == '🦀').count(), 24);
}

#[tokio::test]
async fn an_oversized_stack_keeps_the_newest_notification_visible() {
    let mut app = sized("popup-stack", 40, 12);
    for index in 0..6 {
        notify(
            &mut app,
            &format!("Message {index}\nExtra detail one\nExtra detail two\nExtra detail three"),
            &format!("Error {index}"),
        );
    }

    assert!(app.screen().contains("Message 5"));
    assert!(app.screen().contains("Error 5"));
}

#[tokio::test]
async fn the_full_message_survives_toast_expiry_and_reaches_its_tail() {
    let mut app = sized("popup-full-message", 60, 14);
    notify(&mut app, &long_message(), "Detailed failure");
    assert!(app.screen().contains("click or ! to read"));

    app.press("!").await;
    key(&mut app, KeyCode::End);
    assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));

    app.app.notifications.clear();
    app.app.screen.touch();
    app.app.handle_event(AppEvent::Tick);
    app.draw();
    assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));
    key(&mut app, KeyCode::Home);
    assert!(app.screen().contains("Message detail 000"));
    app.press("G").await;
    assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));

    for close in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('!')] {
        key(&mut app, close);
        assert!(!app.screen().contains("close"));
        app.press("!").await;
        key(&mut app, KeyCode::End);
        assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));
    }
}

#[tokio::test]
async fn full_message_focus_blocks_browser_navigation_and_closing_restores_it() {
    let mut app = Harness::start("popup-focus").await;
    let cursor = app.app.home.cursor();
    let history = app.app.home.pages().cloned().collect::<Vec<_>>();
    let tab = app.app.tab;
    notify(&mut app, &long_message(), "Detailed failure");
    app.press("!").await;

    for code in [
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::End,
        KeyCode::Home,
    ] {
        key(&mut app, code);
        assert_eq!(app.app.home.cursor(), cursor);
        assert_eq!(app.app.home.pages().cloned().collect::<Vec<_>>(), history);
        assert_eq!(app.app.tab, tab);
    }
    app.press_all(&["j", "k", "g", "G"]).await;
    assert_eq!(app.app.home.cursor(), cursor);
    key(&mut app, KeyCode::Esc);
    app.press("j").await;
    assert_ne!(app.app.home.cursor(), cursor);
}

#[tokio::test]
async fn notification_clicks_and_wheels_do_not_reach_covered_browser_rows() {
    let mut app = sized("popup-click", 60, 14);
    app.draw();
    app.app.start_background();
    app.settle().await;
    let underneath = app.app.screen.hit_map().clone();
    let cursor = app.app.home.cursor();
    let history = app.app.home.pages().cloned().collect::<Vec<_>>();
    let player_calls = app.player.calls();
    notify(
        &mut app,
        "First detail\nSecond detail\nThird detail",
        "Playback failed",
    );
    let area = app.area_of(|hit| matches!(hit, Hit::Notification(_)));
    let mut covered_row = None;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if matches!(underneath.at(x, y), Some(Hit::Row(_))) {
                covered_row = Some((x, y));
            }
        }
    }
    let (x, y) = covered_row.expect("the toast must cover a live browser row");

    app.wheel(x, y, false).await;
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.app.home.pages().cloned().collect::<Vec<_>>(), history);
    key(&mut app, KeyCode::Esc);
    app.click_at(x, y, MouseButton::Left).await;
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.app.home.pages().cloned().collect::<Vec<_>>(), history);
    assert_eq!(app.player.calls(), player_calls);
    assert!(app.screen().contains("First detail"));
    assert!(app.screen().contains("close"));
}

#[tokio::test]
async fn full_message_scroll_remains_valid_through_shrink_and_growth() {
    let mut app = sized("popup-resize", 60, 14);
    notify(&mut app, &long_message(), "Detailed failure");
    app.press("!").await;
    key(&mut app, KeyCode::End);

    resize(&mut app, 140, 45);
    assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));
    resize(&mut app, 20, 8);
    key(&mut app, KeyCode::End);
    assert!(app.screen().contains("close"));
    resize(&mut app, 140, 45);
    assert!(app.screen().contains("FINAL_MESSAGE_SENTINEL"));

    for (width, height) in [(40, 12), (140, 45), (60, 14)] {
        resize(&mut app, width, height);
        key(&mut app, KeyCode::End);
        assert!(
            app.screen().contains("FINAL_MESSAGE_SENTINEL"),
            "tail at {width}x{height}"
        );
        key(&mut app, KeyCode::Home);
        assert!(
            app.screen().contains("Message detail 000"),
            "head at {width}x{height}"
        );
    }
}

#[tokio::test]
async fn help_focus_scrolls_to_the_final_binding_without_moving_the_browser() {
    let mut app = Harness::start("help-popup-focus").await;
    resize(&mut app, 80, 18);
    let cursor = app.app.home.cursor();
    let history = app.app.home.pages().cloned().collect::<Vec<_>>();
    let tab = app.app.tab;
    app.press("?").await;
    assert!(app.app.help_visible());
    for arrow in [KeyCode::Down, KeyCode::Up, KeyCode::Left, KeyCode::Right] {
        key(&mut app, arrow);
        assert_eq!(app.app.home.cursor(), cursor);
        assert_eq!(app.app.tab, tab);
    }
    key(&mut app, KeyCode::PageDown);
    key(&mut app, KeyCode::End);

    let last = BINDINGS
        .iter()
        .rev()
        .find(|binding| binding.show)
        .expect("visible help binding");
    assert!(app.screen().contains(last.description));
    assert!(app.screen().contains("close"));
    assert_eq!(app.app.home.cursor(), cursor);
    assert_eq!(app.app.home.pages().cloned().collect::<Vec<_>>(), history);

    key(&mut app, KeyCode::Esc);
    assert!(!app.app.help_visible());
    app.press("j").await;
    assert_ne!(app.app.home.cursor(), cursor);
}

#[tokio::test]
async fn help_scroll_clamps_after_growth_and_stays_reachable_after_shrink() {
    let mut app = sized("help-popup-resize", 60, 14);
    app.draw();
    app.press("?").await;
    key(&mut app, KeyCode::End);
    let last = BINDINGS
        .iter()
        .rev()
        .find(|binding| binding.show)
        .expect("visible help binding");

    resize(&mut app, 140, 80);
    assert!(app.screen().contains("Play/Pause"));
    assert!(app.screen().contains(last.description));
    for (width, height) in [(40, 12), (80, 18), (140, 45)] {
        resize(&mut app, width, height);
        key(&mut app, KeyCode::End);
        assert!(
            app.screen().contains(last.description),
            "last binding at {width}x{height}"
        );
        key(&mut app, KeyCode::Home);
        assert!(
            app.screen().contains("Play/Pause"),
            "first binding at {width}x{height}"
        );
    }
}
