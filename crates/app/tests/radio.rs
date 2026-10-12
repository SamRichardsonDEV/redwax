mod support;

use std::sync::Arc;

use redwax::app::{AppEvent, Tab};
use redwax_player::PlayerControl;
use support::{FakeSource, Harness, Paged, Setup, tracks};

async fn station(name: &str, paged: Paged) -> Harness {
    let mut setup = Setup::new(name);
    setup.source = Arc::new(FakeSource {
        radio_paged: Some(paged),
        ..FakeSource::default()
    });
    let mut app = setup.build();
    app.app.start_background();
    app.settle().await;
    app.press("r").await;
    app
}

async fn advance_station(app: &mut Harness, index: usize) {
    app.player.jump(index).await.unwrap();
    app.app
        .handle_event(AppEvent::PlayerChanged(app.player.snapshot()));
    app.settle().await;
}

#[tokio::test]
async fn radio_continues_through_multiple_batches_without_restarting_playback() {
    let mut app = station("radio-refill", Paged::new(&[30, 20, 15])).await;
    assert_eq!(app.app.player_state.queue.len(), 31);
    assert!(!app.source.called("radio_continue:1"));
    app.at_position(42.5).await;
    advance_station(&mut app, 25).await;
    assert_eq!(app.app.player_state.queue.len(), 51);
    assert_eq!(app.app.player_state.index, Some(25));
    assert_eq!(app.app.player_state.position, 42.5);
    advance_station(&mut app, 45).await;
    assert_eq!(app.app.player_state.queue.len(), 66);
    assert_eq!(app.app.player_state.index, Some(45));
    assert_eq!(
        app.player.calls(),
        [
            "play:0",
            "jump:25",
            "enqueue:20:false",
            "jump:45",
            "enqueue:15:false",
        ]
    );
    app.press("q").await;
    assert_eq!(app.app.tab, Tab::Queue);
    assert_eq!(app.app.queue.top().unwrap().title, "Radio queue");
    assert!(
        app.app
            .queue
            .top()
            .unwrap()
            .subtitle
            .as_ref()
            .unwrap()
            .contains("continues automatically")
    );
}

#[tokio::test]
async fn clearing_radio_cancels_a_waiting_refill_and_keeps_the_queue_cleared() {
    let (paged, gate) = Paged::gated(&[30, 20]);
    let mut app = station("radio-clear-refill", paged).await;
    advance_station(&mut app, 25).await;
    assert!(app.source.called("radio_continue:1"));
    app.press_all(&["c", "y", "enter"]).await;
    gate.add_permits(1);
    app.settle().await;
    app.app.handle_event(AppEvent::Tick);
    app.settle().await;
    assert_eq!(app.app.player_state.queue.len(), 1);
    assert_eq!(app.app.queue.top().unwrap().title, "Queue");
    assert!(!app.player.calls().iter().any(|call| call.starts_with("enqueue:")));
}

#[tokio::test]
async fn selecting_another_page_queue_cancels_a_waiting_radio_refill() {
    let (paged, gate) = Paged::gated(&[30, 20]);
    let mut app = station("radio-replace-refill", paged).await;
    advance_station(&mut app, 25).await;
    assert!(app.source.called("radio_continue:1"));
    app.press("enter").await;
    gate.add_permits(1);
    app.settle().await;
    assert_eq!(app.player.calls(), ["play:0", "jump:25", "play:0"]);
    assert_eq!(app.app.player_state.queue, vec![tracks()[0].clone()]);
    assert_eq!(app.app.queue.top().unwrap().title, "Queue");
}

#[tokio::test]
async fn ordinary_queues_do_not_turn_into_radio_at_their_end() {
    let mut app = Harness::start("ordinary-queue-no-radio").await;
    app.playing_queue(tracks(), 2);
    app.app.handle_event(AppEvent::Tick);
    app.settle().await;
    assert!(!app.source.calls().iter().any(|call| call.starts_with("radio")));
    assert!(app.player.calls().is_empty());
    assert_eq!(app.player.snapshot().queue.len(), 3);
}
