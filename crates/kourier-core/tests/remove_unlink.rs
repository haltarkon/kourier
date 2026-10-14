//! "Remove Account…" and "Unlink…" (link feedback L4, architecture §4.3 step 7): the network is
//! always asked to forget the device, also when no actor runs (restart backoff, a removal a quit
//! cut off), the user is told once what became of it (`UiEvent::LogoutOutcome`), and an unlink
//! keeps the chats readable.

mod support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{AccountLabel, UiCommand, UiEvent};
use kourier_fake::{Flavor, Script, Step, scripts};
use kourier_model::{AccountId, AccountState, DeviceLogout, LogoutReport, UserId};
use std::time::Duration;

use support::{CORE_KEY, Env, WAIT, add_linked, eventually};

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn script(name: &str) -> Script {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = kourier_model::ConvKey::Direct(alice.clone());
    Script::new(name, vec![me]).deliver(vec![BackendEvent::Message(scripts::remote(
        &d,
        &alice,
        &(scripts::T0 + 1000).to_string(),
        scripts::T0 + 1000,
        "first",
    ))])
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
async fn a_removal_tells_that_the_device_was_removed() {
    let mut env = Env::new(Flavor::SignalLike, script("remove")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    assert!(env.control.has_device(uuid));
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    removed(&mut env, account).await;
    assert!(!env.control.has_device(uuid));
    assert!(env.accounts().await.is_empty());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_the_network_did_not_confirm_says_so_with_the_device_name() {
    let mut s = script("offline-remove");
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    // Removed here all the same (the user's decision: remove anyway and tell once).
    removed(&mut env, account).await;
    assert!(env.accounts().await.is_empty());
    assert!(!env.secret_exists(uuid));
    assert!(env.control.has_device(uuid), "the server still has it");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remove_during_the_restart_backoff_still_logs_the_device_out() {
    let s = script("backoff-remove")
        .step(Step::WaitAcked)
        .step(Step::Panic { times: 1 });
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let clock = env.clock.clone();
    eventually("backoff timer", WAIT, || {
        let c = clock.clone();
        async move { c.sleepers() > 0 }
    })
    .await;
    assert_eq!(env.control.spawns(uuid).len(), 1);
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    removed(&mut env, account).await;
    assert_eq!(
        env.control.spawns(uuid).len(),
        2,
        "an actor was started for the logout"
    );
    assert!(env.control.commands(uuid).contains(&"Logout"));
    assert!(!env.control.has_device(uuid));
    assert!(env.accounts().await.is_empty());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_interrupted_by_a_quit_logs_out_at_the_next_start() {
    let mut env = Env::new(Flavor::SignalLike, script("cut-remove")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    // What a quit during the removal's logout leaves: the row says `Removing`, the keys and
    // the device are still there.
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute(
            "UPDATE accounts SET state = 'Removing' WHERE id = ?1",
            [account.0],
        )
        .expect("mark removing");
    }
    let spawns = env.control.spawns(uuid).len();
    env.restart().await;
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    removed(&mut env, account).await;
    assert_eq!(env.control.spawns(uuid).len(), spawns + 1);
    assert!(!env.control.has_device(uuid));
    assert!(env.accounts().await.is_empty());
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_logs_the_device_out_and_keeps_the_chats() {
    let mut env = Env::new(Flavor::SignalLike, script("unlink")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let conv = eventually_one_chat(&env).await;
    env.handle
        .send(UiCommand::Unlink { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(!was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    let row = env.account(account).await.expect("the account stays");
    assert!(
        matches!(row.state, AccountState::LoggedOut { .. }),
        "{:?}",
        row.state
    );
    assert!(!env.control.has_device(uuid));
    assert!(!env.secret_exists(uuid), "the credentials are wiped");
    assert_eq!(env.timeline(conv).await.len(), 1, "the chat stays readable");
    // A removal of the logged-out account has no device left to tell.
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NoDevice);
    removed(&mut env, account).await;
    env.stop().await;
}

async fn eventually_one_chat(env: &Env) -> kourier_model::ConversationId {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let convs = env.conversations().await;
        if let Some(c) = convs.first()
            && !env.timeline(c.id).await.is_empty()
        {
            return c.id;
        }
        assert!(tokio::time::Instant::now() < deadline, "no chat");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// D10 (link feedback L1/L4): a profile made by the old double link holds two Active accounts
/// of ONE identity, each its own device. "Remove Account…" on one logs out only its device and
/// deletes only its rows and files; the other keeps running and keeps its chats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_one_of_two_links_of_one_identity_keeps_the_other() {
    let mut first = script("twin-a");
    first.device = Some(2);
    let mut env = Env::new(Flavor::SignalLike, first).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let keep = add_linked(&mut env).await;
    env.wait_finished(keep).await;
    // The second link gets its own identity (today's rule refuses a second link of one
    // identity); the profile is then made what the old bug left: one identity, two devices.
    let mut second = script("twin-b");
    second.own_ids = vec![uid("aci:00000000-0000-4000-8000-0000000000bb")];
    second.device = Some(3);
    env.control.push_script(second);
    let gone = add_linked(&mut env).await;
    env.wait_finished(gone).await;
    let keep_uuid = env.account(keep).await.expect("row").uuid;
    let gone_uuid = env.account(gone).await.expect("row").uuid;
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute(
            "UPDATE accounts SET own_ids = (SELECT own_ids FROM accounts WHERE id = ?1),
               display_name = (SELECT display_name FROM accounts WHERE id = ?1),
               number = (SELECT number FROM accounts WHERE id = ?1)
             WHERE id = ?2",
            [keep.0, gone.0],
        )
        .expect("one identity");
    }
    env.restart().await;
    let rows = env.accounts().await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.state == AccountState::Active));
    let devices: Vec<_> = rows.iter().map(|r| r.own_device).collect();
    assert!(
        devices.contains(&Some(2)) && devices.contains(&Some(3)),
        "{devices:?}"
    );
    assert_eq!(own_ids(&env, keep), own_ids(&env, gone), "one identity");
    let kept_chats = chats_of(&env, keep);
    assert!(kept_chats > 0 && chats_of(&env, gone) > 0);

    env.handle
        .send(UiCommand::RemoveAccount { account: gone })
        .expect("send");
    let (_, was_removed, report, who) = outcome_event(&mut env, gone).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    // The notice can tell this link from the other by what the phone shows: the device and the
    // day it was linked (F2).
    assert!(who.twin, "an account that looks like another says so");
    assert_eq!(who.own_device, Some(3));
    assert!(who.linked_at > 0);
    removed(&mut env, gone).await;
    assert!(
        !env.control.has_device(gone_uuid),
        "its device is logged out"
    );
    assert!(env.control.has_device(keep_uuid), "the other device stays");
    assert!(!env.control.commands(keep_uuid).contains(&"Logout"));
    assert!(!env.account_dir(gone_uuid).exists());
    assert!(env.account_dir(keep_uuid).exists());
    assert!(env.secret_exists(keep_uuid));
    assert_eq!(chats_of(&env, gone), 0);
    assert_eq!(
        chats_of(&env, keep),
        kept_chats,
        "the other keeps its chats"
    );
    let rows = env.accounts().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, keep);
    assert_eq!(rows[0].state, AccountState::Active);
    env.stop().await;
}

fn own_ids(env: &Env, id: AccountId) -> String {
    env.peek(|c| {
        c.query_row("SELECT own_ids FROM accounts WHERE id = ?1", [id.0], |r| {
            r.get(0)
        })
        .expect("own ids")
    })
}

fn chats_of(env: &Env, id: AccountId) -> i64 {
    env.peek(|c| {
        c.query_row(
            "SELECT count(*) FROM conversations WHERE account_id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .expect("count")
    })
}

/// The outcome with everything the event carries.
async fn outcome_event(
    env: &mut Env,
    account: AccountId,
) -> (u64, bool, LogoutReport, AccountLabel) {
    env.event("LogoutOutcome", |e| match e {
        UiEvent::LogoutOutcome {
            seq,
            account: a,
            removed,
            report,
            who,
        } if *a == account => Some((*seq, *removed, report.clone(), who.clone())),
        _ => None,
    })
    .await
}

/// Starts the core again with nobody listening (the real app's start runs before any
/// front-end exists); [`front_end_ready`] then plays the front-end.
async fn restart_with_nobody_listening(env: &mut Env) {
    env.stop().await;
    let core = support::start_core(env.dir.path(), &env.fake, &env.vault, &env.clock).await;
    env.handle = core.handle();
    env.core = Some(core);
}

/// The front-end subscribes, then says it is ready (`UiReady`).
fn front_end_ready(env: &mut Env) {
    env.events = env.handle.subscribe();
    env.handle.send(UiCommand::UiReady).expect("send");
}

/// Lets a delayed fake logout go when the test ends (also by a failed assertion): the account's
/// supervisor waits for it, and the runtime of the test would wait for the supervisor.
struct ReleaseLogout(kourier_backend_api::FakeClock);

impl Drop for ReleaseLogout {
    fn drop(&mut self) {
        self.0.advance(Duration::from_secs(60));
    }
}

fn row_state(env: &Env, account: AccountId) -> Option<String> {
    env.peek(|c| {
        c.query_row(
            "SELECT state FROM accounts WHERE id = ?1",
            [account.0],
            |r| r.get(0),
        )
        .ok()
    })
}

async fn row_state_becomes(env: &Env, account: AccountId, state: &str) {
    eventually(state, WAIT, || async move {
        row_state(env, account).as_deref() == Some(state)
    })
    .await;
}

fn mark(env: &Env, account: AccountId, state: &str) {
    let conn =
        kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY)).expect("open core.db");
    conn.execute(
        "UPDATE accounts SET state = ?2 WHERE id = ?1",
        (account.0, state),
    )
    .expect("mark");
}

/// F1: a removal the start finishes runs before any front-end listens. The user must still be
/// told what became of the device, with the account's name (its row is gone by then).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_front_end_that_is_ready_late_is_told_of_the_removal_the_start_finished() {
    let mut s = script("late");
    s.display_name = Some("Ann".into());
    s.number = Some("+15550001".into());
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.stop().await;
    mark(&env, account, "Removing");
    restart_with_nobody_listening(&mut env).await;
    // The removal is done before anybody listens.
    {
        let e = &env;
        eventually("the removal is finished", WAIT, || async move {
            row_state(e, account).is_none()
        })
        .await;
    }
    front_end_ready(&mut env);
    let (seq, was_removed, report, who) = outcome_event(&mut env, account).await;
    assert!(seq > 0);
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(who.display_name.as_deref(), Some("Ann"));
    assert_eq!(who.number.as_deref(), Some("+15550001"));
    assert!(
        who.linked_at > 0,
        "the link day is read before the row goes"
    );
    assert!(!who.twin);
    // What was told is not told again at the next `UiReady`. (The outcome can come both live
    // and from the replay when it is emitted in the moment the front-end subscribes: the same
    // `seq`, which a front-end shows once.)
    env.handle.send(UiCommand::UiReady).expect("send");
    while let Ok((again, ..)) = tokio::time::timeout(Duration::from_millis(300), async {
        outcome_event(&mut env, account).await
    })
    .await
    {
        assert_eq!(again, seq, "a second outcome");
    }
    env.stop().await;
}

/// The start of a core in which the front-end is already listening: the live event and the
/// one sent again at `UiReady` carry the same `seq`, so a front-end shows one notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outcome_told_live_and_again_at_ui_ready_has_one_seq() {
    let mut env = Env::new(Flavor::SignalLike, script("seq")).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (live, _, _, _) = outcome_event(&mut env, account).await;
    removed(&mut env, account).await;
    // The front-end only says it is ready now: the outcome came before.
    env.handle.send(UiCommand::UiReady).expect("send");
    let (again, _, _, _) = outcome_event(&mut env, account).await;
    assert_eq!(live, again, "the same outcome carries the same seq");
    env.stop().await;
}

/// F5: "Unlink…" marks the account `Unlinking` before the logout, so a kill during it is
/// finished at the next start; the mark goes when the account is logged out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlink_marks_the_account_until_the_device_is_logged_out() {
    let mut s = script("unlink-mark");
    s.logout_delay = Some(Duration::from_secs(1));
    let mut env = Env::new(Flavor::SignalLike, s).await;
    let _release = ReleaseLogout(env.clock.clone());
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.handle
        .send(UiCommand::Unlink { account })
        .expect("send");
    row_state_becomes(&env, account, "Unlinking").await;
    let row = env.account(account).await.expect("the account stays");
    assert_eq!(row.state, AccountState::Unlinking);
    // The phone's answer comes.
    env.clock.advance(Duration::from_secs(1));
    let (was_removed, report) = outcome(&mut env, account).await;
    assert!(!was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    row_state_becomes(&env, account, "LoggedOut").await;
    env.stop().await;
}

/// F5: a kill during the unlink's logout leaves the `Unlinking` mark; the next start tries the
/// logout once more, logs the account out with its chats kept and tells what became of the
/// device.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlink_cut_off_by_a_kill_is_finished_at_the_next_start() {
    let mut env = Env::new(Flavor::SignalLike, script("unlink-cut")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let conv = eventually_one_chat(&env).await;
    env.stop().await;
    // What the kill leaves: the mark, the keys, the device.
    mark(&env, account, "Unlinking");
    assert!(env.control.has_device(uuid));
    let spawns = env.control.spawns(uuid).len();
    restart_with_nobody_listening(&mut env).await;
    row_state_becomes(&env, account, "LoggedOut").await;
    front_end_ready(&mut env);
    let (_, was_removed, report, who) = outcome_event(&mut env, account).await;
    assert!(!was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    assert!(who.linked_at > 0);
    assert_eq!(env.control.spawns(uuid).len(), spawns + 1, "one attempt");
    assert!(!env.control.has_device(uuid));
    assert!(!env.secret_exists(uuid), "the credentials are wiped");
    assert_eq!(env.timeline(conv).await.len(), 1, "the chat is kept");
    env.stop().await;
}

/// The actor's exit can overtake the report it sent right before it (the core had not read it
/// yet when the exit was ready): the report is taken all the same, and the user is not told
/// "not reached" about a device that is gone. The core runs on ONE thread here and the test
/// holds it while the actor, on its own thread, sends its events and exits; so the exit is
/// ready and the report queued when the core reads again.
#[tokio::test(flavor = "current_thread")]
async fn a_report_the_exit_overtook_is_still_taken() {
    let mut s = script("overtaken");
    s.logout_delay = Some(Duration::from_secs(1));
    s.logout_backlog = 3;
    let mut env = Env::new(Flavor::SignalLike, s).await;
    let _release = ReleaseLogout(env.clock.clone());
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    // The supervisor marks the row, sends `Logout` and waits; the actor takes it and waits on
    // its clock.
    row_state_becomes(&env, account, "Removing").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    env.clock.advance(Duration::from_secs(1));
    // The only thread of the runtime is busy: the actor sends and exits meanwhile.
    std::thread::sleep(Duration::from_millis(300));
    let (_, report) = outcome(&mut env, account).await;
    assert_eq!(report.outcome, DeviceLogout::Removed, "the report was lost");
    removed(&mut env, account).await;
    assert!(!env.control.has_device(uuid));
    env.stop().await;
}

/// F7: the removal is told only when it is done. A shred that fails keeps the `Removing` row
/// and tells nothing; the start that finishes it tells once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_shred_tells_nothing_until_the_removal_is_done() {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(Flavor::SignalLike, script("shred-silent")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    // A secrets directory that cannot be written: the shred cannot remove the secret, which
    // stays readable for the logout.
    let secrets = env.data().join("secrets");
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let mut told = Vec::new();
    env.event("AccountError", |e| {
        match e {
            UiEvent::LogoutOutcome { .. } => told.push("LogoutOutcome"),
            UiEvent::AccountRemoved { .. } => told.push("AccountRemoved"),
            UiEvent::AccountError { account: a, .. } if *a == account => return Some(()),
            _ => {}
        }
        None
    })
    .await;
    assert!(
        told.is_empty(),
        "told before the removal was done: {told:?}"
    );
    // Nothing more comes in a while either.
    let more = tokio::time::timeout(Duration::from_millis(300), async {
        outcome_event(&mut env, account).await
    })
    .await;
    assert!(more.is_err(), "told although the shred failed");
    assert_eq!(row_state(&env, account).as_deref(), Some("Removing"));
    // The keyring works again; the next start finishes the removal and tells once.
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    restart_with_nobody_listening(&mut env).await;
    {
        let e = &env;
        eventually("the removal is finished", WAIT, || async move {
            row_state(e, account).is_none()
        })
        .await;
    }
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(was_removed);
    assert!(matches!(
        report.outcome,
        DeviceLogout::Removed | DeviceLogout::NoDevice
    ));
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}

fn exec(env: &Env, sql: &str) {
    let conn =
        kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY)).expect("open core.db");
    conn.execute_batch(sql).expect("exec");
}

/// L5 (left by L4): a kill between the shred and the deletion of the row leaves a `Removing`
/// row without keys, so the next start cannot log out again. What the first logout told (here
/// the NotReached instruction with the device name) is kept on the row before the shred and
/// told when the start finishes the removal. The kill is played by a trigger that refuses the
/// row's deletion (the shred has run by then).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_after_the_shred_still_tells_the_outcome_at_the_next_start() {
    let mut s = script("kill-after-shred");
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    exec(
        &env,
        "CREATE TRIGGER l5_hold_delete BEFORE DELETE ON accounts
         BEGIN SELECT RAISE(ABORT, 'held by the test'); END;",
    );
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let mut told = Vec::new();
    env.event("AccountError", |e| {
        match e {
            UiEvent::LogoutOutcome { .. } => told.push("LogoutOutcome"),
            UiEvent::AccountError { account: a, .. } if *a == account => return Some(()),
            _ => {}
        }
        None
    })
    .await;
    assert!(told.is_empty(), "told before the removal was done");
    assert!(!env.secret_exists(uuid), "the shred ran");
    assert_eq!(row_state(&env, account).as_deref(), Some("Removing"));
    let spawns = env.control.spawns(uuid).len();
    env.stop().await;
    exec(&env, "DROP TRIGGER l5_hold_delete;");
    restart_with_nobody_listening(&mut env).await;
    {
        let e = &env;
        eventually("the removal is finished", WAIT, || async move {
            row_state(e, account).is_none()
        })
        .await;
    }
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns,
        "no second logout was tried without keys"
    );
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}

/// A removal while the account still receives (link feedback L5; L4-build "Left" 6): the
/// backend is still delivering a long catch-up when the user removes the account. The removal
/// must not wait for that catch-up (it took ~20 s with ERROR "quarantined" lines: the core
/// ingested the rest for an account it was removing), and nothing of it may be left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_during_a_long_receive_does_not_wait_for_it() {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = kourier_model::ConvKey::Direct(alice.clone());
    let mut s = Script::new("remove-while-receiving", vec![me]);
    for i in 0..3000i64 {
        let at = scripts::T0 + 1000 + i;
        s = s.deliver(vec![BackendEvent::Message(scripts::remote(
            &d,
            &alice,
            &at.to_string(),
            at,
            "catch-up",
        ))]);
    }
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    {
        let e = &env;
        eventually("the catch-up has begun", WAIT, || async move {
            chats_of(e, account) > 0
        })
        .await;
    }
    let at_remove = messages_of(&env, account);
    assert!(at_remove < 3000, "the catch-up was over before the removal");
    let began = std::time::Instant::now();
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let (was_removed, report) = outcome(&mut env, account).await;
    let took = began.elapsed();
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    removed(&mut env, account).await;
    assert!(
        took < Duration::from_secs(4),
        "the removal waited for the catch-up: {took:?}"
    );
    assert!(!env.control.has_device(uuid));
    assert!(env.accounts().await.is_empty());
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}

fn messages_of(env: &Env, id: AccountId) -> i64 {
    env.peek(|c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE account_id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .expect("count")
    })
}

// ---- link feedback L5, fix round: a removal or an unlink that a start finishes ----

/// What a first run kept on the row, as `local::set_removal_report` writes it.
const KEPT_NOT_REACHED: &str = r#"{"outcome":"NotReached","device_name":"Workstation"}"#;
const KEPT_REMOVED: &str = r#"{"outcome":"Removed","device_name":"Workstation"}"#;

/// The row as a run that kept `json` and was then cut off leaves it.
fn keep_on_row(env: &Env, account: AccountId, state: &str, json: &str) {
    exec(
        env,
        &format!(
            "UPDATE accounts SET state = '{state}', state_reason = '{json}' WHERE id = {}",
            account.0
        ),
    );
}

/// The outcome is told BEFORE the account directory is removed (on purpose, L5): a test that
/// read the outcome waits for the directory, never asserts at once (both kept-outcome tests
/// failed that way under load in the L6 sweep).
async fn dir_is_gone(env: &Env, uuid: kourier_model::AccountUuid) {
    let dir = env.account_dir(uuid);
    eventually("the account directory is removed", WAIT, || {
        let dir = dir.clone();
        async move { !dir.exists() }
    })
    .await;
}

async fn row_is_gone(env: &Env, account: AccountId) {
    eventually("the removal is finished", WAIT, || async move {
        row_state(env, account).is_none()
    })
    .await;
}

/// A kept NotReached whose keys are still here (the shred failed, or the kill came before
/// it): the start tries the logout ONCE more, and the server answers now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_unreachable_outcome_is_tried_once_more_at_the_next_start() {
    let mut s = script("kept-retry");
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    keep_on_row(&env, account, "Removing", KEPT_NOT_REACHED);
    assert!(env.control.has_device(uuid));
    assert!(env.secret_exists(uuid), "the kill came before the shred");
    let spawns = env.control.spawns(uuid).len();
    restart_with_nobody_listening(&mut env).await;
    row_is_gone(&env, account).await;
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(
        report.outcome,
        DeviceLogout::Removed,
        "the better of the two outcomes is told"
    );
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns + 1,
        "exactly one more attempt"
    );
    assert!(!env.control.has_device(uuid));
    dir_is_gone(&env, uuid).await;
    env.stop().await;
}

/// A kept `Removed` is told as it is: another logout would find nothing, so none is tried
/// (the device stays on the mock, which only a logout would remove).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_removed_outcome_is_told_without_another_logout() {
    let mut env = Env::new(Flavor::SignalLike, script("kept-removed")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    keep_on_row(&env, account, "Removing", KEPT_REMOVED);
    let spawns = env.control.spawns(uuid).len();
    restart_with_nobody_listening(&mut env).await;
    row_is_gone(&env, account).await;
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns,
        "no logout was tried"
    );
    assert!(env.control.has_device(uuid), "nobody logged the device out");
    dir_is_gone(&env, uuid).await;
    env.stop().await;
}

/// The start that finishes a removal writes the kept report back WITH the state, in one
/// statement: a log of every update of the row sees no moment without it, so a second kill
/// cannot lose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_outcome_is_never_cleared_by_the_start_that_finishes_the_removal() {
    let mut s = script("kept-never-cleared");
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.stop().await;
    keep_on_row(&env, account, "Removing", KEPT_NOT_REACHED);
    exec(
        &env,
        "CREATE TABLE l5_log(state_reason TEXT);
         CREATE TRIGGER l5_log_updates AFTER UPDATE ON accounts WHEN NEW.state = 'Removing'
         BEGIN INSERT INTO l5_log VALUES (NEW.state_reason); END;",
    );
    restart_with_nobody_listening(&mut env).await;
    row_is_gone(&env, account).await;
    front_end_ready(&mut env);
    let (_, _, report, _) = outcome_event(&mut env, account).await;
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    let (updates, without): (i64, i64) = env.peek(|c| {
        c.query_row(
            "SELECT count(*), coalesce(sum(state_reason IS NULL), 0) FROM l5_log",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("log")
    });
    assert!(updates >= 2, "the log saw the start's writes: {updates}");
    assert_eq!(without, 0, "the row was without its kept outcome");
    env.stop().await;
}

/// The twin of F7 for a removal that was UNREACHABLE the first time: the outcome is kept, the
/// shred fails (the keys stay), and the next start tries the logout once more before it
/// tells. (The second try is unreachable too here, so NotReached is told, with the name.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_shred_after_an_unreachable_logout_tries_the_logout_once_more_at_the_next_start() {
    use std::os::unix::fs::PermissionsExt;
    let mut s = script("shred-then-retry");
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let secrets = env.data().join("secrets");
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    env.event("AccountError", |e| match e {
        UiEvent::AccountError { account: a, .. } if *a == account => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(row_state(&env, account).as_deref(), Some("Removing"));
    assert!(env.secret_exists(uuid), "the shred failed");
    let spawns = env.control.spawns(uuid).len();
    env.stop().await;
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    restart_with_nobody_listening(&mut env).await;
    row_is_gone(&env, account).await;
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns + 1,
        "the keys were still here: one more logout was tried"
    );
    env.stop().await;
}

/// The unlink's twin of the L5 kill-after-the-shred: the outcome is kept on the `Unlinking`
/// row before the credentials are wiped, so a kill between the wipe and the row's new state
/// still tells it at the next start (it said "no device" before: nothing was kept). The kill
/// is played by a trigger that refuses the `LoggedOut` state (the wipe has run by then).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlink_cut_off_after_the_wipe_still_tells_what_its_logout_told() {
    let mut s = script("unlink-kill-after-wipe");
    s.logout_unreachable = true;
    s.device_name = Some("Workstation".into());
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.stop().await;
    exec(
        &env,
        "CREATE TRIGGER l5_hold_logout BEFORE UPDATE ON accounts WHEN NEW.state = 'LoggedOut'
         BEGIN SELECT RAISE(ABORT, 'held by the test'); END;",
    );
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    env.handle
        .send(UiCommand::Unlink { account })
        .expect("send");
    {
        let e = &env;
        eventually("the credentials are wiped", WAIT, || async move {
            !e.secret_exists(uuid)
        })
        .await;
    }
    assert_eq!(row_state(&env, account).as_deref(), Some("Unlinking"));
    let spawns = env.control.spawns(uuid).len();
    env.stop().await;
    exec(&env, "DROP TRIGGER l5_hold_logout;");
    restart_with_nobody_listening(&mut env).await;
    row_state_becomes(&env, account, "LoggedOut").await;
    front_end_ready(&mut env);
    let (_, was_removed, report, _) = outcome_event(&mut env, account).await;
    assert!(!was_removed);
    assert_eq!(report.outcome, DeviceLogout::NotReached);
    assert_eq!(report.device_name.as_deref(), Some("Workstation"));
    assert_eq!(
        env.control.spawns(uuid).len(),
        spawns,
        "no logout without credentials"
    );
    env.stop().await;
}

/// Lets a held directory removal go when the test ends (also by a failed assertion).
struct ReleaseDirRemoval(std::sync::Arc<kourier_core::testing::DirVault>);

impl Drop for ReleaseDirRemoval {
    fn drop(&mut self) {
        self.0.release_dir_removals();
    }
}

/// L5 fix 3: the outcome is told right after the row's deletion, before the wait for the
/// thumbnail computes and the removal of the account's directory (a big account's directory
/// takes a while, and a kill in that time lost the notice: no row, nothing kept). The
/// directory removal is held here: the notice must come anyway, and the end (`AccountRemoved`)
/// only after the release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outcome_is_told_before_the_account_directory_is_removed() {
    let mut env = Env::new(Flavor::SignalLike, script("tell-first")).await;
    let _release = ReleaseDirRemoval(env.vault.clone());
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    env.vault.hold_dir_removals();
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    let mut ended = false;
    let (was_removed, report) = env
        .event("LogoutOutcome", |e| match e {
            UiEvent::AccountRemoved { account: a } if *a == account => {
                ended = true;
                None
            }
            UiEvent::LogoutOutcome {
                account: a,
                removed,
                report,
                ..
            } if *a == account => Some((*removed, report.clone())),
            _ => None,
        })
        .await;
    assert!(!ended, "the end came before the notice");
    assert!(was_removed);
    assert_eq!(report.outcome, DeviceLogout::Removed);
    assert!(
        env.account_dir(uuid).exists(),
        "told while the directory removal still waits"
    );
    assert_eq!(row_state(&env, account), None, "the row is gone already");
    env.vault.release_dir_removals();
    removed(&mut env, account).await;
    assert!(!env.account_dir(uuid).exists());
    env.stop().await;
}
