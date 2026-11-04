//! The 463 in the core (K-A, decision 5): a network that refuses a message because its
//! recipient has not written to this account (`ErrorKind::ReachOutRefused`, WhatsApp's first
//! contact from a companion device) fails the message with `FailReason::ReachOutRefused`, the
//! chat's other waiting messages fail the same way WITHOUT an attempt of their own (each would
//! be one more reach-out), `RetryMessage` is refused for it by the rule the row's
//! `MessageActions::retry` answers, taking the message back stays, other chats are untouched,
//! and nothing holds the account (no account-wide hold in K-A).

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use kourier_core::facade::{
    CoreError, MessageRow, Query, QueryData, Refusal, TimelineAnchor, TimelineQuery, UiCommand,
    UiEvent,
};
use kourier_fake::{Flavor, Script, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, BackendError, ConversationId, ErrorKind, FailReason, OutgoingState,
    Text,
};

use support::send::{self, d_alice, d_bob, own_messages};
use support::{Env, WAIT, add_linked, eventually};

fn refusal() -> BackendError {
    BackendError::new(ErrorKind::ReachOutRefused, "463")
}

fn failed_for_good(reason: FailReason) -> OutgoingState {
    OutgoingState::Failed {
        reason,
        retryable: false,
    }
}

/// A linked account whose chat with Alice answers a transmit after one minute and the chat
/// with Bob after two (on the account clock, which the test moves), the first attempt in
/// Alice's chat being refused as a reach-out.
async fn linked(script: Script) -> (Env, AccountId, AccountUuid, ConversationId, ConversationId) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.wait_finished(account).await;
    let db = env.core_db();
    eventually("the setup chats", WAIT, || {
        let db = db.clone();
        async move { send::conv_id(&db, &d_alice()).is_some() && send::conv_id(&db, &d_bob()).is_some() }
    })
    .await;
    let alice = send::conv_id(&db, &d_alice()).expect("alice");
    let bob = send::conv_id(&db, &d_bob()).expect("bob");
    (env, account, uuid, alice, bob)
}

fn text(env: &Env, conv: ConversationId, body: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(body),
            reply_to: None,
        })
        .expect("send");
}

/// Waits for the `n`-th own message of `conv`.
async fn nth(env: &Env, conv: ConversationId, n: usize) -> send::Own {
    let db = env.core_db();
    eventually("the message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, conv).len() >= n }
    })
    .await;
    own_messages(&db, conv)[n - 1].clone()
}

async fn state_is(env: &Env, conv: ConversationId, n: usize, want: OutgoingState) {
    let db = env.core_db();
    eventually(&format!("message {n} is {want:?}"), WAIT, || {
        let (db, want) = (db.clone(), want.clone());
        async move {
            own_messages(&db, conv)
                .get(n - 1)
                .is_some_and(|m| m.out_state.as_ref() == Some(&want))
        }
    })
    .await;
}

async fn transmits_reach(env: &Env, uuid: AccountUuid, n: usize) {
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= n))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {n} transmits"));
}

async fn row(env: &Env, conv: ConversationId, message: kourier_model::MessageId) -> MessageRow {
    let q = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 50,
    };
    match env
        .handle
        .query(Query::Timeline(q))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(p) => p.rows.into_iter().find(|r| r.id == message).expect("row"),
        other => panic!("unexpected {other:?}"),
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn script(name: &str) -> Script {
    send::script(name)
        .delay_reply(d_alice(), Duration::from_secs(60))
        .delay_reply(d_bob(), Duration::from_secs(120))
        .fail_transmit(TransmitRule::attempts(refusal(), 1).in_conv(d_alice()))
}

/// The refusal fails the message and, without an attempt, every other message of that chat
/// that still waits; a chat of another person keeps its queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reach_out_refusal_fails_the_chats_waiting_messages_without_an_attempt() {
    let (env, _account, uuid, alice, bob) = linked(script("reachout-queue")).await;
    // The first message is in flight (its answer is held for a minute); two more wait behind
    // it. Bob's chat has the same: one in flight (two minutes), one waiting.
    text(&env, alice, "first");
    nth(&env, alice, 1).await;
    transmits_reach(&env, uuid, 1).await;
    text(&env, alice, "second");
    text(&env, alice, "third");
    nth(&env, alice, 3).await;
    text(&env, bob, "bob one");
    nth(&env, bob, 1).await;
    transmits_reach(&env, uuid, 2).await;
    text(&env, bob, "bob two");
    nth(&env, bob, 2).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "one in flight per chat, the rest wait"
    );
    state_is(&env, alice, 2, OutgoingState::Queued).await;
    state_is(&env, alice, 3, OutgoingState::Queued).await;

    // The minute is up: the answer for the first message is the refusal.
    env.clock.advance(Duration::from_secs(60));
    let want = failed_for_good(FailReason::ReachOutRefused);
    state_is(&env, alice, 1, want.clone()).await;
    state_is(&env, alice, 2, want.clone()).await;
    state_is(&env, alice, 3, want.clone()).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "the waiting messages of Alice's chat were not tried"
    );
    // Bob's chat is another person: nothing of it changed.
    state_is(&env, bob, 2, OutgoingState::Queued).await;
    assert_eq!(
        send::outbox_len(&env.core_db()),
        5,
        "the failed rows stay for taking back, Bob's two are still queued or in flight"
    );

    // The second minute: Bob's first message is answered and his second one goes out. The
    // account was not held by the refusal.
    env.clock.advance(Duration::from_secs(60));
    transmits_reach(&env, uuid, 3).await;
    assert_eq!(
        env.control.transmits(uuid)[2].message,
        own_messages(&env.core_db(), bob)[1].id
    );
}

/// "Send Again" of a refused message is refused by the core (with the reach-out reason, no
/// attempt), the row says so (`MessageActions::retry` false) while taking it back stays; a
/// message that failed for another reason keeps the button.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_again_is_refused_for_a_reach_out_and_the_row_says_so() {
    let (mut env, _account, uuid, alice, bob) = linked(
        send::script("reachout-retry")
            .fail_transmit(TransmitRule::attempts(refusal(), 1).in_conv(d_alice()))
            .fail_transmit(TransmitRule::attempts(send::unregistered(), 1).in_conv(d_bob())),
    )
    .await;
    text(&env, alice, "to a stranger");
    let refused = nth(&env, alice, 1).await;
    state_is(&env, alice, 1, failed_for_good(FailReason::ReachOutRefused)).await;
    text(&env, bob, "to an unregistered");
    let other = nth(&env, bob, 1).await;
    state_is(&env, bob, 1, failed_for_good(FailReason::Unregistered)).await;
    transmits_reach(&env, uuid, 2).await;

    // The flags: the refused one cannot be sent again, can be taken back; the other can.
    let r = row(&env, alice, refused.id).await;
    assert!(!r.actions.retry && r.actions.take_back, "{:?}", r.actions);
    let o = row(&env, bob, other.id).await;
    assert!(o.actions.retry && o.actions.take_back, "{:?}", o.actions);

    // The command follows the flag.
    env.handle
        .send(UiCommand::RetryMessage {
            message: refused.id,
        })
        .expect("send");
    let error: Arc<CoreError> = env
        .event("the refusal", |e| match e {
            UiEvent::ActionFailed { action, error, .. } if *action == "RetryMessage" => {
                Some(error.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(error.refusal(), Some(Refusal::ReachOutRefused), "{error:?}");
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "a refused retry makes no attempt"
    );
    state_is(&env, alice, 1, failed_for_good(FailReason::ReachOutRefused)).await;

    // The other failure is sent again as before.
    env.handle
        .send(UiCommand::RetryMessage { message: other.id })
        .expect("send");
    transmits_reach(&env, uuid, 3).await;

    // And the refused one can still be taken back.
    env.handle
        .send(UiCommand::CancelMessage {
            message: refused.id,
        })
        .expect("send");
    let db = env.core_db();
    eventually("the refused message taken back", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, alice).is_empty() }
    })
    .await;
}
