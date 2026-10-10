//! Account lifecycle and supervision through the real runtime and `FakeNetwork` (§4.3): linking
//! with rotating QR codes, failed and cancelled logins, unlink wipe, relink into the same account
//! (and refusal of another identity), removal, restarts with capped exponential backoff on the
//! fake clock, rebuilding a deleted `core.db`, durable read commands, roaming changes, the
//! catch-up summary, and the per-network scripts end to end.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, StartMode};
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::{Flavor, LoginScript, Script, Step, scripts};
use kourier_model::{
    AccountState, ConnState, ConversationChange, LoginStepKind, NetworkKind, Redacted, UserId,
};
use support::{Env, WAIT, add_linked, eventually};

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn small_script(name: &str) -> Script {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = kourier_model::ConvKey::Direct(alice.clone());
    Script::new(name, vec![me])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d,
            &alice,
            &(scripts::T0 + 1000).to_string(),
            scripts::T0 + 1000,
            "first",
        ))])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d,
            &alice,
            &(scripts::T0 + 2000).to_string(),
            scripts::T0 + 2000,
            "second",
        ))])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linking_with_rotating_qr_codes() {
    let mut script = small_script("qr");
    script.login = LoginScript::Qr {
        ask_device_name: true,
        codes: 6,
        valid_for: Duration::from_secs(45),
        scanned_at: Some(2),
        linked_as: None,
    };
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    let account = env
        .event("AccountAdded", |e| match e {
            UiEvent::AccountAdded { account } => Some(*account),
            _ => None,
        })
        .await;
    assert_eq!(
        env.account(account).await.map(|a| a.state),
        Some(AccountState::Linking)
    );
    let txn = env
        .event("device name input", |e| match e {
            UiEvent::LoginStep { step, .. } => match &step.kind {
                LoginStepKind::Input { fields } if fields[0].id == "device-name" => Some(step.txn),
                _ => None,
            },
            _ => None,
        })
        .await;
    env.handle
        .send(UiCommand::SubmitLoginInput {
            account,
            txn,
            values: vec![("device-name".into(), Redacted::new("Test PC".into()))],
        })
        .expect("send");
    let mut codes = Vec::new();
    for i in 0..3 {
        let data = env
            .event("QR code", |e| match e {
                UiEvent::LoginStep { step, .. } => match &step.kind {
                    LoginStepKind::ShowQr { data, valid_for } => {
                        assert_eq!(*valid_for, Some(Duration::from_secs(45)));
                        Some(data.expose().clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .await;
        codes.push(data);
        if i < 2 {
            // The code rotates when its validity ends on the account clock.
            let clock = env.clock.clone();
            eventually("QR rotation timer", WAIT, || {
                let c = clock.clone();
                async move { c.sleepers() > 0 }
            })
            .await;
            let before = env.clock.next_deadline().expect("deadline");
            env.clock.advance_to_next();
            assert_eq!(before, env.clock.now_ms());
        }
    }
    codes.dedup();
    assert_eq!(codes.len(), 3, "every code is new");
    env.event("Done", |e| match e {
        UiEvent::LoginStep { step, .. } if step.kind == LoginStepKind::Done => Some(()),
        _ => None,
    })
    .await;
    let h = env.handle.clone();
    eventually("active", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a)) if a.iter().any(|r| r.state == AccountState::Active))
        }
    })
    .await;
    env.wait_finished(account).await;
    let convs = env.conversations().await;
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].unread, 2);
    assert!(
        env.control.violations().is_empty(),
        "{:?}",
        env.control.violations()
    );
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_cancelled_logins_remove_the_account() {
    let mut failing = small_script("fail");
    failing.login = LoginScript::Fail {
        detail: "phone refused".into(),
    };
    let mut env = Env::new(Flavor::SignalLike, failing).await;
    env.handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    let account = env
        .event("AccountAdded", |e| match e {
            UiEvent::AccountAdded { account } => Some(*account),
            _ => None,
        })
        .await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account: a } if *a == account => Some(()),
        _ => None,
    })
    .await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.account_dir(uuid).exists(), "directory deleted");
    assert!(!env.secret_exists(uuid), "secret deleted");

    // Cancel while the QR code is shown.
    let mut waiting = small_script("cancel");
    waiting.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 3,
        valid_for: Duration::from_secs(45),
        scanned_at: None,
        linked_as: None,
    };
    env.control.push_script(waiting);
    env.handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    let account = env
        .event("AccountAdded", |e| match e {
            UiEvent::AccountAdded { account } => Some(*account),
            _ => None,
        })
        .await;
    env.event("QR", |e| match e {
        UiEvent::LoginStep { step, .. } if matches!(step.kind, LoginStepKind::ShowQr { .. }) => {
            Some(())
        }
        _ => None,
    })
    .await;
    env.handle
        .send(UiCommand::CancelLogin { account })
        .expect("send");
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account: a } if *a == account => Some(()),
        _ => None,
    })
    .await;
    assert!(env.accounts().await.is_empty());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_wipes_credentials_and_relink_keeps_the_account() {
    let script = small_script("unlink").step(Step::Unlink {
        reason: "device unlinked".into(),
    });
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The script unlinks right after linking: do not wait to observe Active.
    let account = support::add_account(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let h = env.handle.clone();
    eventually("logged out", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.iter().any(|r| matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    // Credentials, identity keys and sessions are gone; history stays readable.
    assert!(!env.secret_exists(uuid));
    assert!(!env.backend_db_exists(uuid));
    assert!(!env.account_dir(uuid).join("key.wrap").exists());
    let convs = env.conversations().await;
    assert_eq!(convs.len(), 1);
    let conv = convs[0].id;
    assert_eq!(env.timeline(conv).await.len(), 2);

    // Relinking the same identity continues the same account and conversations.
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = kourier_model::ConvKey::Direct(alice.clone());
    env.control.push_script(
        Script::new("relink", vec![me]).deliver(vec![BackendEvent::Message(scripts::remote(
            &d,
            &alice,
            &(scripts::T0 + 3000).to_string(),
            scripts::T0 + 3000,
            "third",
        ))]),
    );
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("active again", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && r.state == AccountState::Active))
        }
    })
    .await;
    env.wait_finished(account).await;
    assert_eq!(env.accounts().await.len(), 1, "the same AccountId");
    assert_eq!(env.timeline(conv).await.len(), 3);
    assert_eq!(env.control.spawns(uuid).first(), Some(&StartMode::Fresh));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relinking_another_identity_is_refused() {
    let script = small_script("unlink").step(Step::Unlink {
        reason: "device unlinked".into(),
    });
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The script unlinks right after linking: do not wait to observe Active.
    let account = support::add_account(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let h = env.handle.clone();
    eventually("logged out", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.iter().any(|r| matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    let other = uid("aci:00000000-0000-4000-8000-0000000000ff");
    env.control.push_script(Script::new("other", vec![other]));
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    let detail = env
        .event("refusal", |e| match e {
            UiEvent::AccountError {
                account: a, detail, ..
            } if *a == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert!(detail.contains("different account"), "{detail}");
    let row = env.account(account).await.expect("row");
    assert!(matches!(row.state, AccountState::LoggedOut { .. }));
    assert!(
        !env.secret_exists(uuid),
        "the half-made credentials are wiped"
    );
    assert!(!env.backend_db_exists(uuid));
    assert_eq!(env.timeline(env.conversations().await[0].id).await.len(), 2);
    env.stop().await;
}

/// The core's own check (behind the backend's): a relink that shares only a secondary id (the
/// phone number's PNI moved to another account) is refused, even when a lenient backend lets it
/// through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relinking_an_account_that_shares_only_a_secondary_id_is_refused() {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let number = uid("pni:00000000-0000-4000-8000-0000000000bb");
    let mut script = small_script("two-ids").step(Step::Unlink {
        reason: "device unlinked".into(),
    });
    script.own_ids = vec![me, number.clone()];
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = support::add_account(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let h = env.handle.clone();
    eventually("logged out", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.iter().any(|r| matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    // The fake accepts any shared id; the core must not.
    let other = uid("aci:00000000-0000-4000-8000-0000000000ff");
    env.control
        .push_script(Script::new("same-number", vec![other, number]));
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    let detail = env
        .event("refusal", |e| match e {
            UiEvent::AccountError {
                account: a, detail, ..
            } if *a == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert!(detail.contains("different account"), "{detail}");
    let h = env.handle.clone();
    eventually("credentials wiped", WAIT, || {
        let h = h.clone();
        let (secret, db) = (env.secret_exists(uuid), env.backend_db_exists(uuid));
        async move {
            !secret
                && !db
                && matches!(h.query(kourier_core::facade::Query::Accounts).await,
                    Ok(kourier_core::facade::QueryData::Accounts(a))
                    if a.iter().any(|r| r.id == account && matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    // The old history is untouched and the other account's messages never arrived.
    assert_eq!(env.timeline(env.conversations().await[0].id).await.len(), 2);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_shreds_and_deletes_everything() {
    let mut env = Env::new(Flavor::SignalLike, small_script("remove")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    assert!(env.secret_exists(uuid));
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account: a } if *a == account => Some(()),
        _ => None,
    })
    .await;
    assert!(env.control.commands(uuid).contains(&"Logout"));
    assert!(!env.secret_exists(uuid), "the account secret is shredded");
    assert!(!env.account_dir(uuid).exists());
    assert!(env.accounts().await.is_empty());
    assert!(env.conversations().await.is_empty());
    let rows: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(rows, 0, "messages cascade with the account row");
    env.stop().await;
}

/// A shred that fails (keyring trouble) does not drop the account with its secret alive: the row
/// stays `Removing`, hidden, and the next start finishes the removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_shred_keeps_the_removal_pending_until_the_next_start() {
    let mut env = Env::new(Flavor::SignalLike, small_script("shred-fails")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    // A directory where the secret file was makes the shred fail.
    let secret = env.data().join("secrets").join(uuid.to_string());
    std::fs::remove_file(&secret).expect("remove secret");
    std::fs::create_dir(&secret).expect("block the secret path");
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let detail = env
        .event("AccountError", |e| match e {
            UiEvent::AccountError {
                account: a,
                detail,
                restartable: false,
            } if *a == account => Some(detail.clone()),
            _ => None,
        })
        .await;
    assert!(detail.contains("could not be removed"), "{detail}");
    let row = env.account(account).await.expect("the row stays");
    assert_eq!(row.state, AccountState::Removing);
    assert!(env.account_dir(uuid).join("key.wrap").exists());
    assert!(env.conversations().await.is_empty(), "hidden from the list");
    // The keyring works again: the next start finishes the removal.
    env.stop().await;
    std::fs::remove_dir(&secret).expect("unblock");
    env.restart().await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_backend_restarts_with_capped_backoff_on_the_clock() {
    let script = small_script("panics")
        .step(Step::WaitAcked)
        .step(Step::Panic { times: 3 })
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &kourier_model::ConvKey::Direct(uid("aci:00000000-0000-4000-8000-0000000a11ce")),
            &uid("aci:00000000-0000-4000-8000-0000000a11ce"),
            &(scripts::T0 + 9000).to_string(),
            scripts::T0 + 9000,
            "after the crashes",
        ))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let policy = kourier_core::RestartPolicy::default();
    for attempt in 1..=3u32 {
        let clock = env.clock.clone();
        eventually("backoff timer", WAIT, || {
            let c = clock.clone();
            async move { c.sleepers() > 0 }
        })
        .await;
        let wait = env.clock.next_deadline().expect("deadline") - env.clock.now_ms();
        let base = 1000u64 << (attempt - 1);
        assert!(
            (base..=base + base / 4).contains(&wait),
            "attempt {attempt}: {wait} ms"
        );
        assert_eq!(
            Duration::from_millis(wait),
            policy.delay(attempt, kourier_model::stable_hash(uuid.0.as_bytes()))
        );
        // Nothing restarts before the clock says so.
        assert_eq!(env.control.spawns(uuid).len(), attempt as usize);
        env.clock.advance_to_next();
        let control = env.control.clone();
        tokio::time::timeout(
            WAIT,
            control.wait_for(|c| c.spawns(uuid).len() == attempt as usize + 1),
        )
        .await
        .expect("restarted");
    }
    env.wait_finished(account).await;
    let spawns = env.control.spawns(uuid);
    assert_eq!(
        spawns,
        [
            StartMode::Fresh,
            StartMode::Resume,
            StartMode::Resume,
            StartMode::Resume
        ]
    );
    let conv = env.conversations().await[0].id;
    let rows = env.timeline(conv).await;
    assert_eq!(
        rows.len(),
        3,
        "every message exactly once across the crashes"
    );
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_that_keeps_crashing_is_halted_until_restart() {
    let script = small_script("crashloop")
        .step(Step::WaitAcked)
        .step(Step::Panic { times: 6 });
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    // Failures 1-4 back off; the 5th within 10 minutes halts the account.
    for n in 1..=4usize {
        let clock = env.clock.clone();
        eventually("backoff timer", WAIT, || {
            let c = clock.clone();
            async move { c.sleepers() > 0 }
        })
        .await;
        env.clock.advance_to_next();
        let control = env.control.clone();
        tokio::time::timeout(WAIT, control.wait_for(|c| c.spawns(uuid).len() == n + 1))
            .await
            .expect("restarted");
    }
    let restartable = env
        .event("AccountError", |e| match e {
            UiEvent::AccountError {
                account: a,
                restartable,
                ..
            } if *a == account => Some(*restartable),
            _ => None,
        })
        .await;
    assert!(restartable);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(env.clock.sleepers(), 0, "no restart is scheduled");
    assert_eq!(env.control.spawns(uuid).len(), 5);
    // The user restarts: the 6th scripted panic, then the script passes.
    env.handle
        .send(UiCommand::RestartAccount { account })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.spawns(uuid).len() == 6))
        .await
        .expect("restarted by the user");
    let clock = env.clock.clone();
    eventually("backoff after restart", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    let wait = env.clock.next_deadline().expect("deadline") - env.clock.now_ms();
    assert!(
        (1000..=1250).contains(&wait),
        "the failure count starts over: {wait}"
    );
    env.clock.advance_to_next();
    env.wait_finished(account).await;
    env.stop().await;
}

/// After an actor stayed up for a whole failure window, the next crash starts the backoff over
/// (a backend that fails once a day never reaches the 5-minute cap).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backoff_starts_over_after_a_quiet_window() {
    let quiet = Duration::from_secs(11 * 60);
    let script = small_script("quiet")
        .step(Step::WaitAcked)
        .step(Step::Panic { times: 2 })
        .step(Step::Sleep(quiet))
        .step(Step::Panic { times: 1 });
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let next_wait = |env: &Env| env.clock.next_deadline().expect("deadline") - env.clock.now_ms();
    // Two crashes in a row back off 1 s, then 2 s.
    for (n, base) in [(1usize, 1000u64), (2, 2000)] {
        let clock = env.clock.clone();
        eventually("backoff timer", WAIT, || {
            let c = clock.clone();
            async move { c.sleepers() > 0 }
        })
        .await;
        let wait = next_wait(&env);
        assert!(
            (base..=base + base / 4).contains(&wait),
            "crash {n}: {wait} ms"
        );
        env.clock.advance_to_next();
        let control = env.control.clone();
        tokio::time::timeout(WAIT, control.wait_for(|c| c.spawns(uuid).len() == n + 1))
            .await
            .expect("restarted");
    }
    // The third actor runs for 11 minutes, then crashes once more.
    let clock = env.clock.clone();
    eventually("the actor's sleep", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    assert_eq!(Duration::from_millis(next_wait(&env)), quiet);
    env.clock.advance_to_next();
    let clock = env.clock.clone();
    eventually("backoff timer", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    let wait = next_wait(&env);
    assert!(
        (1000..=1250).contains(&wait),
        "a new failure streak starts at the first delay: {wait} ms"
    );
    env.clock.advance_to_next();
    env.wait_finished(account).await;
    assert_eq!(env.control.spawns(uuid).len(), 4);
    env.stop().await;
}

/// An unlink wipe that was cut off before `LoggedOut` was recorded leaves an `Active` row without
/// keys: the account is logged out (relink offered), not restarted until it halts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_account_without_keys_is_logged_out_not_restarted() {
    let mut env = Env::new(Flavor::SignalLike, small_script("keys-gone")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    std::fs::remove_file(env.account_dir(uuid).join("key.wrap")).expect("remove key.wrap");
    env.restart().await;
    let h = env.handle.clone();
    eventually("logged out", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    assert_eq!(env.clock.sleepers(), 0, "no restart is scheduled");
    assert!(!env.secret_exists(uuid));
    assert!(!env.backend_db_exists(uuid));
    assert_eq!(env.timeline(env.conversations().await[0].id).await.len(), 2);
    env.stop().await;
}

/// A `key.wrap` that this profile's key does not open is not a shredded account: its secret
/// is there. With `core.db` gone as well (a profile restored without its keys, a new master
/// key made for a profile that had data), the start used to take the directory for the rest
/// of a removed account and delete it, `backend.db` and all. It is left as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_directory_the_key_does_not_open_is_left_untouched() {
    let mut env = Env::new(Flavor::SignalLike, small_script("wrong-key")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    let secret = env.data().join("secrets").join(uuid.to_string());
    let backend_db = std::fs::read(env.account_dir(uuid).join("backend.db")).expect("backend.db");
    // The secret is there and does not open the wrap.
    std::fs::write(&secret, "not the key").expect("damage the secret");

    // The account is known to core.db: not logged out, not wiped, not restarted for ever.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The account's supervisor starts with the core, before this test can subscribe to the
    // core's events: the start's own `AccountError` may be out already (it was, about one
    // run in ten). So the test does what the user does with that error, "Restart": the halted
    // account starts again, fails on the same key, and says so again. Whichever of the two
    // errors is seen here, it comes after the subscription; a restart requested while the
    // start is still running is taken once the account is halted.
    env.handle
        .send(UiCommand::RestartAccount { account })
        .expect("send");
    let restartable = env
        .event("AccountError", |e| match e {
            UiEvent::AccountError {
                account: a,
                restartable,
                detail,
            } if *a == account => {
                assert!(detail.contains("nothing was deleted"), "{detail}");
                Some(*restartable)
            }
            _ => None,
        })
        .await;
    assert!(restartable);
    assert_eq!(
        env.account(account).await.expect("row").state,
        AccountState::Active,
        "a key that does not open is not a logout"
    );
    assert_eq!(env.clock.sleepers(), 0, "no restart is scheduled");
    assert_eq!(
        std::fs::read(env.account_dir(uuid).join("backend.db")).expect("backend.db stays"),
        backend_db
    );
    env.stop().await;

    // With core.db gone too, the directory is unknown to the core: still not deleted.
    for suffix in ["", "-wal", "-shm"] {
        let mut p = env.core_db().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    assert!(env.accounts().await.is_empty(), "nothing to rebuild from");
    assert!(env.account_dir(uuid).join("key.wrap").exists());
    assert_eq!(
        std::fs::read(env.account_dir(uuid).join("backend.db")).expect("backend.db stays"),
        backend_db
    );
    assert!(env.secret_exists(uuid));
    env.stop().await;

    // A secret that is truly gone is what a shred leaves: that directory still goes.
    std::fs::remove_file(&secret).expect("remove the secret");
    env.restart().await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.account_dir(uuid).exists(), "a shredded account's rest");
    env.stop().await;
}

/// A backend that says it cannot run the account as it was started (Signal: the account
/// belongs to another server than this start names) would say so at every restart. The
/// account is halted at once with the backend's words, and nothing of it is wiped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_that_refuses_its_account_as_started_is_not_restarted() {
    let script = small_script("refused")
        .step(Step::WaitAcked)
        .step(Step::Fail(kourier_model::BackendError::unsupported(
            "this account is linked to elsewhere",
        )));
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let (detail, restartable) = env
        .event("AccountError", |e| match e {
            UiEvent::AccountError {
                account: a,
                detail,
                restartable,
            } if *a == account => Some((detail.clone(), *restartable)),
            _ => None,
        })
        .await;
    assert_eq!(detail, "this account is linked to elsewhere");
    assert!(!restartable);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(env.clock.sleepers(), 0, "no restart is scheduled");
    assert_eq!(env.control.spawns(uuid).len(), 1);
    assert_eq!(
        env.account(account).await.expect("row").state,
        AccountState::Active
    );
    assert!(env.backend_db_exists(uuid) && env.secret_exists(uuid));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_core_db_is_rebuilt_with_the_account_intact() {
    let script = scripts::signal_like().expect("script");
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let before = env.account(account).await.expect("row");
    let groups_before = env
        .conversations()
        .await
        .into_iter()
        .filter(|c| c.kind == kourier_model::ConvKind::Group)
        .count();
    env.stop().await;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = env.core_db().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let h = env.handle.clone();
    eventually("rebuilt account active with its identity", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(kourier_core::facade::Query::Accounts).await,
                Ok(kourier_core::facade::QueryData::Accounts(a))
                if a.len() == 1 && a[0].state == AccountState::Active && a[0].own_device == Some(2))
        }
    })
    .await;
    let after = env.accounts().await.remove(0);
    assert_eq!(
        after.uuid, before.uuid,
        "the same account, from its key.wrap"
    );
    assert_eq!(after.display_name.as_deref(), Some("Test Account"));
    assert_eq!(
        env.control.spawns(after.uuid).last(),
        Some(&StartMode::Rebuild)
    );
    let env_ref = &env;
    eventually("mirrored conversations", WAIT, || async move {
        env_ref
            .conversations()
            .await
            .iter()
            .filter(|c| c.kind == kourier_model::ConvKind::Group)
            .count()
            == groups_before
    })
    .await;
    let convs = env.conversations().await;
    let group = convs
        .iter()
        .find(|c| c.kind == kourier_model::ConvKind::Group)
        .expect("group");
    assert_eq!(group.title.as_deref(), Some("Weekend Plans"));
    // The PNI→ACI merge is mirrored state too: Carol has one chat and both names.
    let carol = convs
        .iter()
        .find(|c| {
            c.peer
                .as_ref()
                .is_some_and(|p| p.as_str().ends_with("ca202"))
        })
        .expect("carol");
    // The address-book name wins over the profile name.
    assert_eq!(carol.title.as_deref(), Some("Carol Phone"));
    assert!(
        !convs
            .iter()
            .any(|c| c.peer.as_ref().is_some_and(|p| p.scheme() == "pni"))
    );
    // Messages lived only in core.db (Signal has no history to re-deliver).
    let messages: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(messages, 0);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_read_queues_a_durable_command_until_the_backend_persists_it() {
    let mut env = Env::new(Flavor::SignalLike, small_script("read")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let conv = env.conversations().await[0].id;
    assert_eq!(env.conversations().await[0].unread, 2);
    env.handle.send(UiCommand::MarkRead { conv }).expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            let cmds = c.commands(uuid);
            cmds.contains(&"SyncReadMarker") && cmds.contains(&"SendReceipts")
        }),
    )
    .await
    .expect("commands sent");
    let env_ref = &env;
    eventually("CommandPersisted ingested", WAIT, || async move {
        env_ref.peek(|c| {
            c.query_row("SELECT count(*) FROM pending_commands", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("count")
        }) == 0
    })
    .await;
    assert_eq!(env.conversations().await[0].unread, 0);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roaming_changes_are_confirmed_by_events_or_rejected() {
    // Signal-like: accepted, confirmed through a ConversationUpsert from the backend.
    let mut env = Env::new(Flavor::SignalLike, small_script("archive")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let conv = env.conversations().await[0].id;
    env.handle
        .send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetArchived { archived: true },
        })
        .expect("send");
    let env_ref = &env;
    eventually("archived", WAIT, || async move {
        env_ref
            .conversations()
            .await
            .iter()
            .any(|c| c.id == conv && c.archived)
    })
    .await;
    env.stop().await;

    // Threema-like: the transaction is rejected; nothing changes.
    let mut env = Env::new(
        Flavor::ThreemaLike,
        scripts::threema_like().expect("script"),
    )
    .await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let conv = env.conversations().await[0].id;
    env.handle
        .send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetArchived { archived: true },
        })
        .expect("send");
    let action = env
        .event("ActionFailed", |e| match e {
            UiEvent::ActionFailed { action, error, .. } => Some((*action, error.to_string())),
            _ => None,
        })
        .await;
    assert_eq!(action.0, "ChangeConversation");
    assert!(action.1.contains("transaction rejected"), "{}", action.1);
    assert!(env.conversations().await.iter().all(|c| !c.archived));
    env.stop().await;
}

/// A chat another device marked unread is read when the user reads it: the mark goes with the
/// unread messages, and again when only the mark was there; the network is told each time
/// (Signal Desktop clears the mark when a chat is read).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_a_chat_clears_the_mark_of_another_device() {
    let mut env = Env::new(Flavor::SignalLike, small_script("marked")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let conv = env.conversations().await[0].id;
    let mark = |env: &Env| {
        env.handle
            .send(UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetMarkedUnread {
                    marked_unread: true,
                },
            })
            .expect("send");
    };
    let env_ref = &env;
    let marked = |want: bool, what: &'static str| async move {
        eventually(what, WAIT, || async move {
            env_ref
                .conversations()
                .await
                .iter()
                .any(|c| c.id == conv && c.marked_unread == want)
        })
        .await;
    };

    // With unread messages.
    mark(&env);
    marked(true, "marked unread").await;
    assert!(env.conversations().await[0].unread > 0);
    env.handle.send(UiCommand::MarkRead { conv }).expect("send");
    marked(false, "the mark goes with the unread messages").await;
    assert_eq!(env.conversations().await[0].unread, 0);

    // Only the mark.
    mark(&env);
    marked(true, "marked unread again").await;
    env.handle.send(UiCommand::MarkRead { conv }).expect("send");
    marked(false, "the mark alone goes too").await;

    // The network heard the two marks and the two times the mark was cleared.
    let uuid = env.account(account).await.expect("account").uuid;
    let env_ref = &env;
    eventually("the network is told", WAIT, || async move {
        env_ref
            .control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "ChangeConversation")
            .count()
            == 4
    })
    .await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catch_up_messages_are_summarized_once_when_syncing_ends() {
    let mut env = Env::new(Flavor::SignalLike, scripts::signal_like().expect("script")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let mut notified = 0;
    let mut syncing_seen = false;
    let summary = env
        .event("CatchUpSummary", |e| match e {
            UiEvent::ConnState {
                state: ConnState::Syncing { .. },
                ..
            } => {
                syncing_seen = true;
                None
            }
            UiEvent::Notify(_) => {
                notified += 1;
                None
            }
            UiEvent::CatchUpSummary {
                account: a,
                messages,
                conversations,
            } if *a == account => Some((*messages, *conversations)),
            _ => None,
        })
        .await;
    assert!(syncing_seen);
    assert_eq!(
        notified, 0,
        "catch-up messages are never notified one by one"
    );
    assert_eq!(summary, (2, 2));
    env.wait_finished(account).await;
    env.stop().await;
}

/// Every per-network script through the whole stack (backend.db, core_outbox, forwarder,
/// supervisor, writer) ends in the state the ingest harness computes, with no contract
/// violation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_network_scripts_end_to_end() {
    for (flavor, script) in scripts::all().expect("scripts") {
        let mut env = Env::new(flavor, script.clone()).await;
        env.handle.send(UiCommand::UiReady).expect("send");
        let account = add_linked(&mut env).await;
        env.wait_finished(account).await;
        assert!(
            env.control.violations().is_empty(),
            "{}: {:?}",
            script.name,
            env.control.violations()
        );
        let got = env.peek(|c| kourier_core::testing::dump(c, DumpMode::Exact).expect("dump"));
        env.stop().await;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([1; 32]),
            NetworkKind::Fake,
            flavor.descriptor().msg_id_scope,
            &script.own_ids,
        )
        .expect("harness");
        for d in script.deliveries() {
            h.apply(d).expect("apply");
        }
        let expected = h.dump(DumpMode::Exact).expect("dump");
        let strip = |s: &str| -> Vec<String> {
            s.lines()
                .filter(|l| !l.starts_with("account|"))
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(strip(&expected), strip(&got), "{}", script.name);
    }
}

/// Queries answer with their token; typing is forwarded and cleared by the author's next
/// message; the notification policy skips the focused conversation (§8.5, §8.7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_tokens_typing_and_notification_focus() {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");
    let (d_alice, d_bob) = (
        kourier_model::ConvKey::Direct(alice.clone()),
        kourier_model::ConvKey::Direct(bob.clone()),
    );
    let t = |n: i64| scripts::T0 + n * 1000;
    let script = Script::new("focus", vec![me])
        .deliver(vec![
            BackendEvent::ConversationUpsert(kourier_model::ConversationPatch::new(
                d_alice.clone(),
            )),
            BackendEvent::ConversationUpsert(kourier_model::ConversationPatch::new(d_bob.clone())),
        ])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(10)))
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d_alice,
            &alice,
            &t(1).to_string(),
            t(1),
            "to the open chat",
        ))])
        .step(Step::Ephemeral(Box::new(BackendEvent::Typing {
            conv: d_bob.clone(),
            by: bob.clone(),
            device: Some(1),
            on: true,
            ttl: Duration::from_secs(15),
        })))
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d_bob,
            &bob,
            &t(2).to_string(),
            t(2),
            "elsewhere",
        ))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;

    let token = kourier_core::facade::QueryToken {
        model: 7,
        generation: 3,
    };
    env.handle
        .send(UiCommand::Query {
            token,
            query: kourier_core::facade::Query::Accounts,
        })
        .expect("send");
    let echoed = env
        .event("QueryResult", |e| match e {
            UiEvent::QueryResult { token, result } => Some((*token, result.is_ok())),
            _ => None,
        })
        .await;
    assert_eq!(echoed, (token, true));

    let env_ref = &env;
    eventually("two conversations", WAIT, || async move {
        env_ref.conversations().await.len() == 2
    })
    .await;
    let convs = env.conversations().await;
    let alice_conv = convs
        .iter()
        .find(|c| c.peer.as_ref() == Some(&alice))
        .expect("alice")
        .id;
    let bob_conv = convs
        .iter()
        .find(|c| c.peer.as_ref() == Some(&bob))
        .expect("bob")
        .id;
    env.handle
        .send(UiCommand::SetPresence {
            focused: true,
            visible: Some(alice_conv),
        })
        .expect("send");
    let clock = env.clock.clone();
    eventually("script sleeping", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    // Presence is applied before the clock moves on.
    tokio::time::sleep(Duration::from_millis(50)).await;
    env.clock.advance_to_next();

    let mut typing = Vec::new();
    let mut notified = Vec::new();
    env.event("bob's message notified", |e| match e {
        UiEvent::Typing {
            account: a,
            conv,
            user,
            on,
            ..
        } if *a == account => {
            typing.push((*conv, user.clone(), *on));
            None
        }
        UiEvent::Notify(n) => {
            notified.push(n.conv);
            (n.conv == bob_conv).then_some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(notified, [bob_conv], "the focused chat is not notified");
    // Bob's message cleared his typing indicator (emitted with the message's invalidations).
    assert_eq!(
        typing,
        [
            (bob_conv, bob.clone(), true),
            (bob_conv, bob.clone(), false)
        ]
    );
    env.wait_finished(account).await;
    env.stop().await;
}

/// Slow acks: a backend that waits before forwarding never loses or reorders events; its
/// connection states reach the UI (disconnect and reconnect). `ConnState` is a watch value, so
/// each state is held on the clock until the test saw it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_forwarding_and_disconnects() {
    let hold = Step::Sleep(Duration::from_secs(10));
    let mut script = small_script("slow")
        .step(Step::WaitAcked)
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .step(hold.clone())
        .step(Step::Conn(ConnState::Connecting))
        .step(hold)
        .step(Step::Conn(ConnState::Online { detail: None }));
    script.forward_delay = Some(Duration::from_secs(2));
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = support::add_account(&mut env).await;
    // Each forwarded batch waits on the clock: drive it until both messages are in.
    let deadline = tokio::time::Instant::now() + WAIT;
    let texts = loop {
        let convs = env.conversations().await;
        if let Some(c) = convs.first() {
            let texts: Vec<String> = env
                .timeline(c.id)
                .await
                .iter()
                .map(|r| match &r.body {
                    kourier_model::Body::Text(t) => t.body.clone(),
                    _ => String::new(),
                })
                .collect();
            if texts.len() == 2 {
                break texts;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "slow forwarding stalled"
        );
        if env.clock.sleepers() > 0 {
            env.clock.advance_to_next();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(texts, ["first", "second"]);
    for expected in ["Offline", "Connecting", "Online"] {
        env.event(expected, |e| match e {
            UiEvent::ConnState { account: a, state }
                if *a == account && state.name() == expected =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
        if expected != "Online" {
            let clock = env.clock.clone();
            eventually("held state", WAIT, || {
                let c = clock.clone();
                async move { c.sleepers() > 0 }
            })
            .await;
            env.clock.advance_to_next();
        }
    }
    env.wait_finished(account).await;
    env.stop().await;
}
