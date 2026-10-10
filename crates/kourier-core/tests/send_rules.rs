//! Phase H on the core's send path (architecture §3.8, §4.8), on the fake backend with the fake
//! clock:
//!
//! - a send that gave up on attempts without a definite answer may be with the server: a
//!   delete for everyone of it is sent and an edit is a revision, while a send the network
//!   refused is taken back and edited in place, with no window (also after a restart);
//! - a reply to an own message nobody has yet is sent, behind it;
//! - a text longer than one message of the chat carries goes out as several messages;
//! - a member whose safety number changed holds the rest of a group send like a whole send
//!   that ended that way, and the members that were reached are not asked again;
//! - the wait for a challenge answer ends with each row's retry budget;
//! - when the head of a conversation's queue goes with a tombstone, the rows behind it start
//!   at once;
//! - no typing notice goes to a blocked chat, an unaccepted request, a chat we cannot send in
//!   or ourselves.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice, Clock, OutgoingKind};
use kourier_core::facade::{ActionSubject, MessageRow, UiCommand, UiEvent};
use kourier_fake::scripts::{T0, signal_features, target};
use kourier_fake::{Flavor, Script, Step, TransmitRecord, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, CannotSend, ConnState, ConvKey, ConversationId, ConversationPatch,
    Deletion, FailReason, MessageId, OutgoingState, Text, UserAction, UserId,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

const HOUR: u64 = 3600 * 1000;
const T0U: u64 = T0 as u64;
const CHALLENGE_URL: &str = "https://example.invalid/challenge";

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

async fn until(env: &Env, what: &str, f: impl Fn(&kourier_db::rusqlite::Connection) -> bool) {
    let db = env.core_db();
    let f = std::sync::Arc::new(f);
    eventually(what, WAIT, || {
        let (db, f) = (db.clone(), f.clone());
        async move { core_db(&db, |c| f(c)) }
    })
    .await;
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

fn send(env: &Env, conv: ConversationId, body: &str, reply_to: Option<MessageId>) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(body),
            reply_to,
        })
        .expect("send");
}

/// The `n`-th own message of `conv` (waits for it).
async fn queued(env: &Env, conv: ConversationId, n: usize) -> Own {
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, conv).len() >= n }
    })
    .await;
    own_messages(&db, conv)[n - 1].clone()
}

async fn transmits(env: &Env, uuid: AccountUuid, n: usize) -> Vec<TransmitRecord> {
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= n))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {n} transmits"));
    env.control.transmits(uuid)
}

/// Gives the core time to do something it must not do (real time; the fake clock stands still).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn sleeping_until(env: &Env, at: u64) {
    let clock = env.clock.clone();
    eventually(&format!("a sleep until {at}"), WAIT, || {
        let clock = clock.clone();
        async move { clock.deadlines().contains(&at) }
    })
    .await;
}

async fn out_state(env: &Env, conv: ConversationId, message: MessageId, want: OutgoingState) {
    let db = env.core_db();
    let w = want.clone();
    eventually(&format!("out_state {want:?}"), WAIT, || {
        let (db, w) = (db.clone(), w.clone());
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == message && m.out_state.as_ref() == Some(&w))
        }
    })
    .await;
}

async fn is_sent(env: &Env, conv: ConversationId, message: MessageId) {
    let db = env.core_db();
    eventually("the message is sent", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == message && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
}

async fn row(env: &Env, conv: ConversationId, id: MessageId) -> MessageRow {
    env.timeline(conv)
        .await
        .into_iter()
        .find(|r| r.id == id)
        .expect("the row")
}

/// The state of the message's own send row (`None`: it has none).
fn send_row(env: &Env, message: MessageId) -> Option<String> {
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
            [message.0],
            |r| r.get(0),
        )
        .ok()
    })
}

/// What was transmitted in `conv`: (kind, id, the members it was narrowed to).
fn wire(
    sent: &[TransmitRecord],
    conv: &ConvKey,
) -> Vec<(&'static str, String, Option<Vec<UserId>>)> {
    sent.iter()
        .filter(|t| t.req.conv == *conv)
        .map(|t| {
            (
                t.req.kind.name(),
                t.req.local.to_string(),
                t.req.only.clone(),
            )
        })
        .collect()
}

fn far_too_long() -> kourier_model::BackendError {
    rate_limited(Some(Duration::from_secs(2 * 24 * 3600)))
}

// ---------- sends that gave up ----------

/// The server asks to wait longer than the retry budget lasts, so the send gives up: `Failed`
/// and retryable. Nothing says that nobody has the message (a time-out or a temporary error is
/// no answer about that), so it is not simply "ours to take back": an edit is a revision that
/// waits for the retry, and a delete for everyone is sent. A send the network refused for good
/// is different: nobody has it, it is edited in place whatever the clock says, and deleting it
/// only removes it. All of this holds after a restart (it is read from what is stored).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_gave_up_may_be_with_the_server() {
    let s = script("gave-up")
        .fail_transmit(
            TransmitRule::attempts(far_too_long(), 1)
                .of_kind("Message")
                .in_conv(d_alice()),
        )
        .fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_bob()),
        );
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (a, b) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    send(&env, a, "maybe out there", None);
    let maybe = queued(&env, a, 1).await;
    let gave_up = OutgoingState::Failed {
        reason: FailReason::RateLimited,
        retryable: true,
    };
    out_state(&env, a, maybe.id, gave_up.clone()).await;
    send(&env, b, "never left", None);
    let never = queued(&env, b, 1).await;
    let refused = OutgoingState::Failed {
        reason: FailReason::Unregistered,
        retryable: false,
    };
    out_state(&env, b, never.id, refused.clone()).await;
    assert_eq!(transmits(&env, uuid, 2).await.len(), 2);

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    out_state(&env, a, maybe.id, gave_up).await;
    let flags = row(&env, a, maybe.id).await.actions;
    assert!(
        !flags.take_back && flags.delete_for_everyone && flags.delete_for_me && flags.edit,
        "{flags:?}"
    );
    let flags = row(&env, b, never.id).await.actions;
    assert!(
        flags.take_back && flags.delete_for_everyone && flags.edit,
        "{flags:?}"
    );
    assert_eq!(flags.until, None, "nothing of it depends on time");

    // The one that gave up: its edit is a revision, held until the message is retried.
    env.handle
        .send(UiCommand::EditMessage {
            message: maybe.id,
            text: Text::plain("maybe out there, reworded"),
        })
        .expect("send");
    until(&env, "the edit is a revision", move |c| {
        c.query_row(
            "SELECT revision = 1 AND (SELECT count(*) FROM outbox
               WHERE message_id = ?1 AND kind = 'Edit') = 1 FROM messages WHERE id = ?1",
            [maybe.id.0],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
    })
    .await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "nothing went out for a message that may have reached nobody"
    );

    // A delete for everyone goes out, in case somebody has the message; the held edit goes
    // with the message instead of being sent.
    env.handle
        .send(UiCommand::DeleteMessage {
            message: maybe.id,
            for_everyone: true,
        })
        .expect("send");
    let sent = transmits(&env, uuid, 3).await;
    let delete = sent.last().expect("the delete");
    match &delete.req.kind {
        OutgoingKind::Delete {
            target,
            for_everyone: true,
        } => assert_eq!(target.local.as_ref(), maybe.local, "it names the message"),
        other => panic!("not a delete for everyone: {}", other.name()),
    }
    assert_eq!(delete.req.only, None, "to everyone");
    until(&env, "the delete is done", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1",
            [maybe.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(
        row(&env, a, maybe.id).await.deleted,
        Some(Deletion::ForEveryone { by: None })
    );
    assert_eq!(
        env.control.transmits(uuid).len(),
        3,
        "the edit was not sent"
    );

    // The refused one: edited in place, days later, and a delete just removes it.
    env.clock.advance(Duration::from_secs(3 * 24 * 3600));
    env.handle
        .send(UiCommand::EditMessage {
            message: never.id,
            text: Text::plain("never left, reworded"),
        })
        .expect("send");
    let db = env.core_db();
    eventually("the edit in place", WAIT, || {
        let db = db.clone();
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT body LIKE '%reworded%' AND revision = 0 FROM messages WHERE id = ?1",
                    [never.id.0],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
            })
        }
    })
    .await;
    env.handle
        .send(UiCommand::DeleteMessage {
            message: never.id,
            for_everyone: true,
        })
        .expect("send");
    until(&env, "the refused message is gone", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE id = ?1",
            [never.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        3,
        "nothing was sent for a message that never left"
    );
    env.stop().await;
}

/// "Send again" does not make a message ours to take back. The send that gave up may be with
/// the server, and it still may be after the user asked for another try: until that try is
/// answered, a delete for everyone of it is sent, not just carried out here. The user is
/// offline when they retry and then think better of it (the time-outs that used up the budget
/// rarely end the moment the user looks). A message the network refused starts from nothing
/// when it is retried: nobody has it, and deleting it only removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_does_not_forget_what_may_be_with_the_server() {
    let s = script("gave-up-retry")
        .fail_transmit(
            TransmitRule::attempts(far_too_long(), 1)
                .of_kind("Message")
                .in_conv(d_alice()),
        )
        .fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_bob()),
        );
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (a, b) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    send(&env, a, "maybe out there", None);
    let maybe = queued(&env, a, 1).await;
    let gave_up = OutgoingState::Failed {
        reason: FailReason::RateLimited,
        retryable: true,
    };
    out_state(&env, a, maybe.id, gave_up.clone()).await;
    send(&env, b, "never left", None);
    let never = queued(&env, b, 1).await;
    let refused = OutgoingState::Failed {
        reason: FailReason::Unregistered,
        retryable: false,
    };
    out_state(&env, b, never.id, refused).await;
    assert_eq!(transmits(&env, uuid, 2).await.len(), 2);

    // Started again without a connection (no `UiReady`, so the backend is not told to
    // connect): what is retried now waits in the outbox.
    env.restart().await;
    out_state(&env, a, maybe.id, gave_up).await;
    for message in [maybe.id, never.id] {
        env.handle
            .send(UiCommand::RetryMessage { message })
            .expect("send");
    }
    out_state(&env, a, maybe.id, OutgoingState::Queued).await;
    out_state(&env, b, never.id, OutgoingState::Queued).await;
    let flags = row(&env, a, maybe.id).await.actions;
    assert!(
        !flags.take_back && flags.delete_for_everyone,
        "it may be with the server all the same: {flags:?}"
    );
    let flags = row(&env, b, never.id).await.actions;
    assert!(flags.take_back, "nobody has it: {flags:?}");

    for message in [maybe.id, never.id] {
        env.handle
            .send(UiCommand::DeleteMessage {
                message,
                for_everyone: true,
            })
            .expect("send");
    }
    until(
        &env,
        "one is a tombstone with its delete queued, the other is gone",
        move |c| {
            c.query_row(
                "SELECT (SELECT deleted FROM messages WHERE id = ?1) = 'ForEveryone'
               AND (SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Delete') = 1
               AND (SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Message') = 0
               AND (SELECT count(*) FROM messages WHERE id = ?2) = 0",
                [maybe.id.0, never.id.0],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        },
    )
    .await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "offline: nothing left"
    );

    // Online: the delete for everyone goes out, and nothing else (not the message, and
    // nothing for the one that never left).
    env.handle.send(UiCommand::UiReady).expect("send");
    let sent = transmits(&env, uuid, 3).await;
    let delete = sent.last().expect("the delete");
    match &delete.req.kind {
        OutgoingKind::Delete {
            target,
            for_everyone: true,
        } => assert_eq!(target.local.as_ref(), maybe.local, "it names the message"),
        other => panic!("not a delete for everyone: {}", other.name()),
    }
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 3);
    env.stop().await;
}

// ---------- replies ----------

/// Signal Desktop offers no reply to an own message that is not sent yet; that is its menu,
/// no rule of the network. Kourier queues the reply behind the message: both go out in order,
/// the reply quoting the message by the id it was given at the enqueue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_to_an_own_message_nobody_has_goes_out_behind_it() {
    let s = script("reply-unsent")
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .step(Step::Sleep(Duration::from_secs(60)))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (mut env, _account, uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    sleeping_until(&env, T0U + 60_000).await;
    send(&env, a, "the original", None);
    let original = queued(&env, a, 1).await;
    let flags = row(&env, a, original.id).await.actions;
    assert!(flags.reply && flags.take_back, "{flags:?}");
    send(&env, a, "and one more thing", Some(original.id));
    let reply = queued(&env, a, 2).await;
    settle().await;
    assert!(env.control.transmits(uuid).is_empty(), "offline");
    while let Ok(e) = env.events.try_recv() {
        assert!(
            !matches!(e, UiEvent::ActionFailed { .. }),
            "the reply was accepted: {e:?}"
        );
    }

    env.clock.advance(Duration::from_secs(60));
    let sent = transmits(&env, uuid, 2).await;
    assert_eq!(
        sent.iter().map(|t| t.message).collect::<Vec<_>>(),
        [original.id, reply.id],
        "the message first, then its reply"
    );
    let OutgoingKind::Message {
        reply_to: Some(quote),
        ..
    } = &sent[1].req.kind
    else {
        panic!("the reply carries no quote");
    };
    assert_eq!(quote.target.local.as_ref(), original.local);
    assert_eq!(
        quote.snapshot.as_ref().and_then(|s| s.text.clone()),
        Some(Text::plain("the original"))
    );
    is_sent(&env, a, reply.id).await;
    env.stop().await;
}

/// A text that is too long for one message of the chat is not refused and does not fail at
/// the backend: it goes out as several messages, cut between words, in order, each with its
/// own id and outbox row (stored together), the first carrying the quote. Put together they
/// are the text. Formatting follows its piece.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_too_long_for_one_message_is_sent_as_several() {
    // This chat's messages carry 40 bytes of text.
    let mut small = ConversationPatch::new(d_bob());
    let mut features = signal_features(false);
    features.max_text_bytes = Some(40);
    small.features = Some(features);
    let s = script("long-text").deliver(vec![BackendEvent::ConversationUpsert(small)]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let b = conv(&env, &d_bob()).await;
    send(&env, b, "what do you think?", None);
    let question = queued(&env, b, 1).await;
    is_sent(&env, b, question.id).await;

    let body = "one two three four five six seven eight nine ten eleven twelve \
                thirteen fourteen fifteen sixteen seventeen";
    assert_eq!(body.len(), 106);
    let bold = kourier_model::TextRange {
        start: 28,
        len: 22,
        kind: kourier_model::RangeKind::Bold,
    };
    assert_eq!(&body[28..50], "seven eight nine ten e");
    env.handle
        .send(UiCommand::SendText {
            conv: b,
            text: Text {
                body: body.into(),
                ranges: vec![bold],
            },
            reply_to: Some(question.id),
        })
        .expect("send");
    let sent = transmits(&env, uuid, 4).await;
    let pieces: Vec<(Text, bool)> = sent[1..]
        .iter()
        .map(|t| match &t.req.kind {
            OutgoingKind::Message {
                body: kourier_model::Body::Text(text),
                reply_to,
                ..
            } => (text.clone(), reply_to.is_some()),
            other => panic!("not a text: {}", other.name()),
        })
        .collect();
    let bodies: Vec<&str> = pieces.iter().map(|(t, _)| t.body.as_str()).collect();
    assert_eq!(
        bodies,
        [
            "one two three four five six seven eight ",
            "nine ten eleven twelve thirteen ",
            "fourteen fifteen sixteen seventeen",
        ]
    );
    assert_eq!(bodies.concat(), body, "together they are the text");
    assert!(bodies.iter().all(|p| p.len() <= 40));
    assert_eq!(
        pieces.iter().map(|(_, quotes)| *quotes).collect::<Vec<_>>(),
        [true, false, false],
        "the quote on the first"
    );
    let ranges: Vec<Vec<(u32, u32)>> = pieces
        .iter()
        .map(|(t, _)| t.ranges.iter().map(|r| (r.start, r.len)).collect())
        .collect();
    assert_eq!(
        ranges,
        [vec![(28, 12)], vec![(0, 10)], vec![]],
        "the bold stretch goes on in the next message"
    );
    // Three messages of their own, in the order of the text, with increasing ids.
    let own = own_messages(&env.core_db(), b);
    assert_eq!(own.len(), 4);
    assert!(own.windows(2).all(|w| w[0].sent_at < w[1].sent_at));
    assert_eq!(
        sent[1..].iter().map(|t| t.message).collect::<Vec<_>>(),
        own[1..].iter().map(|m| m.id).collect::<Vec<_>>()
    );
    for m in &own[1..] {
        is_sent(&env, b, m.id).await;
    }
    while let Ok(e) = env.events.try_recv() {
        assert!(!matches!(e, UiEvent::ActionFailed { .. }), "{e:?}");
    }
    env.stop().await;
}

// ---------- a member whose safety number changed ----------

/// The user the next safety-number banner names.
async fn whom(env: &mut Env) -> UserId {
    env.event("IdentityChanged notice", |e| match e {
        UiEvent::Notice {
            notice: BackendNotice::IdentityChanged { user },
            ..
        } => Some(user.clone()),
        _ => None,
    })
    .await
}

/// One member of a group has a new safety number. The others get the message; the send is
/// not over for that member: the conversation's queue waits for the user, the banner names
/// whom to confirm, and after the confirmation the same message goes to that member alone
/// (its id unchanged, as a follow-up send), then what waited behind it. Other chats go on
/// meanwhile. The same holds for a reaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_with_a_new_safety_number_holds_the_rest_of_a_group_send() {
    let s = script("partial-identity")
        .fail_transmit(
            TransmitRule::member(bob(), untrusted(bob()), 1)
                .of_kind("Message")
                .in_conv(group()),
        )
        .fail_transmit(
            TransmitRule::member(bob(), untrusted(bob()), 1)
                .of_kind("React")
                .in_conv(group()),
        );
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (g, a) = (conv(&env, &group()).await, conv(&env, &d_alice()).await);
    send(&env, g, "one", None);
    let one = queued(&env, g, 1).await;
    assert_eq!(
        whom(&mut env).await,
        bob(),
        "the banner names whom to confirm"
    );
    out_state(
        &env,
        g,
        one.id,
        OutgoingState::Failed {
            reason: FailReason::UntrustedIdentity,
            retryable: true,
        },
    )
    .await;
    // Alice has it, bob waits (not failed for good), and the row stays the head for bob.
    let states = send_state(&env.core_db(), one.id);
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(states.iter().any(|(u, s)| u == ALICE && s.contains("Sent")));
    assert!(
        states
            .iter()
            .any(|(u, s)| u == BOB && s.contains("\"retryable\":true")),
        "{states:?}"
    );
    assert_eq!(send_row(&env, one.id).as_deref(), Some("Queued"));

    send(&env, g, "two", None);
    let two = queued(&env, g, 2).await;
    send(&env, a, "elsewhere", None);
    let elsewhere = queued(&env, a, 1).await;
    is_sent(&env, a, elsewhere.id).await;
    settle().await;
    assert_eq!(
        wire(&env.control.transmits(uuid), &group()),
        [("Message", one.local.clone(), None)],
        "the group's queue waits: nothing again to alice, nothing of the next message"
    );

    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: bob(),
        })
        .expect("send");
    is_sent(&env, g, one.id).await;
    is_sent(&env, g, two.id).await;
    assert_eq!(
        wire(&env.control.transmits(uuid), &group()),
        [
            ("Message", one.local.clone(), None),
            ("Message", one.local.clone(), Some(vec![bob()])),
            ("Message", two.local.clone(), None),
        ]
    );
    let resend = env
        .control
        .transmits(uuid)
        .into_iter()
        .find(|t| t.req.only.is_some())
        .expect("the resend");
    assert!(resend.req.is_recipient_update, "a follow-up send");

    // A reaction that one member did not get for the same reason waits for the same answer;
    // it is neither rolled back nor reported.
    while env.events.try_recv().is_ok() {}
    env.handle
        .send(UiCommand::React {
            message: one.id,
            emoji: "👍".into(),
            remove: false,
        })
        .expect("send");
    assert_eq!(whom(&mut env).await, bob());
    settle().await;
    let reactions = |sent: &[TransmitRecord]| -> Vec<Option<Vec<UserId>>> {
        sent.iter()
            .filter(|t| t.req.kind.name() == "React")
            .map(|t| t.req.only.clone())
            .collect()
    };
    assert_eq!(reactions(&env.control.transmits(uuid)), [None]);
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: bob(),
        })
        .expect("send");
    until(&env, "the reaction is done", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(
        reactions(&env.control.transmits(uuid)),
        [None, Some(vec![bob()])],
        "to bob alone the second time"
    );
    let own: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT count(*) FROM reactions WHERE message_id = ?1 AND user_id = '' AND removed = 0",
            [one.id.0],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(own, 1, "the reaction stands");
    while let Ok(e) = env.events.try_recv() {
        assert!(!matches!(e, UiEvent::ActionFailed { .. }), "{e:?}");
    }
    env.stop().await;
}

// ---------- the wait for a challenge answer ----------

/// The network wants a challenge solved and the user does not answer. Every row the account's
/// pause holds gives up when its own day is over, as if its last attempt had failed: a message
/// is `Failed` and can be retried, a reaction is rolled back and reported. A message typed
/// twelve hours into the wait has twelve hours more. Nothing is sent by itself afterwards,
/// not after a restart either; "Retry" and the solved challenge send the message with the id
/// it always had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_for_a_challenge_answer_ends_with_the_retry_budget() {
    let s = script("challenge-budget")
        .fail_transmit(TransmitRule::challenge(CHALLENGE_URL))
        .accept_challenge_answer("solved");
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    let from_alice = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT id FROM messages WHERE net_author <> '' AND conversation_id = ?1",
            [a.0],
            |r| r.get(0).map(MessageId),
        )
        .expect("alice's message")
    });
    send(&env, a, "first", None);
    let first = queued(&env, a, 1).await;
    env.event("the challenge", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::SolveChallenge { .. }),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    // A reaction made now waits behind the paused message, never attempted.
    env.handle
        .send(UiCommand::React {
            message: from_alice,
            emoji: "👍".into(),
            remove: false,
        })
        .expect("send");
    until(&env, "the reaction is queued", |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind = 'React'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    let day = 24 * HOUR;
    sleeping_until(&env, T0U + day).await;

    // Twelve hours in, another message is typed.
    env.clock.advance(Duration::from_millis(12 * HOUR));
    send(&env, a, "second", None);
    let second = queued(&env, a, 2).await;
    settle().await;
    assert_eq!(
        send_row(&env, first.id).as_deref(),
        Some("Queued"),
        "still waiting"
    );

    // The first day ends one millisecond from now.
    env.clock.advance(Duration::from_millis(12 * HOUR - 1));
    settle().await;
    assert_eq!(
        send_row(&env, first.id).as_deref(),
        Some("Queued"),
        "not early"
    );
    while env.events.try_recv().is_ok() {}
    env.clock.advance(Duration::from_millis(1));
    let gave_up = OutgoingState::Failed {
        reason: FailReason::Challenge,
        retryable: true,
    };
    out_state(&env, a, first.id, gave_up.clone()).await;
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Failed"));
    let (action, subject) = env
        .event("the reaction is reported", |e| match e {
            UiEvent::ActionFailed {
                action, subject, ..
            } => Some((*action, subject.clone())),
            _ => None,
        })
        .await;
    assert_eq!(
        (action, subject),
        ("React", ActionSubject::Message(from_alice))
    );
    let left: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT (SELECT count(*) FROM reactions WHERE message_id = ?1)
                  + (SELECT count(*) FROM outbox WHERE kind = 'React')",
            [from_alice.0],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(left, 0, "rolled back, its row gone");
    assert_eq!(
        send_row(&env, second.id).as_deref(),
        Some("Queued"),
        "the second message has its own day"
    );
    sleeping_until(&env, T0U + 36 * HOUR).await;
    env.clock.advance(Duration::from_millis(12 * HOUR));
    out_state(&env, a, second.id, gave_up.clone()).await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "only the challenged attempt of the first message ever went out"
    );

    // Nothing starts by itself, not after a restart either.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1);
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Failed"));

    // "Retry" meets the challenge again; the answer lets the message through.
    env.handle
        .send(UiCommand::RetryMessage { message: first.id })
        .expect("send");
    env.event("the challenge again", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::SolveChallenge { .. }),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    env.handle
        .send(UiCommand::SubmitChallenge {
            account,
            answer: kourier_model::Redacted::new("solved".to_owned()),
        })
        .expect("send");
    is_sent(&env, a, first.id).await;
    let sent = env.control.transmits(uuid);
    assert!(
        sent.iter()
            .filter(|t| t.message == first.id)
            .all(|t| t.req.local.as_ref() == first.local),
        "every attempt of it carries the id from the enqueue"
    );
    assert!(
        sent.iter().all(|t| t.message != second.id),
        "the second message waits for its own retry"
    );
    env.stop().await;
}

// ---------- the queue behind a tombstone ----------

/// The head of a chat's queue waits out a long rate limit, another message waits behind it.
/// The phone deletes the first message. Its row goes with the tombstone, and the second
/// message is sent at once: not when the timer that was armed for the row that is gone
/// fires, ten minutes later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_behind_a_deleted_head_start_at_once() {
    // The core clock starts at T0 and does not move before the send: its id is T0.
    let local = T0.to_string();
    let gone = BackendEvent::Deleted {
        target: target(Some(&d_alice()), None, &local),
        by: None,
        for_everyone: false,
        at: T0 + 100,
    };
    let wait = Duration::from_secs(600);
    let s = script("behind-a-tombstone")
        .fail_transmit(
            TransmitRule::attempts(rate_limited(Some(wait)), 1)
                .of_kind("Message")
                .in_conv(d_alice()),
        )
        .step(Step::Sleep(Duration::from_millis(100)))
        .deliver(vec![gone]);
    let (env, _account, uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "wrong chat", None);
    let first = queued(&env, a, 1).await;
    assert_eq!(first.local, local);
    transmits(&env, uuid, 1).await;
    sleeping_until(&env, T0U + 600_000).await;
    send(&env, a, "right chat", None);
    let second = queued(&env, a, 2).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "it waits behind the head"
    );

    // The phone's delete arrives a tenth of a second later; the rate limit has 10 minutes
    // to go.
    sleeping_until(&env, T0U + 100).await;
    env.clock.advance(Duration::from_millis(100));
    let sent = transmits(&env, uuid, 2).await;
    assert_eq!(sent[1].message, second.id);
    is_sent(&env, a, second.id).await;
    assert_eq!(
        env.clock.now_ms(),
        T0U + 100,
        "nothing waited out the rate limit"
    );
    assert_eq!(
        send_row(&env, first.id),
        None,
        "the deleted message's row is gone"
    );
}

// ---------- typing ----------

/// A typing notice tells the other side that we are there and looking at their chat. It goes
/// to an ordinary chat, and to nobody we blocked, no request we did not accept, no chat we
/// cannot send in and not to ourselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_goes_only_where_a_message_could() {
    let direct = |user: &str| ConvKey::Direct(uid(user));
    let blocked = direct("aci:00000000-0000-4000-8000-00000000b10c");
    let request = direct("aci:00000000-0000-4000-8000-0000000004e9");
    let not_accepted = direct("aci:00000000-0000-4000-8000-00000000a0ac");
    let myself = direct(ME);
    let chat = |key: &ConvKey, f: &dyn Fn(&mut ConversationPatch)| {
        let mut p = ConversationPatch::new(key.clone());
        p.features = Some(signal_features(false));
        f(&mut p);
        BackendEvent::ConversationUpsert(p)
    };
    let s = script("typing").deliver(vec![
        chat(&blocked, &|p| p.blocked = Some(true)),
        chat(&request, &|p| p.request_pending = Some(true)),
        chat(&not_accepted, &|p| {
            if let Some(f) = p.features.as_mut() {
                f.cannot_send = Some(CannotSend::NotAccepted);
            }
        }),
        chat(&myself, &|p| p.is_self = Some(true)),
    ]);
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let typing = |env: &Env| {
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "SetTyping")
            .count()
    };
    let type_in = |env: &Env, conv: ConversationId| {
        env.handle
            .send(UiCommand::SetTyping { conv, on: true })
            .expect("send");
    };
    let a = conv(&env, &d_alice()).await;
    type_in(&env, a);
    let control = env.control.clone();
    let wait_for = |n: usize| {
        let control = control.clone();
        async move {
            tokio::time::timeout(
                WAIT,
                control.wait_for(|c| {
                    c.commands(uuid)
                        .iter()
                        .filter(|c| **c == "SetTyping")
                        .count()
                        >= n
                }),
            )
            .await
            .expect("the typing notice");
        }
    };
    wait_for(1).await;
    for key in [&blocked, &request, &not_accepted, &myself] {
        let c = conv(&env, key).await;
        type_in(&env, c);
    }
    // Typing commands are handled in the order they were sent: once bob's is through, the
    // four before it were looked at.
    let b = conv(&env, &d_bob()).await;
    type_in(&env, b);
    wait_for(2).await;
    settle().await;
    assert_eq!(typing(&env), 2, "alice and bob, nobody else");
}
