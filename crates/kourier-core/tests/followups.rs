//! The core items left open by phase F (architecture §13) and the phase E leftovers on the core
//! side, on the fake backend with the fake clock:
//!
//! - an edit typed behind an edit the network then refuses targets the latest revision the
//!   network has (§3.5.3);
//! - nothing about a message goes to people who never got it: a message whose send failed for
//!   everyone is edited in place, and its reactions and queued edits wait for its retry (§4.8);
//! - a `Failed` outbox row of a message deleted from another device does not stay;
//! - Note to Self edits have no window and no count (Signal Desktop);
//! - an expired message does not come back with a late copy, for a bounded time, also across a
//!   restart, and the sweeper never sleeps longer than a minute at a time;
//! - a merge keeps the disappearing timer of whichever copy carried one;
//! - a remote delete and an expiry both withdraw the message's notification (§8.7).

mod support;

use std::collections::HashMap;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, OutgoingKind, OutgoingRequest};
use kourier_core::facade::{
    MessageRow, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::{T0, own, remote, signal_features, target};
use kourier_fake::{Flavor, Step, TransmitRecord, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, BackendError, Body, ConvKey, ConversationId, ConversationPatch,
    DisappearKind, ErrorKind, Expiry, MessageId, MsgIdScope, NetworkKind, OutgoingState, Text,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

const DAY: i64 = 24 * 3600 * 1000;

async fn linked(script: kourier_fake::Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

/// Waits until `f` holds on `core.db`.
async fn until(env: &Env, what: &str, f: impl Fn(&kourier_db::rusqlite::Connection) -> bool) {
    let db = env.core_db();
    let f = std::sync::Arc::new(f);
    eventually(what, WAIT, || {
        let (db, f) = (db.clone(), f.clone());
        async move { core_db(&db, |c| f(c)) }
    })
    .await;
}

fn count(c: &kourier_db::rusqlite::Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// The message with network id `local` (waits for it).
async fn message(env: &Env, local: &str) -> (ConversationId, MessageId) {
    let l = local.to_owned();
    until(env, "the message", move |c| {
        c.query_row("SELECT 1 FROM messages WHERE net_local = ?1", [&l], |_| {
            Ok(())
        })
        .is_ok()
    })
    .await;
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT conversation_id, id FROM messages WHERE net_local = ?1",
            [local],
            |r| Ok((ConversationId(r.get(0)?), MessageId(r.get(1)?))),
        )
        .expect("message")
    })
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let (db, k) = (env.core_db(), key.clone());
    eventually("the conversation", WAIT, || {
        let (db, k) = (db.clone(), k.clone());
        async move { conv_id(&db, &k).is_some() }
    })
    .await;
    conv_id(&env.core_db(), key).expect("conversation")
}

async fn row(env: &Env, conv: ConversationId, id: MessageId) -> Option<MessageRow> {
    env.timeline(conv).await.into_iter().find(|r| r.id == id)
}

fn text(body: &Body) -> &str {
    match body {
        Body::Text(t) => &t.body,
        _ => "",
    }
}

fn send(env: &Env, conv: ConversationId, body: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(body),
            reply_to: None,
        })
        .expect("send");
}

fn edit(env: &Env, message: MessageId, body: &str) {
    env.handle
        .send(UiCommand::EditMessage {
            message,
            text: Text::plain(body),
        })
        .expect("send");
}

fn react(env: &Env, message: MessageId, emoji: &str) {
    env.handle
        .send(UiCommand::React {
            message,
            emoji: emoji.into(),
            remove: false,
        })
        .expect("send");
}

/// The error of the next `ActionFailed` of `action`.
async fn failed(env: &mut Env, action: &'static str) -> String {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a, error, ..
        } if *a == action => Some(error.to_string()),
        _ => None,
    })
    .await
}

async fn transmits(env: &Env, uuid: AccountUuid, n: usize) -> Vec<TransmitRecord> {
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= n))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {n} transmits"));
    env.control.transmits(uuid)
}

/// Waits until something sleeps on the fake clock until `at` (a script's `Sleep` step, a
/// timer of the core): only then does advancing the clock wake it.
async fn sleeping_until(env: &Env, at: i64) {
    let clock = env.clock.clone();
    let at = u64::try_from(at).expect("a time");
    eventually(&format!("a sleep until {at}"), WAIT, || {
        let clock = clock.clone();
        async move { clock.deadlines().contains(&at) }
    })
    .await;
}

/// Gives the core time to do something it must not do (real time; the fake clock stands still).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Waits until the own message `id` shows a failed send.
async fn send_failed(env: &Env, conv: ConversationId, id: MessageId) {
    let db = env.core_db();
    eventually("the send failed", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == id && matches!(m.out_state, Some(OutgoingState::Failed { .. })))
        }
    })
    .await;
}

fn refused() -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, "refused by the server")
}

/// An own message sent `age_ms` before T0 from another device.
fn own_at(conv: &ConvKey, age_ms: i64) -> BackendEvent {
    let at = T0 - age_ms;
    BackendEvent::Message(own(conv, &at.to_string(), at, "first version"))
}

/// What each transmit was, for one conversation: kind names in order.
fn kinds(sent: &[TransmitRecord], conv: &ConvKey) -> Vec<&'static str> {
    sent.iter()
        .filter(|t| t.req.conv == *conv)
        .map(|t| t.req.kind.name())
        .collect()
}

// ---------- edits and reactions ----------

/// The user edits twice in a row; the first edit is still on its way when the second is typed,
/// so the second is written against the first. The network refuses the first for good (it is
/// rolled back). The second then goes out naming the original, the latest revision the
/// recipients have, not the edit they never got (Signal Desktop computes the target when it
/// sends, `getTargetOfThisEditTimestamp`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_behind_a_refused_edit_targets_what_the_network_has() {
    let original = (T0 - 90_000).to_string();
    let s = script("edit-chain")
        .deliver(vec![own_at(&d_alice(), 90_000)])
        .delay_reply(d_alice(), Duration::from_secs(5))
        .fail_transmit(TransmitRule::attempts(refused(), 1).of_kind("Edit"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (conv, mine) = message(&env, &original).await;

    edit(&env, mine, "second version");
    let first = transmits(&env, uuid, 1).await[0].req.clone();
    edit(&env, mine, "third version");
    until(&env, "both edits are in the outbox", |c| {
        count(c, "SELECT count(*) FROM outbox") == 2
    })
    .await;
    // As typed, the second edit names the first: that was the latest revision then.
    let stored: String = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT request FROM outbox WHERE state = 'Queued'",
            [],
            |r| r.get(0),
        )
        .expect("the queued edit")
    });
    let stored: OutgoingRequest = serde_json::from_str(&stored).expect("request");
    let OutgoingKind::Edit { target, .. } = &stored.kind else {
        panic!("not an edit: {stored:?}");
    };
    assert_eq!(target.local, first.local);

    // The answer to the first edit arrives: refused.
    env.clock.advance(Duration::from_secs(5));
    assert!(failed(&mut env, "EditMessage").await.contains("refused"));
    let sent = transmits(&env, uuid, 2).await;
    let OutgoingKind::Edit { target, body } = &sent[1].req.kind else {
        panic!("not an edit: {:?}", sent[1].req);
    };
    assert_eq!(
        target.local.as_ref(),
        original,
        "the latest revision the network has is the original"
    );
    assert_eq!(body, &Body::text("third version"));
    assert_eq!(sent[1].req.local, stored.local, "its own id is unchanged");

    env.clock.advance(Duration::from_secs(5));
    until(&env, "the second edit is done", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    let r = row(&env, conv, mine).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (1, "third version"));
    let revisions: Vec<String> = core_db(&env.core_db(), |c| {
        let mut s = c
            .prepare("SELECT net_local FROM message_revisions ORDER BY revision")
            .expect("prepare");
        s.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    });
    assert_eq!(revisions, [original, stored.local.to_string()]);
    env.stop().await;
}

/// A message whose send failed for everyone: nobody has it. An edit changes it in place (the
/// retry sends the new text, nobody ever sees the old one), a reaction stays local, and
/// neither is sent until the message itself goes out: not at once, not after a restart. The
/// retry sends the message, then the reaction.
///
/// An edit typed while the message waited for a retry (an attempt may have reached the
/// server) is a revision queued behind it; when the send then fails for everyone, the edit
/// waits too and follows the message when it is retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_of_a_message_nobody_got_wait_for_its_retry() {
    let message_to = |c: ConvKey| {
        move |e: BackendError, n: u32| {
            TransmitRule::attempts(e, n)
                .of_kind("Message")
                .in_conv(c.clone())
        }
    };
    let (to_alice, to_bob) = (message_to(d_alice()), message_to(d_bob()));
    let s = script("unsent")
        .fail_transmit(to_alice(unregistered(), 1))
        .fail_transmit(to_bob(transient(), 2))
        .fail_transmit(to_bob(unregistered(), 1));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (a, b, g) = (
        conv(&env, &d_alice()).await,
        conv(&env, &d_bob()).await,
        conv(&env, &group()).await,
    );

    // --- failed for everyone at once: edit in place, reaction local ---
    send(&env, a, "helo");
    transmits(&env, uuid, 1).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    send_failed(&env, a, mine.id).await;
    react(&env, mine.id, "👍");
    edit(&env, mine.id, "hello");
    let h = env.handle.clone();
    eventually("edited in place, with the reaction", WAIT, || {
        let h = h.clone();
        async move {
            let q = Query::Timeline(TimelineQuery {
                conv: a,
                anchor: TimelineAnchor::Newest,
                limit: 10,
            });
            let Ok(QueryData::Timeline(p)) = h.query(q).await else {
                return false;
            };
            p.rows.iter().any(|r| {
                r.id == mine.id
                    && text(&r.body) == "hello"
                    && r.revision == 0
                    && r.reactions.iter().any(|x| x.emoji == "👍" && x.by_me())
            })
        }
    })
    .await;
    let db = env.core_db();
    let rows_of = |id: MessageId| -> Vec<(String, String)> {
        core_db(&db, |c| {
            let mut s = c
                .prepare("SELECT kind, state FROM outbox WHERE message_id = ?1 ORDER BY id")
                .expect("prepare");
            s.query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows")
        })
    };
    let pair = |k: &str, s: &str| (k.to_owned(), s.to_owned());
    assert_eq!(
        rows_of(mine.id),
        [pair("Message", "Failed"), pair("React", "Queued")],
        "no edit row: the stored request was changed"
    );
    // The dispatcher has looked at the outbox since (a later send went out): nothing of the
    // unsent message was sent.
    send(&env, g, "ping");
    let sent = transmits(&env, uuid, 2).await;
    assert_eq!(kinds(&sent, &group()), ["Message"]);
    settle().await;
    assert_eq!(kinds(&env.control.transmits(uuid), &d_alice()), ["Message"]);

    // Not after a restart either.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    send(&env, g, "ping again");
    transmits(&env, uuid, 3).await;
    settle().await;
    assert_eq!(kinds(&env.control.transmits(uuid), &d_alice()), ["Message"]);
    assert_eq!(
        rows_of(mine.id),
        [pair("Message", "Failed"), pair("React", "Queued")]
    );

    // "Retry": the message with the edited text and its first id, then the reaction.
    env.handle
        .send(UiCommand::RetryMessage { message: mine.id })
        .expect("send");
    until(&env, "the message and its reaction are sent", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1",
            [mine.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    let sent = env.control.transmits(uuid);
    assert_eq!(kinds(&sent, &d_alice()), ["Message", "Message", "React"]);
    let to_alice: Vec<&TransmitRecord> = sent.iter().filter(|t| t.req.conv == d_alice()).collect();
    let OutgoingKind::Message { body, .. } = &to_alice[1].req.kind else {
        panic!("not a message");
    };
    assert_eq!(
        body,
        &Body::text("hello"),
        "alice never sees the first text"
    );
    assert_eq!(to_alice[1].req.local.as_ref(), mine.local);
    let OutgoingKind::React { target, emoji, .. } = &to_alice[2].req.kind else {
        panic!("not a reaction");
    };
    assert_eq!(
        (target.local.as_ref(), emoji.as_str()),
        (&*mine.local, "👍")
    );
    let r = row(&env, a, mine.id).await.expect("row");
    assert!(matches!(r.out_state, Some(OutgoingState::Sent { .. })));
    assert_eq!((r.revision, r.reactions.len()), (0, 1));

    // --- an edit queued behind a send that then fails for everyone ---
    let before = env.control.transmits(uuid).len();
    send(&env, b, "see you at five");
    // Two temporary failures: the message waits 190 ms (fake clock) for its third attempt.
    transmits(&env, uuid, before + 2).await;
    let waiting = own_messages(&env.core_db(), b)[0].clone();
    until(&env, "waiting for the retry", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND state = 'Queued'
               AND attempts = 2",
            [waiting.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    edit(&env, waiting.id, "see you at six");
    until(&env, "the edit is a revision with its own row", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Edit'",
            [waiting.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    env.clock.advance(Duration::from_millis(190));
    transmits(&env, uuid, before + 3).await;
    send_failed(&env, b, waiting.id).await;
    send(&env, g, "ping once more");
    transmits(&env, uuid, before + 4).await;
    settle().await;
    assert_eq!(
        kinds(&env.control.transmits(uuid), &d_bob()),
        ["Message", "Message", "Message"],
        "the edit is held: bob has no message to edit"
    );
    assert_eq!(
        rows_of(waiting.id),
        [pair("Message", "Failed"), pair("Edit", "Queued")]
    );
    assert_eq!(
        text(&row(&env, b, waiting.id).await.expect("row").body),
        "see you at six"
    );
    // Another edit now is a revision as well, not a change in place: the stored request is the
    // text an earlier attempt may have delivered, and the held edit builds on it.
    edit(&env, waiting.id, "see you at seven");
    until(&env, "the second edit has its own row", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Edit'",
            [waiting.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 2)
    })
    .await;
    assert_eq!(
        rows_of(waiting.id),
        [
            pair("Message", "Failed"),
            pair("Edit", "Queued"),
            pair("Edit", "Queued")
        ]
    );
    let r = row(&env, b, waiting.id).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (2, "see you at seven"));
    settle().await;
    assert_eq!(
        kinds(&env.control.transmits(uuid), &d_bob()),
        ["Message", "Message", "Message"],
        "both edits are held"
    );
    env.handle
        .send(UiCommand::RetryMessage {
            message: waiting.id,
        })
        .expect("send");
    until(&env, "the message and its edits are sent", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1",
            [waiting.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    let sent = env.control.transmits(uuid);
    assert_eq!(
        kinds(&sent, &d_bob()),
        ["Message", "Message", "Message", "Message", "Edit", "Edit"]
    );
    let to_bob: Vec<&TransmitRecord> = sent.iter().filter(|t| t.req.conv == d_bob()).collect();
    let OutgoingKind::Message { body, .. } = &to_bob[3].req.kind else {
        panic!("not a message");
    };
    assert_eq!(
        body,
        &Body::text("see you at five"),
        "the text as first sent"
    );
    let OutgoingKind::Edit { target, body } = &to_bob[4].req.kind else {
        panic!("not an edit");
    };
    assert_eq!(target.local.as_ref(), waiting.local);
    assert_eq!(body, &Body::text("see you at six"));
    let first_edit = to_bob[4].req.local.clone();
    let OutgoingKind::Edit { target, body } = &to_bob[5].req.kind else {
        panic!("not an edit");
    };
    assert_eq!(target.local, first_edit, "the second edit names the first");
    assert_eq!(body, &Body::text("see you at seven"));
    env.stop().await;
}

/// What was held with a message nobody had starts its retry budget with the message's "Retry",
/// not when the user typed it: a reaction made more than a day before the retry still gets its
/// retries after a temporary failure instead of being rolled back at the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_held_gets_a_fresh_budget_with_the_retry() {
    let s = script("held-budget")
        .fail_transmit(TransmitRule::attempts(unregistered(), 1).of_kind("Message"))
        .fail_transmit(TransmitRule::attempts(transient(), 1).of_kind("React"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "hello?");
    transmits(&env, uuid, 1).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    send_failed(&env, a, mine.id).await;
    react(&env, mine.id, "👍");
    until(&env, "the reaction is held", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'React'",
            [mine.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;

    // More than the 24 h budget later the user retries the message.
    env.clock
        .advance(kourier_core::OUTBOX_BUDGET + Duration::from_secs(3600));
    env.handle
        .send(UiCommand::RetryMessage { message: mine.id })
        .expect("send");
    until(&env, "the message and its reaction are sent", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    assert_eq!(
        kinds(&env.control.transmits(uuid), &d_alice()),
        ["Message", "Message", "React", "React"],
        "the reaction was tried again after its temporary failure"
    );
    let r = row(&env, a, mine.id).await.expect("row");
    assert!(
        r.reactions.iter().any(|x| x.emoji == "👍" && x.by_me()),
        "kept, not rolled back: {:?}",
        r.reactions
    );
    env.stop().await;
}

/// The delete from another device arrives while the message's own transmit is in flight, and
/// the transmit then fails for good. The row is answered first (a transmit in flight keeps its
/// row) and goes with the answer: a `Failed` row of a tombstone would wait for a "Retry" that
/// is refused, for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_deleted_elsewhere_during_its_send_leaves_no_row() {
    // The core clock starts at T0 and does not move before the send: its id is T0.
    let local = T0.to_string();
    let gone = BackendEvent::Deleted {
        target: target(Some(&d_alice()), None, &local),
        by: None,
        for_everyone: true,
        at: T0 + 100,
    };
    let s = script("deleted-in-flight")
        .delay_reply(d_alice(), Duration::from_secs(5))
        .fail_transmit(TransmitRule::attempts(unregistered(), 1).of_kind("Message"))
        .step(Step::Sleep(Duration::from_millis(100)))
        .deliver(vec![gone]);
    let (mut env, account, uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "wrong chat");
    transmits(&env, uuid, 1).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    assert_eq!(mine.local, local);

    // The phone deletes it; the answer to the transmit is still on its way.
    sleeping_until(&env, T0 + 100).await;
    env.clock.advance(Duration::from_millis(100));
    env.wait_finished(account).await;
    until(&env, "deleted by the phone", move |c| {
        c.query_row(
            "SELECT deleted FROM messages WHERE id = ?1",
            [mine.id.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .is_ok_and(|d| d.as_deref() == Some("ForEveryone"))
    })
    .await;
    let in_flight: Vec<String> = core_db(&env.core_db(), |c| {
        let mut s = c.prepare("SELECT state FROM outbox").expect("prepare");
        s.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    });
    assert_eq!(in_flight, ["Transmitting"], "answered first");

    // The answer: refused for good.
    env.clock.advance(Duration::from_secs(5));
    until(&env, "the row went with the answer", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "nothing is sent again"
    );
    env.stop().await;
}

/// A message whose send failed keeps a `Failed` outbox row for "Retry". When the message is
/// then deleted from another device, nothing would ever remove that row: it goes with the
/// delete. A row an earlier build left behind goes at the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_row_of_a_message_deleted_elsewhere_does_not_stay() {
    // The core clock starts at T0 and does not move before the send: its id is T0.
    let local = T0.to_string();
    let gone = BackendEvent::Deleted {
        target: target(Some(&d_alice()), None, &local),
        by: None,
        for_everyone: true,
        at: T0 + 100,
    };
    let s = script("failed-then-deleted")
        .fail_transmit(TransmitRule::attempts(unregistered(), 1).of_kind("Message"))
        .step(Step::Sleep(Duration::from_millis(100)))
        .deliver(vec![gone]);
    let (mut env, account, uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "wrong chat");
    transmits(&env, uuid, 1).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    assert_eq!(mine.local, local);
    send_failed(&env, a, mine.id).await;
    assert_eq!(outbox_len(&env.core_db()), 1, "kept for Retry");

    // The phone deletes it.
    sleeping_until(&env, T0 + 100).await;
    env.clock.advance(Duration::from_millis(100));
    env.wait_finished(account).await;
    until(&env, "deleted by the phone", move |c| {
        c.query_row(
            "SELECT deleted FROM messages WHERE id = ?1",
            [mine.id.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .is_ok_and(|d| d.as_deref() == Some("ForEveryone"))
    })
    .await;
    assert_eq!(outbox_len(&env.core_db()), 0, "its row went with it");
    env.handle
        .send(UiCommand::RetryMessage { message: mine.id })
        .expect("send");
    assert!(
        failed(&mut env, "RetryMessage")
            .await
            .contains("nothing to retry")
    );

    // A row left by an earlier build (the tombstone has no way to lose it) goes at a start.
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(support::CORE_KEY))
            .expect("open core.db");
        conn.execute(
            "INSERT INTO outbox(message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (?1, ?2, 'Message', '{}', 'Failed', 1, 0)",
            (mine.id.0, a.0),
        )
        .expect("insert");
    }
    assert_eq!(outbox_len(&env.core_db()), 1);
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    until(&env, "the leftover row is gone", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "nothing was sent for it"
    );
    env.stop().await;
}

/// The cleanup at an actor start takes the rows of tombstones that nothing can answer any
/// more, but never a delete: a delete for everyone that still waited for its retry when
/// Kourier stopped goes out after the restart, or the others would keep the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_that_waited_goes_out_after_a_restart() {
    let s = script("delete-waits")
        .fail_transmit(TransmitRule::attempts(transient(), 2).of_kind("Delete"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "oops");
    transmits(&env, uuid, 1).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    until(&env, "the message is sent", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    env.handle
        .send(UiCommand::DeleteMessage {
            message: mine.id,
            for_everyone: true,
        })
        .expect("send");
    // Two temporary failures; the third attempt waits on the clock.
    transmits(&env, uuid, 3).await;
    until(&env, "the delete waits for its retry", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Delete'
               AND state = 'Queued' AND attempts = 2 AND next_attempt_at IS NOT NULL",
            [mine.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;

    env.stop().await;
    env.clock.advance(Duration::from_secs(1));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let all = transmits(&env, uuid, 4).await;
    assert_eq!(
        kinds(&all, &d_alice()),
        ["Message", "Delete", "Delete", "Delete"]
    );
    assert_eq!(all[3].req.local, all[1].req.local, "with its first id");
    until(&env, "the delete is done", |c| {
        count(c, "SELECT count(*) FROM outbox") == 0
    })
    .await;
    let deleted: Option<String> = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT deleted FROM messages WHERE id = ?1",
            [mine.id.0],
            |r| r.get(0),
        )
        .expect("the tombstone")
    });
    assert_eq!(deleted.as_deref(), Some("ForEveryone"));
    env.stop().await;
}

/// Signal Desktop does not bind Note to Self by the edit window or the edit count
/// (`canEditMessage`, `isWithinMaxEdits`): a note from three days ago takes its 11th and 12th
/// edit, and the row says so (`actions.edit`, no `until`). The same message in an ordinary
/// chat is too old.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn note_to_self_is_not_bound_by_the_edit_window_or_count() {
    let me = ConvKey::Direct(uid(ME));
    let mut notes = ConversationPatch::new(me.clone());
    notes.features = Some(signal_features(false));
    let note_local = (T0 - 3 * DAY).to_string();
    let other_local = (T0 - 3 * DAY - 1).to_string();
    let s = script("note-to-self").deliver(vec![
        BackendEvent::ConversationUpsert(notes),
        own_at(&me, 3 * DAY),
        own_at(&d_alice(), 3 * DAY + 1),
    ]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (notes_conv, note) = message(&env, &note_local).await;
    let (alice_conv, other) = message(&env, &other_local).await;

    let flags = row(&env, notes_conv, note).await.expect("row").actions;
    assert!(flags.edit, "{flags:?}");
    assert_eq!(flags.until, None, "nothing ends: the delete window is over");
    assert!(
        !row(&env, alice_conv, other)
            .await
            .expect("row")
            .actions
            .edit
    );
    edit(&env, other, "too late");
    assert!(failed(&mut env, "EditMessage").await.contains("too old"));

    for n in 1..=12u16 {
        edit(&env, note, &format!("shopping list v{n}"));
        until(&env, "the edit is sent", move |c| {
            count(c, "SELECT count(*) FROM outbox") == 0
                && c.query_row(
                    "SELECT revision FROM messages WHERE id = ?1",
                    [note.0],
                    |r| r.get::<_, i64>(0),
                )
                .is_ok_and(|r| r == i64::from(n))
        })
        .await;
    }
    let r = row(&env, notes_conv, note).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (12, "shopping list v12"));
    assert!(r.actions.edit, "and a 13th would be taken");
    assert_eq!(kinds(&env.control.transmits(uuid), &me).len(), 12);
    env.stop().await;
}

/// The receiving side of the same rule (`handleEditMessage` skips the 48 h check for Note to
/// Self): an edit made on the phone long after the note applies; in an ordinary chat it is
/// ignored.
#[test]
fn a_late_edit_from_another_device_applies_in_note_to_self_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    let me = ConvKey::Direct(uid(ME));
    let features = |key: &ConvKey| {
        let mut p = ConversationPatch::new(key.clone());
        p.features = Some(signal_features(false));
        BackendEvent::ConversationUpsert(p)
    };
    let late = 1_000 + 30 * DAY;
    let edited = |key: &ConvKey, local: &str| BackendEvent::Edited {
        target: target(Some(key), None, local),
        by: None,
        new_local: Some(format!("{local}-edit").into()),
        body: Body::text("edited a month later"),
        edited_at: late,
        keeps_quote: None,
    };
    h.apply(vec![
        features(&me),
        features(&d_alice()),
        BackendEvent::Message(own(&me, "1000", 1_000, "a note")),
        BackendEvent::Message(own(&d_alice(), "1001", 1_001, "a message")),
        edited(&me, "1000"),
        edited(&d_alice(), "1001"),
    ])
    .expect("apply");
    let body = |local: &str| -> (i64, String) {
        h.connection()
            .query_row(
                "SELECT revision, preview FROM messages WHERE net_local = ?1",
                [local],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row")
    };
    assert_eq!(body("1000"), (1, "edited a month later".to_owned()));
    assert_eq!(body("1001"), (0, "a message".to_owned()));
}

// ---------- disappearing messages ----------

fn harness(dir: &tempfile::TempDir, scope: MsgIdScope) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        scope,
        &[uid(ME)],
    )
    .expect("harness")
}

fn after_send(secs: u64) -> Option<Expiry> {
    Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(secs),
        started_at: None,
    })
}

/// A message from alice that disappears `secs` after it was sent.
fn secret(local: &str, sent_at: i64, secs: u64) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), local, sent_at, "burn after reading");
    m.expiry = after_send(secs);
    BackendEvent::Message(m)
}

fn marker(n: i64) -> BackendEvent {
    BackendEvent::Message(remote(
        &d_alice(),
        &alice(),
        &format!("marker-{n}"),
        T0 + n,
        "still here",
    ))
}

/// The sweeper deletes the row of an expired message. The same message delivered again later
/// (the server repeats an envelope, a second copy arrives) must not show up as a new message:
/// its key is remembered. Also when the copy comes after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_message_does_not_come_back_with_a_late_copy() {
    let local = (T0 + 1).to_string();
    let s = script("late-copy")
        .deliver(vec![secret(&local, T0 + 1, 60)])
        .step(Step::Sleep(Duration::from_secs(90)))
        .deliver(vec![secret(&local, T0 + 1, 60), marker(1)])
        .step(Step::Sleep(Duration::from_secs(90)))
        .deliver(vec![secret(&local, T0 + 1, 60), marker(2)]);
    let (mut env, account, _uuid) = linked(s).await;
    let (conv, id) = message(&env, &local).await;
    let present = |c: &kourier_db::rusqlite::Connection, local: &str| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE net_local = ?1",
            [local],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 1
    };

    sleeping_until(&env, T0 + 90_000).await;
    env.clock.advance(Duration::from_secs(61));
    until(&env, "the message expired", move |c| {
        c.query_row("SELECT count(*) FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, i64>(0)
        })
        .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(
        core_db(&env.core_db(), |c| count(
            c,
            "SELECT count(*) FROM expired_messages"
        )),
        1
    );

    // The late copy, with a message that proves the delivery was applied.
    env.clock.advance(Duration::from_secs(30));
    until(&env, "the second delivery", |c| present(c, "marker-1")).await;
    assert!(!core_db(&env.core_db(), |c| present(c, &local)));
    assert!(
        env.timeline(conv)
            .await
            .iter()
            .all(|r| text(&r.body) != "burn after reading")
    );

    // The memory is in core.db: the copy that comes after a restart is dropped too.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let mut events = env.handle.subscribe();
    // The restarted script sleeps its 90 s again, from now.
    sleeping_until(&env, T0 + 91_000 + 90_000).await;
    env.clock.advance(Duration::from_secs(90));
    until(&env, "the third delivery", |c| present(c, "marker-2")).await;
    env.wait_finished(account).await;
    assert!(!core_db(&env.core_db(), |c| present(c, &local)));
    assert_eq!(
        env.timeline(conv).await.len(),
        3,
        "alice's first, two markers"
    );
    // Only the marker was announced.
    let mut notified = Vec::new();
    while let Ok(e) = events.try_recv() {
        if let UiEvent::Notify(n) = e {
            notified.push(n.message);
        }
    }
    let (_, second_marker) = message(&env, "marker-2").await;
    assert_eq!(notified, [second_marker]);
    env.stop().await;
}

/// The memory of expired messages is bounded ([`kourier_core::EXPIRED_MESSAGE_MEMORY`]): within it a
/// copy is dropped, in any conversation for an account-wide key and only in its own for a
/// per-conversation key; after it the key is forgotten and the row purged.
#[test]
fn expired_keys_are_remembered_for_a_bounded_time() {
    let remember = i64::try_from(kourier_core::EXPIRED_MESSAGE_MEMORY.as_millis()).expect("ms");
    let start = 1_700_000_000_000;
    let msg = |conv: &ConvKey, author: &kourier_model::UserId| {
        let mut m = remote(conv, author, "777", start, "gone in a minute");
        m.expiry = after_send(60);
        BackendEvent::Message(m)
    };
    let rows = |h: &IngestHarness, table: &str| -> i64 {
        count(h.connection(), &format!("SELECT count(*) FROM {table}"))
    };

    // Signal-like: (author, id) is the key in every conversation.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    h.set_now(start);
    h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
    assert_eq!(h.sweep().expect("sweep"), 0, "not due yet");
    h.set_now(start + 60_000);
    assert_eq!(h.sweep().expect("sweep"), 1);
    assert_eq!((rows(&h, "messages"), rows(&h, "expired_messages")), (0, 1));
    h.apply(vec![msg(&d_alice(), &alice()), msg(&group(), &alice())])
        .expect("apply");
    assert_eq!(rows(&h, "messages"), 0, "dropped, also as a group message");
    assert_eq!(rows(&h, "conversations"), 1, "and no empty group was made");
    // Another author's message with the same id is another message.
    h.apply(vec![msg(&d_bob(), &bob())]).expect("apply");
    assert_eq!(rows(&h, "messages"), 1);
    // Just inside the memory: still dropped. Just outside: forgotten, a copy is a message.
    h.set_now(start + 60_000 + remember);
    h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
    assert_eq!(rows(&h, "messages"), 1);
    let late = start + 60_000 + remember + 1;
    h.set_now(late);
    h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
    assert_eq!(
        rows(&h, "messages"),
        2,
        "bob's and the late copy of alice's"
    );
    // The next sweep purges the stale key (and takes both messages, long due, with new keys).
    assert_eq!(rows(&h, "expired_messages"), 1);
    h.sweep().expect("sweep");
    assert_eq!((rows(&h, "messages"), rows(&h, "expired_messages")), (0, 2));
    let oldest: i64 = h
        .connection()
        .query_row("SELECT min(expired_at) FROM expired_messages", [], |r| {
            r.get(0)
        })
        .expect("min");
    assert_eq!(oldest, late, "the key of the first expiry is gone");

    // WhatsApp-like: the key is per conversation.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::Conversation);
    h.set_now(start);
    h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
    h.set_now(start + 60_000);
    h.sweep().expect("sweep");
    h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
    assert_eq!(rows(&h, "messages"), 0);
    h.apply(vec![msg(&group(), &alice())]).expect("apply");
    assert_eq!(
        rows(&h, "messages"),
        1,
        "the same id in another chat is new"
    );
}

/// A late copy that arrives under the sender's other identity is dropped as well once the
/// identities are merged: the remembered key follows the merge.
#[test]
fn a_remembered_key_follows_an_identity_merge() {
    // Account-wide keys name the author, per-conversation keys the conversation: both are
    // rewritten by the merge.
    for scope in [MsgIdScope::AccountAuthor, MsgIdScope::Conversation] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, scope);
        let pni = uid("pni:00000000-0000-4000-8000-0000000a11ce");
        let d_pni = ConvKey::Direct(pni.clone());
        let start = 1_700_000_000_000;
        let msg = |conv: &ConvKey, author: &kourier_model::UserId| {
            let mut m = remote(conv, author, "888", start, "gone");
            m.expiry = after_send(60);
            BackendEvent::Message(m)
        };
        h.set_now(start);
        h.apply(vec![msg(&d_pni, &pni)]).expect("apply");
        h.set_now(start + 60_000);
        h.sweep().expect("sweep");
        h.apply(vec![BackendEvent::IdentityMerged {
            from: pni,
            into: alice(),
        }])
        .expect("apply");
        h.apply(vec![msg(&d_alice(), &alice())]).expect("apply");
        assert_eq!(
            count(h.connection(), "SELECT count(*) FROM messages"),
            0,
            "{scope:?}"
        );
    }
}

/// The sweeper's sleeps on the injected clock: a deadline a week away is slept towards one
/// minute at a time (a tokio timer does not run during a suspend, so one long sleep would end
/// late by the whole suspend); the last sleep ends at the deadline itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweeper_never_sleeps_longer_than_a_minute() {
    let week = 7 * 24 * 3600;
    let local = (T0 + 1).to_string();
    let s = script("long-timer").deliver(vec![secret(&local, T0 + 1, week)]);
    let (env, account, _uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (_, id) = message(&env, &local).await;
    let expires = T0 as u64 + 1 + week * 1000;
    let armed_for = |at: u64| sleeping_until(&env, i64::try_from(at).expect("a time"));

    let now = env.clock.now_ms();
    armed_for(now + 60_000).await;
    assert!(
        !env.clock.deadlines().contains(&expires),
        "{:?}",
        env.clock.deadlines()
    );
    // It wakes, sees nothing due and sleeps the next minute.
    env.clock.advance(Duration::from_secs(60));
    armed_for(now + 120_000).await;
    assert!(env.clock.deadlines().iter().all(|d| *d <= now + 120_000));
    // Less than a minute before the deadline: it sleeps until the deadline, not past it.
    env.clock.set(expires - 20_000);
    armed_for(expires).await;
    assert!(core_db(&env.core_db(), |c| count(
        c,
        "SELECT count(*) FROM messages"
    ) == 2));
    env.clock.advance(Duration::from_secs(20));
    until(&env, "gone on time", move |c| {
        c.query_row("SELECT count(*) FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, i64>(0)
        })
        .is_ok_and(|n| n == 0)
    })
    .await;
}

/// Two copies of one message, one with a disappearing timer and one without (a copy that
/// arrived under the sender's other identity, a history import): the merged message has the
/// timer, whichever row is kept. With two timers the earlier end counts.
#[test]
fn a_merge_keeps_the_expiry_of_whichever_copy_carried_one() {
    let pni = uid("pni:00000000-0000-4000-8000-0000000a11ce");
    let d_pni = ConvKey::Direct(pni.clone());
    let plain = |conv: &ConvKey, author: &kourier_model::UserId| {
        BackendEvent::Message(remote(conv, author, "150", 150, "burn after reading"))
    };
    let timed = |conv: &ConvKey, author: &kourier_model::UserId, secs: u64| {
        let mut m = remote(conv, author, "150", 150, "burn after reading");
        m.expiry = after_send(secs);
        BackendEvent::Message(m)
    };
    let merge = BackendEvent::IdentityMerged {
        from: pni.clone(),
        into: alice(),
    };
    // (kind, timer, started, expires) of the one message left.
    type Timer = (Option<String>, Option<i64>, Option<i64>, Option<i64>);
    let merged = |first: BackendEvent, second: BackendEvent| -> Timer {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![first]).expect("apply");
        h.apply(vec![second]).expect("apply");
        assert_eq!(count(h.connection(), "SELECT count(*) FROM messages"), 2);
        h.apply(vec![merge.clone()]).expect("apply");
        assert_eq!(count(h.connection(), "SELECT count(*) FROM messages"), 1);
        h.connection()
            .query_row(
                "SELECT expiry_kind, expiry_timer_ms, expiry_started_at, expires_at FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("row")
    };
    let minute: Timer = (
        Some("AfterSend".to_owned()),
        Some(60_000),
        Some(150),
        Some(60_150),
    );
    // The older row is kept. It has no timer, the copy that goes has one.
    assert_eq!(
        merged(plain(&d_pni, &pni), timed(&d_alice(), &alice(), 60)),
        minute
    );
    assert_eq!(
        merged(plain(&d_alice(), &alice()), timed(&d_pni, &pni, 60)),
        minute
    );
    // The kept row has it.
    assert_eq!(
        merged(timed(&d_pni, &pni, 60), plain(&d_alice(), &alice())),
        minute
    );
    // Both have one: the kept row's timer, the earlier end.
    let both = merged(timed(&d_pni, &pni, 3600), timed(&d_alice(), &alice(), 60));
    assert_eq!((both.1, both.3), (Some(3_600_000), Some(60_150)));
}

/// The conversation's own timer in a merge of two chats: the side whose timer the network
/// versioned later wins, also when that is "off" (the older side's timer must not come back).
/// Without versions the kept side's timer stands, and an unset one is filled from the other.
#[test]
fn a_merge_keeps_the_newer_conversation_timer() {
    let pni = uid("pni:00000000-0000-4000-8000-0000000a11ce");
    let d_pni = ConvKey::Direct(pni.clone());
    let timer = |key: &ConvKey, secs: Option<u64>, version: Option<u32>| {
        let mut p = ConversationPatch::new(key.clone());
        p.timer = match secs {
            Some(s) => kourier_model::Patch::Set(Duration::from_secs(s)),
            None => kourier_model::Patch::Clear,
        };
        p.timer_version = version;
        BackendEvent::ConversationUpsert(p)
    };
    let merged = |from: BackendEvent, into: BackendEvent| -> (Option<i64>, Option<i64>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![from, into]).expect("apply");
        assert_eq!(
            count(h.connection(), "SELECT count(*) FROM conversations"),
            2
        );
        h.apply(vec![BackendEvent::IdentityMerged {
            from: pni.clone(),
            into: alice(),
        }])
        .expect("apply");
        h.connection()
            .query_row(
                "SELECT timer_secs, timer_version FROM conversations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the one conversation")
    };
    assert_eq!(
        merged(
            timer(&d_pni, Some(60), Some(5)),
            timer(&d_alice(), None, Some(3))
        ),
        (Some(60), Some(5)),
        "the chat that goes has the newer timer"
    );
    assert_eq!(
        merged(
            timer(&d_pni, Some(60), Some(2)),
            timer(&d_alice(), None, Some(7))
        ),
        (None, Some(7)),
        "switched off later: it stays off"
    );
    assert_eq!(
        merged(
            timer(&d_pni, Some(60), Some(4)),
            timer(&d_alice(), Some(3600), None)
        ),
        (Some(60), Some(4)),
        "a versioned timer beats one without a version"
    );
    assert_eq!(
        merged(timer(&d_pni, Some(60), None), timer(&d_alice(), None, None)),
        (Some(60), None),
        "no versions: an unset timer is filled"
    );
    assert_eq!(
        merged(
            timer(&d_pni, Some(60), None),
            timer(&d_alice(), Some(3600), None)
        ),
        (Some(3600), None),
        "no versions: the kept chat's timer stands"
    );
}

// ---------- notifications ----------

/// What the desktop keeps per shown notification (`kourier-app`'s `NotificationBook`): the
/// message it announced and the revision it showed.
type Shown = HashMap<ConversationId, (MessageId, u16)>;

/// The desktop's rule for closing a notification (`kourier-app` `desktop::policy::
/// changed_since_shown`, asked on `MessagesChanged` for a shown message and on
/// `TimelineInvalidated` of its conversation), on what the core answers: the shown message is
/// gone, deleted, or edited since.
async fn withdrawn(env: &Env, conv: ConversationId, shown: (MessageId, u16)) -> bool {
    let q = Query::Timeline(TimelineQuery {
        conv,
        anchor: TimelineAnchor::Around(shown.0),
        limit: 2,
    });
    let QueryData::Timeline(page) = env.handle.query(q).await.expect("timeline") else {
        panic!("not a timeline");
    };
    match page.rows.into_iter().find(|r| r.id == shown.0) {
        None => true,
        Some(r) => r.deleted.is_some() || r.revision != shown.1,
    }
}

/// §8.7, S13 "done when": the core announces a new message (`Notify`); when its author
/// deletes it, and when its timer runs out, the core tells the notification's owner so
/// (`MessagesChanged` naming the message, or `TimelineInvalidated` of its conversation) and
/// answers that the message is deleted or gone, which is what closes the notification. The
/// desktop side of the same chain (D-Bus `CloseNotification`) is `kourier-app`'s
/// `notifications_end_to_end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_delete_and_an_expiry_both_withdraw_the_notification() {
    let deleted_local = (T0 + 10).to_string();
    let expiring_local = (T0 + 20).to_string();
    let s = script("withdraw")
        .deliver(vec![BackendEvent::Message(remote(
            &d_bob(),
            &bob(),
            &deleted_local,
            T0 + 10,
            "sent by mistake",
        ))])
        .deliver(vec![secret(&expiring_local, T0 + 20, 60)])
        .step(Step::Sleep(Duration::from_secs(10)))
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&d_bob()), Some(&bob()), &deleted_local),
            by: Some(bob()),
            for_everyone: true,
            at: T0 + 5_000,
        }]);
    let (mut env, _account, _uuid) = linked(s).await;
    let (bob_conv, by_mistake) = message(&env, &deleted_local).await;
    let (alice_conv, expiring) = message(&env, &expiring_local).await;

    // Both are announced (the desktop shows one notification per conversation).
    let mut shown: Shown = HashMap::new();
    while !(shown.contains_key(&bob_conv) && shown.get(&alice_conv).map(|s| s.0) == Some(expiring))
    {
        let n = env
            .event("a notification", |e| match e {
                UiEvent::Notify(n) => Some(*n),
                _ => None,
            })
            .await;
        shown.insert(n.conv, (n.message, 0));
    }
    assert_eq!(shown[&bob_conv], (by_mistake, 0));
    for (conv, s) in &shown {
        assert!(!withdrawn(&env, *conv, *s).await, "still showing");
    }

    // Bob deletes his message for everyone.
    sleeping_until(&env, T0 + 10_000).await;
    env.clock.advance(Duration::from_secs(10));
    env.event("the change of bob's message", |e| match e {
        UiEvent::MessagesChanged { conv, ids }
            if *conv == bob_conv && ids.contains(&by_mistake) =>
        {
            Some(())
        }
        UiEvent::TimelineInvalidated { conv } if *conv == bob_conv => Some(()),
        _ => None,
    })
    .await;
    assert!(withdrawn(&env, bob_conv, shown[&bob_conv]).await);
    assert!(
        !withdrawn(&env, alice_conv, shown[&alice_conv]).await,
        "alice's is untouched"
    );

    // Alice's message disappears when its minute is over.
    env.clock.advance(Duration::from_secs(51));
    env.event("the reload of alice's timeline", |e| match e {
        UiEvent::TimelineInvalidated { conv } if *conv == alice_conv => Some(()),
        _ => None,
    })
    .await;
    assert!(withdrawn(&env, alice_conv, shown[&alice_conv]).await);
    // The other trigger the desktop closes on: nothing unread is left in either chat.
    let unread: Vec<(ConversationId, u32)> = env
        .conversations()
        .await
        .iter()
        .filter(|c| c.id == bob_conv || c.id == alice_conv)
        .map(|c| (c.id, c.unread))
        .collect();
    assert!(
        unread.iter().any(|(c, n)| *c == bob_conv && *n == 0),
        "{unread:?}"
    );
    // No notification was raised for the delete or the expiry.
    let mut events = Vec::new();
    while let Ok(e) = env.events.try_recv() {
        events.push(e);
    }
    assert!(
        !events.iter().any(|e| matches!(e, UiEvent::Notify(_))),
        "{events:?}"
    );
    env.stop().await;
}
