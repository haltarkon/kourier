//! A10a (chunk N2): the core's automatic clear of a "marked unread" mark is DURABLE for an account
//! whose caps state `roaming_writes` (a chat setting the network keeps for the account's other
//! devices; architecture §4.1 principle 4).
//!
//! Reading a marked chat used to send a plain, best-effort `ChangeConversation` after the read
//! committed: a kill in that window lost the clear for ever. Now, for an account that states the
//! flag (the switch on or off), `local::mark_read` stores
//! `DurableCommand::ChangeConversation { SetMarkedUnread { false }, asked_at }` in its own
//! transaction, the runtime hands it to the supervisor, and the supervisor sends it again at every
//! start until the backend answers `CommandPersisted`. An account that states nothing keeps the
//! plain send and stores no row. A stored clear of an account that does not state the flag any
//! more is dropped at a start, not resent for ever.
//!
//! The clock: `asked_at` is `Shared::now`, the clock `CoreConfig` was started with, and the
//! supervisor hands the very same `Arc<dyn Clock>` to every backend (`AccountContext::clock`), so
//! the core and the backend read ONE clock. Here that clock is the test's `FakeClock`, set to a
//! time of its own before the reading; the stored `asked_at` is that time.

use crate::support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, DurableCommand};
use kourier_core::facade::UiCommand;
use kourier_fake::{Flavor, Script};
use kourier_model::{AccountCaps, AccountUuid, ConversationChange, ConversationId, ReceiptKind};
use support::send::{ME, ME_PNI, conv_id, core_db, d_alice, setup, uid};
use support::{CORE_KEY, Env, WAIT, add_linked, eventually};

/// What the reading's `asked_at` must be: the test sets the core's clock to this before it reads
/// (`T0` + 90 s and a few ms: a time of its own, close enough to the script's to move no timer).
const READ_AT: u64 = 1_760_000_090_123;

const CHILD_HOME: &str = "KOURIER_TEST_ROAMING_CLEAR_HOME";
const SIGABRT: i32 = 6;

fn mark(on: bool) -> ConversationChange {
    ConversationChange::SetMarkedUnread { marked_unread: on }
}

fn clearing(at: u64) -> DurableCommand {
    DurableCommand::ChangeConversation {
        conv: d_alice(),
        change: mark(false),
        asked_at: i64::try_from(at).expect("a time"),
    }
}

/// The signal-like account of `send::setup` (one unread message from alice, read receipts on)
/// whose backend states `flags` in its caps first.
fn script(name: &str, flags: &[(&str, bool)]) -> Script {
    let mut caps = AccountCaps::default();
    for (flag, on) in flags {
        caps.flags.insert((*flag).to_owned(), *on);
    }
    let mut events = setup();
    // The core refuses a mark the UI sets for an account that does not state the flag ON, so for
    // such an account the chat is marked by the PHONE (another device), as the script delivers it.
    if !flags
        .iter()
        .any(|(flag, on)| *flag == AccountCaps::FLAG_ROAMING_WRITES && *on)
    {
        let mut marked = kourier_model::ConversationPatch::new(d_alice());
        marked.marked_unread = Some(true);
        events.push(BackendEvent::ConversationUpsert(marked));
    }
    // (The script states the account's caps itself, also "no roaming flag": the fake adds none.)
    Script::new(name, vec![uid(ME), uid(ME_PNI)])
        .with_roaming_writes(None)
        .deliver(vec![BackendEvent::AccountCaps(caps)])
        .deliver(events)
}

fn roaming(on: bool) -> [(&'static str, bool); 1] {
    [(AccountCaps::FLAG_ROAMING_WRITES, on)]
}

/// A linked account whose chat with alice is marked unread (by a mark the UI set and the fake
/// confirmed, like a mark from another device) and has one unread message.
async fn marked_chat(env: &mut Env) -> (ConversationId, AccountUuid) {
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(env).await;
    env.wait_finished(account).await;
    let conv = conv_id(&env.core_db(), &d_alice()).expect("the chat");
    // (An account that states the flag on is marked by the UI; the others by their script.)
    if env.account(account).await.expect("account").roaming_writes == Some(true) {
        env.handle
            .send(UiCommand::ChangeConversation {
                conv,
                change: mark(true),
            })
            .expect("send");
    }
    let env_ref = &*env;
    eventually("the chat is marked unread", WAIT, || async move {
        env_ref
            .conversations()
            .await
            .iter()
            .any(|c| c.id == conv && c.marked_unread)
    })
    .await;
    let uuid = env.account(account).await.expect("account").uuid;
    (conv, uuid)
}

fn pending_clearings(env: &Env) -> i64 {
    env.peek(|c| {
        c.query_row(
            "SELECT count(*) FROM pending_commands WHERE command LIKE '%ChangeConversation%'",
            [],
            |r| r.get(0),
        )
        .expect("count")
    })
}

/// A flagged account (the switch on, and off: both are a statement) clears the mark with a
/// durable command carrying the reading's time and sends NO plain one: the network hears the
/// UI's mark (switch on; with the switch off the core refuses the click and the phone marks the
/// chat) and, behind the reading, the durable clear; the backend's `CommandPersisted` removes the
/// row. A mute sent after the reading (switch on) proves nothing else of the reading's was on its
/// way (the network hears the changes of a chat in the UI's order).
/// Mutants: the durable clear is not stored (the network never hears it); the plain send stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flagged_account_clears_the_mark_durably_and_not_plainly() {
    for on in [true, false] {
        let mut env = Env::new(Flavor::SignalLike, script("flagged", &roaming(on))).await;
        let (conv, uuid) = marked_chat(&mut env).await;
        env.clock.set(READ_AT);
        env.handle.send(UiCommand::MarkRead { conv }).expect("send");
        let env_ref = &env;
        eventually("the backend took the durable clear", WAIT, || async move {
            env_ref
                .control
                .durable_commands(uuid)
                .contains(&clearing(READ_AT))
        })
        .await;

        // Behind the reading, in the UI's order: nothing plain came before the mute. (With the
        // switch off the mute is refused by the core, and the phone's mark was no change of
        // ours: the network hears nothing plain at all.)
        let mute = ConversationChange::SetMuted {
            until: Some(5_000_000_000_000),
        };
        let expected = if on {
            env.handle
                .send(UiCommand::ChangeConversation {
                    conv,
                    change: mute.clone(),
                })
                .expect("send");
            eventually("the network heard the mute", WAIT, || async move {
                env_ref.control.changes(uuid).len() >= 2
            })
            .await;
            vec![mark(true), mute]
        } else {
            Vec::new()
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            env.control.changes(uuid),
            expected,
            "on={on}: no plain clearing beside the durable one"
        );
        let durable_clears = env
            .control
            .durable_commands(uuid)
            .into_iter()
            .filter(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
            .count();
        assert_eq!(durable_clears, 1, "on={on}: one durable clear, once");
        assert_eq!(
            env.control
                .commands(uuid)
                .iter()
                .filter(|c| **c == "ChangeConversationDurable")
                .count(),
            1
        );

        // The backend's CommandPersisted removed every row it was sent (the read sync too).
        eventually("the rows are confirmed and gone", WAIT, || async move {
            pending_clearings(env_ref) == 0
        })
        .await;
        let row = env.conversations().await;
        let row = row.iter().find(|c| c.id == conv).expect("the chat");
        assert!(!row.marked_unread && row.unread == 0, "on={on}: read here");
        env.stop().await;
    }
}

/// F6 (A10a review, N-C): the clearing's `asked_at` is the core's clock when the UI SENT the
/// reading, not when the reading ran. The reading waits for its turn behind an earlier change of
/// the same chat (a timer change, held in its `allocate_for_change` write: the test holds the
/// writer of `core.db` with a transaction of its own), the clock moves on meanwhile (a phone's
/// mark merged in that window is NEWER than the reading), and the stored clear must still say the
/// time the UI sent the reading. The test knows the core has taken the reading when a command
/// sent behind it (a switch for an account that does not exist) has failed: the core handles
/// its commands one after the other.
/// Mutant: the time is taken after the turn's wait (`asked_at.max(now)` in `mark_read`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reading_that_waits_for_its_turn_stores_the_time_the_ui_sent_it() {
    let mut env = Env::new(Flavor::SignalLike, script("turn", &roaming(true))).await;
    let (conv, uuid) = marked_chat(&mut env).await;
    env.clock.set(READ_AT);

    // The writer of core.db is held: the timer change stops at its write and keeps the chat's
    // turn; the reading goes in behind it.
    let holder =
        kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY)).expect("open core.db");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the writer");
    env.handle
        .send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetTimer {
                timer: Some(Duration::from_secs(3_600)),
            },
        })
        .expect("send");
    env.handle.send(UiCommand::MarkRead { conv }).expect("send");
    env.handle
        .send(UiCommand::SetRoamingWrites {
            account: kourier_model::AccountId(9_999),
            on: false,
        })
        .expect("send");
    env.event("the core has taken the reading", |e| match e {
        kourier_core::facade::UiEvent::ActionFailed {
            action: "SetRoamingWrites",
            account: Some(kourier_model::AccountId(9_999)),
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    // The reading is still waiting (nothing was stored); the clock moves on, then the writer
    // is released and the turns run.
    assert_eq!(
        pending_clearings(&env),
        0,
        "nothing stored while the writer is held"
    );
    env.clock.set(READ_AT + 5_000);
    holder.execute_batch("COMMIT").expect("release the writer");
    drop(holder);

    let env_ref = &env;
    eventually("the backend took the durable clear", WAIT, || async move {
        env_ref
            .control
            .durable_commands(uuid)
            .iter()
            .any(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
    })
    .await;
    let clears: Vec<_> = env
        .control
        .durable_commands(uuid)
        .into_iter()
        .filter(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
        .collect();
    assert_eq!(
        clears,
        vec![clearing(READ_AT)],
        "the moment the UI sent the reading, not the moment it ran (READ_AT + 5 s)"
    );
    env.stop().await;
}

/// An account that states no `roaming_writes` (nothing at all, or only another flag) keeps the
/// plain send after the commit and stores no durable row for it: the phone marked the chat (the
/// core refuses the UI's mark of such an account), the network hears the clearing, and the
/// backend is never sent a durable change.
/// Mutant: the flag is not read, the clear is always durable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unflagged_account_still_gets_the_plain_send_and_no_durable_row() {
    for (what, flags) in [
        ("no caps flag", Vec::new()),
        (
            "another flag",
            vec![(AccountCaps::FLAG_ATTACHMENT_RESTORE, true)],
        ),
    ] {
        let mut env = Env::new(Flavor::SignalLike, script("unflagged", &flags)).await;
        let (conv, uuid) = marked_chat(&mut env).await;
        env.clock.set(READ_AT);
        env.handle.send(UiCommand::MarkRead { conv }).expect("send");
        let env_ref = &env;
        eventually("the network heard the clearing", WAIT, || async move {
            env_ref.control.changes(uuid) == vec![mark(false)]
        })
        .await;
        // The read sync and the receipts are confirmed and gone; none was ever a change.
        eventually("the rows are confirmed and gone", WAIT, || async move {
            env_ref.peek(|c| {
                c.query_row("SELECT count(*) FROM pending_commands", [], |r| {
                    r.get::<_, i64>(0)
                })
                .expect("count")
            }) == 0
        })
        .await;
        assert!(
            !env.control
                .durable_commands(uuid)
                .iter()
                .any(|c| matches!(c, DurableCommand::ChangeConversation { .. })),
            "{what}: no durable clear"
        );
        assert!(
            !env.control
                .commands(uuid)
                .contains(&"ChangeConversationDurable"),
            "{what}: the backend was sent none"
        );
        assert_eq!(env.control.changes(uuid), vec![mark(false)]);
        env.stop().await;
    }
}

/// The child of the kill test: a flagged account, the chat marked, the core's clock set to
/// `READ_AT`, the chat read. `KOURIER_FAILPOINT` aborts the process right after the reading's
/// transaction committed (`FAILPOINT_AFTER_PENDING_COMMAND`), before anything was sent. Run by
/// itself it does nothing.
#[test]
fn roaming_clear_child() {
    let Ok(home) = std::env::var(CHILD_HOME) else {
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async move {
            let mut env = Env::inside(
                Path::new(&home),
                Flavor::SignalLike,
                script("kill", &roaming(true)),
            )
            .await;
            let (conv, _) = marked_chat(&mut env).await;
            env.clock.set(READ_AT);
            env.handle.send(UiCommand::MarkRead { conv }).expect("send");
            // The failpoint kills the process behind the commit; getting here is the failure.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
}

/// Runs the child to its death at the failpoint behind the reading's commit.
fn kill_child(home: &Path) {
    let log = home.join("child.log");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(support::child_filter(module_path!(), "roaming_clear_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_HOME, home)
        .env(
            kourier_db::failpoint::ENV,
            kourier_core::FAILPOINT_AFTER_PENDING_COMMAND,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log).expect("log")))
        .spawn()
        .expect("spawn the child");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            panic!("the child hung");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        status.signal(),
        Some(SIGABRT),
        "the child was killed at the failpoint, not {status:?}:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// The `core.db` of the profile the child left in `home`.
fn left_db(home: &Path) -> PathBuf {
    std::fs::read_dir(home)
        .expect("read the home")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("data").exists())
        .expect("the child's profile")
        .join("data")
        .join("core.db")
}

/// The stored durable commands, in order.
fn stored(db: &Path) -> Vec<DurableCommand> {
    core_db(db, |c| {
        let mut q = c
            .prepare("SELECT command FROM pending_commands ORDER BY id")
            .expect("prepare");
        q.query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .map(|json| serde_json::from_str(&json.expect("row")).expect("a durable command"))
            .collect()
    })
}

/// THE KILL: the core dies right after the reading's transaction committed, before the clear was
/// handed to anyone. The clear is in `pending_commands` (it was stored in the SAME transaction
/// that read the chat: the read itself committed, the chat is unmarked and read), and the next
/// start sends it to the backend, once, with the `asked_at` of the reading; the backend's
/// `CommandPersisted` removes the row.
/// Mutant: the durable clear is not stored (the row is not there, the network never hears it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_right_after_the_reading_still_sends_the_clear_at_the_next_start() {
    let home = tempfile::tempdir().expect("tempdir");
    kill_child(home.path());

    // What the kill left: the reading committed AND the clear with it.
    let db = left_db(home.path());
    let marked_unread: (bool, i64) = core_db(&db, |c| {
        c.query_row(
            "SELECT marked_unread, unread FROM conversations WHERE conv_key = ?1",
            [d_alice().encode()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the chat")
    });
    assert_eq!(marked_unread, (false, 0), "the reading committed");
    let rows = stored(&db);
    assert!(
        rows.contains(&clearing(READ_AT)),
        "the clear was stored in the reading's transaction: {rows:?}"
    );
    // The pinned spelling of the row (the JSON the next version must still read).
    let json: String = core_db(&db, |c| {
        c.query_row(
            "SELECT command FROM pending_commands WHERE command LIKE '%ChangeConversation%'",
            [],
            |r| r.get(0),
        )
        .expect("the row")
    });
    let v: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(
        v["ChangeConversation"]["change"]["SetMarkedUnread"]["marked_unread"],
        false
    );
    assert_eq!(v["ChangeConversation"]["asked_at"], READ_AT);

    // A clean start over the same files sends it.
    let env = Env::adopt(
        home.path(),
        Flavor::SignalLike,
        script("kill", &roaming(true)),
    )
    .await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = env.accounts().await[0].id;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let env_ref = &env;
    eventually("the next start sent the clear", WAIT, || async move {
        env_ref
            .control
            .durable_commands(uuid)
            .contains(&clearing(READ_AT))
    })
    .await;
    eventually(
        "its CommandPersisted removed the row",
        WAIT,
        || async move { pending_clearings(env_ref) == 0 },
    )
    .await;
    assert_eq!(
        env.control
            .durable_commands(uuid)
            .iter()
            .filter(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
            .count(),
        1,
        "once"
    );
    assert!(
        env.control.changes(uuid).is_empty(),
        "and not as a plain change"
    );
}

/// A closed profile that holds, besides what its account stored itself, one stored durable
/// roaming change and one stored receipt command (both written into `core.db` by hand while the
/// core is stopped), then started again. `flag`: what the account's caps state about
/// `roaming_writes`.
async fn restarted_with_stale_rows(flag: Option<bool>) -> (Env, AccountUuid) {
    let flags: Vec<(&str, bool)> = flag.map(|on| roaming(on)[0]).into_iter().collect();
    let mut env = Env::new(Flavor::SignalLike, script("stale", &flags)).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let id = account.0;
    env.stop().await;
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        let sibling = DurableCommand::SendReceipts {
            conv: d_alice(),
            targets: Vec::new(),
            kind: ReceiptKind::Read,
        };
        for cmd in [clearing(READ_AT), sibling] {
            conn.execute(
                "INSERT INTO pending_commands(account_id, command, created_at) VALUES (?1, ?2, 1)",
                (id, serde_json::to_string(&cmd).expect("json")),
            )
            .expect("insert");
        }
    }
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    (env, uuid)
}

/// A durable clear whose account does not state the flag at a start is DROPPED: the backend is
/// never sent it and the row is gone (not left to come back at every start); the other stored
/// command next to it is sent as ever. Mutant: the filter is gone (the clear is sent again), or
/// it drops every stored command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_durable_clear_of_an_account_whose_flag_went_away_is_dropped_at_start() {
    let (mut env, uuid) = restarted_with_stale_rows(None).await;
    let env_ref = &env;
    // The receipt command is sent (it is after the clear in the table: whatever was sent
    // before it has been heard by then).
    eventually("the other stored command was sent", WAIT, || async move {
        env_ref
            .control
            .durable_commands(uuid)
            .iter()
            .any(|c| matches!(c, DurableCommand::SendReceipts { .. }))
    })
    .await;
    assert!(
        !env.control
            .durable_commands(uuid)
            .iter()
            .any(|c| matches!(c, DurableCommand::ChangeConversation { .. })),
        "the clear of an account without the flag is not sent"
    );
    assert_eq!(
        pending_clearings(&env),
        0,
        "and not kept for the next start"
    );
    env.stop().await;

    // The next start has nothing of it left to drop or send.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = env.accounts().await[0].id;
    env.wait_finished(account).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !env.control
            .durable_commands(uuid)
            .iter()
            .any(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
    );
    assert_eq!(pending_clearings(&env), 0);
    env.stop().await;
}

/// The same stored clear on an account that still states the flag (the switch off too): it is
/// sent again at the start and confirmed. Mutant: the filter drops it whatever the flag says.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_durable_clear_of_an_account_that_still_states_the_flag_is_sent_again() {
    for on in [true, false] {
        let (mut env, uuid) = restarted_with_stale_rows(Some(on)).await;
        let env_ref = &env;
        eventually("the clear was sent again", WAIT, || async move {
            env_ref
                .control
                .durable_commands(uuid)
                .contains(&clearing(READ_AT))
        })
        .await;
        eventually("and confirmed", WAIT, || async move {
            pending_clearings(env_ref) == 0
        })
        .await;
        env.stop().await;
    }
}
