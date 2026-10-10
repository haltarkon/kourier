//! Phase I on the core's send path (architecture §4.8), on the fake backend with the fake
//! clock: waits for the user and the retry budget.
//!
//! - a row whose budget ran out where waiting does not count (offline) still gets its one
//!   late attempt when a challenge stands in front of it, after the challenge; the row that
//!   met the challenge on its own late attempt fails instead of waiting on;
//! - a challenge is kept across a restart: the account shows it again and nothing is sent
//!   into it, with its token for the answer and the time it ends by itself; one a job of the
//!   backend met is kept too, and a user's "Retry" does not take that one off show;
//! - the wait for a safety number to be confirmed ends with each row's budget, like the wait
//!   for a challenge answer;
//! - when the sweeper deletes the head of a conversation's queue, the rows behind it start
//!   at once.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice, Clock};
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_fake::scripts::T0;
use kourier_fake::{Flavor, Script, Step, TransmitRecord, TransmitRule, challenge_error_after};
use kourier_model::{
    AccountId, AccountUuid, ConnState, ConvKey, ConversationId, ConversationPatch, FailReason,
    MessageId, OutgoingState, Patch, Text, UserAction,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

const HOUR: u64 = 3600 * 1000;
const T0U: u64 = T0 as u64;
const URL: &str = "https://example.invalid/challenge";

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
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

fn send(env: &Env, conv: ConversationId, body: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(body),
            reply_to: None,
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

async fn shows_challenge(env: &Env, account: AccountId) -> bool {
    matches!(
        env.account(account).await.expect("account").conn,
        ConnState::NeedsUserAction(UserAction::SolveChallenge { .. })
    )
}

async fn challenge_on_show(env: &Env, account: AccountId) {
    eventually("the challenge is on show", WAIT, || async move {
        shows_challenge(env, account).await
    })
    .await;
}

async fn online(env: &Env, account: AccountId) {
    eventually("the account is online", WAIT, || async move {
        env.account(account)
            .await
            .is_some_and(|a| a.conn.is_online())
    })
    .await;
}

fn submit(env: &Env, account: AccountId, answer: &str) {
    env.handle
        .send(UiCommand::SubmitChallenge {
            account,
            answer: answer.to_owned().into(),
        })
        .expect("send");
}

/// The challenge `core.db` keeps for the account, as JSON.
fn stored_challenge(env: &Env) -> Option<String> {
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT value FROM settings WHERE key = 'challenge'",
            [],
            |r| r.get(0),
        )
        .ok()
    })
}

const GAVE_UP: OutgoingState = OutgoingState::Failed {
    reason: FailReason::Challenge,
    retryable: true,
};

// ---------- the late attempt ----------

/// Two messages are typed while the account is offline, and it stays offline for more than a
/// day: waiting offline does not count against the budget, so each still gets one attempt
/// when the account is back. The first one's attempt is answered with a challenge. That was
/// its late attempt: it fails (and can be retried) instead of waiting for an answer that may
/// take days. The second one was not tried yet, and the challenge only stands in front of
/// it: it keeps its attempt through the wait and gets it when the challenge is over. A
/// message typed during the wait has its own day.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_past_its_budget_gets_its_late_attempt_after_a_challenge() {
    let offline = Duration::from_millis(25 * HOUR);
    let wait = Duration::from_secs(600);
    let s = script("late-attempt")
        .fail_transmit(TransmitRule::attempts(challenge_error_after(URL, wait), 1))
        .step(Step::WaitAcked)
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .step(Step::Sleep(offline))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (mut env, account, uuid) = linked(s).await;
    let (a, b) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    sleeping_until(&env, T0U + 25 * HOUR).await;
    send(&env, a, "typed offline, first");
    let first = queued(&env, a, 1).await;
    send(&env, a, "typed offline, second");
    let second = queued(&env, a, 2).await;
    settle().await;
    assert!(env.control.transmits(uuid).is_empty(), "offline");

    // Back after 25 hours: both are past their budget. The first is tried, and challenged.
    env.clock.advance(offline);
    transmits(&env, uuid, 1).await;
    out_state(&env, a, first.id, GAVE_UP).await;
    challenge_on_show(&env, account).await;
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Failed"));
    settle().await;
    assert_eq!(
        send_row(&env, second.id).as_deref(),
        Some("Queued"),
        "the second was not tried yet: it keeps its late attempt"
    );
    assert_eq!(
        own_messages(&env.core_db(), a)[1].out_state,
        Some(OutgoingState::Queued)
    );
    assert_eq!(env.control.transmits(uuid).len(), 1);

    // A message typed while the account waits is held like any other.
    send(&env, b, "typed during the wait");
    let third = queued(&env, b, 1).await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "nothing is sent");

    // The server's time is up: what waited is tried, the late one included.
    sleeping_until(&env, T0U + 25 * HOUR + 600_000).await;
    env.clock.advance(wait);
    is_sent(&env, a, second.id).await;
    is_sent(&env, b, third.id).await;
    online(&env, account).await;
    let sent = env.control.transmits(uuid);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].req.local.as_ref(), second.local, "its first id");
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Failed"));
    assert_eq!(stored_challenge(&env), None, "the wait is over");

    // The first one is the user's to retry, with the id it always had.
    env.handle
        .send(UiCommand::RetryMessage { message: first.id })
        .expect("send");
    is_sent(&env, a, first.id).await;
    let sent = env.control.transmits(uuid);
    assert_eq!(sent.last().expect("a send").req.local.as_ref(), first.local);
    env.stop().await;
}

// ---------- a challenge across a restart ----------

/// The account waits on a challenge when Kourier is closed. At the next start it shows the
/// challenge again without first sending a message into it, holds what is typed, and the
/// answer goes to the network with the token of the send that was challenged, then
/// everything is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_survives_a_restart() {
    let s = script("challenge-restart")
        .fail_transmit(TransmitRule::challenge(URL))
        .accept_challenge_answer("solved");
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held");
    let held = queued(&env, a, 1).await;
    challenge_on_show(&env, account).await;
    assert_eq!(env.control.transmits(uuid).len(), 1);
    let stored = stored_challenge(&env).expect("the challenge is stored");
    assert!(stored.contains(URL), "{stored}");
    assert!(
        stored.contains(kourier_fake::CHALLENGE_TOKEN),
        "with the token the answer needs"
    );

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    challenge_on_show(&env, account).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "nothing was sent into the challenge after the restart"
    );
    assert_eq!(send_row(&env, held.id).as_deref(), Some("Queued"));
    send(&env, a, "typed after the restart");
    let later = queued(&env, a, 2).await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "held as well");

    // A wrong answer changes nothing; the right one lets both through.
    submit(&env, account, "wrong");
    env.event("the refused answer", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "SubmitChallenge" => Some(()),
        _ => None,
    })
    .await;
    assert!(shows_challenge(&env, account).await);
    submit(&env, account, "solved");
    is_sent(&env, a, held.id).await;
    is_sent(&env, a, later.id).await;
    online(&env, account).await;
    assert_eq!(env.control.transmits(uuid).len(), 3);
    assert_eq!(stored_challenge(&env), None, "forgotten once answered");

    // And it stays forgotten: the next start shows no challenge.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    online(&env, account).await;
    settle().await;
    assert!(!shows_challenge(&env, account).await);
    env.stop().await;
}

/// A challenge with a retry time, and a restart in the middle of the wait: the wait goes on
/// to the time the server named, counted from its answer, not from the restart, and then the
/// held message is tried once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_challenge_ends_at_its_time_after_a_restart() {
    let wait = Duration::from_secs(600);
    let s = script("timed-challenge-restart")
        .fail_transmit(TransmitRule::attempts(challenge_error_after(URL, wait), 1));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held for ten minutes");
    let held = queued(&env, a, 1).await;
    challenge_on_show(&env, account).await;

    env.clock.advance(Duration::from_secs(240));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    challenge_on_show(&env, account).await;
    env.clock.advance(Duration::from_secs(359));
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "not before the time");
    assert!(shows_challenge(&env, account).await);
    env.clock.advance(Duration::from_secs(1));
    is_sent(&env, a, held.id).await;
    online(&env, account).await;
    assert_eq!(env.control.transmits(uuid).len(), 2);
    assert_eq!(env.clock.now_ms(), T0U + 600_000);

    // A challenge older than a day is not taken up at all, also one without a time at
    // which it ends: the budget of every send it could hold is over by then, and the
    // server has most likely forgotten it (here it has).
    let s = script("stale-challenge").fail_transmit(TransmitRule::attempts(
        kourier_fake::challenge_error(URL),
        1,
    ));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held");
    let held = queued(&env, a, 1).await;
    challenge_on_show(&env, account).await;
    env.stop().await;
    env.clock.advance(Duration::from_millis(25 * HOUR));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The message is past its budget, but it waited where waiting does not count (Kourier
    // was closed): it gets its one late attempt, and the server takes it.
    is_sent(&env, a, held.id).await;
    assert_eq!(env.control.transmits(uuid).len(), 2);
    assert_eq!(stored_challenge(&env), None);
    env.stop().await;
}

/// A clock as the machine has it around a suspend: the wall clock jumps ahead, and a timer
/// that was armed before does not fire for it. A sleep armed after the jump sees the new
/// time.
#[derive(Clone)]
struct SuspendClock {
    now: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Clock for SuspendClock {
    fn now_ms(&self) -> u64 {
        self.now.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn sleep_until(&self, deadline_ms: u64) -> kourier_backend_api::Sleep {
        let now = self.now.clone();
        Box::pin(async move {
            if now.load(std::sync::atomic::Ordering::SeqCst) < deadline_ms {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// A challenge that ends by itself holds a send until then and no longer. The machine
/// sleeps through the end of the challenge and through the end of the send's budget: the
/// budget did not run out while the send waited for the user (it waited for the machine),
/// so the send is not given up at the resume. It gets its late attempt, and the server
/// takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_that_ended_during_a_suspend_does_not_use_up_the_budget() {
    let s = script("challenge-suspend").fail_transmit(TransmitRule::attempts(
        challenge_error_after(URL, Duration::from_secs(3600)),
        1,
    ));
    let clock = SuspendClock {
        now: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(T0U)),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = kourier_fake::FakeNetwork::new(Flavor::SignalLike, s);
    let vault = std::sync::Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let core = support::start_core_with(
        dir.path(),
        &fake,
        &vault,
        std::sync::Arc::new(clock.clone()),
    )
    .await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        // Not the core's clock: only there because the helpers take an `Env`.
        clock: kourier_backend_api::FakeClock::new(T0U),
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held for an hour");
    let held = queued(&env, a, 1).await;
    challenge_on_show(&env, account).await;
    assert_eq!(send_row(&env, held.id).as_deref(), Some("Queued"));

    // The machine sleeps for 25 hours. Nothing fires by itself afterwards.
    clock
        .now
        .store(T0U + 25 * HOUR, std::sync::atomic::Ordering::SeqCst);
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "the timers slept");
    env.handle.send(UiCommand::Resumed).expect("send");
    is_sent(&env, a, held.id).await;
    assert_eq!(env.control.transmits(uuid).len(), 2, "one late attempt");
    online(&env, account).await;
    assert_eq!(stored_challenge(&env), None);
    env.stop().await;
}

/// A challenge a job of the backend met (`BackendNotice::ChallengeRequired`) holds no send.
/// It is on show again after a restart, and the user's "Retry" of a failed message does not
/// take it off show: nothing the user did there answered it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_announced_challenge_stays_on_show_through_a_retry_and_a_restart() {
    let notice = vec![BackendEvent::Notice(BackendNotice::ChallengeRequired {
        url: URL.into(),
        retry_after: None,
    })];
    let s = script("announced-challenge")
        .fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_bob()),
        )
        .step(Step::WaitAcked)
        .deliver(notice);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    challenge_on_show(&env, account).await;
    let stored = stored_challenge(&env).expect("stored");
    assert!(
        !stored.contains(kourier_fake::CHALLENGE_TOKEN),
        "no send was challenged: {stored}"
    );

    // A message to bob is refused for good; the user retries it.
    let b = conv(&env, &d_bob()).await;
    send(&env, b, "to bob");
    let m = queued(&env, b, 1).await;
    out_state(
        &env,
        b,
        m.id,
        OutgoingState::Failed {
            reason: FailReason::Unregistered,
            retryable: false,
        },
    )
    .await;
    env.handle
        .send(UiCommand::RetryMessage { message: m.id })
        .expect("send");
    is_sent(&env, b, m.id).await;
    settle().await;
    assert!(
        shows_challenge(&env, account).await,
        "the retry did not answer the job's challenge"
    );
    assert_eq!(env.control.transmits(uuid).len(), 2, "and it held no send");

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    challenge_on_show(&env, account).await;
    submit(&env, account, "anything");
    online(&env, account).await;
    assert_eq!(stored_challenge(&env), None);
    env.stop().await;
}

// ---------- the wait for a safety number ----------

/// Alice's safety number changed: the chat's queue waits for the user to confirm it. That
/// wait is bounded like the one for a challenge answer: a message that waited for its whole
/// budget fails (and can be retried) instead of going out whenever the user gets to it, days
/// later. The message behind it is then tried, meets the same number, and waits its own day.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_for_a_safety_number_ends_with_the_retry_budget() {
    let s = script("identity-budget").fail_transmit(
        TransmitRule::attempts(untrusted(alice()), 2)
            .of_kind("Message")
            .in_conv(d_alice()),
    );
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "first");
    let first = queued(&env, a, 1).await;
    let waits = OutgoingState::Failed {
        reason: FailReason::UntrustedIdentity,
        retryable: true,
    };
    out_state(&env, a, first.id, waits.clone()).await;
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Queued"));
    sleeping_until(&env, T0U + 24 * HOUR).await;

    // Twelve hours in, a second message is typed. It waits behind the first.
    env.clock.advance(Duration::from_millis(12 * HOUR));
    send(&env, a, "second");
    let second = queued(&env, a, 2).await;
    env.clock.advance(Duration::from_millis(12 * HOUR - 1));
    settle().await;
    assert_eq!(
        send_row(&env, first.id).as_deref(),
        Some("Queued"),
        "not early"
    );
    assert_eq!(env.control.transmits(uuid).len(), 1);

    // The first one's day is over: it gives up. The second is the head now, is tried, and
    // waits for the same confirmation.
    env.clock.advance(Duration::from_millis(1));
    eventually("the first gave up", WAIT, || async {
        send_row(&env, first.id).as_deref() == Some("Failed")
    })
    .await;
    out_state(&env, a, first.id, waits.clone()).await;
    transmits(&env, uuid, 2).await;
    out_state(&env, a, second.id, waits.clone()).await;
    assert_eq!(send_row(&env, second.id).as_deref(), Some("Queued"));

    // Its own day ends twelve hours later.
    sleeping_until(&env, T0U + 36 * HOUR).await;
    env.clock.advance(Duration::from_millis(12 * HOUR));
    eventually("the second gave up", WAIT, || async {
        send_row(&env, second.id).as_deref() == Some("Failed")
    })
    .await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "nothing went out by itself"
    );

    // Days later the user confirms the number: nothing is sent by that. "Retry" sends a
    // message, with the id it always had.
    env.clock.advance(Duration::from_millis(48 * HOUR));
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: alice(),
        })
        .expect("send");
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 2);
    env.handle
        .send(UiCommand::RetryMessage { message: first.id })
        .expect("send");
    is_sent(&env, a, first.id).await;
    let sent = env.control.transmits(uuid);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2].req.local.as_ref(), first.local);
    while env.events.try_recv().is_ok() {}
    env.stop().await;
}

// ---------- the queue behind an expired head ----------

/// A group message with a five-second timer reached bob; alice's copy waits out a
/// ten-minute rate limit, so its row is the head of the group's queue, and a second message
/// waits behind it. After five seconds the message disappears, and its row with it. The
/// second message is sent then, not when the timer that was armed for the row that is gone
/// fires, almost ten minutes later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_behind_a_head_the_sweeper_deleted_start_at_once() {
    let wait = Duration::from_secs(600);
    let mut timer = ConversationPatch::new(group());
    timer.timer = Patch::Set(Duration::from_secs(5));
    let s = script("behind-an-expired-head")
        .deliver(vec![BackendEvent::ConversationUpsert(timer)])
        .fail_transmit(
            TransmitRule::member(alice(), rate_limited(Some(wait)), 1)
                .of_kind("Message")
                .in_conv(group()),
        );
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "gone in five seconds");
    let first = queued(&env, g, 1).await;
    transmits(&env, uuid, 1).await;
    // Bob has it (its timer runs); alice waits ten minutes.
    sleeping_until(&env, T0U + 600_000).await;
    sleeping_until(&env, T0U + 5_000).await;
    assert_eq!(send_row(&env, first.id).as_deref(), Some("Queued"));
    send(&env, g, "behind it");
    let second = queued(&env, g, 2).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "it waits behind the head"
    );

    env.clock.advance(Duration::from_secs(5));
    let sent = transmits(&env, uuid, 2).await;
    assert_eq!(sent[1].message, second.id);
    assert_eq!(sent[1].req.only, None, "a first send, to everyone");
    is_sent(&env, g, second.id).await;
    assert_eq!(
        env.clock.now_ms(),
        T0U + 5_000,
        "nothing waited out the rate limit"
    );
    assert!(
        own_messages(&env.core_db(), g)
            .iter()
            .all(|m| m.id != first.id),
        "the first message expired"
    );
    assert_eq!(send_row(&env, first.id), None);
}
