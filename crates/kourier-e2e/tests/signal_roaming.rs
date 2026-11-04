//! A10c through the real core (`target/next/a10-refresh.md` §5.3): the app's core with the Signal
//! backend against `@signalapp/mock-server`, the harness playing our phone ("Myself") and two
//! contacts ("Alice", "Bruno"). Only what the whole app shows is tested here; the write itself
//! (the plan, the wire editor, the 409, the rate, the gate in the backend) is the mock tests of
//! `kourier-signal/tests/storage_writes.rs`.
//!
//! The phone is read the way the phone reads it (`storageState`: the manifest and every listed
//! item, decrypted with the phone's keys), the core through its facade (the account row, the
//! conversation rows, the refusals).
//!
//! - [`the_switch_is_stated_and_a_click_reaches_the_phone`]: a new account says
//!   `roaming_writes` `Some(false)` and `pin_limit` `Some(4)`; a click is refused by the core
//!   and nothing is written; `SetRoamingWrites` on states `Some(true)`; an archive, a pin and a
//!   mute reach the phone's records, and the core's rows keep them after the landing.
//! - [`a_phone_change_reaches_the_list_and_reading_clears_its_mark`]: the phone archives a chat
//!   and marks another unread (the list follows, Kourier writes nothing); reading the marked chat
//!   clears the phone's mark through the durable clear.
//! - [`a_read_after_a_kill_still_clears_the_phone_mark`]: the core is killed right after the
//!   reading's transaction (before the backend heard of it): the clear is in `pending_commands`
//!   and lands at the next start.
//! - [`with_the_switch_off_nothing_is_written`]: the switch on, then off: the row follows, a click
//!   is refused, reading a phone-marked chat clears it here only, the durable clear is confirmed
//!   and dropped, and the server saw no write.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

use crate::support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kourier_backend_api::BackendEnv;
use kourier_core::facade::{
    ActionSubject, ConversationListQuery, ConversationRow, Query, QueryData, Refusal, Roaming,
    UiCommand, UiEvent,
};
use kourier_model::{AccountId, ConversationChange, ConversationId};
use serde::{Deserialize, Serialize};
use support::harness::{
    CreatePrimary, DeviceInfo, Harness, Primary, StorageContact, StorageSpec, StorageView,
};
use support::{App, WAIT, World, user};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// How long a test waits to be sure something did not happen (the write job runs a second after
/// the last change, so this is more than two of its debounces).
const QUIET: Duration = Duration::from_millis(3000);

/// The limit Signal states and the core shows (`global.pinnedChatLimit` default).
const PIN_LIMIT: u32 = 4;

/// Set (to a JSON [`ChildSpec`]) in the helper process of the kill test;
/// [`roaming_child_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_E2E_ROAMING_CHILD";
/// Prefix of the helper process's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-E2E-ROAMING";

/// What the helper process runs: the app over this profile, then a read of `conv` once it shows
/// the phone's mark.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    env: BackendEnv,
    account: i64,
    conv: i64,
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("fits")
}

/// The mock with our phone ("Myself", who knows Alice and Bruno) and the two contacts.
async fn start() -> Option<(World, Primary)> {
    support::init_tracing();
    let harness = Harness::start_or_skip().await.expect("mock harness")?;
    let tmp = tempfile::tempdir().expect("temp dir");
    let env = support::local_env(harness.handshake(), tmp.path());
    let alice = harness
        .create_primary(&CreatePrimary::named("Alice"))
        .await
        .expect("Alice");
    let bruno = harness
        .create_primary(&CreatePrimary::named("Bruno"))
        .await
        .expect("Bruno");
    let phone = harness
        .create_primary(&CreatePrimary {
            contacts: vec![alice.device_ref.clone(), bruno.device_ref.clone()],
            ..CreatePrimary::named("Myself")
        })
        .await
        .expect("Myself");
    Some((
        World {
            harness,
            tmp,
            env,
            phone,
            alice,
        },
        bruno,
    ))
}

/// The phone's state: Alice and Bruno, accepted, with the flags the test gives them. The core
/// makes a chat for a contact of the Storage Service only when a flag differs from the default
/// (most of the phone's contacts are people with no chat), so a test that wants a chat from the
/// link marks it unread.
fn phone_state(
    world: &World,
    bruno: &Primary,
    alice_archived: bool,
    alice_marked: bool,
    bruno_marked: bool,
) -> StorageSpec {
    StorageSpec {
        primary: world.phone.device_ref.clone(),
        contacts: vec![
            StorageContact {
                archived: Some(alice_archived),
                marked_unread: Some(alice_marked),
                ..StorageContact::of(&world.alice.device_ref)
            },
            StorageContact {
                marked_unread: Some(bruno_marked),
                ..StorageContact::of(&bruno.device_ref)
            },
        ],
        ..StorageSpec::default()
    }
}

/// The phone's state is `spec` before the link; the app (the way the app starts: contacts and
/// storage at the link) is linked to it. Returns the app, the account and the linked device; the
/// chats come with the first manifest, the caller waits for the ones it needs ([`chat_of`]).
async fn linked(world: &World, spec: &StorageSpec) -> (App, AccountId, DeviceInfo) {
    world
        .harness
        .set_storage_state(spec)
        .await
        .expect("storage state");
    let mut app = App::start_with(
        world.tmp.path(),
        &world.env,
        kourier_backend_api::InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(world).await;
    (app, account, device)
}

/// The id of the chat with `peer`, once the core lists it.
async fn chat_of(app: &App, account: AccountId, peer: &Primary) -> ConversationId {
    let peer = user(peer);
    wait_row(app, account, "a chat with the person", |r| {
        r.peer.as_ref() == Some(&peer)
    })
    .await
    .id
}

/// Every chat of the account, the inbox and the archive in one list.
async fn rows(app: &App, account: AccountId) -> Vec<ConversationRow> {
    let query = ConversationListQuery {
        account: Some(account),
        archived: None,
        limit: 100,
        ..ConversationListQuery::default()
    };
    match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => page.rows,
        other => panic!("unexpected {other:?}"),
    }
}

/// Waits until a chat of the account satisfies `pred` and returns its row.
async fn wait_row(
    app: &App,
    account: AccountId,
    what: &str,
    pred: impl Fn(&ConversationRow) -> bool,
) -> ConversationRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let all = rows(app, account).await;
        if let Some(row) = all.iter().find(|r| pred(r)) {
            return row.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; the chats: {all:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The row of the chat `conv` now.
async fn row_of(app: &App, account: AccountId, conv: ConversationId) -> ConversationRow {
    rows(app, account)
        .await
        .into_iter()
        .find(|r| r.id == conv)
        .expect("the chat is listed")
}

/// Waits until the account row states the switch as `writes` (and the pin limit Signal states).
async fn wait_switch(app: &App, account: AccountId, writes: Option<bool>) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let row = app.account(account).await.expect("the account's row");
        if row.roaming_writes == writes && row.pin_limit == Some(PIN_LIMIT) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the account never stated roaming_writes {writes:?} with pin_limit {PIN_LIMIT}: now {:?} / {:?}",
            row.roaming_writes,
            row.pin_limit
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn click(app: &App, conv: ConversationId, change: ConversationChange) {
    app.send(UiCommand::ChangeConversation { conv, change });
}

/// The core's reason for the next refused click on `conv`.
async fn refused(app: &mut App, conv: ConversationId) -> Option<Refusal> {
    app.event("a refused ChangeConversation", |e| match e {
        UiEvent::ActionFailed {
            action: "ChangeConversation",
            subject: ActionSubject::Conversation(c),
            error,
            ..
        } if *c == conv => Some(error.refusal()),
        _ => None,
    })
    .await
}

/// Waits until the phone's state, as the phone reads it, satisfies `done`; returns it.
async fn wait_phone(world: &World, what: &str, done: impl Fn(&StorageView) -> bool) -> StorageView {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let view = world
            .harness
            .storage_state(&world.phone.device_ref)
            .await
            .expect("the phone reads its own storage state");
        if done(&view) {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; the phone's state: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How many writes of Kourier the server applied and how many it answered 409.
async fn writes(world: &World) -> (u64, u64) {
    let counted = world.harness.storage_writes().await.expect("storageWrites");
    (counted.writes, counted.conflicts)
}

/// How many PUTs of Kourier reached the server and were refused (V4): the switch-off oracle must
/// see a PUT the mock turned away too.
async fn puts_refused(world: &World) -> u64 {
    world
        .harness
        .storage_writes()
        .await
        .expect("storageWrites")
        .refused
}

/// How many durable roaming changes `core.db` holds, unanswered (the file is read while no app
/// runs over it).
fn pending_clears(dir: &Path) -> i64 {
    let conn = kourier_db::open(
        &dir.join("data").join("core.db"),
        &kourier_db::DbKey::from(support::CORE_KEY),
    )
    .expect("open core.db");
    conn.query_row(
        "SELECT count(*) FROM pending_commands WHERE command LIKE '%ChangeConversation%'",
        [],
        |r| r.get(0),
    )
    .expect("count the durable roaming changes")
}

/// The switch is stated, the core refuses while it is off, a click reaches the phone while it is
/// on, and the core's rows keep what was clicked once the write landed. Guards the whole chain
/// of A10c from the UI command to the phone's record: the caps statement (flag and limit
/// together), the core's second line, `SetRoamingWrites`, the click's route into the write, and
/// the landing's restatement (it must tell the core what the click already showed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_switch_is_stated_and_a_click_reaches_the_phone() {
    let Some((world, bruno)) = start().await else {
        return;
    };
    let (alice_ref, bruno_ref) = (world.alice.device_ref.clone(), bruno.device_ref.clone());
    // Both chats are marked unread on the phone, so the core has them from the link.
    let (mut app, account, _device) =
        linked(&world, &phone_state(&world, &bruno, false, true, true)).await;
    let alice = chat_of(&app, account, &world.alice).await;
    let bruno_chat = chat_of(&app, account, &bruno).await;

    // A new account: the network can, the switch is off, the limit is Signal's. The chats say so.
    wait_switch(&app, account, Some(false)).await;
    for conv in [alice, bruno_chat] {
        let row = wait_row(&app, account, "a chat that says Off", |r| {
            r.id == conv && r.roaming == Roaming::Off
        })
        .await;
        assert!(!row.archived && row.pinned_order.is_none() && row.muted_until.is_none());
    }

    // The core refuses a click with the switch off; nothing reaches the phone.
    click(
        &app,
        alice,
        ConversationChange::SetArchived { archived: true },
    );
    assert_eq!(
        refused(&mut app, alice).await,
        Some(Refusal::NotAllowedHere),
        "the core's own line"
    );
    assert!(
        !row_of(&app, account, alice).await.archived,
        "and shows nothing"
    );
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        writes(&world).await,
        (0, 0),
        "nothing is written with the switch off"
    );
    let view = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");
    assert!(!view.contact(&alice_ref).archived);

    // Turn it on: the row states it, the chats offer the entries.
    app.send(UiCommand::SetRoamingWrites { account, on: true });
    wait_switch(&app, account, Some(true)).await;
    for conv in [alice, bruno_chat] {
        wait_row(&app, account, "a chat that says On", |r| {
            r.id == conv && r.roaming == Roaming::On
        })
        .await;
    }

    // Three clicks: they reach the phone's records, each the way the phone reads it.
    let hour = now_ms() + 3_600_000;
    click(
        &app,
        alice,
        ConversationChange::SetArchived { archived: true },
    );
    click(
        &app,
        bruno_chat,
        ConversationChange::SetPinned { pinned: true },
    );
    click(
        &app,
        bruno_chat,
        ConversationChange::SetMuted { until: Some(hour) },
    );
    let view = wait_phone(&world, "the clicks on the phone", |v| {
        v.contacts.get(&alice_ref).is_some_and(|c| c.archived)
            && v.pins.len() == 1
            && v.contacts
                .get(&bruno_ref)
                .is_some_and(|c| c.muted_until_ms() != 0)
    })
    .await;
    assert!(view.contact(&alice_ref).archived);
    assert!(
        view.contact(&alice_ref).marked_unread && view.contact(&bruno_ref).marked_unread,
        "the clicks leave the phone's marks alone"
    );
    assert_eq!(view.contact(&alice_ref).muted_until_ms(), 0);
    assert!(!view.contact(&bruno_ref).archived);
    assert_eq!(view.contact(&bruno_ref).muted_until_ms(), hour as u64);
    assert_eq!(view.pins, [format!("contact:{bruno_ref}")]);
    assert_eq!(view.records, 3, "the account and the two contacts");
    assert_eq!(view.orphans, 0, "the replaced records are deleted");
    let (applied, conflicts) = writes(&world).await;
    assert!(
        applied >= 1 && conflicts == 0,
        "writes {applied}, conflicts {conflicts}"
    );

    // After the landing the core still shows what was clicked (the landing restates the written
    // values, which are the same ones), and the account still says On with its limit.
    tokio::time::sleep(QUIET).await;
    let row = row_of(&app, account, alice).await;
    assert!(row.archived && row.pinned_order.is_none() && row.muted_until.is_none());
    assert_eq!(row.roaming, Roaming::On);
    let row = row_of(&app, account, bruno_chat).await;
    assert!(!row.archived && row.marked_unread);
    assert_eq!(row.pinned_order, Some(0));
    assert_eq!(row.muted_until, Some(hour));
    wait_switch(&app, account, Some(true)).await;
    app.stop().await;
    world.shutdown().await;
}

/// A phone change reaches the list and causes no write; reading the chat the phone marked unread
/// clears the phone's mark (the durable clear, written because the switch is on) and leaves the
/// phone's other change alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_change_reaches_the_list_and_reading_clears_its_mark() {
    let Some((world, bruno)) = start().await else {
        return;
    };
    let (alice_ref, bruno_ref) = (world.alice.device_ref.clone(), bruno.device_ref.clone());
    let (app, account, device) =
        linked(&world, &phone_state(&world, &bruno, false, false, false)).await;
    app.send(UiCommand::SetRoamingWrites { account, on: true });
    wait_switch(&app, account, Some(true)).await;

    // The phone archives Alice and marks Bruno unread; the nudge makes our device fetch.
    world
        .harness
        .set_storage_state(&phone_state(&world, &bruno, true, false, true))
        .await
        .expect("the phone's change");
    world
        .harness
        .send_fetch_latest(
            &world.phone.device_ref,
            &device.device_ref,
            "storageManifest",
        )
        .await
        .expect("fetch latest");
    // The chats do not exist before this (a contact with no flag set has none): the phone's
    // change makes both.
    let (alice_peer, bruno_peer) = (user(&world.alice), user(&bruno));
    let alice = wait_row(&app, account, "Alice in the archive", |r| {
        r.peer.as_ref() == Some(&alice_peer) && r.archived
    })
    .await
    .id;
    let bruno_chat = wait_row(&app, account, "Bruno marked unread", |r| {
        r.peer.as_ref() == Some(&bruno_peer) && r.marked_unread
    })
    .await
    .id;
    let before = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        writes(&world).await,
        (0, 0),
        "a phone change alone writes nothing"
    );
    let after = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");
    assert_eq!(
        after.version, before.version,
        "the phone's version did not move"
    );

    // Reading Bruno's chat clears the mark here at once and on the phone through the write.
    app.send(UiCommand::MarkRead { conv: bruno_chat });
    let view = wait_phone(&world, "the phone's mark cleared", |v| {
        v.contacts.get(&bruno_ref).is_some_and(|c| !c.marked_unread)
    })
    .await;
    assert!(
        !view.contact(&bruno_ref).archived,
        "Bruno's other flags are the phone's"
    );
    assert!(
        view.contact(&alice_ref).archived,
        "the phone's archive of Alice stands"
    );
    assert_eq!(view.orphans, 0);
    tokio::time::sleep(QUIET).await;
    let (applied, conflicts) = writes(&world).await;
    assert_eq!((applied, conflicts), (1, 0), "one write for the one clear");
    let row = row_of(&app, account, bruno_chat).await;
    assert!(!row.marked_unread && row.unread == 0);
    assert!(row_of(&app, account, alice).await.archived);
    app.stop().await;
    world.shutdown().await;
}

/// THE KILL: the core dies right after the reading's transaction committed (the failpoint
/// `after_pending_command`), before the backend heard of the clear. The clear is in
/// `pending_commands`; the phone still shows the mark and the server saw no write; the next start
/// sends the stored clear and the phone's mark goes, by exactly one write. Guards the durable
/// clear's route end to end (the core stores it in the reading's own transaction, a start sends
/// it again, the backend files it as an intent and writes it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_after_a_kill_still_clears_the_phone_mark() {
    let Some((world, bruno)) = start().await else {
        return;
    };
    let (alice_ref, bruno_ref) = (world.alice.device_ref.clone(), bruno.device_ref.clone());
    let (app, account, _device) =
        linked(&world, &phone_state(&world, &bruno, false, false, true)).await;
    let bruno_chat = chat_of(&app, account, &bruno).await;
    wait_row(&app, account, "Bruno marked unread", |r| {
        r.id == bruno_chat && r.marked_unread
    })
    .await;
    app.send(UiCommand::SetRoamingWrites { account, on: true });
    wait_switch(&app, account, Some(true)).await;
    app.stop().await;
    assert_eq!(writes(&world).await, (0, 0));
    assert_eq!(pending_clears(world.tmp.path()), 0);

    // The helper app reads the chat and dies behind the reading's commit.
    let spec = ChildSpec {
        dir: world.tmp.path().to_owned(),
        env: world.env.clone(),
        account: account.0,
        conv: bruno_chat.0,
    };
    let point = format!("{}:1", kourier_core::FAILPOINT_AFTER_PENDING_COMMAND);
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg(support::child_filter(
            module_path!(),
            "roaming_child_process",
        ))
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
        .env("KOURIER_FAILPOINT", &point)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the helper process");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let reading = tokio::time::timeout(WAIT, async {
        while let Some(line) = lines.next_line().await.expect("child stdout") {
            if line.trim() == format!("{CHILD_LINE} reading") {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(reading, Ok(true), "the helper app read the chat");
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("the helper app ended")
        .expect("helper status");
    assert_eq!(
        status.signal(),
        Some(6),
        "aborted at the failpoint ({status})"
    );

    // Before the next start: the clear is stored, the phone still shows the mark, nothing written.
    assert_eq!(pending_clears(world.tmp.path()), 1, "the clear is stored");
    let view = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");
    assert!(
        view.contact(&bruno_ref).marked_unread,
        "the phone still shows the mark"
    );
    assert_eq!(writes(&world).await, (0, 0));

    // The next start sends it: the phone's mark goes, by one write, and nothing else moves.
    let app = App::start(world.tmp.path(), &world.env).await;
    app.wait_online(account).await;
    let view = wait_phone(&world, "the phone's mark cleared after the restart", |v| {
        v.contacts.get(&bruno_ref).is_some_and(|c| !c.marked_unread)
    })
    .await;
    assert!(!view.contact(&alice_ref).archived && !view.contact(&alice_ref).marked_unread);
    assert!(!view.contact(&bruno_ref).archived);
    assert_eq!(view.orphans, 0);
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        writes(&world).await,
        (1, 0),
        "exactly one write for the one clear"
    );
    let row = row_of(&app, account, bruno_chat).await;
    assert!(!row.marked_unread, "the chat is read here");
    app.stop().await;
    assert_eq!(
        pending_clears(world.tmp.path()),
        0,
        "the backend answered, the row is gone"
    );
    world.shutdown().await;
}

/// The helper process of the kill test: the app over the given profile, online, then a read of the
/// chat once it shows the phone's mark, until the armed failpoint aborts it.
#[test]
fn roaming_child_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    support::init_tracing();
    let spec: ChildSpec = serde_json::from_str(&spec).expect("child spec");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let app = App::start(&spec.dir, &spec.env).await;
        let account = AccountId(spec.account);
        app.wait_online(account).await;
        let conv = ConversationId(spec.conv);
        wait_row(&app, account, "the phone's mark", |r| {
            r.id == conv && r.marked_unread
        })
        .await;
        println!("{CHILD_LINE} reading");
        app.send(UiCommand::MarkRead { conv });
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        app.stop().await;
    });
}

/// The switch on, then off: the row follows both ways, the core refuses a click, reading a chat
/// the phone marked clears it HERE only, the durable clear is confirmed and dropped (its row is
/// gone after the backend answered), and the server saw no write and the phone's version never
/// moved. Guards the switch-off half of the chain: the caps restated, the core's second line, the
/// backend's gate on the automatic clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_switch_off_nothing_is_written() {
    let Some((world, bruno)) = start().await else {
        return;
    };
    let (alice_ref, bruno_ref) = (world.alice.device_ref.clone(), bruno.device_ref.clone());
    let (mut app, account, _device) =
        linked(&world, &phone_state(&world, &bruno, false, false, true)).await;
    let bruno_chat = chat_of(&app, account, &bruno).await;
    wait_row(&app, account, "Bruno marked unread", |r| {
        r.id == bruno_chat && r.marked_unread
    })
    .await;
    app.send(UiCommand::SetRoamingWrites { account, on: true });
    wait_switch(&app, account, Some(true)).await;
    app.send(UiCommand::SetRoamingWrites { account, on: false });
    wait_switch(&app, account, Some(false)).await;
    wait_row(&app, account, "the chat says Off again", |r| {
        r.id == bruno_chat && r.roaming == Roaming::Off
    })
    .await;
    let before = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");

    click(
        &app,
        bruno_chat,
        ConversationChange::SetArchived { archived: true },
    );
    assert_eq!(
        refused(&mut app, bruno_chat).await,
        Some(Refusal::NotAllowedHere),
        "a click is refused with the switch off"
    );
    assert!(!row_of(&app, account, bruno_chat).await.archived);
    app.send(UiCommand::MarkRead { conv: bruno_chat });
    wait_row(&app, account, "Bruno read here", |r| {
        r.id == bruno_chat && !r.marked_unread
    })
    .await;

    tokio::time::sleep(QUIET).await;
    assert_eq!(writes(&world).await, (0, 0), "nothing is written");
    assert_eq!(
        puts_refused(&world).await,
        0,
        "not even a PUT the mock refused"
    );
    let after = world
        .harness
        .storage_state(&world.phone.device_ref)
        .await
        .expect("phone state");
    assert_eq!(
        after.version, before.version,
        "the phone's version never moved"
    );
    assert!(
        after.contact(&bruno_ref).marked_unread,
        "the phone keeps its mark"
    );
    assert!(!after.contact(&bruno_ref).archived && !after.contact(&alice_ref).archived);
    app.stop().await;
    assert_eq!(
        pending_clears(world.tmp.path()),
        0,
        "the durable clear was confirmed and dropped"
    );
    world.shutdown().await;
}
