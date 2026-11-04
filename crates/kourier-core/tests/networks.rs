//! Accounts of a network this run does not have (K-A, `kourier-core/src/networks.rs`): the
//! network is registered or not at a START (no runtime switch in the core). The fake plays the
//! network that is turned off: the first core runs with it, the next one without it.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::NetworkRegistry;
use kourier_core::facade::{CoreError, UiCommand, UiEvent};
use kourier_fake::{Flavor, Script, Step};
use kourier_model::{
    AccountId, AccountState, ConnState, DeviceLogout, LogoutReport, NetworkKind, Text,
};

use support::send::{self, d_alice};
use support::{CORE_KEY, Env, WAIT, add_linked, eventually, start_core_on};

/// The core again on the same files WITHOUT the network: its accounts are paused.
async fn restart_without_network(env: &mut Env) {
    env.stop().await;
    let core = start_core_on(
        env.dir.path(),
        NetworkRegistry::new(),
        &env.vault,
        Arc::new(env.clock.clone()),
    )
    .await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    env.handle.send(UiCommand::UiReady).expect("send");
}

/// A linked account with the setup chats, waited for until they are all listed.
async fn linked(script: Script) -> (Env, AccountId, kourier_model::AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    // The setup delivery is in (a script that holds the account syncing never "finishes").
    let db = env.core_db();
    eventually("the setup chats", WAIT, || {
        let db = db.clone();
        async move {
            send::conv_id(&db, &d_alice()).is_some()
                && send::conv_id(&db, &send::d_bob()).is_some()
                && send::conv_id(&db, &send::group()).is_some()
        }
    })
    .await;
    (env, account, uuid)
}

/// The next refusal of `action`.
async fn refused(env: &mut Env, action: &'static str) -> Arc<CoreError> {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a, error, ..
        } if *a == action => Some(error.clone()),
        _ => None,
    })
    .await
}

/// Real time for the core to do something it must not do.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_of_a_network_that_is_off_is_kept_and_read_but_not_started() {
    let (mut env, account, uuid) = linked(send::script("off-read")).await;
    let chats_before = env.conversations().await.len();
    assert!(chats_before >= 3, "{chats_before}");
    let spawns = env.control.spawns(uuid).len();
    assert_eq!(spawns, 1);

    restart_without_network(&mut env).await;
    settle().await;

    // The row is as it was: still linked, not logged out, not failed, not removing.
    let row = env.account(account).await.expect("the account stays");
    assert_eq!(row.state, AccountState::Active);
    assert_eq!(row.network, NetworkKind::Fake);
    // A defined state that asks nothing of the user: the default `Offline` of an account
    // without an actor (never `LoggedOut` or `NeedsUserAction`, which are banners that act).
    assert!(
        matches!(row.conn, ConnState::Offline { retry_at_ms: None }),
        "{:?}",
        row.conn
    );
    // Its chats and messages are read as ever.
    assert_eq!(env.conversations().await.len(), chats_before);
    let alice = send::conv_id(&env.core_db(), &d_alice()).expect("alice's chat");
    let rows = env.timeline(alice).await;
    assert_eq!(rows.len(), 1, "the message stays readable");
    // Nothing is started, wiped or shredded.
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns,
        "an actor was started"
    );
    assert!(env.secret_exists(uuid), "the account's secret is kept");
    assert!(
        env.backend_db_exists(uuid),
        "the backend's database is kept"
    );
    assert!(env.account_dir(uuid).exists());
    assert!(!env.control.commands(uuid).contains(&"Logout"));

    // The network comes back with a start: the same account runs again, with no new link.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let control = env.control.clone();
    eventually("the actor starts again", WAIT, || {
        let control = control.clone();
        async move { control.spawns(uuid).len() == spawns + 1 }
    })
    .await;
    assert_eq!(
        env.account(account).await.expect("row").state,
        AccountState::Active
    );
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_new_is_sent_and_no_account_is_added_while_the_network_is_off() {
    let (mut env, account, uuid) = linked(send::script("off-refuse")).await;
    let alice = send::conv_id(&env.core_db(), &d_alice()).expect("alice's chat");
    let before = env.timeline(alice).await.len();
    restart_without_network(&mut env).await;

    env.handle
        .send(UiCommand::SendText {
            conv: alice,
            text: Text::plain("while it is off"),
            reply_to: None,
        })
        .expect("send");
    let e = refused(&mut env, "SendText").await;
    assert!(
        matches!(*e, CoreError::UnknownNetwork(NetworkKind::Fake)),
        "{e:?}"
    );
    env.handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    let e = refused(&mut env, "AddAccount").await;
    assert!(
        matches!(*e, CoreError::UnknownNetwork(NetworkKind::Fake)),
        "{e:?}"
    );
    settle().await;
    assert_eq!(env.timeline(alice).await.len(), before, "no row was stored");
    let accounts = env.accounts().await;
    assert_eq!(accounts.len(), 1, "no account row was made");
    assert_eq!(accounts[0].id, account);
    assert_eq!(env.control.spawns(uuid).len(), 1);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_queued_before_the_network_went_away_goes_out_when_it_is_back() {
    // The connection is down (the outbox drains while it is `Syncing` or `Online` only): the
    // send waits in the outbox.
    let s = send::script("off-queued")
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .step(Step::Sleep(Duration::from_secs(10)))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (mut env, _account, uuid) = linked(s).await;
    let alice = send::conv_id(&env.core_db(), &d_alice()).expect("alice's chat");
    env.handle
        .send(UiCommand::SendText {
            conv: alice,
            text: Text::plain("queued"),
            reply_to: None,
        })
        .expect("send");
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { !send::own_messages(&db, alice).is_empty() }
    })
    .await;
    assert!(env.control.transmits(uuid).is_empty(), "sent while offline");

    restart_without_network(&mut env).await;
    settle().await;
    // Still there, still waiting; no actor to send it.
    assert_eq!(send::own_messages(&env.core_db(), alice).len(), 1);
    assert!(env.control.transmits(uuid).is_empty());

    // The network is back: the account connects, goes online and the message goes out once.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let clock = env.clock.clone();
    eventually("the account offline again", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    env.clock.advance(Duration::from_secs(10));
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the queued message was transmitted");
    assert_eq!(env.control.transmits(uuid).len(), 1);
    assert_eq!(send::own_messages(&env.core_db(), alice).len(), 1, "once");
    env.stop().await;
}

/// C2 (K-A fix round): a paused account's messages offer nothing the core would refuse. A
/// message that waited in the outbox when the network went away offers Take Back (a local
/// delete of a message that reached nobody) and nothing that goes to the network: no edit, no
/// reaction, no delete for everyone, no "Send Again"; and Take Back works without a backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_accounts_message_offers_only_take_back_and_it_works() {
    let s = send::script("off-actions").step(Step::Conn(ConnState::Offline { retry_at_ms: None }));
    let (mut env, _account, uuid) = linked(s).await;
    let alice = send::conv_id(&env.core_db(), &d_alice()).expect("alice's chat");
    env.handle
        .send(UiCommand::SendText {
            conv: alice,
            text: Text::plain("queued"),
            reply_to: None,
        })
        .expect("send");
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { !send::own_messages(&db, alice).is_empty() }
    })
    .await;
    // With the network (offline): the row is the user's own queued message.
    let rows = env.timeline(alice).await;
    let mine = rows
        .iter()
        .find(|r| r.out_state.is_some())
        .expect("own row");
    assert!(
        mine.actions.edit && mine.actions.take_back,
        "with its network the queued message can be edited and taken back: {:?}",
        mine.actions
    );
    let message = mine.id;

    restart_without_network(&mut env).await;
    settle().await;
    let rows = env.timeline(alice).await;
    let mine = rows.iter().find(|r| r.id == message).expect("own row");
    let a = &mine.actions;
    assert!(a.take_back, "Take Back stays: {a:?}");
    // (A queued message's "delete for everyone" IS the take-back: the flag carries both.)
    assert!(
        !a.edit && !a.react && !a.retry && a.delete_for_everyone == a.take_back,
        "something that needs the network is offered: {a:?}"
    );
    assert!(
        rows.iter().all(|r| !r.actions.react && !r.actions.edit),
        "no row of a paused account takes a reaction or an edit"
    );

    env.handle
        .send(UiCommand::CancelMessage { message })
        .expect("send");
    let db = env.core_db();
    eventually("the message is taken back", WAIT, || {
        let db = db.clone();
        async move { send::own_messages(&db, alice).is_empty() }
    })
    .await;
    assert!(env.control.transmits(uuid).is_empty());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_relink_restart_and_cancel_find_no_supervisor_and_leave_the_row_alone() {
    let (mut env, account, _uuid) = linked(send::script("off-controls")).await;
    restart_without_network(&mut env).await;
    for (cmd, name) in [
        (UiCommand::Unlink { account }, "Unlink"),
        (
            UiCommand::Relink {
                account,
                flow: "qr".into(),
            },
            "Relink",
        ),
        (UiCommand::RestartAccount { account }, "RestartAccount"),
        (UiCommand::CancelLogin { account }, "CancelLogin"),
    ] {
        env.handle.send(cmd).expect("send");
        let e = refused(&mut env, name).await;
        assert!(
            matches!(*e, CoreError::UnknownAccount(a) if a == account),
            "{name}: {e:?}"
        );
    }
    settle().await;
    let row = env.account(account).await.expect("the account stays");
    assert_eq!(row.state, AccountState::Active);
    env.stop().await;
}

async fn outcome(env: &mut Env, account: AccountId) -> (bool, LogoutReport) {
    env.event("LogoutOutcome", |e| match e {
        UiEvent::LogoutOutcome {
            account: a,
            removed,
            report,
            ..
        } if *a == account => Some((*removed, report.clone())),
        _ => None,
    })
    .await
}

async fn removed(env: &mut Env, account: AccountId) {
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account: a } if *a == account => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_account_whose_network_is_off_removes_it_here_and_says_the_phone_keeps_it() {
    let (mut env, account, uuid) = linked(send::script("off-remove")).await;
    assert!(env.control.has_device(uuid));
    restart_without_network(&mut env).await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    // The network cannot be asked: the phone keeps listing the device until it is removed there.
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    removed(&mut env, account).await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.secret_exists(uuid), "the secret is shredded");
    assert!(!env.account_dir(uuid).exists());
    assert!(env.control.has_device(uuid), "the server still has it");
    assert_eq!(
        env.control.spawns(uuid).len(),
        1,
        "no actor was started for it"
    );
    assert!(!env.control.commands(uuid).contains(&"Logout"));
    // A second command for the account that is gone is an ordinary refusal.
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let e = refused(&mut env, "RemoveAccount").await;
    assert!(matches!(*e, CoreError::UnknownAccount(_)), "{e:?}");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_logged_out_account_whose_network_is_off_has_no_device_to_name() {
    let s = send::script("off-remove-out").step(Step::Unlink {
        reason: "unlinked from the phone".into(),
    });
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // `add_linked` would wait for `Active`, which the script's unlink may already have left.
    let account = support::add_account(&mut env).await;
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
    restart_without_network(&mut env).await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NoDevice);
    removed(&mut env, account).await;
    assert!(env.accounts().await.is_empty());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_a_quit_cut_off_is_finished_here_when_the_network_is_off_at_the_next_start() {
    let (mut env, account, uuid) = linked(send::script("off-cut")).await;
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute(
            "UPDATE accounts SET state = 'Removing' WHERE id = ?1",
            [account.0],
        )
        .expect("mark removing");
    }
    restart_without_network(&mut env).await;
    let db = env.core_db();
    eventually("the removal finished", WAIT, || {
        let db = db.clone();
        async move {
            let conn = kourier_db::open_read_only(&db, &kourier_db::DbKey::from(CORE_KEY))
                .expect("open core.db");
            conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get::<_, i64>(0))
                .expect("count")
                == 0
        }
    })
    .await;
    assert!(!env.secret_exists(uuid));
    assert_eq!(env.control.spawns(uuid).len(), 1, "no actor for the logout");
    // It tells what became of the device, as every other removal does (C8): nothing was kept
    // on this row, so the phone is taken to keep listing it.
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    env.stop().await;
}

/// C8: the outcome a cut-off run kept on the `Removing` row is the one told when the next
/// start (network off) finishes the removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_off_removal_tells_the_outcome_kept_on_its_row_when_the_network_is_off() {
    let (mut env, account, uuid) = linked(send::script("off-cut-kept")).await;
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        let kept =
            serde_json::to_string(&Some(LogoutReport::new(DeviceLogout::NoDevice))).expect("json");
        conn.execute(
            "UPDATE accounts SET state = 'Removing', state_reason = ?2 WHERE id = ?1",
            (account.0, kept),
        )
        .expect("mark removing");
    }
    restart_without_network(&mut env).await;
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NoDevice, "the kept one");
    // `AccountRemoved` is not waited for as an event: this removal runs while the core starts
    // (`reconcile` spawns it) and can end before this test subscribes, and unlike the outcome the
    // event is not told again at `UiReady`. A busy machine made that happen in about one run of
    // three. The row going is what the removal does.
    let db = env.core_db();
    eventually("the removal finished", WAIT, || {
        let db = db.clone();
        async move {
            let conn = kourier_db::open_read_only(&db, &kourier_db::DbKey::from(CORE_KEY))
                .expect("open core.db");
            conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get::<_, i64>(0))
                .expect("count")
                == 0
        }
    })
    .await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.secret_exists(uuid));
    env.stop().await;
}

/// C8: an unlink a quit cut off, found at a start whose network is off, ends logged out here
/// (the chats stay, the credentials go) and tells that the phone may keep the device; it does
/// not stay `Unlinking` for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_off_unlink_ends_logged_out_here_when_the_network_is_off() {
    let (mut env, account, uuid) = linked(send::script("off-cut-unlink")).await;
    let alice = send::conv_id(&env.core_db(), &d_alice()).expect("alice's chat");
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute(
            "UPDATE accounts SET state = 'Unlinking' WHERE id = ?1",
            [account.0],
        )
        .expect("mark unlinking");
    }
    restart_without_network(&mut env).await;
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(!was_removed, "an unlink, not a removal");
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    let row = env.account(account).await.expect("the account stays");
    assert!(
        matches!(row.state, AccountState::LoggedOut { .. }),
        "{:?}",
        row.state
    );
    assert_eq!(
        send::conv_id(&env.core_db(), &d_alice()),
        Some(alice),
        "the chats are kept"
    );
    assert_eq!(env.control.spawns(uuid).len(), 1, "no actor for the logout");
    env.stop().await;
}
