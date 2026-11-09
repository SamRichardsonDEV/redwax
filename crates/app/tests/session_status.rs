mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use redwax::app::{AppEvent, RequestId};
use redwax_ytm::Error;

use support::Harness;

async fn next_identity_check(app: &mut Harness) -> RequestId {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match app.events.recv().await.expect("the account task responds") {
                AppEvent::AccountReady { request, .. } => return request,
                event => app.app.handle_event(event),
            }
        }
    })
    .await
    .expect("the account task does not wait for user input")
}

#[tokio::test]
async fn an_idle_login_check_marks_network_failure_unconfirmed_and_recovers_without_reloading_library() {
    let mut app = Harness::start("session-status-network").await;
    let identity = app.app.account.clone();
    let library_reads = app
        .source
        .calls()
        .iter()
        .filter(|call| *call == "library")
        .count();

    app.app.handle_event(AppEvent::CheckSession);
    let request = next_identity_check(&mut app).await;
    app.app.handle_event(AppEvent::AccountReady {
        request,
        result: Err(Error::Network("temporary outage".to_owned())),
    });
    app.draw();

    assert_eq!(app.app.account, identity);
    assert!(app.header_row().contains("Login unconfirmed"));
    assert!(app.header_row().contains(&identity));
    assert!(!app.app.account_picker_visible());
    let warning = app.app.theme().warning;
    assert!(app.styles_at(0).iter().any(|style| style.fg == Some(warning)));

    app.app.handle_event(AppEvent::CheckSession);
    app.settle().await;
    assert_eq!(app.app.account, identity);
    assert!(!app.header_row().contains("Login unconfirmed"));
    assert_eq!(
        app.source
            .calls()
            .iter()
            .filter(|call| *call == "library")
            .count(),
        library_reads
    );
}

#[tokio::test]
async fn an_idle_check_that_loses_the_identity_recovers_and_keeps_expiry_visible_after_closing_the_picker() {
    let mut app = Harness::start("session-status-expired").await;
    app.app.deps.session_refresh = Some(Arc::new(|| Err("Browser login expired".to_owned())));

    app.app.handle_event(AppEvent::CheckSession);
    let request = next_identity_check(&mut app).await;
    app.app.handle_event(AppEvent::AccountReady {
        request,
        result: Ok(None),
    });
    app.draw();
    assert!(app.header_row().contains("Refreshing login"));

    app.settle().await;
    assert!(app.app.account.is_empty());
    assert!(app.app.account_picker_visible());
    app.press("escape").await;
    assert!(!app.app.account_picker_visible());
    assert!(app.header_row().contains("Login expired"));
    assert!(app.header_row().contains("Sign in"));
}

#[tokio::test]
async fn repeated_idle_checks_preserve_the_pending_request_and_do_not_start_parallel_probes() {
    let mut app = Harness::start("session-status-coalesced-check").await;
    app.app.handle_event(AppEvent::CheckSession);
    let request = next_identity_check(&mut app).await;
    app.app.handle_event(AppEvent::CheckSession);
    app.app.handle_event(AppEvent::CheckSession);
    app.app.handle_event(AppEvent::AccountReady {
        request,
        result: Err(Error::Network("still unavailable".to_owned())),
    });
    app.settle().await;
    assert!(app.header_row().contains("Login unconfirmed"));
}

#[tokio::test]
async fn mutation_rejections_ignore_old_accounts_and_recover_once_without_replaying_the_write() {
    let mut app = Harness::start("session-status-mutation").await;
    let recovery_count = Arc::new(AtomicUsize::new(0));
    let count = recovery_count.clone();
    app.app.deps.session_refresh = Some(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let generation = app.app.deps.session_generation.load(Ordering::SeqCst);

    app.app.handle_event(AppEvent::SessionRejected {
        generation: generation + 1,
    });
    app.settle().await;
    assert_eq!(recovery_count.load(Ordering::SeqCst), 0);

    app.app.handle_event(AppEvent::SessionRejected { generation });
    app.app.handle_event(AppEvent::SessionRejected { generation });
    app.draw();
    assert!(app.header_row().contains("Refreshing login"));
    app.settle().await;

    assert_eq!(recovery_count.load(Ordering::SeqCst), 1);
    assert!(!app.header_row().contains("Refreshing login"));
    assert!(!app.app.account_picker_visible());
    assert!(
        app.source
            .liked
            .lock()
            .expect("the fake source is idle")
            .is_empty()
    );
    assert!(
        !app.source
            .calls()
            .iter()
            .any(|call| call.starts_with("add_to_playlist:"))
    );
}
