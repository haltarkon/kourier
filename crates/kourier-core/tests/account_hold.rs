//! The account send hold (K-A: WhatsApp's reach-out time lock, `BackendEvent::AccountHold`,
//! `dispatch.rs`): the connection stays up, but messages, reactions, edits and deletes made in
//! Kourier wait in the outbox until the backend lifts the hold or its time passes; receiving
//! and read receipts go on.

use crate::support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock};
use kourier_core::error::Refusal;
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_fake::attachments::Pointer;
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_model::{
    AccountHold, AccountId, AccountUuid, Attachment, AttachmentId, AttachmentKind, Body, ConnState,
    ConversationId, HoldReason, MessageId, OutgoingState, Text,
};
use support::send::{self, d_alice, transient};
use support::{Env, WAIT, add_linked, eventually};

fn hold(until_ms: Option<u64>) -> AccountHold {
    AccountHold {
        until_ms,
        reason: HoldReason::ReachOutLocked,
    }
}

/// Online at once, the hold stated 5 s (fake clock) later, then `rest`.
fn script(name: &str, until_ms: Option<u64>, rest: impl FnOnce(Script) -> Script) -> Script {
    let s = send::script(name)
        .step(Step::Conn(ConnState::Online { detail: None }))
        .step(Step::Sleep(Duration::from_secs(5)))
        .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(Some(
            hold(until_ms),
        )))));
    rest(s)
}

/// [`script`] for an account that states first that its network writes roaming settings (an
/// archive is one: the core refuses the click of an account that does not).
fn roaming_script(
    name: &str,
    until_ms: Option<u64>,
    rest: impl FnOnce(Script) -> Script,
) -> Script {
    let s = send::script(name)
        .deliver(vec![send::roaming_caps(true)])
        .step(Step::Conn(ConnState::Online { detail: None }))
        .step(Step::Sleep(Duration::from_secs(5)))
        .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(Some(
            hold(until_ms),
        )))));
    rest(s)
}

async fn linked(s: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let h = env.handle.clone();
    eventually("online", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                    if a.iter().any(|r| matches!(r.conn, ConnState::Online { .. })))
        }
    })
    .await;
    (env, account, uuid)
}

async fn hold_now(env: &Env, account: AccountId) -> Option<AccountHold> {
    env.account(account).await.expect("account").hold
}

/// Whether the account row shows a hold.
async fn held(env: &Env, account: AccountId) -> bool {
    hold_now(env, account).await.is_some()
}

/// Moves the fake clock on, a second at a time, until `done` holds (a sleep of the script that
/// has not begun yet cannot be jumped over).
async fn run_clock_until<F: std::future::Future<Output = bool>>(
    env: &Env,
    what: &str,
    done: impl FnMut() -> F,
) {
    run_clock_by_until(env, Duration::from_secs(1), what, done).await;
}

/// [`run_clock_until`] with a step of its own (hours, for a hold of days).
async fn run_clock_by_until<F: std::future::Future<Output = bool>>(
    env: &Env,
    step: Duration,
    what: &str,
    mut done: impl FnMut() -> F,
) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        env.clock.advance(step);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The hold row of `core.db` (`settings`, the account's scope, key `hold`), as JSON.
fn stored_hold(env: &Env) -> Option<String> {
    send::core_db(&env.core_db(), |c| {
        c.query_row("SELECT value FROM settings WHERE key = 'hold'", [], |r| {
            r.get(0)
        })
        .ok()
    })
}

/// Alice's chat. The core has it a moment after the account is online (the script's first delivery is
/// ingested on its own task), and under load that moment can be long: a single look at the file fails
/// the test with "alice's chat" (seen once in a loaded run). Every look at the chat waits for it, as every
/// other wait of this file does, bounded by the support's `WAIT`.
async fn alice_conv(env: &Env) -> ConversationId {
    let db = env.core_db();
    eventually("alice's chat", WAIT, || {
        let db = db.clone();
        async move { send::conv_id(&db, &d_alice()).is_some() }
    })
    .await;
    send::conv_id(&db, &d_alice()).expect("alice's chat")
}

/// The state of the newest own message of alice's chat, as the timeline shows it; `None` while the chat or
/// the message (or its state) is not there yet.
fn newest_own_now(env: &Env) -> Option<OutgoingState> {
    let conv = send::conv_id(&env.core_db(), &d_alice())?;
    send::own_messages(&env.core_db(), conv)
        .last()
        .and_then(|m| m.out_state.clone())
}

/// [`newest_own_now`] once there is one: a command the test sent is handled by the core a moment later.
async fn newest_own(env: &Env) -> OutgoingState {
    eventually("an own message with a state", WAIT, || async {
        newest_own_now(env).is_some()
    })
    .await;
    newest_own_now(env).expect("an own message with a state")
}

async fn alice_text(env: &Env, text: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv: alice_conv(env).await,
            text: Text::plain(text),
            reply_to: None,
        })
        .expect("send");
}

async fn sent(env: &Env, n: usize) -> MessageId {
    let (db, conv) = (env.core_db(), alice_conv(env).await);
    eventually("the sent message", WAIT, || {
        let db = db.clone();
        async move {
            send::own_messages(&db, conv)
                .get(n - 1)
                .is_some_and(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
    send::own_messages(&db, conv)[n - 1].id
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(250)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_keeps_what_the_user_makes_in_the_outbox_until_the_backend_lifts_it() {
    let s = script("hold-lift", None, |s| {
        s.step(Step::Sleep(Duration::from_secs(5)))
            .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(None))))
    });
    let (mut env, account, uuid) = linked(s).await;
    // Two own messages go out before the hold, so there is something to edit and to delete.
    alice_text(&env, "mine").await;
    let mine = sent(&env, 1).await;
    alice_text(&env, "second").await;
    let second = sent(&env, 2).await;
    let sent_before = env.control.transmits(uuid).len();
    assert_eq!(sent_before, 2);
    let alices = support_message(&env, T0 - 60_000).await;

    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    let event = hold(None);
    assert_eq!(hold_now(&env, account).await, Some(event));
    // The UI is told, too.
    env.event("AccountHold", |e| match e {
        UiEvent::AccountHold { account: a, hold } if *a == account => Some(*hold),
        _ => None,
    })
    .await;

    // A message, an edit, a reaction and a delete for everyone: all wait.
    alice_text(&env, "held").await;
    env.handle
        .send(UiCommand::EditMessage {
            message: mine,
            text: Text::plain("mine, edited"),
        })
        .expect("send");
    env.handle
        .send(UiCommand::React {
            message: alices,
            emoji: "👍".into(),
            remove: false,
        })
        .expect("send");
    env.handle
        .send(UiCommand::DeleteMessage {
            message: second,
            for_everyone: true,
        })
        .expect("send");
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        sent_before,
        "something left the outbox during the hold"
    );
    let rows = send::own_messages(&env.core_db(), alice_conv(&env).await);
    assert_eq!(rows.len(), 3, "the held message is stored: {rows:?}");

    // The backend lifts it: the four go out.
    run_clock_until(&env, "the hold is lifted", || async {
        !held(&env, account).await
    })
    .await;
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.transmits(uuid).len() >= sent_before + 4),
    )
    .await
    .expect("the four held requests were transmitted after the lift");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_with_a_time_ends_by_itself() {
    let until = u64::try_from(T0).expect("T0") + 60_000;
    let s = script("hold-time", Some(until), |s| s);
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    alice_text(&env, "held").await;
    settle().await;
    assert!(
        env.control.transmits(uuid).is_empty(),
        "sent during the hold"
    );
    // Not yet: a moment before the hold's time nothing goes out.
    let now = env.clock.now_ms();
    env.clock
        .advance(Duration::from_millis(until.saturating_sub(now + 1_000)));
    settle().await;
    assert!(env.control.transmits(uuid).is_empty());
    // Its time passes: the timer wakes the outbox, the message goes, the UI is told.
    env.clock.advance(Duration::from_secs(2));
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the held message went out when the hold ran out");
    eventually("the row has no hold", WAIT, || async {
        !held(&env, account).await
    })
    .await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiving_and_read_receipts_go_on_during_a_hold() {
    let incoming = remote(&d_alice(), &send::alice(), "9001", T0 + 9_000, "while held");
    let s = script("hold-receive", None, |s| {
        s.deliver(vec![BackendEvent::Message(incoming)])
    });
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    // The message that arrived during the hold is in the chat, unread.
    let chat = alice_conv(&env).await;
    let db = env.core_db();
    eventually("the incoming message", WAIT, || {
        let db = db.clone();
        async move {
            send::core_db(&db, |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM messages WHERE net_local = '9001'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    == 1
            })
        }
    })
    .await;
    // Reading the chat tells the network (the read receipts are no send of the user's).
    env.handle
        .send(UiCommand::MarkRead { conv: chat })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.commands(uuid).contains(&"SendReceipts")),
    )
    .await
    .expect("the receipts went out during the hold");
    assert!(hold_now(&env, account).await.is_some(), "still held");
    env.stop().await;
}

/// The summary of a catch-up is not something the user sends: it is told while the hold lasts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catch_up_summary_is_told_while_a_hold_lasts() {
    let mut caught_up = remote(&d_alice(), &send::alice(), "9002", T0 + 9_000, "caught up");
    caught_up.catch_up = true;
    // Syncing, the hold stated, one catch-up message, then the catch-up ends.
    let s = send::script("hold-summary")
        .step(Step::Conn(ConnState::Syncing { fraction: None }))
        .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(Some(
            hold(None),
        )))))
        .deliver(vec![BackendEvent::Message(caught_up)])
        .step(Step::CatchUpDone);
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let summary = env
        .event("the summary", |e| match e {
            UiEvent::CatchUpSummary {
                account: a,
                messages,
                conversations,
            } if *a == account => Some((*messages, *conversations)),
            _ => None,
        })
        .await;
    assert_eq!(summary, (1, 1));
    assert!(hold_now(&env, account).await.is_some(), "the hold stands");
    env.stop().await;
}

/// A download the user asks for sends nothing of theirs to anybody: it goes on during a hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_goes_on_during_a_hold() {
    let mut m = remote(&d_alice(), &send::alice(), "9003", T0 + 9_000, "");
    m.body = Body::Media {
        attachments: vec![Attachment {
            kind: AttachmentKind::Video,
            mime: "video/mp4".into(),
            size: 5000,
            remote: Some(Pointer::Bytes { len: 5000, seed: 7 }.to_blob()),
            ..Attachment::default()
        }],
        caption: None,
    };
    let s = script("hold-download", None, |s| {
        s.deliver(vec![BackendEvent::Message(m)])
    });
    let (mut env, account, _uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    // The video is not fetched by the policy: it waits for the click.
    let db = env.core_db();
    let attachment = || {
        send::core_db(&db, |c| {
            c.query_row("SELECT id, state FROM attachments", [], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .ok()
        })
    };
    eventually("the attachment is stored, not downloaded", WAIT, || async {
        attachment().is_some_and(|(_, state)| state.contains("Remote"))
    })
    .await;
    let (id, _) = attachment().expect("attachment");
    env.handle
        .send(UiCommand::DownloadAttachment {
            attachment: AttachmentId(id),
        })
        .expect("send");
    eventually("the download finished during the hold", WAIT, || async {
        attachment().is_some_and(|(_, state)| state.contains("Local"))
    })
    .await;
    assert!(hold_now(&env, account).await.is_some(), "still held");
    env.stop().await;
}

/// The local id of the message with the network id `local`.
async fn support_message(env: &Env, local: i64) -> MessageId {
    let l = local.to_string();
    let db = env.core_db();
    eventually("the message", WAIT, || {
        let (db, l) = (db.clone(), l.clone());
        async move {
            send::core_db(&db, |c| {
                c.query_row("SELECT 1 FROM messages WHERE net_local = ?1", [&l], |_| {
                    Ok(())
                })
                .is_ok()
            })
        }
    })
    .await;
    send::core_db(&db, |c| {
        c.query_row("SELECT id FROM messages WHERE net_local = ?1", [&l], |r| {
            r.get(0).map(MessageId)
        })
        .expect("message")
    })
}

/// C1 (K-A fix round): the hold survives a restart of Kourier. The backend restates it only
/// after its connection is `Online` (the fake: five seconds later on its clock), and nothing
/// orders that before the first state the core can send in: the core takes the stored hold up
/// at its start, so a message queued during the hold is not sent into the lock meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_survives_a_restart_and_nothing_is_sent_before_it_is_stated_again() {
    let until = u64::try_from(T0).expect("T0") + 3_600_000;
    let s = send::script("hold-restart").after_connect(
        Duration::from_secs(5),
        BackendEvent::AccountHold(Some(hold(Some(until)))),
    );
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    eventually("the hold is stored", WAIT, || async {
        stored_hold(&env).is_some()
    })
    .await;
    alice_text(&env, "held").await;
    settle().await;
    assert!(
        env.control.transmits(uuid).is_empty(),
        "sent during the hold"
    );

    // Kourier restarts. The fake connects and is `Online` at once, and states the hold again
    // only five seconds later: the clock stands, so it has not yet.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let h = env.handle.clone();
    eventually("online again", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                    if a.iter().any(|r| matches!(r.conn, ConnState::Online { .. })))
        }
    })
    .await;
    assert!(
        held(&env, account).await,
        "the account shows the hold of the last run before the backend said anything"
    );
    settle().await;
    assert!(
        env.control.transmits(uuid).is_empty(),
        "a message went out between the connect and the backend's statement of the hold"
    );

    // The backend states it again: still nothing. Its date passes: the message goes.
    env.clock.advance(Duration::from_secs(6));
    settle().await;
    assert!(env.control.transmits(uuid).is_empty());
    let now = env.clock.now_ms();
    env.clock
        .advance(Duration::from_millis(until.saturating_sub(now) + 2_000));
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the held message went out when the hold ran out");
    eventually("the stored hold is forgotten", WAIT, || async {
        stored_hold(&env).is_none()
    })
    .await;
    env.stop().await;
}

/// A stored hold whose date has passed at the next start is not taken up: the message that
/// waited goes out at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_hold_that_ran_out_while_kourier_was_closed_is_not_taken_up() {
    let until = u64::try_from(T0).expect("T0") + 60_000;
    let s = send::script("hold-ran-out").after_connect(
        Duration::from_secs(5),
        BackendEvent::AccountHold(Some(hold(Some(until)))),
    );
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    eventually("the hold is stored", WAIT, || async {
        stored_hold(&env).is_some()
    })
    .await;
    alice_text(&env, "held").await;
    settle().await;
    assert!(env.control.transmits(uuid).is_empty());
    env.stop().await;
    // Closed past the date.
    let now = env.clock.now_ms();
    env.clock
        .advance(Duration::from_millis(until.saturating_sub(now) + 10_000));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the message went out: the stored hold had run out");
    eventually("the stored hold is forgotten", WAIT, || async {
        stored_hold(&env).is_none()
    })
    .await;
    env.stop().await;
}

/// C4 (K-A fix round): a hold is no wait that counts against a message's retry budget. A
/// hold without a date, the clock far past the 24 hours of the budget: the message is still
/// queued (never "Failed" as a challenge the account did not have), and it is sent when the
/// backend lifts the hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_longer_than_the_retry_budget_does_not_fail_what_it_holds() {
    let hours = |h: u64| Duration::from_secs(h * 3600);
    let s = script("hold-budget", None, |s| {
        s.step(Step::Sleep(hours(30)))
            .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(None))))
    });
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    alice_text(&env, "held for days").await;
    // The core has stored the message before the clock moves on: it starts the message's budget.
    newest_own(&env).await;
    settle().await;
    assert!(env.control.transmits(uuid).is_empty());
    // 27 hours in: the budget of a send is a day.
    for _ in 0..27 {
        env.clock.advance(hours(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    settle().await;
    assert!(held(&env, account).await, "the script lifts it at 30 hours");
    assert_eq!(
        newest_own(&env).await,
        OutgoingState::Queued,
        "the hold used the message's budget up"
    );
    assert!(env.control.transmits(uuid).is_empty());
    // The lift: the message goes out.
    run_clock_by_until(&env, hours(1), "the hold is lifted", || async {
        !held(&env, account).await
    })
    .await;
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the held message went out after the lift");
    env.stop().await;
}

/// Fails the test when the newest own message has failed, and says whether it is sent.
fn sent_and_not_failed(env: &Env) -> bool {
    // No message yet is "not sent yet": the caller polls.
    let Some(state) = newest_own_now(env) else {
        return false;
    };
    assert!(
        !matches!(state, OutgoingState::Failed { .. }),
        "the message failed: the time of the hold was counted against its 24 hours ({state:?})"
    );
    matches!(state, OutgoingState::Sent { .. })
}

/// C4 (K-A fix 2): the time a hold stands does not count against a message's 24 hours. A hold
/// of 30 hours lifted by the backend, and the first attempt after the lift fails for a
/// transient reason: the message is tried again and sent. Without the time given back its
/// budget ended during the hold and that one attempt was its last (it failed at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_time_a_hold_stood_is_not_part_of_the_budget_of_what_it_held() {
    let hours = |h: u64| Duration::from_secs(h * 3600);
    let s = script("hold-credit", None, |s| {
        s.step(Step::Sleep(hours(30)))
            .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(None))))
    })
    .fail_transmit(TransmitRule::attempts(transient(), 1));
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    alice_text(&env, "held for a day and more").await;
    // The core has stored the message before the clock moves on: it starts the message's budget.
    newest_own(&env).await;
    settle().await;
    for _ in 0..27 {
        env.clock.advance(hours(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    settle().await;
    assert!(held(&env, account).await, "the script lifts it at 30 hours");
    assert!(env.control.transmits(uuid).is_empty());
    // The lift, the failing first attempt, the backoff, the second attempt.
    run_clock_by_until(&env, hours(1), "the message is sent", || async {
        sent_and_not_failed(&env)
    })
    .await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "the first attempt failed, the second was sent"
    );
    env.stop().await;
}

/// The same across a restart during the hold: the hold is dated (41 hours), Kourier restarts
/// after 40 of them, the date passes, the first attempt fails transiently. The time the hold
/// stood began at its first statement, not at the restart, so the message has its budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_time_of_a_hold_is_kept_across_a_restart_during_it() {
    let hours = |h: u64| Duration::from_secs(h * 3600);
    let until = u64::try_from(T0).expect("T0") + 41 * 3_600_000;
    let s = send::script("hold-credit-restart")
        .after_connect(
            Duration::from_secs(5),
            BackendEvent::AccountHold(Some(hold(Some(until)))),
        )
        .fail_transmit(TransmitRule::attempts(transient(), 1));
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    eventually("the hold is stored", WAIT, || async {
        stored_hold(&env).is_some()
    })
    .await;
    alice_text(&env, "held across a restart").await;
    // The core has stored the message before the clock moves on: it starts the message's budget.
    newest_own(&env).await;
    settle().await;
    for _ in 0..40 {
        env.clock.advance(hours(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    settle().await;
    assert!(held(&env, account).await, "the hold stands until hour 41");
    assert!(env.control.transmits(uuid).is_empty());

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    eventually("the hold of the last run is on show", WAIT, || async {
        held(&env, account).await
    })
    .await;
    assert!(env.control.transmits(uuid).is_empty());
    // The hold's date passes in ONE step that ends exactly on it, and from there the clock moves by
    // seconds. The message's budget now ends 24 hours after the date (the time the hold stood is
    // given back, capped at the date: `give_back_held_time`), and the first attempt, which the
    // timer of the date wakes, is judged against it at the clock's reading when its outcome is
    // recorded. The old steps of an hour went on in the 5 ms the loop sleeps while the core woke,
    // wrote, sent and recorded: at a load of 60+ that took more than the 24 steps of the margin and
    // the message failed with `Network` and the time of the hold counted ("the message failed ...";
    // three package runs on the server, 2026-10-28). Seconds cannot cross the margin in any time a
    // test waits.
    let left = until.saturating_sub(env.clock.now_ms());
    env.clock.advance(Duration::from_millis(left));
    run_clock_until(&env, "the message is sent", || async {
        sent_and_not_failed(&env)
    })
    .await;
    assert_eq!(env.control.transmits(uuid).len(), 2);
    env.stop().await;
}

/// C3 (K-A fix round): a change of the disappearing timer is one more message to the network
/// on some networks, so it is refused while the hold stands, with a refusal that names the
/// hold; nothing reaches the backend. The other changes (an archive) go on, and the timer
/// change works again once the hold is lifted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timer_change_is_refused_during_a_hold() {
    let s = roaming_script("hold-timer", None, |s| {
        s.step(Step::Sleep(Duration::from_secs(60)))
            .step(Step::Ephemeral(Box::new(BackendEvent::AccountHold(None))))
    });
    let (mut env, account, uuid) = linked(s).await;
    run_clock_until(&env, "the hold is stated", || held(&env, account)).await;
    let conv = alice_conv(&env).await;
    env.handle
        .send(UiCommand::SetConversationTimer {
            conv,
            timer: Some(Duration::from_secs(3600)),
        })
        .expect("send");
    let error = env
        .event("the refusal", |e| match e {
            UiEvent::ActionFailed { action, error, .. } if *action == "SetConversationTimer" => {
                Some(error.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(error.refusal(), Some(Refusal::Held), "{error:?}");
    settle().await;
    assert!(
        env.control.changes(uuid).is_empty(),
        "the refused change reached the backend: {:?}",
        env.control.changes(uuid)
    );
    // Not a general freeze: an archive goes through.
    env.handle
        .send(UiCommand::ChangeConversation {
            conv,
            change: kourier_model::ConversationChange::SetArchived { archived: true },
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.changes(uuid).is_empty()))
        .await
        .expect("an archive is not held");
    // Lifted: the timer change is taken again.
    run_clock_until(&env, "the hold is lifted", || async {
        !held(&env, account).await
    })
    .await;
    env.handle
        .send(UiCommand::SetConversationTimer {
            conv,
            timer: Some(Duration::from_secs(3600)),
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.changes(uuid).len() >= 2))
        .await
        .expect("the timer change reached the backend after the lift");
    env.stop().await;
}
