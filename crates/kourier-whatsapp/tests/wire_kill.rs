//! W3a (chunk C10): the KILL LOOPS over the scripted server. A real client (Kourier's builder, the
//! durability hook, the one handler) receives six encrypted messages of a fictitious peer, three
//! inside an offline drain and three live, and the process is ABORTED at a failpoint (a power cut:
//! `KOURIER_FAILPOINT=<name>:<n>`, n = 1, 2, ... until the child ends normally). A second child
//! plays the server's redelivery: it sends again every stanza the first child had not answered with
//! a receipt (the `wire.log` the Wire fsyncs per frame), ends the drain and lets stage 2 run.
//!
//! What every iteration asserts, with the parent reading `backend.db` between and after the children:
//!  - an ACKNOWLEDGED message is durable at the moment of the kill (its receipt is in the log, its
//!    row is in the inbox): nothing was acked before it was durable;
//!  - after the second child, each message is in `core_outbox` exactly once (decoded, by id), every
//!    inbox row is `done`, and no pending row is left: nothing is lost and nothing doubles.
//!
//! The points are the ones the critic named (H3): the inbox and stage-2 zones, upstream's own
//! writes around the hook (the pending row's store and its compare-and-swap, the Signal cache's
//! `sessions_batch`), and the first stanza carries a one-time prekey, so the prekey removal is in
//! the same write. The hook-level twins without a client are `src/receive_kill.rs`.
//!
//! The HANDLER loop is the same shape for the other durable kind: six `<receipt>` stanzas of the
//! peer (plain, nothing to decrypt), the process aborted inside and after the handler's group commit
//! (`wa_inside_event_write` / `wa_after_event_write`), the acknowledgement being upstream's
//! `<ack id=..>` frame. An acknowledged receipt is in `core_outbox` at the kill; after the recovery
//! child every receipt is there at least once (a receipt written but not yet acknowledged when the
//! process died is delivered again, and core ingest is idempotent), and exactly once when it was
//! acknowledged.
//!
//! Every child runs under `unshare -rn` over the scripted server (decision L3); nothing here touches
//! a network. The stanzas are prebuilt ONCE by the parent (the peer's session advances with every
//! encryption) and handed to the children as plain data, as a server would hold them.

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Command, Exit, StartMode, decode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::Connection;
use kourier_whatsapp::failpoints as fp;
use kourier_whatsapp::{StoreOwner, fault};
use wacore::libsignal::protocol::{KeyPair, PreKeyRecord};
use wacore::store::traits::{
    DeviceInfo, DeviceListRecord, DeviceStore, ProtocolStore, SignalStore,
};
use wacore_binary::Node;
use wacore_binary::marshal::{marshal, unmarshal_packed_ref};

use crate::receive::peer_receipt;
use crate::support::peer::{self, Peer};
use crate::support::wire;
use crate::support::{self, db_path, key};
use crate::wire::start_actor;

const ROLE: &str = "WA_WK_ROLE";
const DIR: &str = "WA_WK_DIR";
const STANZAS: &str = "WA_WK_STANZAS";
/// The most hits of one failpoint the six messages make. A loop that goes past it is a bug.
const MAX_HITS: u32 = 24;
/// The id of the one-time prekey the first stanza names.
const PREKEY_ID: u32 = 7001;

/// The ids of the peer's receipts of the handler loop (read receipts for messages of the account).
const RECEIPTS: [&str; 6] = [
    "3EB000000000B001",
    "3EB000000000B002",
    "3EB000000000B003",
    "3EB000000000B004",
    "3EB000000000B005",
    "3EB000000000B006",
];

/// The stanzas' ids, drain first: `(id, offline)`. Three held by the server while the device was
/// away, three that arrive live.
const MESSAGES: [(&str, bool); 6] = [
    ("3EB000000000A001", true),
    ("3EB000000000A002", true),
    ("3EB000000000A003", true),
    ("3EB000000000A004", false),
    ("3EB000000000A005", false),
    ("3EB000000000A006", false),
];

// --- the parent's preparation ---

/// A paired account directory with the peer's device known to it and one one-time prekey in the
/// store (the one the first stanza names), closed.
async fn template() -> (tempfile::TempDir, wacore::store::Device, (u32, KeyPair)) {
    let dir = tempfile::tempdir().expect("temp dir");
    let device = peer::account_device();
    let one_time = KeyPair::generate(&mut rand::make_rng::<rand::rngs::StdRng>());
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    let store = owner.store();
    store.save(&device).await.expect("the paired device");
    let record = PreKeyRecord::new(PREKEY_ID.into(), &one_time)
        .serialize()
        .expect("the prekey record");
    store
        .store_prekey(PREKEY_ID, &record, false)
        .await
        .expect("the one-time prekey");
    store
        .update_device_list(DeviceListRecord {
            user: std::sync::Arc::from(peer::PEER_LID),
            devices: vec![DeviceInfo::new(0, None)].into_boxed_slice(),
            timestamp: 1_700_000_000,
            phash: None,
            raw_id: None,
        })
        .await
        .expect("the peer's devices");
    owner.close();
    (dir, device, (PREKEY_ID, one_time))
}

/// The six stanzas, encrypted once, as one file: for each stanza its packed bytes behind a four-byte
/// big-endian length.
async fn prebuild(device: &wacore::store::Device, one_time: (u32, KeyPair), file: &Path) {
    let mut peer = Peer::new(device, Some(one_time)).await;
    let mut out = Vec::new();
    for (id, offline) in MESSAGES {
        let mut node = peer.text_with_id(&format!("synthetic {id}"), id).await;
        if offline {
            node.attrs.insert("offline", "1");
        }
        let bytes = marshal(&node).expect("the stanza marshals");
        out.extend_from_slice(&u32::try_from(bytes.len()).expect("small").to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    std::fs::write(file, out).expect("write the stanzas");
}

/// The id of a stanza.
fn id_of(node: &Node) -> String {
    node.attrs
        .get("id")
        .map(|v| v.as_str().into_owned())
        .unwrap_or_default()
}

fn read_stanzas(file: &Path) -> Vec<Node> {
    let bytes = std::fs::read(file).expect("read the stanzas");
    let mut nodes = Vec::new();
    let mut rest = bytes.as_slice();
    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest[..4].try_into().expect("length")) as usize;
        let packed = &rest[4..4 + len];
        nodes.push(
            unmarshal_packed_ref(packed)
                .expect("the stanza unpacks")
                .to_owned(),
        );
        rest = &rest[4 + len..];
    }
    nodes
}

/// Copies a directory tree (the template's files; no links).
pub(crate) fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

// --- what a log and a database say ---

/// The ids the clients answered with a DELIVERY `<receipt>` (the stanza's acknowledgement), from the Wire's log: the
/// receipt's own `id` and the `items` of a receipt that answers several messages at once (the offline drain's are
/// batched). Upstream types its delivery receipts by the stanza: none (`-`), `inactive` (a client that is not active:
/// the live messages of these children), `sender` (our own message from another device) or `peer_msg`
/// (`receipt.rs` `delivery_receipt_type`); any other type is NOT an answer (fix F19): a `retry` receipt asks the sender
/// to send the message again, and a stanza only retried is not delivered (the strike of the Telegram lane's run could
/// not tell the two apart).
fn receipted(log: &Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for (kind, receipt_ids) in receipts_in(log) {
        if matches!(kind.as_str(), "-" | "inactive" | "sender" | "peer_msg") {
            ids.extend(receipt_ids);
        }
    }
    ids.remove("-");
    ids
}

/// Every `<receipt>` the clients wrote, from the Wire's log: its `type` (`-` for a delivery receipt) and the ids it
/// names (its own `id` and its `items`).
fn receipts_in(log: &Path) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for line in support::kill_log::lines(log) {
        let mut words = line.split_whitespace();
        if words.next() != Some("receipt") {
            continue;
        }
        let mut kind = "-".to_owned();
        let mut ids = Vec::new();
        for word in words {
            if let Some(t) = word.strip_prefix("type=") {
                kind = t.to_owned();
            } else if let Some(id) = word.strip_prefix("id=") {
                ids.push(id.to_owned());
            } else if let Some(items) = word.strip_prefix("items=") {
                ids.extend(items.split(',').map(str::to_owned));
            }
        }
        found.push((kind, ids));
    }
    found
}

/// What a stuck child prints at its deadline (fix F19), COUNTS AND STATES ONLY (no id, no JID): per stanza of this
/// run its position and the types of the receipts written for it (`-` is a delivery receipt), then the inbox rows
/// that are not `done` (state, attempts, milliseconds until `parked_until`) and the number of pending-inbound rows
/// upstream still holds. A strike names its cause from this: a stanza only retried (a ratchet that advanced in the
/// killed child), a row parked by stage 2's backoff (`PARK_MIN` 5 s doubling: 5 + 10 + 20 s is past the child's 30 s),
/// or a pending replay that never ran.
fn stuck_report(log: &Path, stanzas: &[Node], conn: &Connection) -> String {
    let receipts = receipts_in(log);
    let mut report = String::new();
    for (i, node) in stanzas.iter().enumerate() {
        let id = id_of(node);
        let types: Vec<&str> = receipts
            .iter()
            .filter(|(_, ids)| ids.contains(&id))
            .map(|(kind, _)| kind.as_str())
            .collect();
        report.push_str(&format!("\n  stanza #{i}: receipts {types:?}"));
    }
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(i64::MAX);
    let mut stmt = conn
        .prepare("SELECT state, attempts, parked_until FROM inbox WHERE state != 'done' ORDER BY arrival")
        .expect("prepare");
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .and_then(|rows| rows.collect())
        .expect("the rows that are not done");
    for (state, attempts, parked_until) in rows {
        report.push_str(&format!(
            "\n  inbox row not done: state {state}, attempts {attempts}, parked for {} ms",
            (parked_until - now).max(0)
        ));
    }
    let pending = count(conn, "SELECT count(*) FROM pending_inbound_messages");
    report.push_str(&format!(
        "\n  pending-inbound rows upstream holds: {pending}"
    ));
    report
}

/// Fix F19: only a DELIVERY receipt (none, `inactive`, `sender` or `peer_msg`) answers a stanza, a retry receipt does not, and a stuck child's report
/// names counts and states and nothing that identifies a message. Mutants: a typed receipt counts; the report names an
/// id; the report leaves out the row's state.
#[test]
fn only_a_delivery_receipt_answers_a_stanza_and_the_stuck_report_holds_counts_and_states() {
    use wacore_binary::builder::NodeBuilder;
    const FIRST: &str = "3EB0A1A1A1A1A1A1";
    const RETRIED: &str = "3EB0B2B2B2B2B2B2";
    const BATCHED: &str = "3EB0C3C3C3C3C3C3";
    const SILENT: &str = "3EB0D4D4D4D4D4D4";
    const READ: &str = "3EB0F6F6F6F6F6F6";
    let dir = tempfile::tempdir().expect("dir");
    let log = dir.path().join("wire.log");
    std::fs::write(
        &log,
        format!(
            "receipt type=- class=- xmlns=- id={FIRST} items=-\n\
             receipt type=retry class=- xmlns=- id={RETRIED} items=-\n\
             receipt type=- class=- xmlns=- id=- items={BATCHED}\n\
             receipt type=inactive class=- xmlns=- id={SILENT} items=-\n\
             receipt type=read class=- xmlns=- id={READ} items=-\n\
             message type=text class=- xmlns=- id=3EB0E5E5E5E5E5E5 items=-\n"
        ),
    )
    .expect("the log");
    assert_eq!(
        receipted(&log),
        BTreeSet::from([FIRST.to_owned(), BATCHED.to_owned(), SILENT.to_owned()]),
        "a retry or a read receipt answered a stanza, or an `inactive` or a batched one did not"
    );
    let stanza = |id: &str| NodeBuilder::new("message").attr("id", id).build();
    let stanzas = [stanza(FIRST), stanza(RETRIED)];
    let conn = Connection::open_in_memory().expect("memory");
    conn.execute_batch(
        "CREATE TABLE inbox(arrival INTEGER PRIMARY KEY, state TEXT, attempts INTEGER, parked_until INTEGER);\
         CREATE TABLE pending_inbound_messages(id TEXT);\
         INSERT INTO inbox VALUES (1, 'done', 0, 0), (2, 'new', 3, 0);\
         INSERT INTO pending_inbound_messages VALUES ('x');",
    )
    .expect("the tables");
    let report = stuck_report(&log, &stanzas, &conn);
    assert!(report.contains("stanza #0: receipts [\"-\"]"), "{report}");
    assert!(
        report.contains("stanza #1: receipts [\"retry\"]"),
        "{report}"
    );
    assert!(
        report.contains("inbox row not done: state new, attempts 3"),
        "{report}"
    );
    assert!(
        report.contains("pending-inbound rows upstream holds: 1"),
        "{report}"
    );
    for id in [FIRST, RETRIED, BATCHED, SILENT, READ] {
        assert!(!report.contains(id), "the report names a message id");
    }
}

fn reopen(dir: &Path) -> Connection {
    kourier_db::open(&db_path(dir), &key()).expect("reopen")
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

/// The ids of the `Message` events in `core_outbox`, sorted (each message once means each id once).
fn outbox_messages(conn: &Connection) -> Vec<String> {
    let mut ids = Vec::new();
    for row in outbox::peek(conn, 10_000).expect("peek the outbox") {
        for event in decode_outbox_row(&row.envelope).expect("decodes") {
            if let BackendEvent::Message(m) = event {
                ids.push(m.local.to_string());
            }
        }
    }
    ids.sort();
    ids
}

/// An acknowledged message is durable: a receipt left only for a message that is in the inbox.
fn assert_acked_are_durable(dir: &Path, what: &str) {
    let conn = reopen(dir);
    for id in receipted(&dir.join("wire.log")) {
        let rows = conn
            .query_row("SELECT count(*) FROM inbox WHERE id = ?1", [&id], |r| {
                r.get::<_, i64>(0)
            })
            .expect("count rows");
        assert_eq!(
            rows, 1,
            "{what}: message {id} was acknowledged, but it is not in the inbox"
        );
    }
}

/// The end state: each message once in `core_outbox`, every inbox row done, nothing pending.
fn assert_final(dir: &Path, what: &str) {
    let conn = reopen(dir);
    let expected: Vec<String> = MESSAGES.iter().map(|(id, _)| (*id).to_owned()).collect();
    assert_eq!(
        outbox_messages(&conn),
        expected,
        "{what}: every message is in core_outbox exactly once"
    );
    let total = i64::try_from(MESSAGES.len()).expect("small");
    assert_eq!(
        (
            count(&conn, "SELECT count(*) FROM inbox WHERE state = 'new'"),
            count(&conn, "SELECT count(*) FROM inbox WHERE state = 'done'"),
        ),
        (0, total),
        "{what}: every inbox row is done"
    );
    // Upstream's own probe of the store (`build()` writes a row with a random id and deletes it) is not
    // a message: a kill inside its cleanup leaves it, and upstream's 7-day sweep takes it (D7).
    let left: Vec<String> = conn
        .prepare(
            "SELECT id FROM pending_inbound_messages WHERE id NOT LIKE '__wa_durability_probe_%' \
             ORDER BY id",
        )
        .expect("prepare")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert!(
        left.is_empty(),
        "{what}: no pending row is left, but {left:?} stay (receipted in all: {:?})",
        receipted(&dir.join("wire.log"))
    );
}

/// The ids the clients ACKNOWLEDGED with an `<ack>` frame (upstream's answer to a stanza whose
/// handler returned), from the Wire's log.
fn acked(log: &Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for line in support::kill_log::lines(log) {
        let mut words = line.split_whitespace();
        if words.next() != Some("ack") {
            continue;
        }
        for word in words {
            if let Some(id) = word.strip_prefix("id=") {
                ids.insert(id.to_owned());
            }
        }
    }
    ids.remove("-");
    ids
}

/// How many `Receipts` events `core_outbox` holds per message id they name.
fn receipt_counts(conn: &Connection) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for row in outbox::peek(conn, 10_000).expect("peek the outbox") {
        for event in decode_outbox_row(&row.envelope).expect("decodes") {
            if let BackendEvent::Receipts { targets, .. } = event {
                for target in targets {
                    *counts.entry(target.local.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    counts
}

/// An acknowledged receipt is durable: upstream sent its `<ack>` only for a receipt whose event is
/// in `core_outbox`.
fn assert_acked_receipts_are_durable(dir: &Path, what: &str) {
    let counts = receipt_counts(&reopen(dir));
    for id in acked(&dir.join("wire.log")) {
        assert!(
            counts.get(&id).copied().unwrap_or(0) >= 1,
            "{what}: receipt {id} was acknowledged, but it is not in core_outbox"
        );
    }
}

/// The end state of the handler loop: every receipt is in `core_outbox`, exactly once when it was
/// acknowledged before the kill (`acked_at_kill`; every receipt after a run without a kill), at most
/// twice otherwise (written, not acknowledged, delivered again), and nothing else is there.
fn assert_final_receipts(dir: &Path, what: &str, acked_at_kill: &BTreeSet<String>) {
    let counts = receipt_counts(&reopen(dir));
    let expected: BTreeSet<String> = RECEIPTS.iter().map(|id| (*id).to_owned()).collect();
    assert_eq!(
        counts.keys().cloned().collect::<BTreeSet<_>>(),
        expected,
        "{what}: every receipt is in core_outbox, and nothing else"
    );
    for (id, n) in &counts {
        let most = if acked_at_kill.contains(id) { 1 } else { 2 };
        assert!(
            (1..=most).contains(n),
            "{what}: receipt {id} is in core_outbox {n} times (at most {most})"
        );
    }
    assert_eq!(
        acked(&dir.join("wire.log"))
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected,
        "{what}: every receipt was acknowledged in the end"
    );
}

// --- the child ---

/// The child (a no-op as an ordinary test): the client over the Wire, one role of the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_kill_child() {
    if !support::offline::in_child() {
        return;
    }
    let dir = PathBuf::from(std::env::var(DIR).expect("the parent names the directory"));
    let stanzas = read_stanzas(&PathBuf::from(
        std::env::var(STANZAS).expect("the parent names the stanzas"),
    ));
    let role = std::env::var(ROLE).expect("the parent names the role");
    let log = dir.join("wire.log");
    // What an earlier run already answered is not delivered again (the server's queue is empty of it).
    let answered = if role == "recover" {
        receipted(&log)
    } else {
        BTreeSet::new()
    };
    let todo: Vec<&Node> = stanzas
        .iter()
        .filter(|n| !answered.contains(&id_of(n)))
        .collect();
    let (drain, live): (Vec<&Node>, Vec<&Node>) = todo
        .iter()
        .copied()
        .partition(|n| n.attrs.contains_key("offline"));

    let server = wire::server();
    server.answering();
    server.hold_offline_end();
    server.log_to(&log);
    let actor = start_actor(&dir, StartMode::Resume, &server);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success(peer::ACCOUNT_LID));
    for node in &drain {
        server.send(node);
    }
    server.send(&wire::offline_end(
        u32::try_from(drain.len()).expect("small"),
    ));
    // The live messages one at a time, each behind the receipt of the one before: every one is its own
    // hook batch and its own stage-2 step (stanzas of one sender sent together are ONE batch, so a
    // kill would leave all of them or none), and a kill between two of them leaves some acknowledged
    // and some not.
    for node in &live {
        server.send(node);
        let id = id_of(node);
        let limit = Instant::now() + Duration::from_secs(20);
        while !receipted(&log).contains(&id) {
            assert!(
                Instant::now() < limit,
                "no receipt for the live message {id}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Every stanza of this run is answered, and stage 2 has turned every inbox row into events.
    let deadline = Instant::now() + Duration::from_secs(30);
    let raw = reopen(&dir);
    let total = i64::try_from(stanzas.len()).expect("small");
    loop {
        let answered_now = receipted(&log);
        let all_answered = stanzas.iter().all(|n| answered_now.contains(&id_of(n)));
        let done = count(&raw, "SELECT count(*) FROM inbox WHERE state = 'done'");
        if all_answered && done == total {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the child did not finish: {} of {total} stanzas answered by a delivery receipt, done {done} of {total}{}",
            answered_now.len(),
            stuck_report(&log, &stanzas, &raw)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(raw);
    let mut actor = actor;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// The child of the handler loop (a no-op as an ordinary test): the client over the Wire, online,
/// and the peer's receipts; each is delivered once and the child ends when every one it delivered was
/// acknowledged. The `recover` role delivers only the receipts the first child had not acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_kill_receipts_child() {
    if !support::offline::in_child() {
        return;
    }
    let dir = PathBuf::from(std::env::var(DIR).expect("the parent names the directory"));
    let role = std::env::var(ROLE).expect("the parent names the role");
    let log = dir.join("wire.log");
    let answered = if role == "recover" {
        acked(&log)
    } else {
        BTreeSet::new()
    };
    let todo: Vec<&str> = RECEIPTS
        .iter()
        .copied()
        .filter(|id| !answered.contains(*id))
        .collect();

    let server = wire::server();
    server.answering();
    server.log_to(&log);
    let actor = start_actor(&dir, StartMode::Resume, &server);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    // No wait for `Online`: after a kill the rows the first child committed stay in `core_outbox`
    // (nobody here acknowledges them), and the catch-up machine keeps the account `Syncing` until the
    // core has them. The handler does not depend on it.
    server.send(&wire::success(peer::ACCOUNT_LID));
    for id in &todo {
        server.send(&peer_receipt(id, "read"));
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let answered_now = acked(&log);
        if todo.iter().all(|id| answered_now.contains(*id)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the child did not finish: acknowledged {answered_now:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut actor = actor;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

// --- the parent ---

/// Runs one child; `point` arms the failpoint. Returns how it ended and what it said.
fn run_child(
    dir: &Path,
    stanzas: &Path,
    role: &str,
    point: Option<&str>,
) -> (std::process::ExitStatus, String) {
    run_named_child("wire_kill_child", dir, Some(stanzas), role, point)
}

/// As [`run_child`] for the child `child`; the handler loop's child has no stanza file.
fn run_named_child(
    child: &str,
    dir: &Path,
    stanzas: Option<&Path>,
    role: &str,
    point: Option<&str>,
) -> (std::process::ExitStatus, String) {
    let mut command = support::offline::child_command(module_path!(), child);
    command
        .env(ROLE, role)
        .env(DIR, dir)
        .env_remove(STANZAS)
        .env_remove("KOURIER_FAILPOINT")
        .env_remove(fault::ENV);
    if let Some(stanzas) = stanzas {
        command.env(STANZAS, stanzas);
    }
    if let Some(point) = point {
        command.env("KOURIER_FAILPOINT", point);
    }
    let out = command.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, said)
}

/// Kills the client at `point:1`, `point:2`, ... until the child ends normally; returns how many
/// kills there were.
async fn kill_at_every_hit(point: &str) -> u32 {
    let (template, device, one_time) = template().await;
    // Outside the account's directory: the children copy the directory, not this file.
    let scratch = tempfile::tempdir().expect("temp dir");
    let stanza_file = scratch.path().join("stanzas.bin");
    prebuild(&device, one_time, &stanza_file).await;
    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = tempfile::tempdir().expect("temp dir");
        copy_dir(template.path(), dir.path());
        let what = format!("{point}:{n}");
        let armed = what.clone();
        let (status, said) = run_child(dir.path(), &stanza_file, "first", Some(&armed));
        if status.success() {
            assert_final(dir.path(), &what);
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{what} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{what}: {said}");
        assert_acked_are_durable(dir.path(), &what);
        let (status, said) = run_child(dir.path(), &stanza_file, "recover", None);
        assert!(status.success(), "{what}: the recovery failed: {said}");
        assert_final(dir.path(), &what);
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

/// Both points of a zone: before its commit and after it.
async fn loop_both(inside: &str, after: &str) {
    for point in [inside, after] {
        let kills = kill_at_every_hit(point).await;
        eprintln!("{point}: {kills} kills");
    }
}

/// The handler loop: kills the client at `point:1`, `point:2`, ... while the peer's receipts are
/// handled, until the child ends normally; returns how many kills there were.
async fn kill_receipts_at_every_hit(point: &str) -> u32 {
    let (template, _device, _one_time) = template().await;
    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = tempfile::tempdir().expect("temp dir");
        copy_dir(template.path(), dir.path());
        let what = format!("{point}:{n}");
        let (status, said) = run_named_child(
            "wire_kill_receipts_child",
            dir.path(),
            None,
            "first",
            Some(&what),
        );
        if status.success() {
            // No kill: every receipt was acknowledged on the one connection, so each is there once.
            let all: BTreeSet<String> = RECEIPTS.iter().map(|id| (*id).to_owned()).collect();
            assert_final_receipts(dir.path(), &what, &all);
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{what} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{what}: {said}");
        assert_acked_receipts_are_durable(dir.path(), &what);
        let acked_at_kill = acked(&dir.path().join("wire.log"));
        let (status, said) = run_named_child(
            "wire_kill_receipts_child",
            dir.path(),
            None,
            "recover",
            None,
        );
        assert!(status.success(), "{what}: the recovery failed: {said}");
        assert_final_receipts(dir.path(), &what, &acked_at_kill);
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

/// The pending row of a stanza: at this pin upstream writes it with `replace_pending_inbound_if`
/// (`commit_batch.rs` `persist_pending_group`, an expected-missing compare-and-swap) and deletes it with
/// `delete_pending_inbound_if`, both under the CAS pair; `pending_store` and `pending_store_batch` (the
/// pair of C4) are not called by the receive path at all, so that pair is looped by the hook-level
/// tests of `src/receive_kill.rs` and cannot be hit here.
#[tokio::test]
async fn every_message_once_after_a_kill_in_the_pending_rows_write_and_cleanup() {
    loop_both(fp::INSIDE_PENDING_CAS, fp::AFTER_PENDING_CAS).await;
}

#[tokio::test]
async fn every_message_once_after_a_kill_in_the_signal_cache_flush() {
    loop_both(fp::INSIDE_SESSIONS_BATCH, fp::AFTER_SESSIONS_BATCH).await;
}

#[tokio::test]
async fn every_message_once_after_a_kill_in_the_inbox_zone() {
    loop_both(fp::INSIDE_INBOX_COMMIT, fp::AFTER_INBOX_COMMIT).await;
}

#[tokio::test]
async fn every_message_once_after_a_kill_in_stage_two() {
    loop_both(fp::INSIDE_STAGE2, fp::AFTER_STAGE2).await;
}

/// The handler's group commit (decisions D1, D3, D5): a kill inside it (before the COMMIT) and after
/// it leaves every receipt either unacknowledged (the server sends it again) or durable.
#[tokio::test]
async fn every_receipt_is_written_after_a_kill_in_the_handlers_group_commit() {
    for point in [fp::INSIDE_EVENT_WRITE, fp::AFTER_EVENT_WRITE] {
        let kills = kill_receipts_at_every_hit(point).await;
        eprintln!("{point}: {kills} kills");
    }
}
