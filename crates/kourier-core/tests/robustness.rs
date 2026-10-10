//! S15 on the core side (architecture §4.3, §4.7, §4.8, §9.6, §11 S15), on the fake backend
//! with the fake clock:
//!
//! - reconnect hints (`NetworkChanged`, `Resumed`) reach a linked backend once per burst, not
//!   one command per signal, and never while a login runs;
//! - the outbox across a logout: what waited is kept (also across a restart), nothing of it is
//!   sent through a relink that turns out to be another account, and the same account sends it
//!   once, with its first id;
//! - a relink is checked wherever its `AccountReady` reaches the core: in the running loop, after
//!   the actor reported its exit, and while the core shuts down (where a link of the same account
//!   that finishes on the way out is kept);
//! - clocks: a clock that was set back does not stall the outbox (across a restart and while
//!   running) and ids stay increasing; an account whose backend found the clock wrong is not
//!   swept, also after a restart, until its backend is connected again; a clock that went
//!   wrong while Kourier was not running sweeps nothing before the backend had its say;
//! - an actor that is still linking gets no download, no safety-number confirmation and no
//!   challenge answer.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, BackendEvent, Clock, Envelope, Exit, Network,
    NetworkRegistry, OutgoingKind,
};
use kourier_core::AccountVault;
use kourier_core::facade::{Query, QueryData, UiCommand, UiEvent};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, LoginScript, Script, Step, TransmitRecord, TransmitRule};
use kourier_model::{
    AccountId, AccountState, AccountUuid, BackendError, Body, ConnState, ConvKey, ConversationId,
    DisappearKind, Expiry, MessageId, NetworkDescriptor, NetworkKind, OutgoingState, PersistError,
    Text, UserAction, UserId,
};
use support::send::*;
use support::{Env, WAIT, add_account, add_linked, eventually};

const DAY_MS: u64 = 24 * 3600 * 1000;

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
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

/// Waits until something sleeps on the fake clock until `at`: only then does advancing the
/// clock wake it.
async fn sleeping_until(env: &Env, at: u64) {
    let clock = env.clock.clone();
    eventually(&format!("a sleep until {at}"), WAIT, || {
        let clock = clock.clone();
        async move { clock.deadlines().contains(&at) }
    })
    .await;
}

/// Waits until the own message `id` is sent.
async fn sent(env: &Env, conv: ConversationId, id: MessageId) {
    let db = env.core_db();
    eventually("the message is sent", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == id && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
}

/// Waits until the message's send row waits for a retry after `attempts` attempts.
async fn waiting_for_retry(env: &Env, id: MessageId, attempts: i64) {
    until(env, "waiting for a retry", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1 AND state = 'Queued'
               AND attempts = ?2 AND next_attempt_at IS NOT NULL",
            (id.0, attempts),
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
}

async fn account_state(env: &Env, account: AccountId, what: &str, f: fn(&AccountState) -> bool) {
    let h = env.handle.clone();
    eventually(what, WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && f(&r.state)))
        }
    })
    .await;
}

fn logged_out(s: &AccountState) -> bool {
    matches!(s, AccountState::LoggedOut { .. })
}

fn active(s: &AccountState) -> bool {
    *s == AccountState::Active
}

/// How often the fake got the command `name`.
fn commands(env: &Env, uuid: AccountUuid, name: &str) -> usize {
    env.control
        .commands(uuid)
        .iter()
        .filter(|c| **c == name)
        .count()
}

/// Sends a command the backend records and waits for it: the supervisor has then handled
/// everything the UI sent before it.
async fn handled_so_far(env: &Env, account: AccountId, uuid: AccountUuid) {
    let before = commands(env, uuid, "ConfirmIdentity");
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: alice(),
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.commands(uuid)
                .iter()
                .filter(|n| **n == "ConfirmIdentity")
                .count()
                > before
        }),
    )
    .await
    .expect("the marker command");
}

// ---------- reconnect hints ----------

/// NetworkManager reports one network change as several signals, and a resume comes with its
/// own: the backend gets one command for the burst (`Resumed` when a resume was among the
/// hints, since it covers the change), after the hints settled, and a later change on its own
/// is passed on as one `NetworkChanged`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storm_of_hints_reaches_the_backend_as_one_command() {
    let (env, account, uuid) = linked(script("hints")).await;
    env.wait_finished(account).await;
    let settle_ms = u64::try_from(kourier_core::HINT_SETTLE.as_millis()).expect("ms");
    let hint = |cmd: UiCommand| env.handle.send(cmd).expect("send");
    let counts = || {
        (
            commands(&env, uuid, "NetworkChanged"),
            commands(&env, uuid, "Resumed"),
        )
    };

    let start = env.clock.now_ms();
    for _ in 0..5 {
        hint(UiCommand::NetworkChanged);
    }
    for _ in 0..3 {
        hint(UiCommand::Resumed);
    }
    for _ in 0..4 {
        hint(UiCommand::NetworkChanged);
    }
    handled_so_far(&env, account, uuid).await;
    sleeping_until(&env, start + settle_ms).await;
    assert_eq!(counts(), (0, 0), "collected, not passed on one by one");
    env.clock.advance(kourier_core::HINT_SETTLE);
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.commands(uuid).contains(&"Resumed")),
    )
    .await
    .expect("the hint is passed on");
    settle().await;
    assert_eq!(counts(), (0, 1), "twelve hints, one command");

    // A network change on its own, later.
    let later = env.clock.now_ms();
    for _ in 0..3 {
        hint(UiCommand::NetworkChanged);
    }
    handled_so_far(&env, account, uuid).await;
    sleeping_until(&env, later + settle_ms).await;
    env.clock.advance(kourier_core::HINT_SETTLE);
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.commands(uuid).contains(&"NetworkChanged")),
    )
    .await
    .expect("the hint is passed on");
    settle().await;
    assert_eq!(counts(), (1, 1));
}

/// A hint while a login runs starts nothing: the link connects by itself, and a `Resumed`
/// (which a backend reads as "connect") must not reach an actor that has no account yet. It is
/// not delivered late either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hints_do_not_reach_a_backend_that_is_still_linking() {
    let mut s = script("hint-while-linking");
    s.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 3,
        valid_for: Duration::from_secs(30),
        scanned_at: Some(1),
        linked_as: None,
    };
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    env.event("the first QR code", |e| match e {
        UiEvent::LoginStep { .. } => Some(()),
        _ => None,
    })
    .await;
    let start = env.clock.now_ms();
    sleeping_until(&env, start + 30_000).await;
    env.handle.send(UiCommand::Resumed).expect("send");
    env.handle.send(UiCommand::NetworkChanged).expect("send");
    let settle_ms = u64::try_from(kourier_core::HINT_SETTLE.as_millis()).expect("ms");
    sleeping_until(&env, start + settle_ms).await;
    env.clock.advance(kourier_core::HINT_SETTLE);
    settle().await;
    assert_eq!(commands(&env, uuid, "Resumed"), 0);
    // The second code is scanned; the account links.
    env.clock.advance(Duration::from_secs(30));
    account_state(&env, account, "linked", active).await;
    env.wait_finished(account).await;
    settle().await;
    assert_eq!(
        (
            commands(&env, uuid, "Resumed"),
            commands(&env, uuid, "NetworkChanged")
        ),
        (0, 0),
        "{:?}",
        env.control.commands(uuid)
    );
}

/// A clock as the machine has it around a suspend: the wall clock jumps ahead, but a timer
/// that was armed before does not fire for it (a tokio timer counts running time and ends late
/// by the whole suspend). A sleep armed after the jump sees the new time.
#[derive(Clone)]
struct SuspendClock {
    now: Arc<AtomicU64>,
    /// The deadlines of the sleeps that were armed: made while the clock was before them.
    armed: Arc<Mutex<Vec<u64>>>,
}

impl SuspendClock {
    fn at(now: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(now)),
            armed: Arc::default(),
        }
    }

    /// Waits until a sleep whose deadline lies in `deadlines` is armed. Only a timer that is
    /// armed sleeps through a jump of the clock; one the core makes a moment AFTER the jump
    /// (it announces a restart before it arms the timer for it) ends at once, and that is no
    /// suspend. So the clock jumps only after this, however busy the machine is.
    async fn armed_in(&self, what: &str, deadlines: std::ops::RangeInclusive<u64>) {
        eventually(what, WAIT, || {
            let (armed, deadlines) = (self.armed.clone(), deadlines.clone());
            async move {
                armed
                    .lock()
                    .expect("armed")
                    .iter()
                    .any(|at| deadlines.contains(at))
            }
        })
        .await;
    }
}

impl Clock for SuspendClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, deadline_ms: u64) -> kourier_backend_api::Sleep {
        // Decided when the timer is armed, not when it is first polled.
        let asleep = self.now.load(Ordering::SeqCst) < deadline_ms;
        if asleep {
            self.armed.lock().expect("armed").push(deadline_ms);
        }
        Box::pin(async move {
            if asleep {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// `Resumed` makes the core's own timers look at the clock again (§4.7): the machine slept
/// for an hour, and nothing that was armed before fired. After the hint, the restart of a
/// crashed backend, an expired message and a message waiting for its retry all happen at
/// once, not an hour late.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_wakes_the_timers_that_slept_through_the_suspend() {
    let local = (T0 + 1).to_string();
    let mut secret = remote(&d_alice(), &alice(), &local, T0 + 1, "one minute");
    secret.expiry = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(60),
        started_at: None,
    });
    let s = script("suspend")
        .deliver(vec![BackendEvent::Message(secret)])
        .step(Step::WaitAcked)
        .step(Step::Panic { times: 1 })
        .fail_transmit(TransmitRule::attempts(transient(), 2));
    let t0 = T0 as u64;
    let clock = SuspendClock::at(t0);
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = kourier_fake::FakeNetwork::new(Flavor::SignalLike, s);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let core = support::start_core_with(dir.path(), &fake, &vault, Arc::new(clock.clone())).await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        // Not the core's clock: only there because the helpers take an `Env`.
        clock: kourier_backend_api::FakeClock::new(t0),
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let present = |env: &Env| {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT count(*) FROM messages WHERE net_local = ?1",
                [&local],
                |r| r.get::<_, i64>(0),
            )
            .expect("count")
        }) == 1
    };

    // The backend crashed; its restart is a second away, on a timer.
    env.event("the restart is scheduled", |e| match e {
        UiEvent::ConnState {
            state: ConnState::Offline {
                retry_at_ms: Some(_),
            },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    assert!(present(&env));
    // Both timers are armed (the restart's a second or so ahead, the sweeper's a minute).
    clock
        .armed_in("the restart's timer", t0 + 1..=t0 + 5_000)
        .await;
    clock
        .armed_in("the sweeper's timer", t0 + 30_000..=t0 + 120_000)
        .await;
    // The machine sleeps for an hour. Nothing fires by itself afterwards.
    clock.now.store(t0 + 3_600_000, Ordering::SeqCst);
    settle().await;
    assert_eq!(env.control.spawns(uuid).len(), 1, "the restart timer slept");
    assert!(present(&env), "so did the sweeper's");

    env.handle.send(UiCommand::Resumed).expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.spawns(uuid).len() == 2))
        .await
        .expect("the backend is restarted");
    let db = env.core_db();
    let gone = local.clone();
    eventually("the expired message is swept", WAIT, || {
        let (db, gone) = (db.clone(), gone.clone());
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT count(*) FROM messages WHERE net_local = ?1",
                    [&gone],
                    |r| r.get::<_, i64>(0),
                )
                .is_ok_and(|n| n == 0)
            })
        }
    })
    .await;

    // A message waits 190 ms for its third attempt when the machine sleeps again.
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "before the nap");
    transmits(&env, uuid, 2).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    waiting_for_retry(&env, mine.id, 2).await;
    clock
        .armed_in(
            "the retry's timer",
            t0 + 3_600_000 + 1..=t0 + 3_600_000 + 60_000,
        )
        .await;
    clock.now.store(t0 + 2 * 3_600_000, Ordering::SeqCst);
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 2, "its timer slept");
    env.handle.send(UiCommand::Resumed).expect("send");
    transmits(&env, uuid, 3).await;
    sent(&env, a, mine.id).await;
    env.stop().await;
}

// ---------- the outbox across a logout ----------

/// A message waits for a retry when the device is unlinked. It stays in the outbox, across a
/// restart too. A relink that turns out to be another account (a backend that lets it through
/// because the phone number matches, and reports `Online` before the core has seen who it is)
/// sends nothing: the message was written as the first account. The first account, linked
/// again, sends it once, with the id it got when it was written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_waited_in_the_outbox_goes_out_only_as_the_account_that_wrote_it() {
    let s = script("logout")
        .fail_transmit(TransmitRule::attempts(transient(), 2))
        .step(Step::Sleep(Duration::from_millis(50)))
        .step(Step::Unlink {
            reason: "device unlinked".into(),
        });
    let (mut env, account, uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "do not lose me");
    // Two temporary failures; the third attempt waits 190 ms on the fake clock.
    transmits(&env, uuid, 2).await;
    let mine = own_messages(&env.core_db(), a)[0].clone();
    waiting_for_retry(&env, mine.id, 2).await;

    // The phone unlinks this device.
    sleeping_until(&env, T0 as u64 + 50).await;
    env.clock.advance(Duration::from_millis(50));
    account_state(&env, account, "logged out", logged_out).await;
    let vault = env.vault.clone();
    eventually("the credentials are wiped", WAIT, || {
        let exists = vault.backend_db(uuid).exists();
        async move { !exists }
    })
    .await;
    assert_eq!(outbox_len(&env.core_db()), 1, "the message still waits");

    // A restart while logged out loses nothing and sends nothing.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    account_state(&env, account, "still logged out", logged_out).await;
    assert_eq!(outbox_len(&env.core_db()), 1);
    assert_eq!(own_messages(&env.core_db(), a).len(), 1);
    // Its backoff is long over: it is due the moment anything may be sent.
    env.clock.advance(Duration::from_secs(1));

    // Another account with the same phone number is linked in its place.
    let other = uid("aci:00000000-0000-4000-8000-0000000000ff");
    env.control.push_script(
        Script::new("another-account", vec![other, uid(ME_PNI)])
            .deliver(setup())
            .online_while_linking(),
    );
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    let detail = env
        .event("the refusal", |e| match e {
            UiEvent::AccountError {
                account: acc,
                detail,
                ..
            } if *acc == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert!(detail.contains("different account"), "{detail}");
    let vault = env.vault.clone();
    eventually("the other account's credentials are wiped", WAIT, || {
        let exists = vault.backend_db(uuid).exists();
        async move { !exists }
    })
    .await;
    account_state(&env, account, "logged out again", logged_out).await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        2,
        "nothing was sent as the other account"
    );
    assert_eq!(outbox_len(&env.core_db()), 1, "and nothing was lost");
    assert_eq!(env.timeline(a).await.len(), 2, "alice's message and ours");
    // The account is still the first one: nothing of the other was taken in on its way out.
    let own: String = env.peek(|c| {
        c.query_row("SELECT own_ids FROM accounts", [], |r| r.get(0))
            .expect("own ids")
    });
    assert!(own.contains(ME) && !own.contains("0000000000ff"), "{own}");

    // The first account is linked again (its backend also reports Online early).
    env.control.push_script(
        Script::new("the-same-account", vec![uid(ME), uid(ME_PNI)])
            .deliver(setup())
            .online_while_linking(),
    );
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    account_state(&env, account, "active again", active).await;
    let all = transmits(&env, uuid, 3).await;
    sent(&env, a, mine.id).await;
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 3, "sent once");
    assert_eq!(
        (&*all[2].req.local, all[2].req.sent_at),
        (mine.local.as_str(), mine.sent_at),
        "with the id it was written with"
    );
    let OutgoingKind::Message { body, .. } = &all[2].req.kind else {
        panic!("not a message: {:?}", all[2].req);
    };
    assert_eq!(body, &Body::text("do not lose me"));
    assert_eq!(env.accounts().await.len(), 1, "the same account row");
    env.stop().await;
}

/// A relink that is refused as another account must leave nothing of that account behind.
/// Its backend sends `AccountReady` and its first state in one batch; the core refuses the
/// batch, and the backend's forwarder then offers the rows again one by one, `AccountReady`
/// first, while the core is already stopping the actor. Taking that in would turn the account
/// into the one just refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_relink_takes_nothing_in_on_its_way_out() {
    let s = script("unlinked").step(Step::Unlink {
        reason: "device unlinked".into(),
    });
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The script unlinks right after linking: do not wait to observe Active.
    let account = add_account(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    account_state(&env, account, "logged out", logged_out).await;
    let dump = |env: &Env| {
        env.peek(|c| {
            kourier_core::testing::dump(c, kourier_core::testing::DumpMode::Exact).expect("dump")
        })
    };
    let before = dump(&env);
    assert!(before.contains("LoggedOut"), "{before}");

    let other = uid("aci:00000000-0000-4000-8000-0000000000ff");
    let mallory = uid("aci:00000000-0000-4000-8000-00000000badd");
    let mut contact = kourier_model::ContactPatch::new(mallory.clone());
    contact.profile_name = kourier_model::Patch::Set("Mallory".into());
    let theirs = vec![
        BackendEvent::ContactUpsert(contact),
        BackendEvent::Message(remote(
            &ConvKey::Direct(mallory.clone()),
            &mallory,
            "31337",
            T0 + 31_337,
            "not for you",
        )),
    ];
    for round in 0..3 {
        env.control.push_script(
            Script::new("another-account", vec![other.clone(), uid(ME_PNI)])
                .ready_with(theirs.clone())
                .online_while_linking(),
        );
        env.handle
            .send(UiCommand::Relink {
                account,
                flow: "qr".into(),
            })
            .expect("send");
        let detail = env
            .event("the refusal", |e| match e {
                UiEvent::AccountError {
                    account: acc,
                    detail,
                    ..
                } if *acc == account => Some(detail.clone()),
                _ => None,
            })
            .await;
        assert!(detail.contains("different account"), "{detail}");
        let vault = env.vault.clone();
        eventually("the half-made credentials are wiped", WAIT, || {
            let exists = vault.backend_db(uuid).exists();
            async move { !exists }
        })
        .await;
        account_state(&env, account, "still logged out", logged_out).await;
        assert_eq!(dump(&env), before, "round {round}: core.db is as it was");
    }
    env.stop().await;
}

/// When the `AccountReady` of the next login of [`OddLogin`] reaches the core.
#[derive(Clone)]
enum Odd {
    /// The actor reports its exit first; its `AccountReady` for these ids reaches the core
    /// afterwards, while the supervisor takes in what the actor still sent.
    AfterExit(Vec<UserId>),
    /// The link finishes when the core is already shutting down: `AccountReady` for these ids
    /// arrives after `Command::Shutdown` went out.
    AtShutdown(Vec<UserId>),
    /// The link finishes when the user is already removing the account: `AccountReady` for
    /// these ids arrives after `Command::Logout` went out.
    AtLogout(Vec<UserId>),
}

/// The fake network, except that the next login is played by a thread of this test: orders of
/// exit report, `AccountReady` and shutdown that the fake's actor cannot produce (its event
/// sender closes before its exit is reported, and it ships nothing after `Shutdown`), but a
/// backend with a forwarder of its own can.
#[derive(Clone)]
struct OddLogin {
    inner: kourier_fake::FakeNetwork,
    next: Arc<Mutex<Option<Odd>>>,
    /// The odd actor got `StartLogin`.
    started: Arc<AtomicBool>,
    /// The core's answer to the odd actor's `AccountReady`.
    answer: Arc<Mutex<Option<Result<(), PersistError>>>>,
    /// Every command the odd actor got, by name.
    seen: Arc<Mutex<Vec<&'static str>>>,
}

impl OddLogin {
    fn new(inner: kourier_fake::FakeNetwork) -> Self {
        Self {
            inner,
            next: Arc::default(),
            started: Arc::default(),
            answer: Arc::default(),
            seen: Arc::default(),
        }
    }

    fn seen(&self) -> Vec<&'static str> {
        self.seen.lock().expect("lock").clone()
    }

    fn arm(&self, odd: Odd) {
        self.started.store(false, Ordering::SeqCst);
        *self.answer.lock().expect("lock") = None;
        self.seen.lock().expect("lock").clear();
        *self.next.lock().expect("lock") = Some(odd);
    }

    fn answer(&self) -> Option<Result<(), PersistError>> {
        self.answer.lock().expect("lock").clone()
    }
}

impl Network for OddLogin {
    fn kind(&self) -> NetworkKind {
        self.inner.kind()
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        self.inner.descriptor()
    }

    fn allocate_message_id(&self, own: &[UserId], conv: &ConvKey, now_ms: u64) -> AllocatedId {
        self.inner.allocate_message_id(own, conv, now_ms)
    }

    fn spawn(&self, mut ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let Some(odd) = self.next.lock().expect("lock").take() else {
            return self.inner.spawn(ctx);
        };
        let (started, answer) = (self.started.clone(), self.answer.clone());
        let seen = self.seen.clone();
        let (exit_tx, exited) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            // Until `want` arrives; `false` when the core hung up first.
            let mut wait_for = |want: &str| loop {
                match ctx.commands.blocking_recv() {
                    Some(c) => {
                        seen.lock().expect("lock").push(c.name());
                        if c.name() == want {
                            return true;
                        }
                    }
                    None => return false,
                }
            };
            if !wait_for("StartLogin") {
                return;
            }
            started.store(true, Ordering::SeqCst);
            let ready = |ids: Vec<UserId>| {
                Envelope::durable(vec![BackendEvent::AccountReady {
                    own_ids: ids,
                    device: Some(2),
                    display_name: None,
                    number: None,
                }])
            };
            match odd {
                Odd::AfterExit(ids) => {
                    let _ = exit_tx.send(Exit::LoggedOut {
                        reason: "gone while linking".into(),
                    });
                    // The supervisor sees the exit with nothing waiting in the event queue.
                    std::thread::sleep(Duration::from_millis(300));
                    let (env, waiter) = ready(ids);
                    if ctx.events.blocking_send(env).is_ok() {
                        *answer.lock().expect("lock") = Some(waiter.blocking_wait());
                    }
                }
                Odd::AtShutdown(ids) => {
                    if !wait_for("Shutdown") {
                        return;
                    }
                    let (env, waiter) = ready(ids);
                    if ctx.events.blocking_send(env).is_ok() {
                        *answer.lock().expect("lock") = Some(waiter.blocking_wait());
                    }
                    let _ = exit_tx.send(Exit::Shutdown);
                }
                Odd::AtLogout(ids) => {
                    if !wait_for("Logout") {
                        return;
                    }
                    let (env, waiter) = ready(ids);
                    if ctx.events.blocking_send(env).is_ok() {
                        *answer.lock().expect("lock") = Some(waiter.blocking_wait());
                    }
                    let _ = exit_tx.send(Exit::LoggedOut {
                        reason: "logged out".into(),
                    });
                }
            }
        });
        Ok(AccountHandle {
            exited,
            thread: Some(thread),
        })
    }
}

/// An account on [`OddLogin`] that linked as [`ME`] and was unlinked by its phone at once.
async fn unlinked_on_odd_login() -> (Env, OddLogin, AccountId) {
    let s = script("unlinked").step(Step::Unlink {
        reason: "device unlinked".into(),
    });
    let fake = kourier_fake::FakeNetwork::new(Flavor::SignalLike, s);
    let odd = OddLogin::new(fake.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = kourier_backend_api::FakeClock::new(T0 as u64);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let core = start_on(&dir, &odd, &vault, &clock).await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        clock,
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    account_state(&env, account, "logged out", logged_out).await;
    (env, odd, account)
}

async fn start_on(
    dir: &tempfile::TempDir,
    odd: &OddLogin,
    vault: &Arc<kourier_core::testing::DirVault>,
    clock: &kourier_backend_api::FakeClock,
) -> kourier_core::Core {
    let networks = NetworkRegistry::new()
        .with(Arc::new(odd.clone()))
        .expect("registry");
    support::start_core_on(dir.path(), networks, vault, Arc::new(clock.clone())).await
}

/// `(state, own_ids)` of the only account, as stored.
fn stored_account(env: &Env) -> (String, String) {
    env.peek(|c| {
        c.query_row("SELECT state, own_ids FROM accounts", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("the account")
    })
}

fn relink(env: &Env, account: AccountId) {
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
}

async fn odd_login_started(odd: &OddLogin) {
    let started = odd.started.clone();
    eventually("the login runs", WAIT, || {
        let started = started.clone();
        async move { started.load(Ordering::SeqCst) }
    })
    .await;
}

const OTHER: &str = "aci:00000000-0000-4000-8000-0000000000ff";

/// The identity of a relink is checked wherever its `AccountReady` reaches the core. Here the
/// backend reports its exit first and the event arrives afterwards (a forwarder that outlives
/// the actor's exit report): the supervisor is then taking in what the actor still sent, and
/// another account must be refused there exactly as in the running loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relink_is_checked_after_the_actor_reported_its_exit() {
    let (mut env, odd, account) = unlinked_on_odd_login().await;
    let before = env.peek(|c| {
        kourier_core::testing::dump(c, kourier_core::testing::DumpMode::Exact).expect("dump")
    });
    assert!(before.contains("LoggedOut"), "{before}");

    odd.arm(Odd::AfterExit(vec![uid(OTHER), uid(ME_PNI)]));
    relink(&env, account);
    let detail = env
        .event("the failed relink", |e| match e {
            UiEvent::AccountError {
                account: acc,
                detail,
                ..
            } if *acc == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert_eq!(detail, "gone while linking");
    let answer = odd.answer().expect("the core answered AccountReady");
    let refusal = answer.expect_err("another account is refused");
    assert!(!refusal.retryable, "for good: {refusal:?}");
    assert!(refusal.detail.contains("different account"), "{refusal:?}");
    account_state(&env, account, "still logged out", logged_out).await;
    let (_, own) = stored_account(&env);
    assert!(own.contains(ME) && !own.contains(OTHER), "{own}");
    let after = env.peek(|c| {
        kourier_core::testing::dump(c, kourier_core::testing::DumpMode::Exact).expect("dump")
    });
    assert_eq!(after, before, "core.db is as it was");
    env.stop().await;
}

/// The core shuts down while a relink runs, and the backend's `AccountReady` is already on its
/// way. Another account is refused on the way out too. The same account is taken in: the link
/// exists on the server and in `backend.db`, so the account must start linked next time
/// instead of asking the user to link once more (and leaving a dead device on the phone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_that_finishes_during_shutdown_is_kept_only_for_the_same_account() {
    let (mut env, odd, account) = unlinked_on_odd_login().await;

    odd.arm(Odd::AtShutdown(vec![uid(OTHER), uid(ME_PNI)]));
    relink(&env, account);
    odd_login_started(&odd).await;
    env.stop().await;
    let refusal = odd
        .answer()
        .expect("the core answered AccountReady")
        .expect_err("another account is refused");
    assert!(!refusal.retryable, "for good: {refusal:?}");
    assert!(refusal.detail.contains("different account"), "{refusal:?}");
    let (state, own) = stored_account(&env);
    assert_eq!(state, "LoggedOut");
    assert!(own.contains(ME) && !own.contains(OTHER), "{own}");

    // The same account, again finishing while the core stops.
    let core = start_on(&env.dir, &odd, &env.vault, &env.clock).await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    env.handle.send(UiCommand::UiReady).expect("send");
    account_state(&env, account, "logged out after the restart", logged_out).await;
    odd.arm(Odd::AtShutdown(vec![uid(ME), uid(ME_PNI)]));
    relink(&env, account);
    odd_login_started(&odd).await;
    env.stop().await;
    odd.answer()
        .expect("the core answered AccountReady")
        .expect("the same account is accepted");
    let (state, own) = stored_account(&env);
    assert_eq!(state, "Active", "linked at the next start");
    assert!(own.contains(ME) && !own.contains(OTHER), "{own}");
    assert_eq!(env.peek(|c| row_count(c, "accounts")), 1);
}

/// While a relink runs, its actor gets nothing that would go out in the account's name. The
/// user blocks a chat of the logged-out account and types in it meanwhile: neither the roaming
/// change nor the typing notice may reach an actor that may be about to link as someone else
/// (it would block the contact in that account). The change fails like any command for an
/// account without a backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_that_is_still_linking_gets_no_roaming_change() {
    let (mut env, odd, account) = unlinked_on_odd_login().await;
    let a = conv(&env, &d_alice()).await;
    odd.arm(Odd::AtShutdown(vec![uid(OTHER), uid(ME_PNI)]));
    relink(&env, account);
    odd_login_started(&odd).await;
    env.handle
        .send(UiCommand::ChangeConversation {
            conv: a,
            change: kourier_model::ConversationChange::SetBlocked { blocked: true },
        })
        .expect("send");
    env.handle
        .send(UiCommand::SetTyping { conv: a, on: true })
        .expect("send");
    let error = env
        .event("the refused change", |e| match e {
            UiEvent::ActionFailed {
                action: "ChangeConversation",
                error,
                ..
            } => Some(error.to_string()),
            _ => None,
        })
        .await;
    assert!(error.contains("not running"), "{error}");
    env.stop().await;
    assert_eq!(odd.seen(), ["StartLogin", "Shutdown"]);
}

/// While a relink runs, the user clicks a video, confirms a safety number and answers a
/// challenge. None of it reaches the actor that is linking: it may turn out to be another
/// account, which must not ask its server for this account's files or answer in its name, and
/// it has an empty identity store and no challenged send. The download is stored and goes out
/// once the link is done as the same account; the other two are not repeated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_that_is_still_linking_gets_no_download_confirmation_or_answer() {
    let video = kourier_model::Attachment {
        kind: kourier_model::AttachmentKind::Video,
        mime: "video/mp4".into(),
        size: 5000,
        remote: Some(kourier_fake::attachments::Pointer::Bytes { len: 5000, seed: 7 }.to_blob()),
        ..Default::default()
    };
    let at = T0 - 30_000;
    let mut clip = remote(&d_alice(), &alice(), &at.to_string(), at, "");
    clip.body = Body::Media {
        attachments: vec![video],
        caption: None,
    };
    let mut s = script("linking-commands")
        .deliver(vec![BackendEvent::Message(clip)])
        .step(Step::WaitAcked)
        .step(Step::Unlink {
            reason: "device unlinked".into(),
        });
    s.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 3,
        valid_for: Duration::from_secs(30),
        scanned_at: Some(1),
        linked_as: None,
    };
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let first_code = |env: &mut Env| {
        let pending = env.events.resubscribe();
        async move {
            let mut pending = pending;
            loop {
                if let Ok(UiEvent::LoginStep { .. }) = pending.recv().await {
                    return;
                }
            }
        }
    };
    // The first link: the second code is scanned; the phone unlinks the device at once.
    tokio::time::timeout(WAIT, first_code(&mut env))
        .await
        .expect("the first QR code");
    let start = env.clock.now_ms();
    sleeping_until(&env, start + 30_000).await;
    env.clock.advance(Duration::from_secs(30));
    account_state(&env, account, "logged out", logged_out).await;
    let attachment = kourier_model::AttachmentId(env.peek(|c| {
        c.query_row("SELECT id FROM attachments", [], |r| r.get(0))
            .expect("the video")
    }));

    // The relink shows its first code and waits for the scan.
    let code = first_code(&mut env);
    relink(&env, account);
    tokio::time::timeout(WAIT, code)
        .await
        .expect("the relink's first QR code");
    let start = env.clock.now_ms();
    sleeping_until(&env, start + 30_000).await;
    while env.events.try_recv().is_ok() {}
    env.handle
        .send(UiCommand::DownloadAttachment { attachment })
        .expect("send");
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user: alice(),
        })
        .expect("send");
    env.handle
        .send(UiCommand::SubmitChallenge {
            account,
            answer: kourier_model::Redacted::new("an answer".to_owned()),
        })
        .expect("send");
    let error = env
        .event("the refused answer", |e| match e {
            UiEvent::ActionFailed {
                action: "SubmitChallenge",
                error,
                ..
            } => Some(error.to_string()),
            _ => None,
        })
        .await;
    assert!(error.contains("not running"), "{error}");
    until(&env, "the download is stored", |c| {
        row_count(c, "attachment_downloads") == 1
    })
    .await;
    settle().await;
    for name in ["DownloadAttachment", "ConfirmIdentity", "SubmitChallenge"] {
        assert_eq!(
            commands(&env, uuid, name),
            0,
            "{name} reached an actor that is still linking"
        );
    }

    // The link finishes as the same account: the stored download goes out now.
    env.clock.advance(Duration::from_secs(30));
    account_state(&env, account, "linked again", active).await;
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.commands(uuid)
                .iter()
                .filter(|n| **n == "DownloadAttachment")
                .count()
                == 1
        }),
    )
    .await
    .expect("the download after the link");
    until(&env, "the video is here", |c| {
        row_count(c, "attachment_downloads") == 0
    })
    .await;
    assert_eq!(
        (
            commands(&env, uuid, "DownloadAttachment"),
            commands(&env, uuid, "ConfirmIdentity"),
            commands(&env, uuid, "SubmitChallenge")
        ),
        (1, 0, 0)
    );
    env.stop().await;
}

/// The user removes an account while its relink runs, and the link finishes just then, as
/// another account. Nothing of it is taken in: the account must not turn `Active` (as someone
/// else) in the middle of its removal. That shows when the removal cannot be finished (the
/// keyring refuses the shred): the row stays `Removing`, with the ids it had, and the next
/// start removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_being_removed_does_not_become_whoever_links_meanwhile() {
    let (mut env, odd, account) = unlinked_on_odd_login().await;
    let uuid = env.account(account).await.expect("account").uuid;

    odd.arm(Odd::AtLogout(vec![uid(OTHER), uid(ME_PNI)]));
    relink(&env, account);
    odd_login_started(&odd).await;
    // A directory where the relink's fresh secret is makes the shred fail.
    let secret = env.data().join("secrets").join(uuid.to_string());
    std::fs::remove_file(&secret).expect("remove secret");
    std::fs::create_dir(&secret).expect("block the secret path");
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let detail = env
        .event("the unfinished removal", |e| match e {
            UiEvent::AccountError {
                account: acc,
                detail,
                ..
            } if *acc == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert!(detail.contains("could not be removed"), "{detail}");
    let refusal = odd
        .answer()
        .expect("the core answered AccountReady")
        .expect_err("an account that is going takes nothing in");
    assert!(!refusal.retryable, "for good: {refusal:?}");
    let (state, own) = stored_account(&env);
    assert_eq!(state, "Removing");
    assert!(own.contains(ME) && !own.contains(OTHER), "{own}");

    // The keyring works again: the next start finishes the removal.
    env.stop().await;
    std::fs::remove_dir(&secret).expect("unblock");
    let core = start_on(&env.dir, &odd, &env.vault, &env.clock).await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    assert!(env.accounts().await.is_empty());
    env.stop().await;
}

/// `AccountReady` never brings an account back that is being removed (an actor the core had
/// accepted may state its ids again, for example after a number change, while it logs out).
#[test]
fn account_ready_leaves_a_removal_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = kourier_core::testing::IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        kourier_model::MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let ready = |ids: Vec<UserId>| BackendEvent::AccountReady {
        own_ids: ids,
        device: Some(2),
        display_name: None,
        number: None,
    };
    let state = |h: &kourier_core::testing::IngestHarness| -> (String, Option<String>) {
        h.connection()
            .query_row("SELECT state, state_reason FROM accounts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("the account")
    };
    h.connection()
        .execute(
            "UPDATE accounts SET state = 'LoggedOut', state_reason = 'unlinked'",
            [],
        )
        .expect("update");
    h.apply(vec![ready(vec![uid(ME), uid(ME_PNI)])])
        .expect("apply");
    assert_eq!(state(&h), ("Active".to_owned(), None), "a relink");
    h.connection()
        .execute("UPDATE accounts SET state = 'Removing'", [])
        .expect("update");
    h.apply(vec![ready(vec![uid(ME), uid(ME_PNI)])])
        .expect("apply");
    assert_eq!(state(&h).0, "Removing");
}

fn row_count(c: &kourier_db::rusqlite::Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

// ---------- clocks ----------

/// The clock was ahead and is set right (it jumps back). A message that waited for a retry
/// would wait for a time that is now far in the future. Across a restart the stored wait is
/// read as due (no wait is longer than the retry budget); while running, the dispatcher sees
/// the clock go back at its next look and clears the waits. Ids stay strictly increasing all
/// the same, so receivers order and dedup as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clock_set_back_does_not_stall_the_outbox() {
    let s = script("clock-back")
        .fail_transmit(TransmitRule::attempts(transient(), 2).in_conv(d_alice()))
        .fail_transmit(TransmitRule::attempts(transient(), 2).in_conv(d_bob()));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (a, b) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);

    // --- across a restart ---
    send(&env, a, "one");
    transmits(&env, uuid, 2).await;
    let one = own_messages(&env.core_db(), a)[0].clone();
    waiting_for_retry(&env, one.id, 2).await;
    env.stop().await;
    let t0 = T0 as u64;
    env.clock.set(t0 - 2 * DAY_MS);
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    transmits(&env, uuid, 3).await;
    sent(&env, a, one.id).await;

    // --- while running ---
    send(&env, b, "two");
    transmits(&env, uuid, 5).await;
    let two = own_messages(&env.core_db(), b)[0].clone();
    waiting_for_retry(&env, two.id, 2).await;
    // An hour back: less than the budget, so the stored wait alone looks plausible.
    env.clock.set(t0 - 2 * DAY_MS - 3_600_000);
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        5,
        "nothing woke the outbox"
    );
    // The next look at the outbox (any send) notices the clock.
    send(&env, a, "three");
    transmits(&env, uuid, 7).await;
    sent(&env, b, two.id).await;
    let three = own_messages(&env.core_db(), a)[1].clone();
    sent(&env, a, three.id).await;
    assert!(
        one.sent_at < two.sent_at && two.sent_at < three.sent_at,
        "ids keep increasing although the clock went back: {} {} {}",
        one.sent_at,
        two.sent_at,
        three.sent_at
    );
    assert_eq!(
        env.clock.now_ms(),
        t0 - 2 * DAY_MS - 3_600_000,
        "the clock was not advanced: nothing waited out its old time"
    );
    env.stop().await;
}

/// A backend found the local clock wrong (`FixClock`). By that clock a disappearing message
/// is long due, but a clock known to be wrong must not delete anything: the account is not
/// swept, not after a restart with the clock still wrong either (the backend has said nothing
/// yet then). Once the backend is connected again, the sweeper catches up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_are_not_swept_while_the_clock_is_known_to_be_wrong() {
    let local = (T0 + 1).to_string();
    let mut secret = remote(&d_alice(), &alice(), &local, T0 + 1, "one minute");
    secret.expiry = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(60),
        started_at: None,
    });
    let wrong = ConnState::NeedsUserAction(UserAction::FixClock {
        skew_ms: 24 * 3600 * 1000,
    });
    let s = script("wrong-clock")
        .deliver(vec![BackendEvent::Message(secret)])
        .step(Step::WaitAcked)
        .step(Step::Conn(wrong))
        .step(Step::Sleep(Duration::from_secs(2 * 24 * 3600)));
    let (mut env, account, _uuid) = linked(s).await;
    let present = |env: &Env| {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT count(*) FROM messages WHERE net_local = ?1",
                [&local],
                |r| r.get::<_, i64>(0),
            )
            .expect("count")
        }) == 1
    };
    env.event("the wrong clock is reported", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::FixClock { .. }),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    until(&env, "the hold is stored", |c| {
        c.query_row(
            "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    assert!(present(&env));

    // The clock reads a day ahead: the message's minute is long over by it.
    let t0 = T0 as u64;
    sleeping_until(&env, t0 + 2 * DAY_MS).await;
    env.clock.advance(Duration::from_secs(24 * 3600));
    settle().await;
    assert!(present(&env), "not deleted on a clock known to be wrong");

    // Restarted with the clock still wrong. The backend is not even connected yet (no UI):
    // the stored hold keeps the sweeper off.
    env.restart().await;
    settle().await;
    assert!(present(&env), "nor at the next start");

    // The UI is up, the backend connects and is accepted by its server: the clock is fine
    // again, and what is due goes.
    env.handle.send(UiCommand::UiReady).expect("send");
    let db = env.core_db();
    let gone = local.clone();
    eventually("swept once the clock is trusted again", WAIT, || {
        let (db, gone) = (db.clone(), gone.clone());
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT count(*) FROM messages WHERE net_local = ?1",
                    [&gone],
                    |r| r.get::<_, i64>(0),
                )
                .is_ok_and(|n| n == 0)
            })
        }
    })
    .await;
    until(&env, "the hold is lifted", |c| {
        c.query_row(
            "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    let _ = account;
    env.stop().await;
}

/// The clock goes wrong while Kourier is not running: at the next start it reads a year
/// ahead, and nothing is stored about it (no backend ever complained). By that clock every
/// disappearing message is long due. Nothing is swept before the account's backend had its
/// say: not at the start, and not once the backend, connecting, finds the clock wrong. When
/// the clock is right again and a server took the connection, the message is still there, and
/// it goes when its own minute is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clock_that_went_wrong_while_kourier_was_off_sweeps_nothing() {
    let local = (T0 + 1).to_string();
    let mut secret = remote(&d_alice(), &alice(), &local, T0 + 1, "one minute");
    secret.expiry = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(60),
        started_at: None,
    });
    let s = script("clock-ahead")
        .deliver(vec![BackendEvent::Message(secret)])
        .step(Step::WaitAcked);
    let fake = kourier_fake::FakeNetwork::new(Flavor::SignalLike, s);
    let judge = support::judge::ClockJudge::new(fake.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = kourier_backend_api::FakeClock::new(T0 as u64);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let core = judge
        .start(dir.path(), &vault, Arc::new(clock.clone()))
        .await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        clock,
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let present = |env: &Env| {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT count(*) FROM messages WHERE net_local = ?1",
                [&local],
                |r| r.get::<_, i64>(0),
            )
            .expect("count")
        }) == 1
    };
    assert!(present(&env));
    env.stop().await;

    // A year ahead at the next start; the backend will object when it connects.
    let t0 = T0 as u64;
    env.clock.advance(Duration::from_secs(365 * 24 * 3600));
    let wrong = ConnState::NeedsUserAction(UserAction::FixClock {
        skew_ms: 365 * 24 * 3600 * 1000,
    });
    let verdict = judge.judge_next(wrong);
    let core = judge
        .start(env.dir.path(), &env.vault, Arc::new(env.clock.clone()))
        .await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    settle().await;
    assert!(
        present(&env),
        "not swept at the start: no backend has said anything about the clock yet"
    );
    assert_eq!(
        env.peek(|c| c
            .query_row(
                "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("count")),
        0,
        "nothing was stored about this clock"
    );

    // The UI is up, the backend connects and finds the clock wrong.
    env.handle.send(UiCommand::UiReady).expect("send");
    env.event("the wrong clock is reported", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::FixClock { .. }),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    until(&env, "the hold is stored", |c| {
        c.query_row(
            "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    settle().await;
    assert!(present(&env), "nor after the backend objected");

    // The user sets the clock right (ten seconds after the message came) and the server
    // takes the connection: the message was never due.
    env.clock.set(t0 + 10_000);
    verdict
        .send(ConnState::Online { detail: None })
        .expect("the judge runs");
    until(&env, "the hold is lifted", |c| {
        c.query_row(
            "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    settle().await;
    assert!(
        present(&env),
        "a year was never over, only a wrong clock said so"
    );

    // Its minute ends: now it goes.
    sleeping_until(&env, t0 + 1 + 60_000).await;
    env.clock.advance(Duration::from_secs(60));
    let db = env.core_db();
    let gone = local.clone();
    eventually("swept when it is really due", WAIT, || {
        let (db, gone) = (db.clone(), gone.clone());
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT count(*) FROM messages WHERE net_local = ?1",
                    [&gone],
                    |r| r.get::<_, i64>(0),
                )
                .is_ok_and(|n| n == 0)
            })
        }
    })
    .await;
    env.stop().await;
}
