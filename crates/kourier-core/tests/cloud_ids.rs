//! Cloud ids and read positions, step KT-2: a network whose server names messages, orders them
//! and counts what is unread says so through the contract, and the core keeps its promises under
//! those rules. The storage (the migration found by its NAME, so the test does not depend on its
//! number), what the dump sees of it, and what its index may not take from the hot statements.
//!
//! Then the re-key (`ingest/rekey.rs`) on the fake `Flavor::TelegramLike`, through the real runtime:
//! the first attempt of a message waits an hour (`waits_an_hour`), so a test can look at the row,
//! queue what goes behind it and deliver what waits for it, and move on when the clock does.
//!
//! Then what the server owns, on the real ingest (`IngestHarness`) with the flavour's rules: read
//! positions (`ingest/position.rs`), the counts it states, the deletes it checks.

use crate::support;

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kourier_backend_api::{
    BackendEvent, DurableCommand, IncomingMessage, OutgoingKind, TransmitOutcome,
};
use kourier_core::AccountVault;
use kourier_core::facade::{QuoteState, TimelineAnchor, TimelineQuery, UiCommand};
use kourier_core::testing::{DumpMode, IngestHarness, hot_statements};
use kourier_db::rusqlite::types::Value;
use kourier_db::rusqlite::{
    Connection, ErrorCode, OptionalExtension, StatementStatus, params_from_iter,
};
use kourier_fake::calls::Call;
use kourier_fake::scripts::{T0, remote, signal_features, target, unknown_target};
use kourier_fake::{Flavor, Script, Step, TransmitRecord, TransmitRule};
use kourier_model::{
    AccountId, AccountState, AccountUuid, CallMedia, ConvKey, ConversationId, ConversationPatch,
    Deletion, DisappearKind, Expiry, MessageId, MsgIdScope, NetworkKind, Origin, OutgoingState,
    ReceiptKind, Text, UserId,
};
use support::send::{
    alice, backend_db, bob, conv_id, core_db, d_alice, d_bob, outbox_len, rate_limited, script,
    send_state, server_log,
};
use support::{Env, WAIT, add_account, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

/// The step of this change, by the end of its name (its number changes at the landing).
const STEP: &str = "_cloud_ids";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([9; 32]),
        NetworkKind::Fake,
        MsgIdScope::Conversation,
        &[uid(ME)],
    )
    .expect("harness")
}

/// The index of this step in `MIGRATIONS` (0-based), found by its name.
fn step_index() -> usize {
    kourier_core::MIGRATIONS
        .steps
        .iter()
        .position(|m| m.name.ends_with(STEP))
        .expect("the step is registered")
}

/// Every column of `table`, the generated ones too.
fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
    let mut q = conn
        .prepare("SELECT name FROM pragma_table_xinfo(?1)")
        .expect("prepare");
    q.query_map([table], |r| r.get(0))
        .expect("columns")
        .collect::<Result<_, _>>()
        .expect("names")
}

/// The values of `columns` of every row of `table`.
fn rows_of(conn: &Connection, table: &str, columns: &[String]) -> Vec<Vec<Value>> {
    let mut q = conn
        .prepare(&format!(
            "SELECT {} FROM {table} ORDER BY id",
            columns.join(", ")
        ))
        .expect("prepare");
    q.query_map([], |r| (0..columns.len()).map(|i| r.get(i)).collect())
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("values")
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// How `sql` would run, with every parameter bound to 1: the detail lines of its plan.
fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut q = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare");
    let params = vec![1_i64; q.parameter_count()];
    q.query_map(params_from_iter(params), |r| r.get(3))
        .expect("plan")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// A file the step before left behind, with chats and messages of every kind this test can
/// write, gets the columns NULL on every row and changes in no other value (`quiet`, the generated
/// column no step may redefine by accident, among them); the indexes and the table are there, an
/// alias goes with its message and one id is one alias of a message.
#[test]
fn the_migration_adds_the_columns_empty_and_changes_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = kourier_db::DbKey::from([8; 32]);
    let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).expect("open");
    let at = step_index();
    let steps: &'static [kourier_db::Migration] = kourier_core::MIGRATIONS.steps;
    let before = kourier_db::Migrations::new("core", &steps[..at]);
    kourier_db::migrate(&mut conn, &before).expect("the schema before the step");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind, unread, unread_hint, mentions,
             read_sent_at, read_up_to)
           VALUES (1, 1, 'aci:a', 'Direct', 3, 2, 1, 900, 9000),
                  (2, 1, 'g:gv2:one', 'Group', 0, 0, 0, NULL, NULL);
         INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
             origin, sort_key, sent_at, received_at, body_kind, body, read_state, history, silent)
           VALUES (1, 1, 1, 0, 'aci:a', '100', 'Remote', 1000, 100, 100, 'Text', '{}', 'Unread', 0, 0),
                  (2, 1, 1, 0, '', '101', 'Remote', 1010, 101, 101, 'Text', '{}', 'Read', 1, 0),
                  (3, 1, 2, 1, 'aci:a', '102', 'Remote', 1020, 102, 102, 'System',
                   '{\"System\":{\"VerifiedChanged\":{}}}', 'Read', 0, 1);",
    )
    .expect("rows");
    let msg_columns = columns_of(&conn, "messages");
    let conv_columns = columns_of(&conn, "conversations");
    let messages = rows_of(&conn, "messages", &msg_columns);
    let conversations = rows_of(&conn, "conversations", &conv_columns);
    assert_eq!(
        count(&conn, "SELECT quiet FROM messages WHERE id = 3"),
        1,
        "the file holds a quiet notice"
    );

    // Through this step only: a later migration must not change what this test says about it.
    let through = kourier_db::Migrations::new("core", &steps[..=at]);
    let out = kourier_db::migrate(&mut conn, &through).expect("migrate");
    assert_eq!(
        (out.from, out.to),
        (
            u32::try_from(at).expect("small"),
            u32::try_from(at + 1).expect("small")
        )
    );

    assert_eq!(rows_of(&conn, "messages", &msg_columns), messages);
    assert_eq!(
        rows_of(&conn, "conversations", &conv_columns),
        conversations
    );
    let added = |table: &str, known: &[String]| -> Vec<String> {
        columns_of(&conn, table)
            .into_iter()
            .filter(|c| !known.contains(c))
            .collect()
    };
    assert_eq!(added("messages", &msg_columns), ["net_seq"]);
    assert_eq!(
        added("conversations", &conv_columns),
        [
            "read_seq",
            "peer_read_seq",
            "unread_server",
            "mentions_server"
        ]
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM messages WHERE net_seq IS NOT NULL"
        ),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM conversations WHERE read_seq IS NOT NULL
               OR peer_read_seq IS NOT NULL OR unread_server IS NOT NULL
               OR mentions_server IS NOT NULL"
        ),
        0
    );

    for (kind, name) in [
        ("index", "messages_seq"),
        ("index", "messages_unread_seq"),
        ("index", "message_aliases_by_local"),
        ("table", "message_aliases"),
    ] {
        assert_eq!(
            count(
                &conn,
                &format!(
                    "SELECT count(*) FROM sqlite_master WHERE type = '{kind}' AND name = '{name}'"
                )
            ),
            1,
            "{name}"
        );
    }
    assert_eq!(count(&conn, "SELECT count(*) FROM message_aliases"), 0);

    let alias = |message: i64, local: &str| {
        conn.execute(
            "INSERT INTO message_aliases(message_id, account_id, net_local) VALUES (?1, 1, ?2)",
            (message, local),
        )
    };
    alias(2, "k1").expect("an alias");
    alias(2, "k2").expect("a message may have had two ids");
    let again = alias(2, "k1").expect_err("one id is one alias of a message");
    assert_eq!(
        again.sqlite_error_code(),
        Some(ErrorCode::ConstraintViolation)
    );
    alias(99, "k3").expect_err("no alias of a message that is not there");
    conn.execute("DELETE FROM messages WHERE id = 2", [])
        .expect("delete");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM message_aliases"),
        0,
        "the aliases go with their message"
    );
}

/// `messages_seq` is PARTIAL, so that no statement that does not name `net_seq` can pick it and it
/// never ties with an older key (`.claude/rules/core.md`: no planner statistics, the trap of
/// migration 14), and the hot statements keep their indexes: the timeline page, the key of a
/// message under each scope, the recount. One that does name the column does pick it, so the
/// answer "not picked" is not true by accident. `messages_unread_seq` likewise, and the update of
/// the own position walks it only at or below the position (re-check N1).
/// Mutant: `READ_BY_POSITION` pinned to `messages_unread` again.
#[test]
fn the_index_of_the_positions_takes_nothing_from_the_hot_statements() {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness(&dir);
    let conn = h.connection();
    let definition: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'messages_seq'",
            [],
            |r| r.get(0),
        )
        .expect("the index");
    let condition = definition
        .split_once("WHERE")
        .map(|(_, condition)| condition.split_whitespace().collect::<Vec<_>>().join(" "))
        .expect("a partial index");
    assert_eq!(condition, "net_seq IS NOT NULL", "{definition}");

    let statements = hot_statements();
    assert!(!statements.is_empty());
    for (name, sql) in statements {
        let wants: &[&str] = match name {
            "timeline page" | "timeline page, inclusive" => &["messages_timeline"],
            "by_key, account-wide" => &["messages_key_account"],
            "by_key, per conversation" => &["messages_key_conv"],
            "by_key, per sender" => &["messages_key_conv_author"],
            // A miss is one probe of an index that is empty on every network but one.
            "by_key, an earlier name" => &["message_aliases_by_local"],
            "resolve_target, with an author" => &[
                "messages_key_account",
                "revisions_by_key",
                "message_aliases_by_local",
            ],
            "resolve_target, by id" | "messages with an id" => &[
                "messages_local",
                "revisions_by_key",
                "message_aliases_by_local",
            ],
            "the original of a quote" => &["revisions_by_key", "message_aliases_by_local"],
            "recount" => &["messages_unread", "messages_timeline"],
            "the read watermark" => &["messages_unread"],
            "the read position" => &["messages_unread_seq"],
            "the newest row at a read position"
            | "what a read here reaches"
            | "what the other side's marker moved" => &["messages_seq"],
            other => panic!("a hot statement this test holds to no index: {other}"),
        };
        let steps = plan(conn, &sql);
        for want in wants {
            // ("INDEX x (": `messages_key_conv` is the head of `messages_key_conv_author`.)
            assert!(
                steps.iter().any(|s| s.contains(&format!("INDEX {want} ("))),
                "{name} goes along {want}: {steps:#?}"
            );
        }
        for positions in ["messages_seq", "messages_unread_seq"] {
            assert!(
                wants.contains(&positions)
                    || steps
                        .iter()
                        .all(|s| !s.contains(&format!("INDEX {positions} ("))),
                "{name} leaves the index {positions} alone: {steps:#?}"
            );
        }
        // The timeline's index walks the chat: none of the statements of a position looks there, and
        // the update of the own position names its index (no statistics in core.db) and walks only
        // the unread rows at or below the position, never the chat's other unread rows (N1: a server
        // that restates its position with every new message would walk them each time).
        if name == "the read position" {
            assert!(sql.contains("INDEXED BY messages_unread_seq"), "{sql}");
            assert!(
                steps
                    .iter()
                    .any(|s| s
                        .contains("INDEX messages_unread_seq (conversation_id=? AND net_seq<?)")),
                "{name} walks only what is at or below the position: {steps:#?}"
            );
        }
        if wants.iter().any(|w| w.ends_with("_seq")) {
            assert!(
                steps.iter().all(|s| !s.contains("messages_timeline")),
                "{name} walks no timeline: {steps:#?}"
            );
            assert!(
                steps.iter().all(|s| !s.contains("TEMP B-TREE")),
                "{name} sorts nothing: {steps:#?}"
            );
        }
        // ("SCAN CONSTANT ROW" is the one row of `SELECT EXISTS (...)`, no table.)
        assert!(
            steps
                .iter()
                .all(|s| !s.contains("SCAN") || s.contains("CONSTANT ROW")),
            "{name} walks no table: {steps:#?}"
        );
    }

    let steps = plan(
        conn,
        "SELECT id FROM messages WHERE conversation_id = ?1 AND net_seq > ?2 AND net_seq <= ?3
           AND net_author = ''",
    );
    assert!(
        steps.iter().any(|s| s.contains("INDEX messages_seq (")),
        "a statement that names the column goes along it: {steps:#?}"
    );
}

/// The any-order tests compare dumps, so a new column that the dump does not show could differ
/// unseen: the position of a message, the four of a chat in both modes, and one line per alias,
/// keyed by the message's natural key as a revision's line is.
#[test]
fn the_dump_sees_the_positions_the_counts_and_the_aliases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let conv = ConvKey::Direct(uid(ALICE));
    h.apply(vec![BackendEvent::Message(remote(
        &conv,
        &uid(ALICE),
        "m1",
        1_700_000_000_000,
        "hello",
    ))])
    .expect("a message");
    let lines = |h: &IngestHarness, mode: DumpMode, kind: &str| -> Vec<String> {
        h.dump(mode)
            .expect("dump")
            .lines()
            .filter(|l| l.starts_with(kind))
            .map(str::to_owned)
            .collect()
    };
    for mode in [DumpMode::Exact, DumpMode::OrderInsensitive] {
        for (kind, table, column) in [
            ("msg|", "messages", "net_seq"),
            ("conv|", "conversations", "read_seq"),
            ("conv|", "conversations", "peer_read_seq"),
            ("conv|", "conversations", "unread_server"),
            ("conv|", "conversations", "mentions_server"),
        ] {
            let before = lines(&h, mode, kind);
            assert_eq!(before.len(), 1, "{kind}");
            h.connection()
                .execute(
                    &format!("UPDATE {table} SET {column} = coalesce({column}, 0) + 1"),
                    [],
                )
                .expect("update");
            assert_ne!(lines(&h, mode, kind), before, "{column} in {mode:?}");
        }
    }

    assert!(lines(&h, DumpMode::Exact, "alias|").is_empty());
    h.connection()
        .execute(
            "INSERT INTO message_aliases(message_id, account_id, net_local)
               SELECT id, account_id, 'k1' FROM messages",
            [],
        )
        .expect("an alias");
    let expected = format!("alias|{}/{ALICE}/m1|'a1'|k1", conv.encode());
    for mode in [DumpMode::Exact, DumpMode::OrderInsensitive] {
        assert_eq!(lines(&h, mode, "alias|"), std::slice::from_ref(&expected));
    }
}

// ---------- the re-key ----------

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

/// The answer of the first attempt of a message: nothing went out, try again in an hour. A wait the
/// test ends by moving the clock, so what is queued behind the row and what waits for it is in place
/// when it goes.
fn waits_an_hour(name: &str) -> Script {
    script(name)
        .fail_transmit(TransmitRule::attempts(rate_limited(Some(HOUR)), 1).of_kind("Message"))
}

/// The order of the two things the server says of an accepted message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// The echo names the id the message was allocated under, and comes first.
    EchoFirst,
    /// The answer comes first: the fake holds its forwarder on the clock, which stands still until
    /// the test moves it.
    OutcomeFirst,
    /// The echo comes first and knows no binding: two rows until the answer.
    EchoWithoutBinding,
}

impl Order {
    const ALL: [Self; 3] = [
        Self::EchoFirst,
        Self::OutcomeFirst,
        Self::EchoWithoutBinding,
    ];

    fn on(self, mut script: Script) -> Script {
        match self {
            Self::EchoFirst => script,
            Self::OutcomeFirst => {
                script.forward_delay = Some(Duration::from_secs(5));
                script
            }
            Self::EchoWithoutBinding => script.with_echo_without_binding(),
        }
    }
}

/// An account of the fake `TelegramLike` that has its chats. The clock moves only for a script that
/// holds its forwarder (`held`), up to here: every row of the script is in, so what is held from here
/// is what the test made. Then it stands still until the test moves it.
async fn linked_inside(env: &mut Env, held: bool) -> (AccountId, AccountUuid, ConversationId) {
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(env).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let row = env.account(account).await.expect("the account");
        if row.state == AccountState::Active
            && let Some(conv) = conv_id(&env.core_db(), &d_alice())
            && (!held || env.control.is_finished(row.uuid))
        {
            return (account, row.uuid, conv);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the account did not link"
        );
        if held && env.clock.sleepers() > 0 {
            env.clock.advance_to_next();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn linked(script: Script) -> (Env, AccountId, AccountUuid, ConversationId) {
    let held = script.forward_delay.is_some();
    let mut env = Env::new(Flavor::TelegramLike, script).await;
    let (account, uuid, conv) = linked_inside(&mut env, held).await;
    (env, account, uuid, conv)
}

fn say(env: &Env, conv: ConversationId, text: &str, reply_to: Option<MessageId>) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(text),
            reply_to,
        })
        .expect("send");
}

/// Waits until `f` holds on `core.db`.
async fn until(env: &Env, what: &str, f: impl Fn(&Connection) -> bool) {
    let db = env.core_db();
    let f = std::sync::Arc::new(f);
    eventually(what, WAIT, || {
        let (db, f) = (db.clone(), f.clone());
        async move { core_db(&db, |c| f(c)) }
    })
    .await;
}

/// An own message as `core.db` holds it. Its position is read from the row: the fake's are about
/// 1.7e12 (`sent_at`), never small numbers.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    id: MessageId,
    local: String,
    sent_at: i64,
    sort_key: i64,
    net_seq: Option<i64>,
    out_state: Option<OutgoingState>,
    deleted: Option<String>,
}

fn rows_in(db: &Path, conv: ConversationId) -> Vec<Row> {
    core_db(db, |c| {
        c.prepare(
            "SELECT id, net_local, sent_at, sort_key, net_seq, out_state, deleted FROM messages
             WHERE conversation_id = ?1 AND net_author = '' ORDER BY id",
        )
        .expect("prepare")
        .query_map([conv.0], |r| {
            let state: Option<String> = r.get(5)?;
            Ok(Row {
                id: MessageId(r.get(0)?),
                local: r.get(1)?,
                sent_at: r.get(2)?,
                sort_key: r.get(3)?,
                net_seq: r.get(4)?,
                out_state: state.map(|s| serde_json::from_str(&s).expect("a state")),
                deleted: r.get(6)?,
            })
        })
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
    })
}

/// The `n`-th own message of `conv`, once its first attempt was answered and it waits for the next.
async fn waiting_row(env: &Env, conv: ConversationId, n: usize) -> Row {
    until(env, "the message waits for its next attempt", |c| {
        let waiting: i64 = c
            .query_row(
                "SELECT count(*) FROM outbox WHERE conversation_id = ?1 AND kind = 'Message'
                   AND (state = 'Queued' OR state = 'Staged') AND attempts > 0",
                [conv.0],
                |r| r.get(0),
            )
            .expect("count");
        waiting > 0
    })
    .await;
    rows_in(&env.core_db(), conv)[n - 1].clone()
}

/// What the server calls a message of ours ([`Flavor::TelegramLike`]): its `sent_at` in decimal.
fn assigned_of(row: &Row) -> String {
    row.sent_at.to_string()
}

/// The other names of `message`.
fn aliases_of(db: &Path, message: MessageId) -> Vec<String> {
    core_db(db, |c| {
        c.prepare("SELECT net_local FROM message_aliases WHERE message_id = ?1 ORDER BY net_local")
            .expect("prepare")
            .query_map([message.0], |r| r.get(0))
            .expect("aliases")
            .collect::<Result<_, _>>()
            .expect("rows")
    })
}

/// How many rows wait for a target.
fn parked(c: &Connection) -> i64 {
    c.query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count")
}

/// What the fake transmitted of `kind` and accepted, in order.
fn accepted(env: &Env, uuid: AccountUuid, kind: &str) -> Vec<TransmitRecord> {
    accepted_by(&env.control, uuid, kind)
}

/// The id a change names.
fn target_of(kind: &OutgoingKind) -> String {
    match kind {
        OutgoingKind::Edit { target, .. }
        | OutgoingKind::Delete { target, .. }
        | OutgoingKind::React { target, .. } => target.local.to_string(),
        OutgoingKind::Message { .. } => panic!("a message names no target"),
    }
}

/// The id a stored request of `kind` names now.
fn queued_target(c: &Connection, kind: &str) -> String {
    let request: String = c
        .query_row("SELECT request FROM outbox WHERE kind = ?1", [kind], |r| {
            r.get(0)
        })
        .expect("the row");
    let req: kourier_backend_api::OutgoingRequest =
        serde_json::from_str(&request).expect("a request");
    target_of(&req.kind)
}

/// The fake's `core_outbox`: what the core did not ack yet, and what it refused for good.
async fn in_the_forwarder(env: &Env, uuid: AccountUuid) -> (usize, usize) {
    let key = env.vault.unwrap(uuid).await.expect("backend.db key");
    let backend = backend_db(&env.vault.backend_db(uuid), &key);
    (
        usize::try_from(kourier_db::outbox::len(&backend).expect("len")).expect("small"),
        kourier_db::outbox::poison(&backend, 10)
            .expect("poison")
            .len(),
    )
}

/// Moves the clock while the fake's forwarder holds something, until the core has it all.
async fn let_the_forwarder_go(env: &Env, uuid: AccountUuid) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while in_the_forwarder(env, uuid).await.0 > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the forwarder did not empty"
        );
        if env.clock.sleepers() > 0 {
            env.clock.advance_to_next();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until the OUTCOME of the first send of `message` is recorded: the fake has accepted it and
/// the core has the recipient's state `Sent` in `send_state`. In the echo-first order the echo alone
/// empties the outbox and gives the key (R2-H2), so a test that reads the row on those two reads it
/// before the outcome has met it.
async fn the_outcome_is_recorded(env: &Env, uuid: AccountUuid, message: MessageId) {
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| !accepted_by(c, uuid, "Message").is_empty()),
    )
    .await
    .expect("the fake accepted the message");
    until(env, "the recipient's state is a sent one", move |c| {
        let (all, sent): (i64, i64) = c
            .query_row(
                "SELECT count(*), coalesce(sum(state LIKE '{\"Sent\"%'), 0) FROM send_state
                 WHERE message_id = ?1",
                [message.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("count");
        all > 0 && all == sent
    })
    .await;
}

/// What the fake transmitted of `kind` and accepted, in order.
fn accepted_by(
    control: &kourier_fake::FakeControl,
    uuid: AccountUuid,
    kind: &str,
) -> Vec<TransmitRecord> {
    control
        .transmits(uuid)
        .into_iter()
        .filter(|t| t.req.kind.name() == kind && !matches!(t.outcome, TransmitOutcome::Failed(_)))
        .collect()
}

/// Test 1: a message of ours with a file ends as ONE row whatever the order of the server's answer
/// and its echo: the row it was, with its position and its file, under the server's id and at the
/// server's position; the allocated id is its other name; the outbox row is gone; the recipient's
/// state is a sent one; the server has the message once, under the allocated id. It waits for the
/// OUTCOME too, so the echo-first order meets it: the answer finds the row under the server's id
/// already (`rekey_own`'s "answered twice" branch) and leaves it as the echo made it.
/// Mutants: no alias row; `merge_rows` not called for an echo without a binding; `record_outcome`
/// leaves the recipient's state alone for `SentAs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_echo_with_the_servers_id_is_one_row_in_every_order() {
    for order in Order::ALL {
        let (env, _, uuid, conv) = linked(order.on(waits_an_hour("orders"))).await;
        let files = tempfile::tempdir().expect("sources");
        let path = files.path().join("notes.txt");
        std::fs::write(&path, "the content of notes.txt").expect("write");
        env.handle
            .send(UiCommand::SendFiles {
                request: Default::default(),
                as_file: Vec::new(),
                with_location: Vec::new(),
                conv,
                paths: vec![path].into(),
                caption: None,
                reply_to: None,
            })
            .expect("send");
        let before = waiting_row(&env, conv, 1).await;
        let allocated = before.local.clone();
        assert!(allocated.starts_with('k'), "{order:?}: {allocated}");
        let attachments = |db: &Path| -> Vec<(i64, Option<i64>)> {
            core_db(db, |c| {
                c.prepare("SELECT id, file_id FROM attachments WHERE message_id = ?1")
                    .expect("prepare")
                    .query_map([before.id.0], |r| Ok((r.get(0)?, r.get(1)?)))
                    .expect("attachments")
                    .collect::<Result<_, _>>()
                    .expect("rows")
            })
        };
        let files_before = attachments(&env.core_db());
        assert!(
            files_before.iter().all(|(_, file)| file.is_some()) && files_before.len() == 1,
            "{order:?}: {files_before:?}"
        );

        env.clock.advance(HOUR);
        let db = env.core_db();
        let want = assigned_of(&before);
        let wanted = want.clone();
        until(&env, "the send is done", move |c| {
            let done: i64 = c
                .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
                .expect("count");
            let key: i64 = c
                .query_row(
                    "SELECT count(*) FROM messages WHERE net_local = ?1 AND net_author = ''",
                    [wanted.as_str()],
                    |r| r.get(0),
                )
                .expect("count");
            done == 0 && key == 1
        })
        .await;
        // What the echo made of the row (in the echo-first order the answer has not met it yet, or
        // has: either way the answer must leave all of this alone).
        let shape = |db: &Path| {
            let rows = rows_in(db, conv);
            (
                rows.iter()
                    .map(|r| (r.id, r.local.clone(), r.sent_at, r.sort_key, r.net_seq))
                    .collect::<Vec<_>>(),
                aliases_of(db, before.id),
            )
        };
        let echoed = (order == Order::EchoFirst).then(|| shape(&db));
        the_outcome_is_recorded(&env, uuid, before.id).await;
        if let Some(echoed) = echoed {
            assert_eq!(
                shape(&db),
                echoed,
                "{order:?}: the answer leaves the row as the echo made it"
            );
        }
        if order == Order::OutcomeFirst {
            assert_eq!(
                in_the_forwarder(&env, uuid).await.0,
                1,
                "the answer came first: the echo is still held"
            );
        }
        let_the_forwarder_go(&env, uuid).await;

        let rows = rows_in(&db, conv);
        assert_eq!(rows.len(), 1, "{order:?}: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.id, before.id, "{order:?}: the row it was");
        assert_eq!(row.sort_key, before.sort_key, "{order:?}: its position");
        assert_eq!(row.local, want, "{order:?}: the server's id");
        assert_eq!(row.net_seq, Some(before.sent_at), "{order:?}");
        assert!(
            matches!(row.out_state, Some(OutgoingState::Sent { .. })),
            "{order:?}: {:?}",
            row.out_state
        );
        assert_eq!(attachments(&db), files_before, "{order:?}: its file");
        assert_eq!(
            aliases_of(&db, row.id),
            std::slice::from_ref(&allocated),
            "{order:?}"
        );
        assert_eq!(outbox_len(&db), 0, "{order:?}");
        let states = send_state(&db, row.id);
        assert!(
            !states.is_empty() && states.iter().all(|(_, s)| s.starts_with("{\"Sent\"")),
            "{order:?}: the recipient's state is a sent one: {states:?}"
        );
        assert_eq!(
            in_the_forwarder(&env, uuid).await.1,
            0,
            "{order:?}: nothing poisoned"
        );
        let key = env.vault.unwrap(uuid).await.expect("backend.db key");
        let log = server_log(&env.vault.backend_db(uuid), &key);
        let held: Vec<&str> = log.iter().map(|d| d.local.as_str()).collect();
        assert_eq!(held, [allocated.as_str()], "{order:?}");
    }
}

/// Test 2, first half: a reaction and an edit queued behind a message that waits go out naming the
/// id the server gave it, whichever of the answer and the echo re-keyed it, and the message itself
/// goes out under the allocated id. Mutant: no queue rewrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reaction_and_an_edit_behind_an_unsent_message_name_the_assigned_id() {
    for order in Order::ALL {
        let (env, _, uuid, conv) = linked(order.on(waits_an_hour("behind"))).await;
        say(&env, conv, "hello", None);
        let row = waiting_row(&env, conv, 1).await;
        env.handle
            .send(UiCommand::React {
                message: row.id,
                emoji: "\u{1f44d}".into(),
                remove: false,
            })
            .expect("send");
        env.handle
            .send(UiCommand::EditMessage {
                message: row.id,
                text: Text::plain("edited"),
            })
            .expect("send");
        until(&env, "both queued", |c| {
            c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                .expect("count")
                == 3
        })
        .await;
        for kind in ["React", "Edit"] {
            assert_eq!(
                core_db(&env.core_db(), |c| queued_target(c, kind)),
                row.local,
                "{order:?}: {kind} names the only id there is"
            );
        }

        env.clock.advance(HOUR);
        let control = env.control.clone();
        tokio::time::timeout(
            WAIT,
            control.wait_for(|c| {
                c.transmits(uuid)
                    .iter()
                    .filter(|t| t.req.kind.name() != "Message")
                    .count()
                    >= 2
            }),
        )
        .await
        .expect("both went out");
        let want = assigned_of(&row);
        for kind in ["React", "Edit"] {
            let sent = accepted(&env, uuid, kind);
            assert_eq!(sent.len(), 1, "{order:?}: {kind}");
            assert_eq!(target_of(&sent[0].req.kind), want, "{order:?}: {kind}");
        }
        for sent in env
            .control
            .transmits(uuid)
            .iter()
            .filter(|t| t.req.kind.name() == "Message")
        {
            assert_eq!(*sent.req.local, *row.local, "{order:?}: the allocated id");
        }
        let_the_forwarder_go(&env, uuid).await;
        assert_eq!(rows_in(&env.core_db(), conv).len(), 1, "{order:?}");
    }
}

/// An echo the test delivers, as the fake's server would: under the server's id, naming the allocated
/// id of the message sent first at `T0` (the clock stands there until the test moves it).
fn late_echo() -> IncomingMessage {
    let allocated = Flavor::TelegramLike.allocate(u64::try_from(T0).expect("positive"));
    let mut m = IncomingMessage::new(
        d_alice(),
        T0.to_string(),
        Origin::OwnThisDevice,
        kourier_model::Body::text("hello"),
        T0,
        T0,
    );
    m.server_at = Some(T0);
    m.net_seq = Some(T0);
    m.allocated_as = Some(allocated.local);
    m
}

/// Test 2, second half: a delete for everyone after an attempt that was never answered. The message
/// row goes, a tombstone stays, and the delete waits in the queue naming the allocated id, the only
/// one there is. A late echo re-keys the tombstone and what the queue names, so the delete goes out
/// under the server's id. (A delete of a message IN FLIGHT is refused: `actions::delete`.)
/// Mutant: no queue rewrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_echo_rekeys_a_delete_queued_after_an_unanswered_attempt() {
    let s = waits_an_hour("late-echo")
        .fail_transmit(TransmitRule::attempts(rate_limited(Some(HOUR)), 1).of_kind("Delete"))
        .step(Step::Sleep(MINUTE))
        .deliver(vec![BackendEvent::Message(late_echo())]);
    let (env, _, uuid, conv) = linked(s).await;
    say(&env, conv, "hello", None);
    let row = waiting_row(&env, conv, 1).await;
    assert_eq!(
        row.sent_at, T0,
        "the clock stood still, so the script's echo is its echo"
    );
    env.handle
        .send(UiCommand::DeleteMessage {
            message: row.id,
            for_everyone: true,
        })
        .expect("send");
    until(&env, "the delete was tried once", |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind = 'Delete' AND attempts > 0 AND state = 'Queued'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 1
    })
    .await;
    assert_eq!(
        core_db(&env.core_db(), |c| queued_target(c, "Delete")),
        row.local,
        "before the echo it names the allocated id"
    );

    env.clock.advance(MINUTE + Duration::from_secs(1));
    let want = assigned_of(&row);
    let wanted = want.clone();
    until(&env, "the echo re-keyed the tombstone", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE net_local = ?1 AND deleted = 'ForEveryone'",
            [wanted.as_str()],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 1
    })
    .await;
    assert_eq!(
        core_db(&env.core_db(), |c| queued_target(c, "Delete")),
        want,
        "and what the queue names"
    );

    env.clock.advance(HOUR);
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.transmits(uuid).iter().any(|t| {
                t.req.kind.name() == "Delete" && !matches!(t.outcome, TransmitOutcome::Failed(_))
            })
        }),
    )
    .await
    .expect("the delete went out");
    let sent = accepted(&env, uuid, "Delete");
    assert_eq!(target_of(&sent[0].req.kind), want);
    let rows = rows_in(&env.core_db(), conv);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].deleted.as_deref(), Some("ForEveryone"));
    assert_eq!(rows[0].id, row.id);
    assert_eq!(in_the_forwarder(&env, uuid).await, (0, 0));
}

/// Test 3: what names the server's id before the server said it waits, and applies at the re-key: a
/// reaction and a receipt on the first message, and on the second a delete, on features with
/// `delete_receive: None` (a server that checks deletes has checked it; under the old rule it would be
/// consumed and lost). In both orders (R2-M1): echo first, where the echo's re-key applies them, and
/// outcome first, where the ANSWER re-keys and applies them under the network's rules with the echo
/// still held back; `ingest::rekey::tests` has the same through the answer without the runtime. In the
/// outcome-first order the script is done when the account has linked (the held forwarder lets the
/// clock run to its end), so the three are parked before the test sends anything, and it sets the
/// clock back to `T0` for the ids the script names.
/// Mutants: no `apply_pending`; `record_outcome` runs under the default server rules.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_waited_under_the_servers_id_applies_at_the_rekey() {
    for order in [Order::EchoFirst, Order::OutcomeFirst] {
        what_waited_applies(order).await;
    }
}

async fn what_waited_applies(order: Order) {
    // The two messages the test sends with the clock at `T0`: the allocator's ids rise by one.
    let (first, second) = (T0.to_string(), (T0 + 1).to_string());
    let mut features = signal_features(false);
    features.delete_receive = None;
    let mut chat = ConversationPatch::new(d_alice());
    chat.features = Some(features);
    let own = |local: &str| target(Some(&d_alice()), None, local);
    let s = waits_an_hour("parked")
        .deliver(vec![BackendEvent::ConversationUpsert(chat)])
        .step(Step::Sleep(MINUTE))
        .deliver(vec![
            BackendEvent::Reaction {
                target: own(&first),
                by: Some(alice()),
                emoji: "\u{1f44d}".into(),
                remove: false,
                at: T0 + 10,
            },
            BackendEvent::Receipts {
                targets: vec![own(&first)],
                by: alice(),
                kind: ReceiptKind::Read,
                at: T0 + 10,
            },
            BackendEvent::Deleted {
                target: own(&second),
                by: None,
                for_everyone: true,
                at: T0 + 10,
            },
        ]);
    let (env, _, uuid, conv) = linked(order.on(s)).await;
    if order == Order::OutcomeFirst {
        // (The held forwarder let the clock run through the script's minute; the script's ids are
        // the ones the clock gives at `T0`.)
        env.clock.set(u64::try_from(T0).expect("positive"));
    }
    say(&env, conv, "one", None);
    say(&env, conv, "two", None);
    let one = waiting_row(&env, conv, 1).await;
    until(&env, "both are stored", |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind = 'Message'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 2
    })
    .await;
    let two = rows_in(&env.core_db(), conv)[1].clone();
    assert_eq!(
        (assigned_of(&one), assigned_of(&two)),
        (first, second),
        "{order:?}: the script names these"
    );

    if order == Order::EchoFirst {
        env.clock.advance(MINUTE + Duration::from_secs(1));
    }
    until(&env, "three wait", |c| parked(c) == 3).await;
    let db = env.core_db();
    assert!(
        core_db(&db, |c| {
            let shown: i64 = c
                .query_row("SELECT count(*) FROM reactions", [], |r| r.get(0))
                .expect("count");
            shown == 0
        }),
        "{order:?}: nothing applied yet"
    );

    env.clock.advance(HOUR);
    until(&env, "they applied", |c| parked(c) == 0).await;
    until(&env, "both sends are done", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .expect("count")
            == 0
    })
    .await;
    if order == Order::OutcomeFirst {
        assert!(
            in_the_forwarder(&env, uuid).await.0 > 0,
            "the answers came first: the echoes are still held, and the ANSWER applied them"
        );
    }
    let_the_forwarder_go(&env, uuid).await;
    let rows = rows_in(&db, conv);
    assert_eq!(rows.len(), 2, "{order:?}: {rows:?}");
    assert_eq!(
        rows[0].out_state,
        Some(OutgoingState::Read),
        "{order:?}: the receipt"
    );
    assert_eq!(rows[0].deleted, None, "{order:?}");
    assert_eq!(
        rows[1].deleted.as_deref(),
        Some("ForEveryone"),
        "{order:?}: the delete"
    );
    assert_eq!(
        core_db(&db, |c| c
            .query_row(
                "SELECT user_id FROM reactions WHERE message_id = ?1",
                [one.id.0],
                |r| r.get::<_, String>(0)
            )
            .expect("the reaction")),
        ALICE
    );
    assert_eq!(in_the_forwarder(&env, uuid).await, (0, 0));
}

/// Test 4: an edit made while the message was unsent copied the temporary id into revision 0: it
/// carries the new id after the re-key. A modifier under either id finds the row, and a quote made
/// before the re-key resolves by the alias, while its queued request goes out naming the new id.
/// Mutants: no alias row; the revision's id left; no alias arm in the quote's lookup or in
/// `resolve_target`; no queue rewrite; the quote's target not renamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_and_a_quote_made_while_unsent_follow_the_rekey() {
    let old = Flavor::TelegramLike
        .allocate(u64::try_from(T0).expect("positive"))
        .local;
    let new = T0.to_string();
    let own = |local: &str| target(Some(&d_alice()), None, local);
    let s = waits_an_hour("edit-quote")
        .step(Step::Sleep(HOUR * 2))
        .deliver(vec![
            BackendEvent::Reaction {
                target: own(&old),
                by: Some(alice()),
                emoji: "\u{1f44d}".into(),
                remove: false,
                at: T0 + 10,
            },
            BackendEvent::Reaction {
                target: own(&new),
                by: Some(bob()),
                emoji: "\u{1f44d}".into(),
                remove: false,
                at: T0 + 10,
            },
        ]);
    let (env, _, uuid, conv) = linked(s).await;
    say(&env, conv, "hello", None);
    let row = waiting_row(&env, conv, 1).await;
    assert_eq!(
        (row.local.as_str(), row.sent_at),
        (&*old, T0),
        "the script names these"
    );
    env.handle
        .send(UiCommand::EditMessage {
            message: row.id,
            text: Text::plain("edited"),
        })
        .expect("send");
    say(&env, conv, "about that", Some(row.id));
    until(&env, "the edit and the quote are queued", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .expect("count")
            == 3
    })
    .await;
    let db = env.core_db();
    let first_revision = |db: &Path| -> String {
        core_db(db, |c| {
            c.query_row(
                "SELECT net_local FROM message_revisions WHERE message_id = ?1 AND edited_at IS NULL",
                [row.id.0],
                |r| r.get(0),
            )
            .expect("revision 0")
        })
    };
    assert_eq!(first_revision(&db), *old);

    env.clock.advance(HOUR);
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            let sent = c.transmits(uuid);
            sent.iter().any(|t| t.req.kind.name() == "Edit")
                && sent
                    .iter()
                    .filter(|t| t.req.kind.name() == "Message")
                    .count()
                    >= 3
        }),
    )
    .await
    .expect("the edit and the quote went out");
    until(&env, "the queue is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .expect("count")
            == 0
    })
    .await;

    assert_eq!(first_revision(&db), new, "revision 0 carries the new id");
    assert_eq!(
        core_db(&db, |c| c
            .query_row(
                "SELECT count(*) FROM message_revisions WHERE message_id = ?1",
                [row.id.0],
                |r| r.get::<_, i64>(0)
            )
            .expect("count")),
        2,
        "the original and the edit"
    );
    assert_eq!(target_of(&accepted(&env, uuid, "Edit")[0].req.kind), new);
    let quoting = env
        .control
        .transmits(uuid)
        .into_iter()
        .rev()
        .find_map(|t| match t.req.kind {
            OutgoingKind::Message {
                reply_to: Some(reply),
                ..
            } => Some(reply),
            _ => None,
        })
        .expect("the quoting send");
    assert_eq!(*quoting.target.local, *new, "the quote goes out naming it");

    // The row of the quote still says what it was made with, and finds its original by that name.
    assert!(
        core_db(&db, |c| c
            .query_row(
                "SELECT count(*) FROM messages WHERE instr(reply_to, ?1) > 0",
                [&*old],
                |r| r.get::<_, i64>(0)
            )
            .expect("count"))
            == 1,
        "the stored quote is not rewritten"
    );
    let timeline = env.timeline(conv).await;
    let quote = timeline
        .iter()
        .find_map(|m| m.reply.clone())
        .expect("a row with a quote");
    assert_eq!(quote.state, QuoteState::Known);
    assert_eq!(quote.target, Some(row.id), "by the alias");

    // A later modifier under either id.
    env.clock.advance(HOUR * 2);
    until(&env, "both reactions found the row", move |c| {
        c.query_row(
            "SELECT count(*) FROM reactions WHERE message_id = ?1 AND user_id <> ''",
            [row.id.0],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 2
    })
    .await;
    assert_eq!(core_db(&db, parked), 0);
}

const CHILD_HOME: &str = "KOURIER_TEST_CLOUD_IDS_HOME";
const CHILD_ORDER: &str = "KOURIER_TEST_CLOUD_IDS_ORDER";
const SIGABRT: i32 = 6;

fn order_named(name: &str) -> Order {
    match name {
        "EchoFirst" => Order::EchoFirst,
        "OutcomeFirst" => Order::OutcomeFirst,
        _ => Order::EchoWithoutBinding,
    }
}

/// The child of the kill test: a message that waits, a reaction and an edit queued behind it, then
/// the clock moves and the attempt goes. The failpoint kills the process around the commit of the
/// answer. Run by itself it does nothing.
#[test]
fn rekey_child() {
    let (Ok(home), Ok(order)) = (std::env::var(CHILD_HOME), std::env::var(CHILD_ORDER)) else {
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async move {
            let order = order_named(&order);
            let script = order.on(waits_an_hour("kill"));
            let mut env = Env::inside(Path::new(&home), Flavor::TelegramLike, script).await;
            let (_, _, conv) = linked_inside(&mut env, order == Order::OutcomeFirst).await;
            say(&env, conv, "hello", None);
            let row = waiting_row(&env, conv, 1).await;
            env.handle
                .send(UiCommand::React {
                    message: row.id,
                    emoji: "\u{1f44d}".into(),
                    remove: false,
                })
                .expect("send");
            env.handle
                .send(UiCommand::EditMessage {
                    message: row.id,
                    text: Text::plain("edited"),
                })
                .expect("send");
            until(&env, "both queued", |c| {
                c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                    .expect("count")
                    == 3
            })
            .await;
            env.clock.advance(HOUR);
            // The failpoint kills the process; getting here is the failure.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
}

/// Runs the child to its death at the `n`-th hit of `failpoint`.
fn kill_child(home: &Path, order: Order, failpoint: &str, n: u32) {
    let log = home.join("child.log");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(support::child_filter(module_path!(), "rekey_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_HOME, home)
        .env(CHILD_ORDER, format!("{order:?}"))
        .env(kourier_db::failpoint::ENV, format!("{failpoint}:{n}"))
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
        "killed at {failpoint}:{n}, not {status:?}:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// The profile a killed child left in `home` (made by [`Env::inside`]): its `core.db`.
fn left_by_the_child(home: &Path) -> std::path::PathBuf {
    std::fs::read_dir(home)
        .expect("read the home")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("data").exists())
        .expect("the child's profile")
        .join("data")
        .join("core.db")
}

/// What the queue in `core.db` names: `(kind, the id)` of every reaction and edit still queued.
fn queued_changes(db: &Path) -> Vec<(String, String)> {
    core_db(db, |c| {
        c.prepare("SELECT kind, request FROM outbox WHERE kind IN ('React', 'Edit') ORDER BY id")
            .expect("prepare")
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .expect("rows")
            .map(|row| {
                let (kind, request) = row.expect("a row");
                let req: kourier_backend_api::OutgoingRequest =
                    serde_json::from_str(&request).expect("a request");
                (kind, target_of(&req.kind))
            })
            .collect()
    })
}

/// Test 5: a kill before and after the commit of the answer (the second hit: the first is the attempt
/// that was told to wait), in each order. The file the child left says what the kill kept: what the
/// commit holds (the echo's, always, in the echo-first order, and the answer's when the kill came
/// after it) names the server's id, what it rolled back the allocated one. After the restart: one
/// row, the same `MessageId`, the server's id, and the queue's targets right: the reaction and the
/// edit go out naming it, the message (if it goes again) under the allocated id, and the server has
/// it once.
/// Mutants: no queue rewrite (the target of a change goes out stale); no alias row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_around_the_outcome_leaves_one_row_and_the_queue_right() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    for order in Order::ALL {
        for failpoint in [
            kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT,
            kourier_core::FAILPOINT_AFTER_OUTCOME,
        ] {
            let what = format!("{order:?} {failpoint}");
            let home = tempfile::tempdir().expect("tempdir");
            kill_child(home.path(), order, failpoint, 2);
            // The file as the kill left it, before a restart can send anything: the targets the
            // queue names are durable state (R2-M2).
            let left = left_by_the_child(home.path());
            let sent_at: i64 = core_db(&left, |c| {
                c.query_row(
                    "SELECT sent_at FROM messages WHERE net_author = '' ORDER BY id LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .expect("the message")
            });
            let committed =
                failpoint == kourier_core::FAILPOINT_AFTER_OUTCOME || order == Order::EchoFirst;
            let named = if committed {
                sent_at.to_string()
            } else {
                Flavor::TelegramLike
                    .allocate(u64::try_from(sent_at).expect("positive"))
                    .local
                    .to_string()
            };
            let queued = queued_changes(&left);
            if order != Order::EchoFirst {
                // (Echo first the echo finished the row, so a change may have gone out already.)
                assert_eq!(
                    queued.len(),
                    2,
                    "{what}: the reaction and the edit wait: {queued:?}"
                );
            }
            for (kind, target) in &queued {
                assert_eq!(*target, named, "{what}: the queued {kind} in the file");
            }
            let mut env = Env::adopt(
                home.path(),
                Flavor::TelegramLike,
                order.on(waits_an_hour("kill")),
            )
            .await;
            env.handle.send(UiCommand::UiReady).expect("send");
            // (The new process's clock starts at `T0` again: past every wait of the first run.)
            env.clock.advance(HOUR * 2);
            let db = env.core_db();
            let deadline = tokio::time::Instant::now() + WAIT;
            let (conv, uuid) = loop {
                let row = env
                    .accounts()
                    .await
                    .into_iter()
                    .next()
                    .expect("the account");
                if let Some(conv) = conv_id(&db, &d_alice())
                    && outbox_len(&db) == 0
                    && in_the_forwarder(&env, row.uuid).await.0 == 0
                {
                    break (conv, row.uuid);
                }
                assert!(tokio::time::Instant::now() < deadline, "{what}: stuck");
                if env.clock.sleepers() > 0 {
                    env.clock.advance_to_next();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let rows = rows_in(&db, conv);
            assert_eq!(rows.len(), 1, "{what}: {rows:?}");
            let row = &rows[0];
            let new = assigned_of(row);
            assert_eq!(row.local, new, "{what}");
            assert_eq!(row.net_seq, Some(row.sent_at), "{what}");
            let allocated = Flavor::TelegramLike
                .allocate(u64::try_from(row.sent_at).expect("positive"))
                .local;
            assert_eq!(aliases_of(&db, row.id), [allocated.to_string()], "{what}");
            for kind in ["React", "Edit"] {
                let sent = accepted(&env, uuid, kind);
                // (Echo first the echo finished the row, so a change may have gone out before the
                // answer, and before the kill. An answer that comes first holds them back.)
                if order != Order::EchoFirst {
                    assert_eq!(sent.len(), 1, "{what}: {kind}");
                }
                for one in sent {
                    assert_eq!(target_of(&one.req.kind), new, "{what}: {kind}");
                }
            }
            for sent in env
                .control
                .transmits(uuid)
                .iter()
                .filter(|t| t.req.kind.name() == "Message")
            {
                assert_eq!(*sent.req.local, *allocated, "{what}");
            }
            let key = env.vault.unwrap(uuid).await.expect("backend.db key");
            let log = server_log(&env.vault.backend_db(uuid), &key);
            assert_eq!(
                log.iter().filter(|d| *d.local == *allocated).count(),
                1,
                "{what}: the server has it once: {log:?}"
            );
            assert_eq!(in_the_forwarder(&env, uuid).await.1, 0, "{what}");
            env.stop().await;
        }
    }
}

/// Test 6: the id the server gives our message is taken by a remote row of the chat. No error, no
/// re-key: the message keeps its allocated id, the send counts, the row of the outbox is gone, no
/// envelope is poisoned, and the next message goes on. (A clash with a call line, and one in another
/// chat, are `ingest::rekey::tests`': a line's id has a prefix the fake's server never gives.)
/// Mutant: no guard against the clash (the rows are merged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_id_a_remote_row_holds_is_no_error_and_the_send_counts() {
    let theirs = remote(&d_alice(), &alice(), &T0.to_string(), T0, "hers");
    let s = waits_an_hour("clash")
        .step(Step::Sleep(MINUTE))
        .deliver(vec![BackendEvent::Message(theirs)]);
    let (env, _, uuid, conv) = linked(s).await;
    say(&env, conv, "hello", None);
    let row = waiting_row(&env, conv, 1).await;
    assert_eq!(row.sent_at, T0, "the script's row has its id");
    env.clock.advance(MINUTE + Duration::from_secs(1));
    let db = env.core_db();
    until(&env, "her row is in", |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE net_author <> '' AND net_local = ?1",
            [T0.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 1
    })
    .await;
    env.clock.advance(HOUR);
    until(&env, "the send is done", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .expect("count")
            == 0
    })
    .await;
    let rows = rows_in(&db, conv);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, row.id);
    assert_eq!(rows[0].local, row.local, "it keeps its allocated id");
    assert_eq!(rows[0].net_seq, None);
    assert!(aliases_of(&db, row.id).is_empty());
    until(&env, "the send counts", move |c| {
        let state: Option<String> = c
            .query_row(
                "SELECT out_state FROM messages WHERE id = ?1",
                [row.id.0],
                |r| r.get(0),
            )
            .expect("the row");
        state.is_some_and(|s| s.starts_with("{\"Sent\""))
    })
    .await;
    assert_eq!(
        in_the_forwarder(&env, uuid).await,
        (0, 0),
        "nothing poisoned"
    );

    say(&env, conv, "and on", None);
    until(&env, "the next message goes on", |c| {
        c.query_row("SELECT count(*) FROM message_aliases", [], |r| {
            r.get::<_, i64>(0)
        })
        .expect("count")
            == 1
    })
    .await;
    let next = rows_in(&db, conv)[1].clone();
    assert_eq!(next.local, assigned_of(&next), "the next one is re-keyed");
}

/// A message of hers in a script at the position `seq` (its time is the position too).
fn hers_at(local: &str, seq: i64) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), local, seq, "hers");
    m.net_seq = Some(seq);
    BackendEvent::Message(m)
}

/// The `n`-th own message of `conv`, once the server named it (it has its position).
async fn named_row(env: &Env, conv: ConversationId, n: usize) -> Row {
    until(env, "the server named the message", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages
             WHERE conversation_id = ?1 AND net_author = '' AND net_seq IS NOT NULL",
            [conv.0],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            >= i64::try_from(n).expect("small")
    })
    .await;
    rows_in(&env.core_db(), conv)[n - 1].clone()
}

/// How the core holds the row of hers `local`: its read state.
fn her_read_state(db: &Path, local: &str) -> String {
    core_db(db, |c| {
        c.query_row(
            "SELECT coalesce(read_state, '-') FROM messages WHERE net_local = ?1 AND net_author <> ''",
            [local],
            |r| r.get(0),
        )
        .expect("her row")
    })
}

/// R1-L3: the positions of the two sides cut ONE chat where their messages interleave, through the
/// runtime and the fake that numbers like Telegram. Her rows stand at `T0-10`, `T0+10` and `T0+30`;
/// ours get the positions the server gave them, their `sent_at`: `T0` and `T0+20`. "Read up to
/// `T0+10`" (her row's own position: the boundary counts) reads hers at or below it and ours is no
/// row it touches; "the peer read up to `T0+10`" falls between our two and reads the first only; a
/// later "up to `T0+20`" (our second's own position) reads it. The row of hers with no position
/// stays unread whatever the time says. Two own messages of two chats get two ids and two positions.
/// Mutant: `net_seq <= ?2` as `net_seq < ?2` in `READ_BY_POSITION`; the peer's range `<= ?3` as `< ?3`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_positions_cut_a_chat_where_own_and_incoming_rows_interleave() {
    let s = script("interleave")
        .deliver(vec![
            hers_at("a1", T0 - 10),
            hers_at("a2", T0 + 10),
            hers_at("a3", T0 + 30),
        ])
        .step(Step::Sleep(MINUTE))
        .deliver(vec![read_up_to(T0 + 10), peer_read_up_to(T0 + 10)])
        .step(Step::Sleep(MINUTE))
        .deliver(vec![peer_read_up_to(T0 + 20)]);
    let (env, _, uuid, conv) = linked(s).await;
    let db = env.core_db();
    let timed = (T0 - 60_000).to_string();
    assert_eq!(
        ["a1", "a2", "a3", timed.as_str()].map(|l| her_read_state(&db, l)),
        ["Unread"; 4],
        "nothing is read yet"
    );

    // Ours: at `T0`, at `T0+20`, and one in another chat right behind it, at the same clock.
    say(&env, conv, "one", None);
    let one = named_row(&env, conv, 1).await;
    the_outcome_is_recorded(&env, uuid, one.id).await;
    env.clock.advance(Duration::from_millis(20));
    say(&env, conv, "two", None);
    let two = named_row(&env, conv, 2).await;
    the_outcome_is_recorded(&env, uuid, two.id).await;
    let bob_conv = conv_id(&db, &d_bob()).expect("bob's chat");
    say(&env, bob_conv, "other", None);
    let three = named_row(&env, bob_conv, 1).await;
    the_outcome_is_recorded(&env, uuid, three.id).await;
    assert_eq!(
        (one.net_seq, two.net_seq, three.net_seq),
        (Some(T0), Some(T0 + 20), Some(T0 + 21)),
        "their positions are the times they were sent at"
    );
    assert_eq!(one.local, assigned_of(&one), "re-keyed");
    assert_eq!(two.local, assigned_of(&two), "re-keyed");
    assert_ne!(two.local, three.local, "two chats, two ids");

    // Up to `T0+10`: hers at or below it, ours first; nothing above it, and nothing by time.
    env.clock.advance(MINUTE);
    until(&env, "the first marks arrived", |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE net_local = 'a2' AND read_state = 'Read'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 1
    })
    .await;
    until(&env, "the peer read the first", {
        let first = one.id;
        move |c| {
            let state: Option<String> = c
                .query_row(
                    "SELECT out_state FROM messages WHERE id = ?1",
                    [first.0],
                    |r| r.get(0),
                )
                .expect("the row");
            state.is_some_and(|s| s == "\"Read\"")
        }
    })
    .await;
    assert_eq!(
        ["a1", "a2", "a3", timed.as_str()].map(|l| her_read_state(&db, l)),
        ["Read", "Read", "Unread", "Unread"],
        "hers: at or below the position (it counts) read, above it not, no position not by time"
    );
    let states = |db: &Path| {
        (
            rows_in(db, conv)
                .iter()
                .map(|r| r.out_state.clone())
                .collect::<Vec<_>>(),
            rows_in(db, bob_conv)[0].out_state.clone(),
        )
    };
    let (ours, other) = states(&db);
    assert_eq!(
        ours[0],
        Some(OutgoingState::Read),
        "our first, below the peer's position"
    );
    assert!(
        matches!(ours[1], Some(OutgoingState::Sent { .. })),
        "our second, above it: {:?}",
        ours[1]
    );
    assert!(
        matches!(other, Some(OutgoingState::Sent { .. })),
        "and another chat's: {other:?}"
    );

    // The peer's position rises to our second's own: it counts.
    env.clock.advance(MINUTE);
    until(&env, "the peer read the second", {
        let second = two.id;
        move |c| {
            let state: Option<String> = c
                .query_row(
                    "SELECT out_state FROM messages WHERE id = ?1",
                    [second.0],
                    |r| r.get(0),
                )
                .expect("the row");
            state.is_some_and(|s| s == "\"Read\"")
        }
    })
    .await;
    let (ours, other) = states(&db);
    assert_eq!(ours, [Some(OutgoingState::Read), Some(OutgoingState::Read)]);
    assert!(
        matches!(other, Some(OutgoingState::Sent { .. })),
        "another chat: {other:?}"
    );
    assert_eq!(her_read_state(&db, "a3"), "Unread", "still above");
    assert_eq!(in_the_forwarder(&env, uuid).await, (0, 0));
}

// ---------- what the server owns: positions, counts, deletes ----------

const AT: i64 = 1_700_000_000_000;

/// A harness under the rules of the fake `TelegramLike`.
fn flagged(dir: &tempfile::TempDir) -> IngestHarness {
    harness(dir).with_server(Flavor::TelegramLike.descriptor().server)
}

/// A message of hers at a stated position.
fn hers(local: &str, sent_at: i64, seq: Option<i64>) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), local, sent_at, "hers");
    m.net_seq = seq;
    BackendEvent::Message(m)
}

/// A message of ours at a stated position.
fn ours(local: &str, sent_at: i64, seq: Option<i64>) -> BackendEvent {
    let mut m = kourier_fake::scripts::own(&d_alice(), local, sent_at, "ours");
    m.net_seq = seq;
    BackendEvent::Message(m)
}

/// `e`, a message, at the position `seq`.
fn at_seq(e: BackendEvent, seq: i64) -> BackendEvent {
    let BackendEvent::Message(mut m) = e else {
        panic!("a message");
    };
    m.net_seq = Some(seq);
    BackendEvent::Message(m)
}

fn read_up_to(seq: i64) -> BackendEvent {
    BackendEvent::ReadUpTo {
        conv: d_alice(),
        seq,
    }
}

fn peer_read_up_to(seq: i64) -> BackendEvent {
    BackendEvent::PeerReadUpTo {
        conv: d_alice(),
        seq,
    }
}

/// A read marker of another own device, naming a row of hers.
fn marker_on(local: &str) -> BackendEvent {
    BackendEvent::ReadMarker {
        conv: Some(d_alice()),
        up_to: target(Some(&d_alice()), Some(&alice()), local),
    }
}

/// The chat as [`ConversationPatch`] states its counts and the hint.
fn stated(unread: Option<u32>, mentions: Option<u32>, hint: Option<u32>) -> BackendEvent {
    let mut chat = ConversationPatch::new(d_alice());
    chat.server_unread = unread;
    chat.server_mentions = mentions;
    chat.unread_hint = hint;
    BackendEvent::ConversationUpsert(chat)
}

fn read_state_of(h: &IngestHarness, local: &str) -> Option<String> {
    h.connection()
        .query_row(
            "SELECT read_state FROM messages WHERE net_local = ?1",
            [local],
            |r| r.get(0),
        )
        .expect("the row")
}

/// The read states of `locals`, `-` for none.
fn read_states(h: &IngestHarness, locals: &[&str]) -> Vec<String> {
    locals
        .iter()
        .map(|l| read_state_of(h, l).unwrap_or_else(|| "-".to_owned()))
        .collect()
}

fn out_state_of(h: &IngestHarness, local: &str) -> Option<OutgoingState> {
    let state: Option<String> = h
        .connection()
        .query_row(
            "SELECT out_state FROM messages WHERE net_local = ?1",
            [local],
            |r| r.get(0),
        )
        .expect("the row");
    state.map(|s| serde_json::from_str(&s).expect("a state"))
}

fn is_read_by_peer(h: &IngestHarness, local: &str) -> bool {
    out_state_of(h, local) == Some(OutgoingState::Read)
}

/// A column of a chat of the harness.
fn column_of(h: &IngestHarness, key: &ConvKey, column: &str) -> Option<i64> {
    h.connection()
        .query_row(
            &format!("SELECT {column} FROM conversations WHERE conv_key = ?1"),
            [key.encode()],
            |r| r.get(0),
        )
        .expect("the chat")
}

/// A column of her chat.
fn chat_column(h: &IngestHarness, column: &str) -> Option<i64> {
    column_of(h, &d_alice(), column)
}

fn message_column(h: &IngestHarness, local: &str, column: &str) -> Option<i64> {
    h.connection()
        .query_row(
            &format!("SELECT {column} FROM messages WHERE net_local = ?1"),
            [local],
            |r| r.get(0),
        )
        .expect("the row")
}

fn her_chat(h: &IngestHarness) -> ConversationId {
    ConversationId(
        h.connection()
            .query_row(
                "SELECT id FROM conversations WHERE conv_key = ?1",
                [d_alice().encode()],
                |r| r.get(0),
            )
            .expect("the chat"),
    )
}

/// Test 7: "read up to 120". What is at or below reads, what is above stays unread, also in the same
/// second; one that arrives later at or below is read and does not notify, and its `AfterRead` timer
/// runs; a replay is a no-op and a lower position moves nothing; a chat the core does not have is not
/// made. The time rule still works on a row with no position, and leaves a row with one alone.
/// Mutants: `net_seq <=` as `<` in the update; `READ_UP_TO` without `net_seq IS NULL`; `message()`'s
/// time rule left on for a row with a position.
#[test]
fn a_read_position_reads_what_is_at_or_below_it_and_nothing_by_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let mut timed = remote(&d_alice(), &alice(), "110", AT, "hers");
    timed.net_seq = Some(110);
    timed.expiry = Some(Expiry {
        kind: DisappearKind::AfterRead,
        timer: Duration::from_secs(60),
        started_at: None,
    });
    // One second: a date cannot tell them apart.
    h.apply(vec![
        hers("100", AT, Some(100)),
        BackendEvent::Message(timed),
        hers("120", AT, Some(120)),
        hers("130", AT, Some(130)),
    ])
    .expect("rows");
    assert_eq!(chat_column(&h, "unread"), Some(4));

    let seen = h
        .apply_announcing(vec![read_up_to(120)])
        .expect("the position");
    assert_eq!(
        read_states(&h, &["100", "110", "120", "130"]),
        ["Read", "Read", "Read", "Unread"]
    );
    assert_eq!(seen.changed.len(), 3, "the rows it moved");
    assert_eq!(chat_column(&h, "unread"), Some(1));
    assert_eq!(chat_column(&h, "read_seq"), Some(120));
    assert_eq!(
        chat_column(&h, "read_up_to"),
        message_column(&h, "120", "sort_key")
    );
    assert_eq!(chat_column(&h, "read_sent_at"), None, "no time was said");
    assert!(
        message_column(&h, "110", "expiry_started_at").is_some(),
        "reading starts its timer"
    );

    let late = h
        .apply_announcing(vec![hers("115", AT, Some(115))])
        .expect("a late row");
    assert_eq!(read_state_of(&h, "115").as_deref(), Some("Read"));
    assert!(late.notify.is_empty(), "{late:?}");
    assert_eq!(chat_column(&h, "unread"), Some(1));
    let next = h
        .apply_announcing(vec![hers("140", AT, Some(140))])
        .expect("a next row");
    assert_eq!(read_state_of(&h, "140").as_deref(), Some("Unread"));
    assert_eq!(next.notify.len(), 1, "{next:?}");

    let before = h.dump(DumpMode::Exact).expect("dump");
    for seq in [120, 90] {
        let again = h
            .apply_announcing(vec![read_up_to(seq)])
            .expect("the position");
        assert!(again.changed.is_empty(), "{seq}: {again:?}");
        assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before, "{seq}");
    }

    let chats = count(h.connection(), "SELECT count(*) FROM conversations");
    h.apply(vec![
        BackendEvent::ReadUpTo {
            conv: ConvKey::Direct(bob()),
            seq: 5,
        },
        BackendEvent::PeerReadUpTo {
            conv: ConvKey::Direct(bob()),
            seq: 5,
        },
    ])
    .expect("dropped");
    assert_eq!(
        count(h.connection(), "SELECT count(*) FROM conversations"),
        chats,
        "no chat is made by a position"
    );

    // The time rule: on rows with no position, and on nothing else of that second.
    h.apply(vec![hers("t1", AT, None), hers("t2", AT + 5_000, None)])
        .expect("rows");
    h.apply(vec![marker_on("t1")]).expect("the marker");
    assert_eq!(chat_column(&h, "read_sent_at"), Some(AT));
    assert_eq!(
        read_states(&h, &["t1", "t2", "130", "140"]),
        ["Read", "Unread", "Unread", "Unread"]
    );
    h.apply(vec![hers("t3", AT, None), hers("150", AT, Some(150))])
        .expect("rows");
    assert_eq!(
        read_states(&h, &["t3", "150"]),
        ["Read", "Unread"],
        "a new row: by time with no position, never by time with one"
    );
    // And a position does not read a row that has none.
    h.apply(vec![read_up_to(1_000)]).expect("the position");
    assert_eq!(
        read_states(&h, &["t2", "130", "140", "150"]),
        ["Unread", "Read", "Read", "Read"]
    );
    assert_eq!(chat_column(&h, "unread"), Some(1));
}

/// Test 7's call line. A line takes part: at or below the position it is read and above it unread,
/// whatever the watermark says; a line with no position goes by time; a ring that becomes missed
/// goes the same way, a position learnt late runs the rule once, and a read marker naming a line with
/// a position takes the position route. Mutants: the time rule left on for a new line
/// (`SELECT CASE`) and for a line that moves (`read_elsewhere`); no route in `read_marker`.
#[test]
fn a_call_line_with_a_position_is_read_by_it_and_not_by_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let call = |id| Call::direct(&alice(), id, CallMedia::Voice);
    let state = |h: &IngestHarness, id| read_state_of(h, &call(id).local());
    h.apply(vec![hers("t1", AT, None)]).expect("a row");
    h.apply(vec![marker_on("t1"), read_up_to(120)])
        .expect("both marks");
    assert_eq!(chat_column(&h, "read_sent_at"), Some(AT));

    h.apply(vec![
        at_seq(call(1).caller_gave_up(AT), 110),
        at_seq(call(2).caller_gave_up(AT), 130),
        call(3).caller_gave_up(AT),
    ])
    .expect("lines");
    assert_eq!(
        state(&h, 1).as_deref(),
        Some("Read"),
        "at or below the position"
    );
    assert_eq!(
        message_column(&h, &call(1).local(), "net_seq"),
        Some(110),
        "the line has its position"
    );
    assert_eq!(
        state(&h, 2).as_deref(),
        Some("Unread"),
        "above it, though it began at the watermark"
    );
    assert_eq!(state(&h, 3).as_deref(), Some("Read"), "no position: time");

    h.apply(vec![
        at_seq(call(4).ring(AT), 140),
        at_seq(call(5).ring(AT), 115),
    ])
    .expect("rings");
    assert_eq!(state(&h, 4), None, "a ring has no read state");
    h.apply(vec![
        at_seq(call(4).caller_gave_up(AT), 140),
        call(5).caller_gave_up(AT),
    ])
    .expect("missed");
    assert_eq!(state(&h, 4).as_deref(), Some("Unread"), "moved, above");
    assert_eq!(state(&h, 5).as_deref(), Some("Read"), "moved, below");

    // It began after the watermark, so time leaves it unread: then it learns where it stands.
    h.apply(vec![call(6).caller_gave_up(AT + 5_000)])
        .expect("a line");
    assert_eq!(state(&h, 6).as_deref(), Some("Unread"));
    h.apply(vec![at_seq(call(6).caller_gave_up(AT + 5_000), 100)])
        .expect("a repeat");
    assert_eq!(state(&h, 6).as_deref(), Some("Read"), "learnt late");

    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(d_alice()),
        up_to: target(Some(&d_alice()), None, &call(2).local()),
    }])
    .expect("a marker on a line");
    assert_eq!(state(&h, 2).as_deref(), Some("Read"));
    assert_eq!(state(&h, 4).as_deref(), Some("Unread"), "the same second");
    assert_eq!(chat_column(&h, "read_seq"), Some(130));
    assert_eq!(chat_column(&h, "read_sent_at"), Some(AT), "no time is said");

    h.apply(vec![read_up_to(140)]).expect("the position");
    assert_eq!(state(&h, 4).as_deref(), Some("Read"));
    assert_eq!(chat_column(&h, "unread"), Some(0));
}

/// Test 8: a `ReadMarker` naming a row with a position reads by it: a neighbour of the same second
/// stays unread and the watermark stays where it was, also for a marker that waited for its row.
/// Mutant: no route in `read_marker`.
#[test]
fn a_read_marker_naming_a_row_with_a_position_takes_the_position_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![
        hers("100", AT, Some(100)),
        hers("110", AT, Some(110)),
        hers("t0", AT, None),
    ])
    .expect("rows");
    h.apply(vec![marker_on("100")]).expect("the marker");
    assert_eq!(
        read_states(&h, &["100", "110", "t0"]),
        ["Read", "Unread", "Unread"]
    );
    assert_eq!(chat_column(&h, "read_seq"), Some(100));
    assert_eq!(chat_column(&h, "read_sent_at"), None);
    assert_eq!(
        chat_column(&h, "read_up_to"),
        message_column(&h, "100", "sort_key")
    );

    h.apply(vec![marker_on("120")]).expect("it waits");
    assert_eq!(
        count(h.connection(), "SELECT count(*) FROM pending_modifiers"),
        1
    );
    h.apply(vec![hers("120", AT, Some(120))]).expect("its row");
    assert_eq!(
        read_states(&h, &["110", "120", "t0"]),
        ["Read", "Read", "Unread"]
    );
    assert_eq!(chat_column(&h, "read_seq"), Some(120));
    assert_eq!(chat_column(&h, "read_sent_at"), None);
}

/// A row that learns its position from a later copy takes it, and the two position rules run for it,
/// also where the copy goes no further: a tombstone. Its position does not change after that. A copy
/// of another author under the same id (a collision) is ignored and teaches the row nothing (R1-L1:
/// `a_copy_under_the_key_of_another_author_teaches_the_row_no_position` holds its mutant). Mutants:
/// the late-position arm of `existing` removed; the two rules not run in `position_learnt`.
#[test]
fn a_position_learnt_late_runs_the_two_rules_for_that_row_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![
        hers("a", AT, None),
        hers("b", AT, None),
        ours("c", AT, None),
        ours("d", AT, None),
        ours("gone", AT, None),
        hers("theirs", AT, None),
    ])
    .expect("rows");
    h.apply(vec![read_up_to(200), peer_read_up_to(200)])
        .expect("the positions");
    assert_eq!(
        read_states(&h, &["a", "b"]),
        ["Unread", "Unread"],
        "a row with no position is not touched"
    );
    assert!(!is_read_by_peer(&h, "c"));

    h.apply(vec![
        hers("a", AT, Some(150)),
        hers("b", AT, Some(250)),
        ours("c", AT, Some(150)),
        ours("d", AT, Some(250)),
    ])
    .expect("copies");
    assert_eq!(read_states(&h, &["a", "b"]), ["Read", "Unread"]);
    assert!(is_read_by_peer(&h, "c"));
    assert!(!is_read_by_peer(&h, "d"));
    assert_eq!(chat_column(&h, "unread"), Some(2));

    h.apply(vec![hers("a", AT, Some(1)), hers("b", AT, Some(1))])
        .expect("another copy");
    assert_eq!(message_column(&h, "a", "net_seq"), Some(150), "only once");
    assert_eq!(read_states(&h, &["b"]), ["Unread"]);

    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&d_alice()), None, "gone"),
        by: None,
        for_everyone: true,
        at: AT,
    }])
    .expect("a tombstone");
    h.apply(vec![ours("gone", AT, Some(160))]).expect("a copy");
    assert_eq!(message_column(&h, "gone", "net_seq"), Some(160));
    assert!(is_read_by_peer(&h, "gone"));

    let mut other = remote(&d_alice(), &bob(), "theirs", AT, "not hers");
    other.net_seq = Some(170);
    h.apply(vec![BackendEvent::Message(other)])
        .expect("a collision");
    assert_eq!(
        message_column(&h, "theirs", "net_seq"),
        None,
        "not this message: it teaches the row no position"
    );
    assert_eq!(
        read_states(&h, &["theirs"]),
        ["Unread"],
        "and the read position does not reach it"
    );
}

/// Test 9: what the other side read raises our rows at or below it, a later arrival and a re-keyed one
/// too, leaves incoming rows, a call line and a notice alone, and a second marker looks only at
/// `(old, new]`: a row below the old position that the test lowered by hand stays as it is. A replay
/// and a lower position are no-ops. Mutants: `(old, new]` widened to everything; `i64::MIN` gone (the
/// first marker moves nothing).
#[test]
fn what_the_other_side_read_raises_our_rows_and_looks_only_at_what_moved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let line = Call::direct(&alice(), 1, CallMedia::Voice);
    h.apply(vec![
        ours("100", AT, Some(100)),
        ours("120", AT, Some(120)),
        ours("130", AT, Some(130)),
        hers("110", AT, Some(110)),
        at_seq(line.outgoing_answered(AT), 105),
    ])
    .expect("rows");
    let sent = out_state_of(&h, "130");
    assert!(matches!(sent, Some(OutgoingState::Sent { .. })), "{sent:?}");

    h.apply(vec![peer_read_up_to(120)]).expect("the marker");
    assert!(is_read_by_peer(&h, "100"));
    assert!(is_read_by_peer(&h, "120"), "at the position itself");
    assert_eq!(out_state_of(&h, "130"), sent, "above it");
    assert_eq!(chat_column(&h, "peer_read_seq"), Some(120));
    assert_eq!(out_state_of(&h, "110"), None, "hers has no outgoing state");
    assert_eq!(read_state_of(&h, "110").as_deref(), Some("Unread"));
    assert_eq!(out_state_of(&h, &line.local()), None, "a line was not sent");

    h.apply(vec![ours("115", AT, Some(115)), ours("125", AT, Some(125))])
        .expect("later rows");
    assert!(
        is_read_by_peer(&h, "115"),
        "at or below: read when it lands"
    );
    assert_eq!(out_state_of(&h, "125"), sent);

    // The re-key: the message waited under its temporary id, its echo says where it stands.
    let mut queued = kourier_fake::scripts::own(&d_alice(), "k1", AT, "ours");
    queued.initial_out_state = Some(OutgoingState::Queued);
    h.apply(vec![BackendEvent::Message(queued)])
        .expect("a send");
    let mut echo = kourier_fake::scripts::own(&d_alice(), "118", AT, "ours");
    echo.net_seq = Some(118);
    echo.allocated_as = Some("k1".into());
    h.apply(vec![BackendEvent::Message(echo)])
        .expect("its echo");
    assert!(is_read_by_peer(&h, "118"));

    let before = h.dump(DumpMode::Exact).expect("dump");
    for seq in [120, 90] {
        let again = h
            .apply_announcing(vec![peer_read_up_to(seq)])
            .expect("the marker");
        assert!(again.changed.is_empty(), "{seq}: {again:?}");
        assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before, "{seq}");
    }

    let lowered = serde_json::to_string(&sent).expect("json");
    h.connection()
        .execute(
            "UPDATE messages SET out_floor = ?1, out_state = ?1 WHERE net_local = '100'",
            [&lowered],
        )
        .expect("lowered by hand");
    let seen = h
        .apply_announcing(vec![peer_read_up_to(130)])
        .expect("the second marker");
    assert_eq!(out_state_of(&h, "100"), sent, "below the old position");
    assert!(is_read_by_peer(&h, "125"));
    assert!(is_read_by_peer(&h, "130"));
    assert_eq!(seen.changed.len(), 2, "what moved: {seen:?}");
}

/// Test 10: a chat never stated counts rows; a count replaces the local one, the hint beside it loses
/// and a live row does not move it; a read here zeroes it, the network is told, and it stays 0 until
/// the next statement. A plain clear leaves it, a delete of the chat zeroes it. Mutant: `coalesce`
/// order flipped in `RECOUNT`.
#[test]
fn a_count_the_server_states_stands_until_a_read_here() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![hers("1", AT, Some(1))]).expect("a row");
    assert_eq!(chat_column(&h, "unread"), Some(1), "never stated: the rows");
    assert_eq!(chat_column(&h, "unread_server"), None);

    h.apply(vec![stated(Some(5), Some(2), Some(9))])
        .expect("a statement");
    assert_eq!(chat_column(&h, "unread"), Some(5), "the server wins");
    assert_eq!(chat_column(&h, "mentions"), Some(2));
    h.apply(vec![hers("2", AT, Some(2))]).expect("a live row");
    assert_eq!(
        chat_column(&h, "unread"),
        Some(5),
        "a live row moves nothing"
    );

    let commands = h
        .mark_read(her_chat(&h))
        .expect("a read")
        .expect("something was unread");
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, DurableCommand::SyncReadMarker { .. })),
        "{commands:?}"
    );
    for column in ["unread", "mentions", "unread_server", "mentions_server"] {
        assert_eq!(chat_column(&h, column), Some(0), "{column}");
    }
    h.apply(vec![hers("3", AT, Some(3))]).expect("a live row");
    assert_eq!(
        chat_column(&h, "unread"),
        Some(0),
        "until the next statement"
    );
    h.apply(vec![stated(Some(1), None, None)])
        .expect("a statement");
    assert_eq!(chat_column(&h, "unread"), Some(1));
    assert_eq!(
        chat_column(&h, "mentions_server"),
        Some(0),
        "not stated: kept"
    );

    h.apply(vec![stated(Some(7), Some(3), None)])
        .expect("a statement");
    h.clear_conversation(her_chat(&h), false).expect("a clear");
    assert_eq!(
        chat_column(&h, "unread"),
        Some(7),
        "a plain clear leaves it"
    );
    h.clear_conversation(her_chat(&h), true).expect("a delete");
    for column in ["unread", "mentions", "unread_server", "mentions_server"] {
        assert_eq!(chat_column(&h, column), Some(0), "{column}");
    }
}

/// Test 10: count 5, no unread row: a read here gives 0 and the sync is queued, or a chat stated at 5
/// could never be cleared here. The next read has nothing to do. A count of mentions alone does the
/// same. Mutant: `local.rs` not counting a server count as unread.
#[test]
fn a_stated_count_with_no_unread_row_is_read_here_and_the_network_is_told() {
    for (unread, mentions) in [(Some(5), None), (None, Some(5))] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = flagged(&dir);
        let mut read = remote(&d_alice(), &alice(), "1", AT, "hers");
        read.net_seq = Some(1);
        read.read = true;
        h.apply(vec![BackendEvent::Message(read)]).expect("a row");
        h.apply(vec![stated(unread, mentions, None)])
            .expect("a statement");
        let commands = h
            .mark_read(her_chat(&h))
            .expect("a read")
            .expect("the count says something is unread");
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, DurableCommand::SyncReadMarker { .. })),
            "{commands:?}"
        );
        assert_eq!(chat_column(&h, "unread"), Some(0));
        assert_eq!(chat_column(&h, "mentions"), Some(0));
        assert!(
            h.mark_read(her_chat(&h)).expect("a read").is_none(),
            "nothing is left"
        );
    }
}

/// Test 10: a network whose server owns no count ignores the fields; the rows and the hint count as
/// ever. A chat that states one count of two counts the other from its rows.
#[test]
fn a_stated_count_means_nothing_to_a_network_that_owns_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        hers("1", AT, None),
        stated(Some(5), Some(2), Some(3)),
        hers("2", AT, None),
    ])
    .expect("rows");
    assert_eq!(chat_column(&h, "unread_server"), None);
    assert_eq!(chat_column(&h, "mentions_server"), None);
    assert_eq!(chat_column(&h, "unread"), Some(5), "two rows and the hint");
    assert_eq!(chat_column(&h, "mentions"), Some(0));
    h.mark_read(her_chat(&h))
        .expect("a read")
        .expect("unread rows");
    assert_eq!(chat_column(&h, "unread"), Some(0));
    assert_eq!(
        chat_column(&h, "unread_server"),
        None,
        "still nothing stated"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let mut mention = remote(&d_alice(), &alice(), "1", AT, "hers");
    mention.mentions_me = true;
    h.apply(vec![
        BackendEvent::Message(mention),
        stated(None, Some(4), None),
    ])
    .expect("rows");
    assert_eq!(chat_column(&h, "unread"), Some(1), "the rows");
    assert_eq!(chat_column(&h, "mentions"), Some(4), "the statement");
}

/// A read here: the own position rises to the newest row somebody else wrote, so a late copy of
/// something older does not come back unread, and a row above it is new. Mutant: `read_seq` not
/// raised in `local::mark_read`.
#[test]
fn a_read_here_raises_the_position_so_a_late_older_copy_stays_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![
        hers("100", AT, Some(100)),
        hers("110", AT, Some(110)),
        ours("120", AT, Some(120)),
    ])
    .expect("rows");
    h.mark_read(her_chat(&h)).expect("a read").expect("unread");
    assert_eq!(chat_column(&h, "read_seq"), Some(110), "somebody else's");
    let late = h
        .apply_announcing(vec![hers("105", AT, Some(105)), hers("115", AT, Some(115))])
        .expect("two rows");
    assert_eq!(read_states(&h, &["105", "115"]), ["Read", "Unread"]);
    assert_eq!(late.notify.len(), 1, "{late:?}");
    assert_eq!(chat_column(&h, "read_seq"), Some(110));
}

/// A read here raises the own position only to the newest row `read_up_to` would count (R1-M2): a
/// row deleted for me, or one the sweeper hides, ABOVE the newest visible row is not seen by the
/// person, so the position stays below it and a later copy between the two is not stored read.
/// Mutant: the two filters `AND (deleted IS NULL OR deleted <> 'ForMe') AND expiry_hidden = 0`
/// dropped from `local::NEWEST_READ_POSITION`.
#[test]
fn a_read_here_does_not_raise_the_position_past_a_row_nobody_saw() {
    for hidden_by in ["deleted = 'ForMe'", "expiry_hidden = 1"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = flagged(&dir);
        h.apply(vec![
            hers("100", AT, Some(100)),
            hers("110", AT, Some(110)),
            hers("120", AT, Some(120)),
        ])
        .expect("rows");
        h.connection()
            .execute(
                &format!("UPDATE messages SET {hidden_by} WHERE net_local = '120'"),
                [],
            )
            .expect("the newest row is not seen");
        h.mark_read(her_chat(&h)).expect("a read").expect("unread");
        assert_eq!(
            read_states(&h, &["100", "110"]),
            ["Read", "Read"],
            "{hidden_by}"
        );
        assert_eq!(
            chat_column(&h, "read_seq"),
            Some(110),
            "{hidden_by}: the newest row the person saw"
        );
        // A copy between the two arrives late: the person has not read it.
        let late = h
            .apply_announcing(vec![hers("115", AT, Some(115))])
            .expect("a late copy");
        assert_eq!(
            read_state_of(&h, "115").as_deref(),
            Some("Unread"),
            "{hidden_by}"
        );
        assert_eq!(late.notify.len(), 1, "{hidden_by}: {late:?}");
        assert_eq!(chat_column(&h, "read_seq"), Some(110), "{hidden_by}");
    }
}

/// A `ReadUpTo` whose position does NOT rise above `read_seq` still reads an unread row at or
/// below it (R1-M2): the row the sweeper hid while a read here passed by and that shows again is
/// read when the other session says so. Mutant: `read_rows_at` runs only when the position rose
/// (`rose > 0`) in `read_at_position`.
#[test]
fn a_read_position_that_does_not_rise_still_reads_a_row_that_was_passed_by() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![hers("100", AT, Some(100)), hers("110", AT, Some(110))])
        .expect("rows");
    h.connection()
        .execute(
            "UPDATE messages SET expiry_hidden = 1 WHERE net_local = '100'",
            [],
        )
        .expect("the sweeper hides the older row");
    h.mark_read(her_chat(&h)).expect("a read").expect("unread");
    assert_eq!(chat_column(&h, "read_seq"), Some(110));
    assert_eq!(read_states(&h, &["100", "110"]), ["Unread", "Read"]);
    // The clock is confirmed, the row shows again: unread, at a position below the one read.
    h.connection()
        .execute(
            "UPDATE messages SET expiry_hidden = 0 WHERE net_local = '100'",
            [],
        )
        .expect("it shows again");
    let seen = h
        .apply_announcing(vec![read_up_to(110)])
        .expect("the position");
    assert_eq!(read_states(&h, &["100", "110"]), ["Read", "Read"]);
    assert_eq!(seen.changed.len(), 1, "the row it moved: {seen:?}");
    assert_eq!(chat_column(&h, "unread"), Some(0));
    assert_eq!(chat_column(&h, "read_seq"), Some(110), "the position stays");
}

/// A copy of a message under a key another author holds (scope `Conversation`: a collision) is
/// ignored, and it teaches our row no position either (R1-L1): the collision return comes BEFORE
/// `position_learnt`. The same copy from the author of the row does teach it. Mutant: the
/// `position_learnt` match moved above the collision `if` in `existing`.
#[test]
fn a_copy_under_the_key_of_another_author_teaches_the_row_no_position() {
    for same_author in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = flagged(&dir);
        h.apply(vec![hers("100", AT, None)]).expect("the row");
        let author = if same_author { alice() } else { bob() };
        let mut copy = remote(&d_alice(), &author, "100", AT, "a copy");
        copy.net_seq = Some(500);
        h.apply(vec![BackendEvent::Message(copy)])
            .expect("the copy");
        assert_eq!(
            count(h.connection(), "SELECT count(*) FROM messages"),
            1,
            "{same_author}: one row"
        );
        assert_eq!(
            message_column(&h, "100", "net_seq"),
            same_author.then_some(500),
            "{same_author}: its position"
        );
    }
}

/// The chat's features where the network states no window: a delete for everyone is refused under
/// the old rule whoever sends it.
fn no_window_chat() -> BackendEvent {
    let mut features = signal_features(false);
    features.delete_receive = None;
    let mut chat = ConversationPatch::new(d_alice());
    chat.features = Some(features);
    BackendEvent::ConversationUpsert(chat)
}

fn deleted_for_everyone(author: Option<&UserId>, local: &str, by: Option<UserId>) -> BackendEvent {
    BackendEvent::Deleted {
        target: target(Some(&d_alice()), author, local),
        by,
        for_everyone: true,
        at: AT + 30 * 24 * 3600 * 1000,
    }
}

/// How the timeline states the delete of each row of her chat, by its id.
fn deletions(h: &IngestHarness) -> Vec<Option<Deletion>> {
    h.timeline(&TimelineQuery {
        conv: her_chat(h),
        anchor: TimelineAnchor::Newest,
        limit: 500,
    })
    .expect("timeline")
    .rows
    .into_iter()
    .map(|r| r.deleted)
    .collect()
}

/// Test 11: under a server that checks deletes a delete for everyone is applied with no window and
/// no author check, past any time. With nobody named it names nobody, also on our own row, where
/// "You deleted this message" could be false (`Deletion::Unattributed`, the wording is `preview.rs`'s).
/// One that names the peer is stored as hers: on our row that reads "deleted by an admin"
/// (`preview.rs` asserts it: Telegram's backend names nobody, so it does not show there). A call line
/// goes the same way, and a delete that waited applies when its message comes.
/// Mutants: `by: None` stored as `''`; the call line's arm not letting it through.
#[test]
fn a_delete_the_server_checked_is_applied_as_it_comes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let line = Call::direct(&alice(), 1, CallMedia::Voice);
    h.apply(vec![
        no_window_chat(),
        hers("h1", AT, Some(1)),
        ours("o1", AT, Some(2)),
        hers("h2", AT, Some(3)),
        ours("o2", AT, Some(4)),
        ours("o3", AT, Some(5)),
        at_seq(line.caller_gave_up(AT), 6),
    ])
    .expect("rows");
    h.apply(vec![
        deleted_for_everyone(Some(&alice()), "h1", None),
        deleted_for_everyone(None, "o1", None),
        deleted_for_everyone(Some(&alice()), "h2", Some(alice())),
        deleted_for_everyone(None, "o2", Some(alice())),
        deleted_for_everyone(None, "o3", Some(uid(ME))),
        deleted_for_everyone(None, &line.local(), None),
        // What is not here waits, and applies when it comes.
        deleted_for_everyone(Some(&alice()), "h3", None),
    ])
    .expect("the deletes");
    h.apply(vec![hers("h3", AT, Some(7))]).expect("its message");

    let named = Some(Deletion::ForEveryone { by: Some(alice()) });
    assert_eq!(
        deletions(&h),
        [
            Some(Deletion::Unattributed),
            Some(Deletion::Unattributed),
            named.clone(),
            named,
            Some(Deletion::ForEveryone { by: None }),
            Some(Deletion::Unattributed),
            Some(Deletion::Unattributed),
        ],
        "hers and ours, by nobody; hers and ours, by her; ours, by us; the line; what waited"
    );
    assert_eq!(
        count(
            h.connection(),
            "SELECT count(*) FROM messages WHERE deleted_by IS NULL AND deleted = 'ForEveryone'"
        ),
        4
    );
    assert_eq!(
        read_state_of(&h, &line.local()),
        None,
        "what is left of a line has no read state"
    );
}

/// A delete for everyone that names the message by its id alone and nobody as its deleter: all a
/// server that says only which ids went can state.
fn deleted_by_id_only(local: &str) -> BackendEvent {
    BackendEvent::Deleted {
        target: unknown_target(&d_alice(), local),
        by: None,
        for_everyone: true,
        at: AT + 30 * 24 * 3600 * 1000,
    }
}

/// Test 11, a server that says only WHICH ids went (R2-M6): the delete names no author
/// (`TargetAuthor::Unknown`) and nobody as its deleter. Where the server checks deletes it is applied
/// to her row and to ours alike, with no window, and reads `Deletion::Unattributed` on both; where
/// the server checks nothing the same two are refused (the features state no window).
/// Mutant: `canon_target` reads `TargetAuthor::Unknown` as `Own`.
#[test]
fn a_delete_that_names_only_the_id_applies_to_her_row_and_to_ours() {
    let rows = || {
        vec![
            no_window_chat(),
            hers("h1", AT, Some(1)),
            ours("o1", AT, Some(2)),
        ]
    };
    let deletes = || vec![deleted_by_id_only("h1"), deleted_by_id_only("o1")];

    let dir = tempfile::tempdir().expect("tempdir");
    let mut unchecked = harness(&dir).with_server(Flavor::SignalLike.descriptor().server);
    unchecked.apply(rows()).expect("rows");
    unchecked.apply(deletes()).expect("the deletes");
    assert_eq!(
        deletions(&unchecked),
        [None, None],
        "nobody checked them: no window, refused"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(rows()).expect("rows");
    h.apply(deletes()).expect("the deletes");
    assert_eq!(
        deletions(&h),
        [Some(Deletion::Unattributed), Some(Deletion::Unattributed)],
        "hers and ours, by nobody"
    );
    assert_eq!(
        count(
            h.connection(),
            "SELECT count(*) FROM messages WHERE deleted = 'ForEveryone' AND deleted_by IS NULL"
        ),
        2
    );
}

/// Test 11, the flag off: every line of today's rule stands. The same events on the same features are
/// refused; within a window the author's delete is applied and reads as ever, and a delete for a call
/// line and of somebody else's message are not. It runs on the rules the Signal-like flavour states
/// (the flag off, as every real network has it), not on a harness nobody gave rules.
/// Mutant: the flag check in `delete()` ignored (read as on).
#[test]
fn a_delete_the_server_did_not_check_keeps_todays_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let signal = Flavor::SignalLike.descriptor().server;
    assert!(
        !signal.validates_deletes,
        "the flag is off as the real networks have it"
    );
    let mut h = harness(&dir).with_server(signal);
    let line = Call::direct(&alice(), 1, CallMedia::Voice);
    h.apply(vec![
        no_window_chat(),
        hers("h1", AT, None),
        ours("o1", AT, None),
        line.caller_gave_up(AT),
    ])
    .expect("rows");
    h.apply(vec![
        deleted_for_everyone(Some(&alice()), "h1", None),
        deleted_for_everyone(None, "o1", None),
        deleted_for_everyone(Some(&alice()), "h1", Some(alice())),
        deleted_for_everyone(None, &line.local(), None),
    ])
    .expect("the deletes");
    assert!(
        deletions(&h).iter().all(Option::is_none),
        "no window: refused"
    );

    let mut window = ConversationPatch::new(d_alice());
    window.features = Some(signal_features(false));
    h.apply(vec![BackendEvent::ConversationUpsert(window)])
        .expect("a window");
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&d_alice()), Some(&alice()), "h1"),
        by: None,
        for_everyone: true,
        at: AT + 1,
    }])
    .expect("not hers to delete by us");
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&d_alice()), None, "o1"),
        by: None,
        for_everyone: true,
        at: AT + 1,
    }])
    .expect("our delete, in time");
    assert_eq!(
        deletions(&h),
        [None, Some(Deletion::ForEveryone { by: None }), None],
        "only our own, which reads as ever (`deleted_by = ''`)"
    );
    h.apply(vec![deleted_for_everyone(None, &line.local(), None)])
        .expect("a line");
    assert_eq!(read_state_of(&h, &line.local()).as_deref(), Some("Unread"));
}

/// The cost on every other network (test 12): a read of a chat of 3,000 rows runs the new statement
/// as one probe of an index with nothing in it: as many VM steps as for a chat of one row, none of
/// them a step of a full scan. The plan is `the_index_of_the_positions_takes_nothing...`'s.
#[test]
fn a_read_on_an_account_that_numbers_nothing_costs_one_probe_of_an_empty_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([9; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let many: Vec<BackendEvent> = (0..3_000)
        .map(|i| {
            BackendEvent::Message(remote(&d_alice(), &alice(), &i.to_string(), AT + i, "hers"))
        })
        .collect();
    h.apply(many).expect("many rows");
    h.apply(vec![BackendEvent::Message(remote(
        &ConvKey::Direct(bob()),
        &bob(),
        "one",
        AT,
        "his",
    ))])
    .expect("one row");
    assert_eq!(
        count(
            h.connection(),
            "SELECT count(*) FROM messages WHERE net_seq IS NOT NULL"
        ),
        0,
        "the index is empty"
    );
    let (_, sql) = hot_statements()
        .into_iter()
        .find(|(name, _)| *name == "what a read here reaches")
        .expect("the statement");
    let cost = |key: &ConvKey| -> (i32, i32) {
        let conv = column_of(&h, key, "id").expect("the chat");
        let mut q = h.connection().prepare(&sql).expect("prepare");
        let seq: Option<i64> = q
            .query_row([conv], |r| r.get(0))
            .optional()
            .expect("the look");
        assert_eq!(seq, None);
        (
            q.get_status(StatementStatus::VmStep),
            q.get_status(StatementStatus::FullscanStep),
        )
    };
    let (big, scanned) = cost(&d_alice());
    let (small, _) = cost(&ConvKey::Direct(bob()));
    assert_eq!(big, small, "the length of the chat costs nothing");
    assert_eq!(scanned, 0);

    h.mark_read(her_chat(&h)).expect("a read").expect("unread");
    assert_eq!(chat_column(&h, "unread"), Some(0));
    assert_eq!(chat_column(&h, "read_seq"), None, "no position to reach");
}

/// A merge of two chats: the higher of each position, and a pass over the rows, so the unread rows of
/// either side at or below the own position go read and our rows of either side at or below the
/// peer's go `Read`. Both counts stated: the sum; one: that one. Mutants: the pass removed; the `max`.
#[test]
fn a_merge_takes_the_higher_positions_and_the_rows_of_both_sides_follow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    let alias = uid("pni:00000000-0000-4000-8000-000000000b0b");
    let chat = ConvKey::Direct(bob());
    let other = ConvKey::Direct(alias.clone());
    let row = |conv: &ConvKey, by: Option<&UserId>, local: &str, seq| {
        let mut m = match by {
            Some(by) => remote(conv, by, local, AT, "text"),
            None => kourier_fake::scripts::own(conv, local, AT, "text"),
        };
        m.net_seq = Some(seq);
        BackendEvent::Message(m)
    };
    let counts = |conv: &ConvKey, unread, mentions| {
        let mut p = ConversationPatch::new(conv.clone());
        p.server_unread = unread;
        p.server_mentions = mentions;
        BackendEvent::ConversationUpsert(p)
    };
    h.apply(vec![
        row(&chat, Some(&bob()), "b100", 100),
        row(&chat, None, "b150", 150),
        row(&other, Some(&alias), "p110", 110),
        row(&other, Some(&alias), "p130", 130),
        row(&other, None, "p190", 190),
        row(&other, None, "p210", 210),
        BackendEvent::ReadUpTo {
            conv: chat.clone(),
            seq: 120,
        },
        BackendEvent::ReadUpTo {
            conv: other.clone(),
            seq: 90,
        },
        BackendEvent::PeerReadUpTo {
            conv: chat.clone(),
            seq: 140,
        },
        BackendEvent::PeerReadUpTo {
            conv: other.clone(),
            seq: 200,
        },
        counts(&chat, Some(3), None),
        counts(&other, Some(4), Some(1)),
    ])
    .expect("two chats");
    assert_eq!(read_state_of(&h, "p110").as_deref(), Some("Unread"));
    assert!(!is_read_by_peer(&h, "b150"));

    h.apply(vec![BackendEvent::IdentityMerged {
        from: alias.clone(),
        into: bob(),
    }])
    .expect("the merge");
    assert_eq!(
        count(h.connection(), "SELECT count(*) FROM conversations"),
        1
    );
    assert_eq!(column_of(&h, &chat, "read_seq"), Some(120));
    assert_eq!(column_of(&h, &chat, "peer_read_seq"), Some(200));
    assert_eq!(read_states(&h, &["p110", "p130"]), ["Read", "Unread"]);
    assert!(
        is_read_by_peer(&h, "b150"),
        "our row, the other side's marker"
    );
    assert!(is_read_by_peer(&h, "p190"));
    assert!(!is_read_by_peer(&h, "p210"));
    assert_eq!(column_of(&h, &chat, "unread"), Some(7), "the sum");
    assert_eq!(column_of(&h, &chat, "mentions"), Some(1), "the one stated");
}

// ---------- test 14: the facts about one own message, in every order ----------

/// The id the own message waited under, and the one the server gave it.
const WAITED: &str = "k1";
const SERVED: &str = "118";

/// The own message as the outbox leaves it: queued, under the id it was allocated.
fn waiting_message() -> BackendEvent {
    let mut m = IncomingMessage::new(
        d_alice(),
        WAITED,
        Origin::OwnThisDevice,
        kourier_model::Body::text("ours"),
        AT,
        AT,
    );
    m.initial_out_state = Some(OutgoingState::Queued);
    BackendEvent::Message(m)
}

/// Eight facts the server tells about it, each its own delivery: the echo under the server's id (118)
/// naming the allocated one, the own read position and the peer's (both 120, so the message is below
/// the peer's), a reaction and a delete that name the server's id, an incoming message AT the own
/// position (120) and one ABOVE it (130), and the count the server states for the chat.
fn the_facts() -> Vec<Vec<BackendEvent>> {
    let mut echo = IncomingMessage::new(
        d_alice(),
        SERVED,
        Origin::OwnThisDevice,
        kourier_model::Body::text("ours"),
        AT,
        AT + 500,
    );
    echo.server_at = Some(AT + 500);
    echo.net_seq = Some(118);
    echo.allocated_as = Some(WAITED.into());
    vec![
        vec![BackendEvent::Message(echo)],
        vec![read_up_to(120)],
        vec![peer_read_up_to(120)],
        vec![BackendEvent::Reaction {
            target: target(Some(&d_alice()), None, SERVED),
            by: Some(alice()),
            emoji: "\u{1f44d}".into(),
            remove: false,
            at: AT + 10,
        }],
        vec![deleted_for_everyone(None, SERVED, None)],
        vec![hers("120", AT, Some(120))],
        vec![hers("130", AT, Some(130))],
        vec![stated(Some(1), Some(0), None)],
    ]
}

/// The chat after the waiting message and then the facts in `order`, those of `again` (a fact, the
/// place) delivered a second time as the any-order helpers of `ingest.rs` redeliver.
fn settled(order: &[usize], again: &[(usize, usize)]) -> (tempfile::TempDir, IngestHarness) {
    let facts = the_facts();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = flagged(&dir);
    h.apply(vec![waiting_message()]).expect("the message waits");
    let mut run = order.to_vec();
    for (src, at) in again {
        let copy = run[src % run.len()];
        let at = (*at % (run.len() + 1)).min(run.len());
        run.insert(at, copy);
    }
    for i in run {
        h.apply(facts[i].clone()).expect("a fact");
    }
    (dir, h)
}

/// The dump the existing any-order tests compare: `OrderInsensitive` leaves out what an arrival order
/// may legitimately decide, the live rows' `sort_key` and what derives from it (`last_activity`,
/// `read_up_to`), and nothing else. Every field this test's facts reach is compared.
fn settled_dump(order: &[usize], again: &[(usize, usize)]) -> String {
    settled(order, again)
        .1
        .dump(DumpMode::OrderInsensitive)
        .expect("dump")
}

/// The lines that only one of the two dumps has, `-` the expected one's, `+` the other's.
fn differing_lines(expected: &str, got: &str) -> String {
    let e: std::collections::BTreeSet<&str> = expected.lines().collect();
    let g: std::collections::BTreeSet<&str> = got.lines().collect();
    let mut out = String::new();
    for l in e.difference(&g) {
        out.push_str(&format!("- {l}\n"));
    }
    for l in g.difference(&e) {
        out.push_str(&format!("+ {l}\n"));
    }
    out
}

/// The `k`th arrangement of `0..n` in lexicographic order.
fn arrangement(n: usize, mut k: usize) -> Vec<usize> {
    let mut left: Vec<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n);
    let mut weight: usize = (1..n).product();
    for rest in (1..=n).rev() {
        out.push(left.remove(k / weight));
        k %= weight;
        if rest > 1 {
            weight /= rest - 1;
        }
    }
    out
}

/// What the facts end in, whatever the order (asserted on the canonical order, so that the equality
/// of the dumps below cannot be the equality of one wrong state).
#[track_caller]
fn assert_the_end_state(h: &IngestHarness) {
    let c = h.connection();
    assert_eq!(count(c, "SELECT count(*) FROM messages"), 3, "three rows");
    assert_eq!(
        count(c, "SELECT count(*) FROM messages WHERE net_author = ''"),
        1,
        "the echo and the waiting message are one row"
    );
    assert_eq!(
        count(
            c,
            &format!("SELECT count(*) FROM messages WHERE net_local = '{WAITED}'")
        ),
        0,
        "nothing is left under the old id"
    );
    assert_eq!(
        count(
            c,
            &format!(
                "SELECT count(*) FROM message_aliases a JOIN messages m ON m.id = a.message_id
                 WHERE a.net_local = '{WAITED}' AND m.net_local = '{SERVED}'"
            )
        ),
        1,
        "the old id is another name of it"
    );
    assert_eq!(message_column(h, SERVED, "net_seq"), Some(118));
    let (deleted, by): (Option<String>, Option<String>) = c
        .query_row(
            &format!("SELECT deleted, deleted_by FROM messages WHERE net_local = '{SERVED}'"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the row");
    assert_eq!(
        (deleted.as_deref(), by),
        (Some("ForEveryone"), None),
        "the delete that named the server's id, by nobody"
    );
    assert!(is_read_by_peer(h, SERVED), "below the peer's position");
    assert_eq!(read_states(h, &["120", "130"]), ["Read", "Unread"]);
    assert_eq!(chat_column(h, "read_seq"), Some(120));
    assert_eq!(chat_column(h, "peer_read_seq"), Some(120));
    assert_eq!(chat_column(h, "unread_server"), Some(1));
    assert_eq!(
        chat_column(h, "unread"),
        Some(1),
        "the count the server stated, not the rows"
    );
}

/// Test 14: the facts about one own message under the rules of the fake `TelegramLike` end in the same
/// dump in every order they can arrive in: the echo that re-keys the waiting message, the two read
/// positions, a reaction and a delete under the server's id (each waits for the re-key when it comes
/// first), an incoming message at and above the read position, and the stated count. Of the 8! =
/// 40,320 orders the canonical one, the last and every 271st (150 in all) run here, and
/// `..converge_when_some_come_twice` runs 64 random ones with some facts delivered twice (the whole
/// set takes 36 s, too long for the regular run).
///
/// Compared as the existing any-order tests compare (`DumpMode::OrderInsensitive`): the live rows'
/// `sort_key`, `last_activity` and `read_up_to` are left out because they follow the arrival order
/// (section 3.9). NOTHING ELSE is left out: the facts reach no other field that an arrival order may
/// decide (the echo and the waiting message state one `sent_at`, the harness has one clock, and the
/// echo states the `server_at` the outcome would). The canonical order's end state is asserted by
/// hand, so equal dumps are not the equality of one wrong state, and one order is played to look at
/// the wait itself: the reaction and the delete park behind the echo and are gone when it comes.
#[test]
fn the_facts_about_one_own_message_end_in_one_dump_in_every_order() {
    let canonical: Vec<usize> = (0..8).collect();
    let (_dir, h) = settled(&canonical, &[]);
    assert_the_end_state(&h);
    let expected = h.dump(DumpMode::OrderInsensitive).expect("dump");
    let reactions = count(h.connection(), "SELECT count(*) FROM reactions");

    // The wait: what names the server's id before the message has it is parked, and the echo
    // applies it (a dump equal to the canonical one is no proof that anything waited).
    let (_dir, mut h) = settled(&[3, 4], &[]);
    let parked =
        |h: &IngestHarness| count(h.connection(), "SELECT count(*) FROM pending_modifiers");
    assert_eq!(parked(&h), 2, "the reaction and the delete wait");
    h.apply(the_facts()[0].clone()).expect("the echo");
    assert_eq!(parked(&h), 0, "the re-key applied them");
    assert_eq!(
        count(h.connection(), "SELECT count(*) FROM reactions"),
        reactions,
        "and the reaction is where the canonical order left it"
    );

    let total: usize = (1..=8).product();
    let mut sample: Vec<usize> = (0..total).step_by(271).collect();
    sample.push(total - 1);
    let mut bad = String::new();
    let mut failed = 0;
    for k in sample {
        let order = arrangement(8, k);
        let got = settled_dump(&order, &[]);
        if got != expected {
            failed += 1;
            if failed <= 5 {
                bad.push_str(&format!(
                    "order {order:?}\n{}\n",
                    differing_lines(&expected, &got)
                ));
            }
        }
    }
    assert!(failed == 0, "{failed} orders differ:\n{bad}");
}

use proptest::prelude::{Just, ProptestConfig, Strategy, prop_assert, proptest};

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Test 14, with redeliveries: any order, some facts twice (a forwarder that crashed before it
    /// deleted its row).
    #[test]
    fn the_facts_about_one_own_message_converge_when_some_come_twice(
        order in Just((0..8).collect::<Vec<usize>>()).prop_shuffle(),
        again in proptest::collection::vec((0..8usize, 0..=8usize), 0..4),
    ) {
        let expected = settled_dump(&(0..8).collect::<Vec<usize>>(), &[]);
        let got = settled_dump(&order, &again);
        prop_assert!(
            got == expected,
            "order {:?} again {:?}\n{}",
            order,
            again,
            differing_lines(&expected, &got)
        );
    }
}
