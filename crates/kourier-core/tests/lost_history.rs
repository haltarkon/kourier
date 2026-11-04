//! The lost-history line reaches the UI whenever the UI starts listening (W4 fix F2): a backend
//! appends `BackendNotice::HistoryLost` at its start, before the front-end says `UiReady`, and
//! its "told" is the core's ack; so the core keeps each account's last count for the run and
//! tells it again at `UiReady`, the way it tells the `LogoutOutcome`s.

use crate::support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice};
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_fake::{Flavor, Script};
use kourier_model::AccountId;

use support::{Env, WAIT, add_linked};

fn me() -> kourier_model::UserId {
    kourier_model::UserId::parse("aci:00000000-0000-4000-8000-0000000000aa").expect("id")
}

fn lost(count: u32) -> BackendEvent {
    BackendEvent::Notice(BackendNotice::HistoryLost { count })
}

fn is_lost(e: &UiEvent, account: AccountId, count: u32) -> bool {
    matches!(e, UiEvent::Notice { account: a, notice: BackendNotice::HistoryLost { count: c } }
        if *a == account && *c == count)
}

/// An actor whose first rows carry the notice (it ships with `AccountReady`, before the
/// front-end said `UiReady`), the core's ack of them, and a UI that subscribes only after
/// that and then says `UiReady`: it gets the notice, once.
/// Mutant for the re-check: the replay at `UiReady` removed (`Shared::replay_lost_history`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ui_that_subscribes_after_the_cores_ack_gets_the_lost_history_line_at_ui_ready() {
    let mut script = Script::new("lost-line", vec![me()]);
    script.with_ready = vec![lost(3)];
    let mut env = Env::new(Flavor::WhatsAppLike, script).await;
    // No `UiReady` yet: the front-end is not listening.
    let account = add_linked(&mut env).await;
    // The first receiver (made with the core) heard the notice being applied: from here on
    // the backend counts it told, and a receiver made now cannot hear it any more.
    env.event("the notice, applied before UiReady", |e| {
        is_lost(e, account, 3).then_some(())
    })
    .await;
    let mut late = env.handle.subscribe();
    env.handle.send(UiCommand::UiReady).expect("send");
    let heard = tokio::time::timeout(WAIT, async {
        loop {
            match late.recv().await {
                Ok(e) if is_lost(&e, account, 3) => return true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert_eq!(heard, Ok(true), "the line is told at UiReady");
    // Once: nothing tells it a second time without a new notice.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut again = 0;
    while let Ok(e) = late.try_recv() {
        if is_lost(&e, account, 3) {
            again += 1;
        }
    }
    assert_eq!(again, 0, "told once");
    env.stop().await;
}

/// A removed account is not told again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_account_is_not_told_again_at_ui_ready() {
    let mut script = Script::new("lost-line-removed", vec![me()]);
    script.with_ready = vec![lost(2)];
    let mut env = Env::new(Flavor::WhatsAppLike, script).await;
    let account = add_linked(&mut env).await;
    env.event("the notice", |e| is_lost(e, account, 2).then_some(()))
        .await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    env.event("the account is gone", |e| {
        matches!(e, UiEvent::AccountRemoved { account: a } if *a == account).then_some(())
    })
    .await;
    let mut late = env.handle.subscribe();
    env.handle.send(UiCommand::UiReady).expect("send");
    tokio::time::sleep(Duration::from_millis(400)).await;
    while let Ok(e) = late.try_recv() {
        assert!(!is_lost(&e, account, 2), "told again for a removed account");
    }
    env.stop().await;
}
