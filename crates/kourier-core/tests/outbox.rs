//! Outbox and send semantics on the fake with a fake clock (architecture §4.8, §11 S5): ids are
//! allocated once and survive retries and restarts, Signal's backoff inside the one-day budget,
//! `Partial` resends to the failed members only, permanent failures do not block the queue,
//! identity and challenge pauses, user retry and cancel, echo dedup, drafts and typing, and the
//! two contract fixes of phase C (poison rows, `AccountReady` first).

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage, TransmitOutcome};
use kourier_core::AccountVault;
use kourier_core::facade::{Query, QueryData, UiCommand, UiEvent};
use kourier_fake::scripts::{T0, own};
use kourier_fake::{Flavor, Step, TransmitRecord, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, ConnState, ConvKey, ConversationId, ConversationPatch, FailReason,
    MessageId, Origin, OutgoingState, Text, UserAction,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

const T0U: u64 = T0 as u64;

async fn vault_key(env: &Env, uuid: AccountUuid) -> kourier_backend_api::DbKey {
    env.vault.unwrap(uuid).await.expect("backend.db key")
}

async fn linked(script: kourier_fake::Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

/// Waits until the setup delivery is in `core.db` and returns the conversation of `key`.
async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let db = env.core_db();
    let key2 = key.clone();
    eventually("the conversation", WAIT, || {
        let (db, key) = (db.clone(), key2.clone());
        async move { conv_id(&db, &key).is_some() }
    })
    .await;
    conv_id(&db, key).expect("conversation")
}

fn send(env: &Env, conv: ConversationId, text: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(text),
            reply_to: None,
        })
        .expect("send");
}

/// Waits for the `n`-th own message of `conv` and returns it.
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
    tokio::time::sleep(Duration::from_millis(150)).await;
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

/// Waits until the dispatcher armed its backoff timer and returns the deadline.
async fn armed(env: &Env) -> u64 {
    let clock = env.clock.clone();
    eventually("a backoff timer", WAIT, || {
        let clock = clock.clone();
        async move { clock.sleepers() > 0 }
    })
    .await;
    env.clock.next_deadline().expect("deadline")
}

fn sent() -> OutgoingState {
    OutgoingState::Sent {
        server_at: Some(T0),
    }
}

/// S5 "done when": three transient failures, then success; every attempt carries the id that
/// was allocated at enqueue, and the waits are Signal's 0 ms, 190 ms, 361 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_transient_failures_then_success_keep_the_id() {
    let s = script("transient").fail_transmit(TransmitRule::attempts(transient(), 3));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "hello");
    let msg = queued(&env, c, 1).await;
    assert_eq!((msg.local.as_str(), msg.sent_at), ("1760000000000", T0));

    // Attempt 1 fails; attempt 2 follows at once (0 ms) and fails too.
    transmits(&env, uuid, 2).await;
    assert_eq!(armed(&env).await, T0U + 190, "second wait is 190 ms");
    env.clock.advance(Duration::from_millis(189));
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "not before the backoff"
    );
    env.clock.advance(Duration::from_millis(1));
    transmits(&env, uuid, 3).await;
    assert_eq!(
        armed(&env).await,
        T0U + 190 + 361,
        "third wait is 190 · 1.9 ms"
    );
    env.clock.advance(Duration::from_millis(361));
    let t = transmits(&env, uuid, 4).await;
    out_state(
        &env,
        c,
        msg.id,
        OutgoingState::Sent {
            server_at: Some(T0 + 551),
        },
    )
    .await;

    assert_eq!(t.len(), 4);
    for (i, r) in t.iter().enumerate() {
        assert_eq!(r.message, msg.id, "attempt {i}");
        assert_eq!(&*r.req.local, msg.local, "attempt {i} keeps the id");
        assert_eq!(r.req.sent_at, msg.sent_at, "attempt {i} keeps sent_at");
        assert_eq!(r.req.only, None);
        assert!(!r.req.is_recipient_update);
    }
    assert!(
        t[..3]
            .iter()
            .all(|r| matches!(r.outcome, TransmitOutcome::Failed(_)))
    );
    assert!(matches!(t[3].outcome, TransmitOutcome::Sent { .. }));
    assert_eq!(outbox_len(&env.core_db()), 0);
    let states = send_state(&env.core_db(), msg.id);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0, ALICE);
    assert!(states[0].1.contains("Sent"), "{states:?}");
}

/// S5 "done when": one rate-limited member of a group; the follow-up goes to that member only,
/// flagged as a recipient update, and the server delivered each copy once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rate_limited_member_gets_only_a_recipient_update() {
    let s = script("partial").fail_transmit(TransmitRule::member(bob(), rate_limited(None), 1));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "hi all");
    let msg = queued(&env, g, 1).await;
    let t = transmits(&env, uuid, 2).await;
    out_state(&env, g, msg.id, sent()).await;

    assert_eq!(t[0].req.only, None);
    assert!(!t[0].req.is_recipient_update);
    assert!(
        matches!(&t[0].outcome, TransmitOutcome::Partial { delivered, failed }
            if delivered.len() == 1 && delivered[0].0 == alice() && failed.len() == 1 && failed[0].0 == bob())
    );
    assert_eq!(t[1].req.only, Some(vec![bob()]), "resent only to bob");
    assert!(t[1].req.is_recipient_update);
    assert_eq!(&*t[1].req.local, msg.local);
    assert_eq!(t[1].req.sent_at, msg.sent_at);
    assert_eq!(t.len(), 2);

    let key = vault_key(&env, uuid).await;
    let log = server_log(&env.vault.backend_db(uuid), &key);
    let got: Vec<(String, bool)> = log
        .iter()
        .map(|d| (d.recipient.as_str().to_owned(), d.recipient_update))
        .collect();
    assert_eq!(got, [(ALICE.into(), false), (BOB.into(), true)]);
    assert!(log.iter().all(|d| d.local == msg.local));
    let states = send_state(&env.core_db(), msg.id);
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|(_, s)| s.contains("Sent")), "{states:?}");
}

/// A permanent failure fails that message only; the next one in the same conversation goes
/// out. "Retry" re-queues the failed one with its original id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_failure_does_not_block_the_queue_and_retry_keeps_the_id() {
    let s = script("permanent")
        .fail_transmit(TransmitRule::attempts(unregistered(), 1).in_conv(d_alice()));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "first");
    let first = queued(&env, c, 1).await;
    send(&env, c, "second");
    let second = queued(&env, c, 2).await;
    assert!(second.sent_at > first.sent_at, "ids strictly increase");
    out_state(&env, c, second.id, sent()).await;
    out_state(
        &env,
        c,
        first.id,
        OutgoingState::Failed {
            reason: FailReason::Unregistered,
            retryable: false,
        },
    )
    .await;
    assert_eq!(
        outbox_len(&env.core_db()),
        1,
        "the failed row stays for a retry"
    );

    env.handle
        .send(UiCommand::RetryMessage { message: first.id })
        .expect("send");
    let t = transmits(&env, uuid, 3).await;
    out_state(&env, c, first.id, sent()).await;
    assert_eq!(t[2].message, first.id);
    assert_eq!(&*t[2].req.local, first.local);
    assert_eq!(t[2].req.sent_at, first.sent_at);
    assert_eq!(outbox_len(&env.core_db()), 0);

    // Nothing left to retry or cancel.
    let mut events = env.handle.subscribe();
    env.handle
        .send(UiCommand::CancelMessage { message: first.id })
        .expect("send");
    let failed = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(UiEvent::ActionFailed { action, .. }) = events.recv().await {
                return action;
            }
        }
    })
    .await
    .expect("ActionFailed");
    assert_eq!(failed, "CancelMessage");
}

/// The budget is one day from enqueue: a message that keeps failing transiently ends
/// `Failed{retryable}` after about 111 attempts, all with the same id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_failures_end_after_the_one_day_budget() {
    let s = script("budget").fail_transmit(TransmitRule::attempts(transient(), u32::MAX));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "never");
    let msg = queued(&env, c, 1).await;
    let db = env.core_db();
    let failed = OutgoingState::Failed {
        reason: FailReason::Network,
        retryable: true,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if own_messages(&db, c)[0].out_state.as_ref() == Some(&failed) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "budget never ran out"
        );
        if env.clock.sleepers() > 0 {
            env.clock.advance_to_next();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let t = env.control.transmits(uuid);
    assert!((100..=120).contains(&t.len()), "{} attempts", t.len());
    assert!(t.iter().all(|r| *r.req.local == *msg.local));
    let last = t.last().expect("attempts").at_ms;
    assert!(
        last <= T0U + 24 * 3600 * 1000,
        "no attempt after the budget"
    );
    settle().await;
    assert_eq!(env.clock.sleepers(), 0, "no timer left");
    assert_eq!(outbox_len(&db), 1, "kept for a user retry");
}

/// `UntrustedIdentity` pauses that conversation's queue (other conversations go on) until the
/// user confirms the new safety number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_identity_pauses_the_conversation_until_confirmed() {
    let s = script("identity")
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (ca, cb) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    let confirm = env
        .event("IdentityChanged notice", |e| match e {
            UiEvent::Notice {
                notice: kourier_backend_api::BackendNotice::IdentityChanged { user },
                ..
            } => Some(user.clone()),
            _ => None,
        })
        .await;
    assert_eq!(confirm, alice(), "the banner names whom to confirm");
    send(&env, ca, "two");
    let two = queued(&env, ca, 2).await;
    send(&env, cb, "to bob");
    let to_bob = queued(&env, cb, 1).await;
    out_state(&env, cb, to_bob.id, sent()).await;
    out_state(
        &env,
        ca,
        one.id,
        OutgoingState::Failed {
            reason: FailReason::UntrustedIdentity,
            retryable: true,
        },
    )
    .await;
    settle().await;
    let t = env.control.transmits(uuid);
    assert!(
        t.iter().all(|r| r.message != two.id),
        "the paused queue holds the next message"
    );
    assert_eq!(t.iter().filter(|r| r.message == one.id).count(), 1);

    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: alice(),
        })
        .expect("send");
    out_state(&env, ca, two.id, sent()).await;
    out_state(&env, ca, one.id, sent()).await;
    let t = env.control.transmits(uuid);
    let order: Vec<MessageId> = t
        .iter()
        .filter(|r| r.req.conv == d_alice())
        .map(|r| r.message)
        .collect();
    assert_eq!(order, [one.id, one.id, two.id], "FIFO after the pause");
    assert!(env.control.commands(uuid).contains(&"ConfirmIdentity"));
}

/// `Challenge` pauses the whole account and shows `NeedsUserAction(SolveChallenge)`; a user
/// retry resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_pauses_the_account_until_the_user_retries() {
    let s = script("challenge")
        .fail_transmit(TransmitRule::attempts(challenge(), 1).in_conv(d_alice()));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (ca, cb) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    env.event("SolveChallenge", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::SolveChallenge { url }),
            ..
        } => Some(url.clone()),
        _ => None,
    })
    .await;
    send(&env, cb, "to bob");
    let to_bob = queued(&env, cb, 1).await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "the account waits");

    env.handle
        .send(UiCommand::RetryMessage { message: one.id })
        .expect("send");
    env.event("Online again", |e| match e {
        UiEvent::ConnState {
            state: ConnState::Online { .. },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    out_state(&env, ca, one.id, sent()).await;
    out_state(&env, cb, to_bob.id, sent()).await;
    assert_eq!(env.control.transmits(uuid).len(), 3);
    let shown = env.account(account).await.expect("account").conn;
    assert!(shown.is_online(), "{shown:?}");
}

/// Nothing is transmitted while the account is not `Online`; a queued message that never left
/// can be cancelled; the rest goes out once the account is online again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_waits_for_online_and_a_queued_message_can_be_cancelled() {
    let s = script("offline")
        .step(Step::Conn(ConnState::Connecting))
        .step(Step::Sleep(Duration::from_secs(3600)))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (env, _account, uuid) = linked(s).await;
    let c = conv(&env, &d_alice()).await;
    let h = env.handle.clone();
    eventually("the account connecting", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| r.conn == ConnState::Connecting))
        }
    })
    .await;
    send(&env, c, "cancel me");
    let first = queued(&env, c, 1).await;
    send(&env, c, "keep me");
    let second = queued(&env, c, 2).await;
    assert_eq!(first.out_state, Some(OutgoingState::Queued));
    env.handle
        .send(UiCommand::CancelMessage { message: first.id })
        .expect("send");
    let db = env.core_db();
    eventually("the cancelled message gone", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).len() == 1 }
    })
    .await;
    settle().await;
    assert!(
        env.control.transmits(uuid).is_empty(),
        "offline: nothing sent"
    );

    env.clock.advance(Duration::from_secs(3600));
    let t = transmits(&env, uuid, 1).await;
    out_state(
        &env,
        c,
        second.id,
        OutgoingState::Sent {
            server_at: Some(T0 + 3_600_000),
        },
    )
    .await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].message, second.id);
}

/// An own copy from another device with the outbox row's id merges into it (§4.8 step 6): the
/// row leaves the outbox and no further attempt is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_echo_from_another_device_ends_the_outbox_item() {
    let s = script("echo")
        .fail_transmit(TransmitRule::attempts(transient(), u32::MAX).in_conv(d_alice()))
        .step(Step::Sleep(Duration::from_secs(10)))
        .deliver(vec![BackendEvent::Message(own(
            &d_alice(),
            "1760000000000",
            T0,
            "hello",
        ))]);
    let (env, account, uuid) = linked(s).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "hello");
    let msg = queued(&env, c, 1).await;
    assert_eq!(msg.local, "1760000000000");
    transmits(&env, uuid, 2).await;
    env.clock.advance(Duration::from_secs(10));
    env.wait_finished(account).await;
    let db = env.core_db();
    eventually("the outbox row merged", WAIT, || {
        let db = db.clone();
        async move { outbox_len(&db) == 0 }
    })
    .await;
    out_state(&env, c, msg.id, OutgoingState::Sent { server_at: None }).await;
    assert_eq!(own_messages(&db, c).len(), 1, "one row, not two");
    settle().await;
    let n = env.control.transmits(uuid).len();
    env.clock.advance(Duration::from_secs(3600));
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        n,
        "no attempt after the echo"
    );
}

/// An outbox row can go while its transmit is in flight (here: the own copy from the phone
/// arrives first). The late answer is then not taken for the row queued next: that row has an
/// id of its own (outbox ids are never reused, the dispatcher keys the attempts in flight by
/// them), stays in flight and ends with its own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_answer_for_a_row_that_is_gone_is_not_recorded_for_the_next_row() {
    let local = T0.to_string();
    let s = script("late-answer")
        .delay_reply(d_alice(), Duration::from_secs(60))
        .delay_reply(d_bob(), Duration::from_secs(120))
        .fail_transmit(TransmitRule::attempts(unregistered(), 1).in_conv(d_bob()))
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver(vec![BackendEvent::Message(own(
            &d_alice(),
            &local,
            T0,
            "to alice",
        ))]);
    let (env, _account, uuid) = linked(s).await;
    let db = env.core_db();
    let (to_alice, to_bob) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    // (outbox id, message, state) of every outbox row.
    let rows = |db: &std::path::Path| -> Vec<(i64, MessageId, String)> {
        core_db(db, |c| {
            let mut stmt = c
                .prepare("SELECT id, message_id, state FROM outbox ORDER BY id")
                .expect("prepare");
            stmt.query_map([], |r| Ok((r.get(0)?, MessageId(r.get(1)?), r.get(2)?)))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows")
        })
    };

    // The server has the first message; its answer takes a minute.
    send(&env, to_alice, "to alice");
    let first = queued(&env, to_alice, 1).await;
    assert_eq!(first.local, local);
    transmits(&env, uuid, 1).await;
    let in_flight = rows(&db);
    assert_eq!(in_flight.len(), 1);
    assert_eq!(
        (in_flight[0].1, in_flight[0].2.as_str()),
        (first.id, "Transmitting")
    );
    let first_row = in_flight[0].0;

    // The phone's copy arrives before the answer: the row is done.
    env.clock.advance(Duration::from_secs(5));
    eventually("the echo ends the row", WAIT, || {
        let db = db.clone();
        async move { outbox_len(&db) == 0 }
    })
    .await;

    // The next message, in another conversation, is refused by the server; that answer takes
    // two minutes.
    send(&env, to_bob, "to bob");
    let second = queued(&env, to_bob, 1).await;
    let t = transmits(&env, uuid, 2).await;
    assert!(matches!(t[1].outcome, TransmitOutcome::Failed(_)));
    let sending = rows(&db);
    assert_eq!(sending.len(), 1);
    assert_eq!(
        (sending[0].1, sending[0].2.as_str()),
        (second.id, "Transmitting")
    );
    assert!(sending[0].0 > first_row, "a new id, not the finished row's");

    // The first answer ("accepted") arrives: recorded for the first message, and the second
    // one is still being sent.
    env.clock.advance(Duration::from_secs(55));
    eventually("the first answer is recorded", WAIT, || {
        let db = db.clone();
        async move {
            send_state(&db, first.id)
                .iter()
                .any(|(u, s)| u == ALICE && s.contains("Sent"))
        }
    })
    .await;
    assert_eq!(rows(&db), sending, "the second row is untouched");
    assert_eq!(
        own_messages(&db, to_bob)[0].out_state,
        Some(OutgoingState::Sending { progress: None })
    );

    // Its own answer ends it.
    env.clock.advance(Duration::from_secs(120));
    out_state(
        &env,
        to_bob,
        second.id,
        OutgoingState::Failed {
            reason: FailReason::Unregistered,
            retryable: false,
        },
    )
    .await;
    assert!(matches!(
        own_messages(&db, to_alice)[0].out_state,
        Some(OutgoingState::Sent { .. })
    ));
    assert_eq!(env.control.transmits(uuid).len(), 2);
}

/// A restart resumes the outbox with the stored id and `only`: a member that has the message
/// is never sent it again (in-process restart; the kill test is in `outbox_crash.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_resumes_with_the_same_id_and_only_the_missing_members() {
    let s = script("restart").fail_transmit(TransmitRule::member(
        bob(),
        rate_limited(Some(Duration::from_secs(3600))),
        1,
    ));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "hi all");
    let msg = queued(&env, g, 1).await;
    transmits(&env, uuid, 1).await;
    assert_eq!(
        armed(&env).await,
        T0U + 3_600_000,
        "the server's retry-after"
    );

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    assert_eq!(
        armed(&env).await,
        T0U + 3_600_000,
        "the backoff survives the restart"
    );
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1);
    env.clock.advance(Duration::from_secs(3600));
    let t = transmits(&env, uuid, 2).await;
    out_state(
        &env,
        g,
        msg.id,
        OutgoingState::Sent {
            server_at: Some(T0 + 3_600_000),
        },
    )
    .await;
    assert_eq!(t[1].req.only, Some(vec![bob()]));
    assert!(t[1].req.is_recipient_update);
    assert_eq!(
        (&*t[1].req.local, t[1].req.sent_at),
        (msg.local.as_str(), msg.sent_at)
    );
    let key = vault_key(&env, uuid).await;
    let log = server_log(&env.vault.backend_db(uuid), &key);
    assert_eq!(log.len(), 2, "{log:?}");
}

/// Drafts are core-local; sending clears them. Typing goes to the backend, fire-and-forget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drafts_are_stored_and_typing_is_forwarded() {
    let (env, account, uuid) = linked(script("draft")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let detail = |h: kourier_core::facade::CoreHandle| async move {
        match h.query(Query::Conversation(c)).await.expect("query") {
            QueryData::Conversation(Some(d)) => d,
            other => panic!("unexpected {other:?}"),
        }
    };
    env.handle
        .send(UiCommand::SaveDraft {
            conv: c,
            draft: Some(Text::plain("half a thought")),
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("the draft", WAIT, || {
        let h = h.clone();
        async move { detail(h).await.draft.is_some() }
    })
    .await;
    let d = detail(env.handle.clone()).await;
    assert_eq!(d.draft, Some(Text::plain("half a thought")));
    assert!(d.row.has_draft);
    send(&env, c, "the whole thought");
    queued(&env, c, 1).await;
    eventually("the draft cleared by the send", WAIT, || {
        let h = h.clone();
        async move { detail(h).await.draft.is_none() }
    })
    .await;

    env.handle
        .send(UiCommand::SetTyping { conv: c, on: true })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.commands(uuid).contains(&"SetTyping")),
    )
    .await
    .expect("SetTyping reached the backend");
    let db = env.core_db();
    let typing_rows: i64 = core_db(&db, |c| {
        c.query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(typing_rows, 0, "typing is not durable");
}

/// Contract fix B: during a link, durable events before `AccountReady` are refused for good;
/// the poison policy quarantines them and the link still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_events_before_account_ready_are_refused() {
    let early = ConvKey::Group("gv2:ZWFybHktZ3JvdXA".into());
    let mut s = script("early");
    s.early_events = vec![BackendEvent::ConversationUpsert(ConversationPatch::new(
        early.clone(),
    ))];
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    conv(&env, &d_alice()).await;
    assert_eq!(
        conv_id(&env.core_db(), &early),
        None,
        "refused, not applied"
    );
    let key = vault_key(&env, uuid).await;
    let backend = backend_db(&env.vault.backend_db(uuid), &key);
    let poison = kourier_db::outbox::poison(&backend, 10).expect("poison");
    assert_eq!(poison.len(), 1, "{poison:?}");
    assert!(
        poison[0].reason.contains("before AccountReady"),
        "{}",
        poison[0].reason
    );
    assert!(
        env.control
            .violations()
            .iter()
            .any(|v| v.contains("before AccountReady")),
        "the fake's own conformance check saw it too"
    );
}

/// Contract fix A: a row the core refuses as a contract violation is quarantined and the rows
/// after it are still delivered (the account is not blocked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poison_row_is_quarantined_and_later_rows_still_arrive() {
    let mut bad = IncomingMessage::new(
        d_alice(),
        "",
        Origin::Remote {
            author: alice(),
            device: Some(1),
        },
        kourier_model::Body::text("bad id"),
        T0,
        T0,
    );
    bad.received_at = T0;
    let later = ConvKey::Group("gv2:bGF0ZXItZ3JvdXA".into());
    let s = script("poison")
        .deliver(vec![BackendEvent::Message(bad)])
        .deliver(vec![BackendEvent::ConversationUpsert(
            ConversationPatch::new(later.clone()),
        )]);
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    assert!(
        conv_id(&env.core_db(), &later).is_some(),
        "the later row arrived"
    );
    let key = vault_key(&env, uuid).await;
    let backend = backend_db(&env.vault.backend_db(uuid), &key);
    let poison = kourier_db::outbox::poison(&backend, 10).expect("poison");
    assert_eq!(poison.len(), 1, "{poison:?}");
    assert!(
        poison[0].reason.contains("invalid id in Message"),
        "{}",
        poison[0].reason
    );
    assert_eq!(kourier_db::outbox::len(&backend).expect("len"), 0);
}

/// A group send where one member fails for good and another is rate-limited: the follow-up
/// goes to the rate-limited member only, the message ends `PartiallyFailed`, and the outbox
/// row stays so "Retry" can reach the member that failed for good (§4.8 step 5), with the
/// original id and only that member.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_that_failed_for_good_can_be_retried_after_the_others_got_it() {
    let s = script("mixed")
        .fail_transmit(TransmitRule::member(alice(), unregistered(), 1))
        .fail_transmit(TransmitRule::member(bob(), rate_limited(None), 1));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "hi all");
    let msg = queued(&env, g, 1).await;
    let t = transmits(&env, uuid, 2).await;
    assert_eq!(t[1].req.only, Some(vec![bob()]), "follow-up to bob only");
    out_state(
        &env,
        g,
        msg.id,
        OutgoingState::PartiallyFailed {
            failed: vec![alice()],
        },
    )
    .await;
    assert_eq!(outbox_len(&env.core_db()), 1, "kept for a user retry");

    env.handle
        .send(UiCommand::RetryMessage { message: msg.id })
        .expect("send");
    let t = transmits(&env, uuid, 3).await;
    out_state(&env, g, msg.id, sent()).await;
    assert_eq!(
        t[2].req.only,
        Some(vec![alice()]),
        "the retry reaches alice only"
    );
    assert!(t[2].req.is_recipient_update);
    assert_eq!(
        (&*t[2].req.local, t[2].req.sent_at),
        (msg.local.as_str(), msg.sent_at)
    );
    assert_eq!(outbox_len(&env.core_db()), 0);
    let key = vault_key(&env, uuid).await;
    let mut got: Vec<String> = server_log(&env.vault.backend_db(uuid), &key)
        .iter()
        .map(|d| d.recipient.as_str().to_owned())
        .collect();
    got.sort_unstable();
    let mut both = [ALICE, BOB];
    both.sort_unstable();
    assert_eq!(got, both, "each member exactly once");
}

/// Messages typed in a row are stored, numbered and sent in that order (one FIFO per
/// conversation), and a draft saved right before a send never survives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sends_keep_the_typing_order_and_clear_the_draft_saved_before_them() {
    let (env, account, uuid) = linked(script("order")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    const N: usize = 30;
    for i in 0..N {
        env.handle
            .send(UiCommand::SaveDraft {
                conv: c,
                draft: Some(Text::plain(format!("draft-{i:02}"))),
            })
            .expect("send");
        send(&env, c, &format!("msg-{i:02}"));
    }
    let last = queued(&env, c, N).await;
    let db = env.core_db();
    let bodies: Vec<(i64, String)> = core_db(&db, |conn| {
        let mut stmt = conn
            .prepare(
                "SELECT sent_at, body FROM messages WHERE conversation_id = ?1 AND net_author = ''
                 ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([c.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows")
    });
    for (i, (_, body)) in bodies.iter().enumerate() {
        assert!(body.contains(&format!("\"msg-{i:02}\"")), "row {i}: {body}");
    }
    assert!(
        bodies.windows(2).all(|w| w[0].0 < w[1].0),
        "sent_at follows the typing order"
    );
    out_state(&env, c, last.id, sent()).await;
    let order: Vec<MessageId> = env
        .control
        .transmits(uuid)
        .iter()
        .map(|r| r.message)
        .collect();
    let ids: Vec<MessageId> = own_messages(&db, c).iter().map(|m| m.id).collect();
    assert_eq!(order, ids, "transmitted in the typing order");
    match env
        .handle
        .query(Query::Conversation(c))
        .await
        .expect("query")
    {
        QueryData::Conversation(Some(d)) => assert_eq!(d.draft, None, "the send cleared it"),
        other => panic!("unexpected {other:?}"),
    }
}

/// A safety number confirmed while the backend restarts is not lost: the paused queue resumes,
/// the backend gets `ConfirmIdentity` when it is back, and the message goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identity_confirmed_during_a_restart_still_resumes_the_queue() {
    let s = script("confirm-offline")
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()))
        .step(Step::Sleep(Duration::from_secs(10)))
        .step(Step::Fail(transient()));
    let (mut env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    env.event("IdentityChanged notice", |e| {
        matches!(
            e,
            UiEvent::Notice {
                notice: kourier_backend_api::BackendNotice::IdentityChanged { .. },
                ..
            }
        )
        .then_some(())
    })
    .await;
    env.clock.advance(Duration::from_secs(10));
    env.event("restart backoff", |e| match e {
        UiEvent::ConnState {
            state: ConnState::Offline {
                retry_at_ms: Some(_),
            },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: alice(),
        })
        .expect("send");
    out_state(&env, ca, one.id, OutgoingState::Queued).await;
    assert!(
        !env.control.commands(uuid).contains(&"ConfirmIdentity"),
        "no backend to tell yet"
    );
    env.clock.advance(Duration::from_secs(2));
    let db = env.core_db();
    eventually("the message sent after the restart", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, ca)
                .iter()
                .any(|m| m.id == one.id && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
    assert!(env.control.commands(uuid).contains(&"ConfirmIdentity"));
    let t = env.control.transmits(uuid);
    assert_eq!(t.iter().filter(|r| r.message == one.id).count(), 2);
}

/// While the account's outbox waits for a challenge, a backend that publishes `Online` again
/// does not hide the challenge, and nothing is sent until the user retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_online_keeps_the_challenge_shown() {
    let s = script("challenge-online")
        .fail_transmit(TransmitRule::attempts(challenge(), 1).in_conv(d_alice()))
        .step(Step::Sleep(Duration::from_secs(10)))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (mut env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    env.event("SolveChallenge", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::SolveChallenge { .. }),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    env.clock.advance(Duration::from_secs(10));
    env.wait_finished(account).await;
    settle().await;
    let shown = env.account(account).await.expect("account").conn;
    assert!(
        matches!(
            shown,
            ConnState::NeedsUserAction(UserAction::SolveChallenge { .. })
        ),
        "{shown:?}"
    );
    assert_eq!(env.control.transmits(uuid).len(), 1, "still paused");

    env.handle
        .send(UiCommand::RetryMessage { message: one.id })
        .expect("send");
    let after = OutgoingState::Sent {
        server_at: Some(T0 + 10_000),
    };
    out_state(&env, ca, one.id, after).await;
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
}
