//! Kill tests (WhatsApp's loop, `kourier-whatsapp/tests/kill.rs`): a CHILD process (this binary
//! again, `--exact kill::child`) performs ONE operation of the store and is aborted at a failpoint
//! (`KOURIER_FAILPOINT=<name>:<n>`, like a power cut after the last fsync: no destructor, no
//! rollback, no flush). The parent reopens the file with the same key and holds the state to
//! "exactly the state before, or exactly the complete state after, never in between".
//!
//! Every test LOOPS over n = 1, 2, ... until the child exits normally: an operation written as two
//! transactions hits the same point twice, and a kill at `:2` finds it torn. Each loop must kill at
//! least once. The states are compared in code and never printed (they hold synthetic keys and an
//! ID; a failure says which point and which `n`).
//!
//! The operations: the link's commit (`store::record_link`: the identity row and `AccountReady` in
//! `core_outbox`, TH4's join in miniature), a batch of nonces of both stores
//! (`store::add_nonce_batches`), and THE JOIN'S TWO ZONES (TH4 J4b, decision H1): zone 1
//! (`store::import_join` of the fixture's join: 3 contacts, 2 groups, 1 list, 2 blobs, 10 hashed
//! nonces; no `core_outbox` row) on a fresh file, and zone 2 (`store::ready_join`: the `core_outbox`
//! rows, `AccountReady` first) on a file where zone 1 is done; and THE THREE ZONES OF `deferred` (TH5 A4,
//! schema 3): a batch of deferred inserts whose bound drops the oldest row (`store::add_deferred_batch`,
//! the reflection loop's points), a re-map zone that deletes two rows and defers one again
//! (`store::remap_deferred_steps`) and the start's age bound (`store::expire_deferred`), each all or
//! nothing WITH the kept sum of `deferred_total`; and THE RE-MAP's own zone (TH5 A8, `store::remap_with`, what
//! the actor's start runs): a page's events, its BLOB writes and its deletes or new versions together; and THE
//! CLEAR's zone (TH5b R1, `store::clear_deferred`, what the actor runs for the user's Clear Chat): the chat's
//! rows deleted with the kept sum, its unsent send rows with their receivers (TH6a R5) and its `CommandPersisted` in
//! `core_outbox`, all or nothing; and THE ROWS OF
//! AT MOST 500 EVENTS (TH5b S-L3, `outbox::append_in_rows`): a re-mapped settings delta whose 1,202 events go
//! into three `core_outbox` rows of the one zone, all or none; and THE SEND QUEUE's zones (TH6a S1, schema 4): PREPARE (the
//! task, its receivers and both nonce stores), REFLECTED, ACKS (a batch of 3 of 5, and the last with the update and its nonce),
//! DONE, a job's end (the delete with its cascade, at ACKS and at DONE), a job's give-up and the sweep.
//! Nothing here runs an actor or makes a client.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use kourier_model::UserId;
use kourier_threema::failpoints as fp;
use kourier_threema::store::{self, Identity, SlotState};
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::store::{
    JOIN_LABEL, JOIN_NOW_MS, SEND_LOCAL, SEND_QUEUED, SEND_T0, ZONE_1_TABLES, join_fixture,
    sealed_update, send_id, send_rows, send_update, transmit,
};
use crate::support::{self, db_path, key};

const OP: &str = "THREEMA_KILL_OP";
const DIR: &str = "THREEMA_KILL_DIR";
/// The most hits of one failpoint any operation here makes. A loop that goes past it is a bug.
const MAX_HITS: u32 = 4;

/// The synthetic device of the link operation.
fn identity() -> Identity {
    Identity {
        threema_id: "TH1ZZ9YX".to_owned(),
        client_key: Zeroizing::new([0x41; 32]),
        server_group: "b".to_owned(),
        device_group_key: Zeroizing::new([0x42; 32]),
        device_cookie: Zeroizing::new([0x43; 16]),
        d2m_device_id: u64::MAX,
        csp_device_id: 1 << 63,
        slot_state: SlotState::New,
        device_label: "Kourier (th1-kill)".to_owned(),
        linked_at_ms: 1_761_350_400_000,
    }
}

fn hash(byte: u8) -> [u8; 32] {
    [byte; 32]
}

/// The `now_ms` the child passes to `record_link`: the `created_at` of the outbox row.
const LINK_NOW_MS: i64 = 1_761_350_400_123;

// THE EXPECTED STATES (review F2): written out here as literals, not computed by the store, so a
// reference run that drops a write (the core never told, a nonce in the wrong store) is not
// accepted as "the state after". The check compares the reference run to them; every kill is then
// held to the state before or this one.

/// `dump(.., "link")`'s identity line for `identity()`: `i:` + id + CK + group + DGK + cookie +
/// the two device ids as SQLite stores them (the i64 of the same 64 bits: u64::MAX is -1, 1 << 63 is
/// i64::MIN) + slot + label + linked_at. `hex()` of SQLite is upper case; these digits have no letter.
fn expected_identity_line() -> String {
    format!(
        "i:TH1ZZ9YX{}b{}{}-1,-9223372036854775808newKourier (th1-kill)1761350400000",
        "41".repeat(32),
        "42".repeat(32),
        "43".repeat(16)
    )
}

/// The one event the link must leave in `core_outbox`.
fn expected_link_events() -> Vec<BackendEvent> {
    vec![BackendEvent::AccountReady {
        own_ids: vec![UserId::parse("threema:TH1ZZ9YX").expect("a valid id")],
        device: None,
        display_name: None,
        number: None,
    }]
}

/// The nonce lines of a state, as `dump(.., "nonces")` shows them: `c:` CSP, `d:` D2D.
fn nonce_lines(csp: &[u8], d2d: &[u8]) -> Vec<String> {
    let line = |scope: &str, byte: &u8| format!("{scope}:{}", format!("{byte:02X}").repeat(32));
    csp.iter()
        .map(|b| line("c", b))
        .chain(d2d.iter().map(|b| line("d", b)))
        .collect()
}

/// The rows of `core_outbox`: (seq, envelope, created_at).
fn outbox_rows(dir: &Path) -> Vec<(i64, Vec<u8>, i64)> {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let mut stmt = conn
        .prepare("SELECT seq, envelope, created_at FROM core_outbox ORDER BY seq")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The `at` of zone 2's marker row (S4), `None` before zone 2.
fn marker(dir: &Path) -> Option<i64> {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    conn.query_row("SELECT at FROM join_ready WHERE id = 1", [], |r| r.get(0))
        .optional()
        .expect("the marker")
}

/// The state `dir` must hold before (`after == false`) or after the complete operation, against the
/// literals above. Fails with a sentence; prints no value.
fn assert_literal_state(op: &str, dir: &Path, after: bool) {
    let when = if after { "after" } else { "before" };
    let lines = dump(dir, op);
    match (op, after) {
        ("link", false) => {
            assert!(lines.is_empty(), "link: a fresh file already holds a row");
            assert!(
                outbox_rows(dir).is_empty(),
                "link: a fresh file already tells the core something"
            );
        }
        ("link", true) => {
            // The identity row, the marker row (S4) and the one outbox row (the dump's lines).
            assert!(
                lines.len() == 3
                    && lines[0] == expected_identity_line()
                    && lines[1] == format!("j:1,{LINK_NOW_MS}"),
                "link: the identity row and the marker after the link are not the ones written out"
            );
            let rows = outbox_rows(dir);
            assert!(
                rows.len() == 1 && rows[0].0 == 1 && rows[0].2 == LINK_NOW_MS,
                "link: core_outbox must hold exactly one row (seq 1, created at the link's time)"
            );
            let events = decode_outbox_row(&rows[0].1).expect("the outbox row decodes");
            assert!(
                events == expected_link_events(),
                "link: the outbox row is not exactly [AccountReady for the linked id]"
            );
        }
        ("nonces", false) => assert!(
            lines == nonce_lines(&[1], &[2]),
            "nonces: the state before is not csp 01, d2d 02"
        ),
        ("nonces", true) => assert!(
            lines == nonce_lines(&[1, 3, 4], &[2, 5, 6]),
            "nonces: the state after is not csp 01 03 04, d2d 02 05 06"
        ),
        ("import", false) => {
            assert!(lines.is_empty(), "import: a fresh file already holds a row");
        }
        ("import", true) => {
            assert_zone_1_complete(dir, "import");
            assert!(
                outbox_rows(dir).is_empty(),
                "import: zone 1 wrote a core_outbox row"
            );
            assert!(marker(dir).is_none(), "import: zone 1 wrote the marker");
        }
        ("ready", false) => {
            assert_zone_1_complete(dir, "ready");
            assert!(
                outbox_rows(dir).is_empty(),
                "ready: zone 1 alone already tells the core something"
            );
            assert!(
                marker(dir).is_none(),
                "ready: zone 1 alone already carries the marker"
            );
        }
        ("ready", true) => {
            assert_zone_1_complete(dir, "ready");
            let rows = outbox_rows(dir);
            let events: Vec<Vec<BackendEvent>> = rows
                .iter()
                .map(|r| decode_outbox_row(&r.1).expect("the outbox row decodes"))
                .collect();
            let expected = kourier_threema::store::import_events(&join_fixture(KILL_OWN).import)
                .expect("the import's events");
            assert!(
                rows.len() == 2 && rows.iter().all(|r| r.2 == JOIN_NOW_MS + 1),
                "ready: core_outbox must hold exactly two rows, created at zone 2's time"
            );
            assert!(
                matches!(events[0].first(), Some(BackendEvent::AccountReady { own_ids, .. })
                    if own_ids.as_slice() == [UserId::from_parts("threema", KILL_OWN).expect("id")]),
                "ready: the first event is not AccountReady for the joined id"
            );
            assert!(
                events == expected,
                "ready: the rows are not the import's events"
            );
            assert!(
                marker(dir) == Some(JOIN_NOW_MS + 1),
                "ready: the marker is not there with zone 2's time"
            );
        }
        ("remapped", _) => assert_remapped(dir, after),
        ("split", _) => assert_split(dir, after),
        ("cleared", _) => assert_cleared(dir, after),
        ("deferred" | "remap" | "expire", _) => assert!(
            lines == def_state(op, after),
            "{op}: the {when} state is not the rows and the kept sum written out"
        ),
        (send, _) if send.starts_with("send_") => assert!(
            lines == send_state(op, after),
            "{op}: the {when} state is not the rows written out"
        ),
        (other, _) => panic!("unknown operation {other} ({when})"),
    }
}

/// The joined ID of the zone tests (8 characters, a substring of no other value).
const KILL_OWN: &str = "J4BKILL1";

// THE `deferred` ZONES (TH5 A4) -------------------------------------------------------------------

/// The local time of the deferred rows written before an operation, and the server's time of row n
/// (`DEF_SERVER_AT + n`).
const DEF_T0: i64 = 1_761_350_400_000;
const DEF_SERVER_AT: u64 = 1_761_000_000_000;
/// The real age bound (45 days in ms), written out.
const DEF_AGE: i64 = 3_888_000_000;

/// The synthetic envelope of row `n`: `10 * n` bytes of `0xA0 + n`.
fn def_envelope(n: u8) -> Vec<u8> {
    vec![0xA0 + n; 10 * usize::from(n)]
}

/// Row `n` as the store takes it (reflected, rule `envelope`, version 1).
fn def_row(envelope: &[u8], n: u8) -> store::NewDeferred<'_> {
    store::NewDeferred {
        source: store::DeferredSource::Reflected,
        kind: "envelope",
        map_version: 1,
        server_at: DEF_SERVER_AT + u64::from(n),
        flags: 0,
        envelope,
    }
}

/// Bounds the operations here do not reach, but for `rows`.
fn def_bounds(rows: i64) -> store::DeferredBounds {
    store::DeferredBounds {
        rows,
        bytes: i64::MAX,
        age_ms: DEF_AGE,
    }
}

/// `dump(.., <a deferred op>)`'s line of the row `id` holding row `n`'s envelope, written out.
fn def_line(id: i64, kind: &str, version: u32, n: u8, received_at: i64) -> String {
    format!(
        "r:{id},reflected,{kind},{version},{},0,{received_at},{}",
        DEF_SERVER_AT + u64::from(n),
        format!("{:02X}", 0xA0 + n).repeat(10 * usize::from(n))
    )
}

/// The kept sum's line.
fn def_total(rows: i64, bytes: i64) -> String {
    format!("t:{rows},{bytes}")
}

/// The literal state of a deferred operation (sorted as `dump` sorts: ORDER BY the text).
fn def_state(op: &str, after: bool) -> Vec<String> {
    let mut lines = match (op, after) {
        // Two rows (10 + 20 bytes); the batch adds rows 3, 4, 5 at DEF_T0 + 1 with a bound of 4 rows,
        // so row 1 goes at the third insert.
        ("deferred", false) => vec![
            def_line(1, "envelope", 1, 1, DEF_T0),
            def_line(2, "envelope", 1, 2, DEF_T0),
            def_total(2, 30),
        ],
        ("deferred", true) => vec![
            def_line(2, "envelope", 1, 2, DEF_T0),
            def_line(3, "envelope", 1, 3, DEF_T0 + 1),
            def_line(4, "envelope", 1, 4, DEF_T0 + 1),
            def_line(5, "envelope", 1, 5, DEF_T0 + 1),
            def_total(4, 140),
        ],
        // Three rows at version 1; the zone maps 1 and 3 and defers 2 again at version 2.
        ("remap", false) => vec![
            def_line(1, "envelope", 1, 1, DEF_T0),
            def_line(2, "envelope", 1, 2, DEF_T0),
            def_line(3, "envelope", 1, 3, DEF_T0),
            def_total(3, 60),
        ],
        ("remap", true) => vec![def_line(2, "unknown-type", 2, 2, DEF_T0), def_total(1, 20)],
        // Rows 1 and 2 at DEF_T0, row 3 at DEF_T0 + 10; the start at DEF_T0 + DEF_AGE + 1 drops 1 and 2.
        ("expire", false) => vec![
            def_line(1, "envelope", 1, 1, DEF_T0),
            def_line(2, "envelope", 1, 2, DEF_T0),
            def_line(3, "envelope", 1, 3, DEF_T0 + 10),
            def_total(3, 60),
        ],
        ("expire", true) => vec![def_line(3, "envelope", 1, 3, DEF_T0 + 10), def_total(1, 30)],
        (other, _) => panic!("unknown operation {other}"),
    };
    lines.sort();
    lines
}

/// The rows a deferred operation finds, written by the parent through the store's own API.
fn def_before(op: &str, db: &kourier_db::ZoneDb) {
    let envelopes: Vec<Vec<u8>> = (1..=3).map(def_envelope).collect();
    let n = if op == "deferred" { 2 } else { 3 };
    for (i, e) in envelopes.iter().enumerate().take(n) {
        let k = u8::try_from(i + 1).expect("a small n");
        let at = if op == "expire" && k == 3 {
            DEF_T0 + 10
        } else {
            DEF_T0
        };
        store::add_deferred_batch(db, &[def_row(e, k)], at, &def_bounds(10))
            .expect("the deferred rows before");
    }
}

/// The deferred operation under test, run by the child.
fn def_operation(op: &str, db: &kourier_db::ZoneDb) {
    match op {
        "deferred" => {
            let envelopes: Vec<Vec<u8>> = (3..=5).map(def_envelope).collect();
            let rows: Vec<store::NewDeferred<'_>> = envelopes
                .iter()
                .zip(3u8..)
                .map(|(e, n)| def_row(e, n))
                .collect();
            let dropped = store::add_deferred_batch(db, &rows, DEF_T0 + 1, &def_bounds(4))
                .expect("the batch");
            assert!(
                dropped
                    == store::Dropped {
                        full: 1,
                        expired: 0
                    },
                "deferred: the batch did not drop exactly the oldest row"
            );
        }
        "remap" => store::remap_deferred_steps(
            db,
            &[
                store::RemapStep::Mapped(1),
                store::RemapStep::DeferredAgain {
                    id: 2,
                    kind: "unknown-type",
                    map_version: 2,
                },
                store::RemapStep::Mapped(3),
            ],
        )
        .expect("the re-map zone"),
        "expire" => {
            let gone =
                store::expire_deferred(db, DEF_T0 + DEF_AGE + 1, &store::DeferredBounds::REAL)
                    .expect("the age bound");
            assert!(gone == 2, "expire: the age bound did not drop rows 1 and 2");
        }
        other => panic!("unknown operation {other}"),
    }
}

// THE RE-MAP ITSELF (TH5 A8: `store::remap_with`, the actor's start) ------------------------------

/// The contact of the re-map's rows, and the local time of its zone.
const REMAPPED_PEER: &str = "A8KILLP2";
const REMAPPED_NOW: i64 = DEF_T0 + 5;

/// The plaintexts of the three rows (version 1): a text (mapped now), a poll vote (deferred again) and a
/// type 0x00 (dropped now).
fn remapped_envelopes() -> [Vec<u8>; 3] {
    use crate::support::mock::envelope::{Author, Msg};
    use prost::Message as _;
    let author = Author::new(0x0A08);
    [
        Msg::text([1; 8], DEF_SERVER_AT, "killed text"),
        Msg::raw(
            [2; 8],
            DEF_SERVER_AT,
            crate::support::mock::body::ty::POLL_VOTE,
            vec![7; 20],
        ),
        Msg::raw([3; 8], DEF_SERVER_AT, 0x00, b"zero".to_vec()),
    ]
    .map(|m| author.incoming(REMAPPED_PEER, &m).encode_to_vec())
}

/// A linked file with the contact and the three rows at version 1.
fn remapped_before(db: &kourier_db::ZoneDb) {
    use crate::support::mock::envelope::ContactDelta;
    use kourier_db::rusqlite::params;
    use prost::Message as _;
    db.zone(|tx| {
        store::save_identity(tx, &identity())?;
        store::mark_join_ready(tx, DEF_T0)
    })
    .expect("the identity");
    db.zone(|tx| {
        let sync = ContactDelta::create(REMAPPED_PEER, [7; 32], DEF_SERVER_AT).build();
        tx.execute(
            "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
            params![sync.identity, sync.encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the contact");
    let envelopes = remapped_envelopes();
    let rows: Vec<store::NewDeferred<'_>> = envelopes
        .iter()
        .zip(1u8..)
        .map(|(e, n)| def_row(e, n))
        .collect();
    store::add_deferred_batch(db, &rows, DEF_T0, &def_bounds(10)).expect("the rows before");
}

/// The re-map's states: before, the three rows and NO core_outbox row; after, ONLY the vote at version 2
/// (kind `poll-vote`) and ONE core_outbox row holding the text as a catch-up message: the delete and the
/// events together, never one without the other.
fn assert_remapped(dir: &Path, after: bool) {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let mut stmt = conn
        .prepare("SELECT id, kind, map_version FROM deferred ORDER BY id")
        .expect("prepare");
    let rows: Vec<(i64, String, u32)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let outbox = outbox_rows(dir);
    if !after {
        let envelope = |k: &str| (k.to_owned(), 1);
        assert!(
            rows.iter()
                .map(|(_, k, v)| (k.clone(), *v))
                .collect::<Vec<_>>()
                == vec![
                    envelope("envelope"),
                    envelope("envelope"),
                    envelope("envelope")
                ],
            "remapped: the state before is not the three rows at version 1"
        );
        assert!(
            outbox.is_empty(),
            "remapped: core_outbox holds a row before the re-map"
        );
        return;
    }
    assert!(
        rows == vec![(2, "poll-vote".to_owned(), 2)],
        "remapped: after the re-map only the vote is kept, at version 2"
    );
    assert!(
        outbox.len() == 1 && outbox[0].2 == REMAPPED_NOW,
        "remapped: core_outbox must hold exactly one row, created at the re-map's time"
    );
    let events = decode_outbox_row(&outbox[0].1).expect("the outbox row decodes");
    // Round B, S6: a re-mapped message is a HISTORY row (the core's cut refuses it in a cleared chat).
    assert!(
        matches!(
            events.as_slice(),
            [BackendEvent::HistoryBatch { conv, messages, has_more: false, progress: None }]
                if messages.len() == 1 && messages[0].conv == *conv
        ),
        "remapped: the row is not the text as one history row"
    );
}

// THE ROWS OF AT MOST 500 EVENTS (TH5b S-L3: `outbox::append_in_rows`, in the re-map's zone here) -------

/// The stored contacts the split's settings delta unblocks, and the local time of its zone.
const SPLIT_CONTACTS: u32 = 1_201;
const SPLIT_NOW: i64 = DEF_T0 + 11;

/// The IDs of the stored contacts, in ID order (`L3000000`, `L3000001`, ...).
fn split_ids() -> Vec<String> {
    (0..SPLIT_CONTACTS).map(|i| format!("L3{i:06}")).collect()
}

/// The stored settings (read receipts off) with `ids` blocked.
fn split_settings(ids: &[String]) -> libthreema::protobuf::d2d_sync::Settings {
    use libthreema::protobuf::{common, d2d_sync};
    d2d_sync::Settings {
        read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
        blocked_identities: Some(common::Identities {
            identities: ids.to_vec(),
        }),
        ..Default::default()
    }
}

/// The ONE version-1 row: a settings delta that blocks nobody.
fn split_envelope() -> Vec<u8> {
    use crate::support::mock::envelope::Author;
    use libthreema::protobuf::{common, d2d_sync};
    use prost::Message as _;
    Author::new(0x0B06)
        .settings_update(d2d_sync::Settings {
            blocked_identities: Some(common::Identities { identities: vec![] }),
            ..Default::default()
        })
        .encode_to_vec()
}

/// A linked file with the 1,201 contacts, the stored settings blocking all of them, and the delta's row.
fn split_before(db: &kourier_db::ZoneDb) {
    use crate::support::mock::envelope::ContactDelta;
    use kourier_db::rusqlite::params;
    use prost::Message as _;
    db.zone(|tx| {
        store::save_identity(tx, &identity())?;
        store::mark_join_ready(tx, DEF_T0)
    })
    .expect("the identity");
    let ids = split_ids();
    db.zone(|tx| {
        for id in &ids {
            let sync = ContactDelta::create(id, [7; 32], DEF_SERVER_AT).build();
            tx.execute(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                params![id, sync.encode_to_vec()],
            )?;
        }
        tx.execute(
            "INSERT INTO settings (id, sync) VALUES (1, ?1)",
            [split_settings(&ids).encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the contacts and the settings");
    let envelope = split_envelope();
    store::add_deferred_batch(db, &[def_row(&envelope, 1)], DEF_T0, &def_bounds(10))
        .expect("the row before");
}

/// The split's states: before, the delta's row, the settings blocking all and NO core_outbox row; after, no
/// row, the settings blocking nobody, and THREE core_outbox rows of 500, 500 and 202 events, all created at the
/// zone's time: `AccountSettings` first, then the 1,201 contacts' `ContactUpsert` (unblocked) in ID order. The
/// events are written out, never computed by the crate.
fn assert_split(dir: &Path, after: bool) {
    use kourier_model::{AccountSettingsPatch, ContactPatch};
    use libthreema::protobuf::d2d_sync;
    use prost::Message as _;
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM deferred", [], |r| r.get(0))
        .expect("the deferred rows");
    let stored: Vec<u8> = conn
        .query_row("SELECT sync FROM settings WHERE id = 1", [], |r| r.get(0))
        .expect("the settings row");
    let stored = d2d_sync::Settings::decode(stored.as_slice()).expect("the settings decode");
    let outbox = outbox_rows(dir);
    if !after {
        assert!(
            rows == 1 && stored == split_settings(&split_ids()) && outbox.is_empty(),
            "split: the state before is not the delta's row, the settings blocking all and no core_outbox row"
        );
        return;
    }
    assert!(
        rows == 0 && stored == split_settings(&[]),
        "split: after the zone the row is not gone or the settings still block someone"
    );
    assert!(
        outbox.len() == 3 && outbox.iter().all(|(_, _, at)| *at == SPLIT_NOW),
        "split: core_outbox must hold exactly three rows, created at the zone's time"
    );
    let events: Vec<Vec<BackendEvent>> = outbox
        .iter()
        .map(|(_, row, _)| decode_outbox_row(row).expect("an outbox row decodes"))
        .collect();
    assert!(
        events.iter().map(Vec::len).collect::<Vec<_>>() == [500, 500, 202],
        "split: the rows do not hold 500, 500 and 202 events"
    );
    let mut expected = vec![BackendEvent::AccountSettings(AccountSettingsPatch {
        read_receipts: Some(false),
        ..AccountSettingsPatch::default()
    })];
    for i in 0..SPLIT_CONTACTS {
        let mut patch =
            ContactPatch::new(UserId::from_parts("threema", &format!("L3{i:06}")).expect("a user"));
        patch.hidden = Some(false);
        patch.blocked = Some(false);
        expected.push(BackendEvent::ContactUpsert(patch));
    }
    assert!(
        events.concat() == expected,
        "split: the rows are not AccountSettings and the unblocked contacts in ID order"
    );
}

// THE CLEAR (TH5b R1: `store::clear_deferred`, the actor's arm of `Command::ClearConversation`) ----------

/// The chat the clear empties and the other one, a group of the first, the command and the zone's time.
const CLEAR_PEER: &str = "B5KILLP1";
const CLEAR_OTHER: &str = "B5KILLP2";
const CLEAR_GROUP: u64 = 0x0B05_0B05_0B05_0B05;
const CLEAR_CMD: i64 = 7;
const CLEAR_NOW: i64 = DEF_T0 + 9;
/// The clear's `before_ms`, the time of the send rows it takes.
const CLEAR_BEFORE: i64 = DEF_T0 + 3;

/// The four rows (version 1, kind `envelope`): a vote of the chat, a vote of the other chat, our own call offer
/// into the chat and a group vote of the chat's contact (the group's, not the chat's).
fn cleared_envelopes() -> [Vec<u8>; 4] {
    use crate::support::mock::body::{self, ty};
    use crate::support::mock::envelope::{Author, Conv, Msg};
    use prost::Message as _;
    let author = Author::new(0x0B05);
    let vote = |n: u8| Msg::raw([n; 8], DEF_SERVER_AT, ty::POLL_VOTE, body::filler(20, n));
    [
        author.incoming(CLEAR_PEER, &vote(1)),
        author.incoming(CLEAR_OTHER, &vote(2)),
        author.outgoing(
            &Conv::contact(CLEAR_PEER),
            &Msg::raw([3; 8], DEF_SERVER_AT, ty::CALL_OFFER, body::filler(30, 3)),
        ),
        author.incoming(CLEAR_PEER, &vote(4).in_group(CLEAR_OTHER, CLEAR_GROUP)),
    ]
    .map(|e| e.encode_to_vec())
}

fn cleared_before(db: &kourier_db::ZoneDb) {
    let envelopes = cleared_envelopes();
    let rows: Vec<store::NewDeferred<'_>> = envelopes
        .iter()
        .zip(1u8..)
        .map(|(e, n)| def_row(e, n))
        .collect();
    store::add_deferred_batch(db, &rows, DEF_T0, &def_bounds(10)).expect("the rows before");
    // TH6a R5: two unsent rows of the chat (one past its reflect's zone) and one of the other chat, two receivers each.
    let contact = |id: &str| [&[0x0A, 0x08], id.as_bytes()].concat();
    for (n, conversation) in [
        (1, contact(CLEAR_PEER)),
        (2, contact(CLEAR_PEER)),
        (3, contact(CLEAR_OTHER)),
    ] {
        let mut row = send_rows(transmit(&format!("{n:016x}")), 2);
        row.conversation = conversation;
        row.created_at = CLEAR_BEFORE;
        store::prepare_send_zone(db, &row, &[]).expect("a send row before");
    }
    store::mark_send_reflected(db, 2).expect("past its reflect");
}

/// The send rows of the clear's states: (their ids, the receivers).
fn cleared_sends(conn: &Connection) -> (Vec<i64>, i64) {
    let ids = conn
        .prepare("SELECT id FROM send_task ORDER BY id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let receivers = conn
        .query_row("SELECT count(*) FROM send_receiver", [], |r| r.get(0))
        .expect("the receivers");
    (ids, receivers)
}

/// The clear's states: before, the four rows, their kept sum and NO core_outbox row; after, ONLY rows 2 and 4
/// (the other chat's vote, the group's vote) with their sum, and ONE core_outbox row holding exactly the
/// command's `CommandPersisted`: the deletes and the confirmation together, never one without the other.
fn assert_cleared(dir: &Path, after: bool) {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM deferred ORDER BY id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let total: (i64, i64) = conn
        .query_row(
            "SELECT row_count, byte_sum FROM deferred_total WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the kept sum");
    let lengths = cleared_envelopes().map(|e| i64::try_from(e.len()).expect("a short envelope"));
    let outbox = outbox_rows(dir);
    let sends = cleared_sends(&conn);
    if !after {
        assert!(
            ids == [1, 2, 3, 4] && total == (4, lengths.iter().sum()),
            "cleared: the state before is not the four rows and their sum"
        );
        assert!(
            sends == (vec![1, 2, 3], 6),
            "cleared: the state before is not the three send rows and their receivers"
        );
        assert!(
            outbox.is_empty(),
            "cleared: core_outbox holds a row before the clear"
        );
        return;
    }
    assert!(
        ids == [2, 4] && total == (2, lengths[1] + lengths[3]),
        "cleared: after the clear only the other chat's and the group's rows are kept, with their sum"
    );
    assert!(
        sends == (vec![3], 2),
        "cleared: after the clear only the other chat's send row is kept, with its receivers"
    );
    assert!(
        outbox.len() == 1 && outbox[0].2 == CLEAR_NOW,
        "cleared: core_outbox must hold exactly one row, created at the clear's time"
    );
    let events = decode_outbox_row(&outbox[0].1).expect("the outbox row decodes");
    assert!(
        events
            == [BackendEvent::CommandPersisted {
                cmd: kourier_model::CmdId(CLEAR_CMD)
            }],
        "cleared: the row is not exactly the command's CommandPersisted"
    );
}

// THE SEND QUEUE'S ZONES (TH6a S1, schema 4: `store::prepare_send_zone`, `mark_send_*`, `sweep_sends`) --------------

/// ZONE DONE's update reflect-ack time and its local time.
const SEND_SERVER_AT: i64 = SEND_T0 + 7;
const SEND_DONE_AT: i64 = SEND_T0 + 9;
/// The sweep's now, and the local times of its rows on each side of 14 and 30 days (written out).
const SWEEP_NOW: i64 = 1_800_000_000_000;
const SWEEP_14: i64 = SWEEP_NOW - 1_209_600_000;
const SWEEP_30: i64 = SWEEP_NOW - 2_592_000_000;
/// The eviction loop's clock: now, and the local time of two rows 25 hours old (over `EVICT_AFTER_MS`, written out).
const EVICT_NOW: i64 = SWEEP_NOW;
const EVICT_OLD: i64 = EVICT_NOW - 90_000_000;
/// The `transmit` budget's bytes, written out (64 MiB): the first row of the eviction loop holds all but 128 of them.
const EVICT_BUDGET: i64 = 67_108_864;
/// The three `transmit` rows of the eviction loop: the old one the zone takes, the old one a `Transmit` is attached to
/// (it stays) and the new one.
const EVICT_LOCALS: [&str; 3] = ["00ff112233445571", "00ff112233445572", "00ff112233445573"];

/// The nonce lines: `c:` the CSP nonce of receivers 1..=`csp`, `d:` the reflect's and, with `update`, the update's.
fn send_nonce_lines(csp: u8, update: bool) -> Vec<String> {
    let hex = |byte: u8| format!("{byte:02X}").repeat(32);
    let mut lines: Vec<String> = (1..=csp).map(|n| format!("c:{}", hex(0xC0 + n))).collect();
    lines.push(format!("d:{}", hex(0xD1)));
    if update {
        lines.push(format!("d:{}", hex(0xD2)));
    }
    lines
}

/// The receiver's line of task 1: its state, its box (88 bytes of `0xB0 + n`) while it is pending.
fn send_receiver_line(n: u8, state: &str) -> String {
    let payload = if state == "pending" {
        format!("{:02X}", 0xB0 + n).repeat(88)
    } else {
        String::new()
    };
    format!("r:1,S1RCVR0{n},{state},{payload}")
}

/// The lines of task 1 of `origin` (`transmit` or `job`) that holds `boxes` pending boxes of 88 bytes: its state, its
/// update's owed flag, the times of its end (`null`), and what it holds of its sealed bytes. Its `bytes` is what it HOLDS
/// (S-M1): 88 a box, 40 the sealed reflect, 40 the sealed update.
fn send_task_line(
    origin: &str,
    boxes: u8,
    state: &str,
    end: (&str, &str),
    sealed: (bool, bool),
) -> String {
    let local = if origin == "transmit" {
        SEND_LOCAL
    } else {
        "null"
    };
    let bytes = 88 * usize::from(boxes) + 40 * usize::from(sealed.0) + 40 * usize::from(sealed.1);
    let (sent_at, finished_at) = end;
    let reflect = if sealed.0 {
        "E1".repeat(40)
    } else {
        String::new()
    };
    let update = if sealed.1 {
        "E2".repeat(40)
    } else {
        String::new()
    };
    format!(
        "t:1,{origin},text,{local},-1,0A025331,{SEND_T0},{state},1,{sent_at},{bytes},0,{SEND_QUEUED},{finished_at},{reflect},{update}"
    )
}

/// The literal states of the send zones, sorted as `dump` sorts.
fn send_state(op: &str, after: bool) -> Vec<String> {
    const OPEN: (&str, &str) = ("null", "null");
    let mut lines: Vec<String> = match (op, after) {
        // A fresh file; then the task, three boxes, a blocked receiver and the nonces of both stores.
        ("send_prepare", false) => vec![],
        ("send_prepare", true) => {
            let mut lines = vec![
                send_task_line("transmit", 3, "prepared", OPEN, (true, false)),
                send_receiver_line(1, "pending"),
                send_receiver_line(2, "pending"),
                send_receiver_line(3, "pending"),
                send_receiver_line(4, "blocked"),
            ];
            lines.extend(send_nonce_lines(3, false));
            lines
        }
        // The reflect-ack: the state, and the sealed reflect emptied.
        ("send_reflected", _) => {
            let (state, reflect) = if after {
                ("reflected", false)
            } else {
                ("prepared", true)
            };
            let mut lines = vec![
                send_task_line("transmit", 2, state, OPEN, (reflect, false)),
                send_receiver_line(1, "pending"),
                send_receiver_line(2, "pending"),
            ];
            lines.extend(send_nonce_lines(2, false));
            lines
        }
        // A batch of 3 of 5: the task does not move, no update and no nonce.
        ("send_acks_batch", _) => {
            let acked = if after { "acked" } else { "pending" };
            let held = if after { 2 } else { 5 };
            let mut lines = vec![
                send_task_line("transmit", held, "reflected", OPEN, (false, false)),
                send_receiver_line(1, acked),
                send_receiver_line(2, acked),
                send_receiver_line(3, acked),
                send_receiver_line(4, "pending"),
                send_receiver_line(5, "pending"),
            ];
            lines.extend(send_nonce_lines(5, false));
            lines
        }
        // The last of 5: the receiver, the state, the sealed update and its nonce, together.
        ("send_acks_last", _) => {
            let (state, last) = if after {
                ("acked", "acked")
            } else {
                ("reflected", "pending")
            };
            let mut lines = vec![
                send_task_line("transmit", u8::from(!after), state, OPEN, (false, after)),
                send_receiver_line(1, "acked"),
                send_receiver_line(2, "acked"),
                send_receiver_line(3, "acked"),
                send_receiver_line(4, "acked"),
                send_receiver_line(5, last),
            ];
            lines.extend(send_nonce_lines(5, after));
            lines
        }
        // The update's ack: `done`, its two times, the sealed update emptied; the ledger keeps who was acked.
        ("send_done", _) => {
            let sent_at = SEND_SERVER_AT.to_string();
            let done_at = SEND_DONE_AT.to_string();
            let mut lines = vec![
                if after {
                    send_task_line("transmit", 0, "done", (&sent_at, &done_at), (false, false))
                } else {
                    send_task_line("transmit", 0, "acked", OPEN, (false, true))
                },
                send_receiver_line(1, "acked"),
                send_receiver_line(2, "acked"),
            ];
            lines.extend(send_nonce_lines(2, true));
            lines
        }
        // A job's end at ZONE DONE: the row and its receivers gone, the nonces (permanent) stay.
        ("send_job_done", _) => {
            let mut lines = send_nonce_lines(2, true);
            if !after {
                lines.extend([
                    send_task_line("job", 0, "acked", OPEN, (false, true)),
                    send_receiver_line(1, "acked"),
                    send_receiver_line(2, "acked"),
                ]);
            }
            lines
        }
        // A job's end at its last ACKS zone (no update owed): the same delete.
        ("send_job_acks", _) => {
            let mut lines = send_nonce_lines(2, false);
            if !after {
                let line = send_task_line("job", 1, "reflected", OPEN, (false, false));
                lines.extend([
                    line.replace(",reflected,1,", ",reflected,0,"),
                    send_receiver_line(1, "acked"),
                    send_receiver_line(2, "pending"),
                ]);
            }
            lines
        }
        // A job's give-up: the row and its receivers gone whatever they held, the nonces stay.
        ("send_give_up", _) => {
            let mut lines = send_nonce_lines(2, false);
            if !after {
                lines.extend([
                    send_task_line("job", 1, "reflected", OPEN, (false, false)),
                    send_receiver_line(1, "acked"),
                    send_receiver_line(2, "pending"),
                ]);
            }
            lines
        }
        // A PREPARE that evicts (S-M1): the budget is spent by task 1, 25 hours old and attached to nobody, so it goes
        // with its receiver and the new task 3 comes in the same zone; task 2 is as old and attached, so it stays. The
        // nonces of every row stay, they are permanent.
        ("send_prepare_evicting", _) => {
            let task = |id: usize, bytes: i64, queued_at: i64| {
                format!(
                    "t:{id},transmit,text,{},-1,0A025331,{SEND_T0},prepared,1,null,{bytes},0,{queued_at},null,{},",
                    EVICT_LOCALS[id - 1],
                    "E1".repeat(40)
                )
            };
            let receiver = |id: usize| format!("r:{id},S1RCVR01,pending,{}", "B1".repeat(88));
            let nonce = |scope: char, byte: u8| {
                let hex = format!("{byte:02X}").repeat(32);
                format!("{scope}:{hex}")
            };
            let mut lines = vec![
                task(2, 128, EVICT_OLD),
                receiver(2),
                nonce('c', 0xC1),
                nonce('d', 0xD1),
                nonce('c', 0x21),
                nonce('d', 0x22),
            ];
            if after {
                lines.extend([
                    task(3, 128, EVICT_NOW),
                    receiver(3),
                    nonce('c', 0x31),
                    nonce('d', 0x32),
                ]);
            } else {
                lines.extend([task(1, EVICT_BUDGET - 128, EVICT_OLD), receiver(1)]);
            }
            lines
        }
        // The rows on each side of each age: tasks 1, 3, 5 go, each with its receiver.
        ("send_sweep", _) => {
            let mut lines = vec![
                "t:2,transmit,done".to_owned(),
                "t:4,transmit,prepared".to_owned(),
                "t:6,job,prepared".to_owned(),
                "r:2,S1RCVR01,acked".to_owned(),
                "r:4,S1RCVR01,pending".to_owned(),
                "r:6,S1RCVR01,pending".to_owned(),
            ];
            if !after {
                lines.extend([
                    "t:1,transmit,done".to_owned(),
                    "t:3,transmit,prepared".to_owned(),
                    "t:5,job,prepared".to_owned(),
                    "r:1,S1RCVR01,acked".to_owned(),
                    "r:3,S1RCVR01,pending".to_owned(),
                    "r:5,S1RCVR01,pending".to_owned(),
                ]);
            }
            lines
        }
        (other, _) => panic!("unknown operation {other}"),
    };
    lines.sort();
    lines
}

/// What the child of `send_prepare` prepares: three boxes and a blocked receiver.
fn send_prepare_rows() -> store::SealedRows {
    let mut rows = send_rows(transmit(SEND_LOCAL), 3);
    rows.receivers.push(store::NewReceiver {
        identity: send_id(4),
        fate: store::NewFate::Blocked,
    });
    rows
}

/// The acks of receivers `from..=to` of task 1.
fn send_acks(db: &kourier_db::ZoneDb, from: u8, to: u8, update: bool) -> store::SendEnd {
    let names: Vec<String> = (from..=to).map(send_id).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let sealed = send_update();
    store::mark_send_acks(
        db,
        1,
        &names,
        update.then(|| sealed_update(&sealed)),
        SEND_DONE_AT,
    )
    .expect("the acks")
}

/// The state before a send zone, written by the parent through the store's own zones.
fn send_before(op: &str, db: &kourier_db::ZoneDb) {
    let prepare = |origin: store::NewOrigin, receivers: u8| {
        let task =
            store::prepare_send_zone(db, &send_rows(origin, receivers), &[]).expect("prepare");
        assert!(task == 1, "{op}: the first row of the queue");
    };
    let reflected = |origin: store::NewOrigin, receivers: u8| {
        prepare(origin, receivers);
        store::mark_send_reflected(db, 1).expect("reflected");
    };
    let local = || transmit(SEND_LOCAL);
    match op {
        "send_prepare" => {}
        "send_reflected" => prepare(local(), 2),
        "send_acks_batch" => reflected(local(), 5),
        "send_acks_last" => {
            reflected(local(), 5);
            send_acks(db, 1, 4, false);
        }
        "send_done" => {
            reflected(local(), 2);
            send_acks(db, 1, 2, true);
        }
        "send_job_done" => {
            reflected(store::NewOrigin::Job, 2);
            send_acks(db, 1, 2, true);
        }
        "send_job_acks" => {
            let mut rows = send_rows(store::NewOrigin::Job, 2);
            rows.has_update = false;
            let task = store::prepare_send_zone(db, &rows, &[]).expect("prepare");
            assert!(task == 1, "{op}: the first row of the queue");
            store::mark_send_reflected(db, 1).expect("reflected");
            send_acks(db, 1, 1, false);
        }
        "send_give_up" => {
            reflected(store::NewOrigin::Job, 2);
            send_acks(db, 1, 1, false);
        }
        "send_prepare_evicting" => {
            // Two rows 25 hours old, the first holding all but 128 bytes of the budget (its `bytes` forced: no 64 MiB is
            // written), the second as small as a row is.
            for (local, csp, d2d, queued_at) in [
                (EVICT_LOCALS[0], 0xC1, 0xD1, EVICT_OLD),
                (EVICT_LOCALS[1], 0x21, 0x22, EVICT_OLD),
            ] {
                let mut rows = send_rows(transmit(local), 1);
                rows.queued_at = queued_at;
                rows.csp_nonces = vec![[csp; 32]];
                rows.d2d_nonces = vec![[d2d; 32]];
                store::prepare_send_zone(db, &rows, &[]).expect("an old row");
            }
            db.zone(|tx| {
                tx.execute(
                    "UPDATE send_task SET bytes = ?1 WHERE id = 1",
                    [EVICT_BUDGET - 128],
                )
                .map_err(kourier_db::Error::from)?;
                Ok::<(), kourier_db::Error>(())
            })
            .expect("the first row holds the budget");
        }
        "send_sweep" => {
            let row = |origin: store::NewOrigin, queued_at: i64| {
                let mut rows = send_rows(origin, 1);
                rows.queued_at = queued_at;
                store::prepare_send_zone(db, &rows, &[]).expect("a row")
            };
            for (local, finished_at) in [
                ("00ff112233445501", SWEEP_14 - 1),
                ("00ff112233445502", SWEEP_14),
            ] {
                let task = row(transmit(local), SWEEP_NOW);
                store::mark_send_reflected(db, task).expect("reflected");
                let name = send_id(1);
                let sealed = send_update();
                store::mark_send_acks(db, task, &[&name], Some(sealed_update(&sealed)), SWEEP_NOW)
                    .expect("acked");
                store::mark_send_done(db, task, SWEEP_NOW, finished_at).expect("done");
            }
            row(transmit("00ff112233445503"), SWEEP_30 - 1);
            row(transmit("00ff112233445504"), SWEEP_30);
            row(store::NewOrigin::Job, SWEEP_14 - 1);
            row(store::NewOrigin::Job, SWEEP_14);
        }
        other => panic!("unknown operation {other}"),
    }
}

/// The send zone under test, run by the child.
fn send_operation(op: &str, db: &kourier_db::ZoneDb) {
    match op {
        "send_prepare" => {
            let task = store::prepare_send_zone(db, &send_prepare_rows(), &[]).expect("prepare");
            assert!(task == 1, "send_prepare: the first row of the queue");
        }
        "send_reflected" => store::mark_send_reflected(db, 1).expect("reflected"),
        "send_acks_batch" => assert!(
            send_acks(db, 1, 3, false) == store::SendEnd::Open { pending: 2 },
            "send_acks_batch: two receivers stay pending"
        ),
        "send_acks_last" => assert!(
            send_acks(db, 5, 5, true) == store::SendEnd::Acked,
            "send_acks_last: the row is not acked"
        ),
        "send_done" => assert!(
            store::mark_send_done(db, 1, SEND_SERVER_AT, SEND_DONE_AT).expect("done")
                == store::SendEnd::Done,
            "send_done: the row is not done"
        ),
        "send_job_done" => assert!(
            store::mark_send_done(db, 1, SEND_SERVER_AT, SEND_DONE_AT).expect("done")
                == store::SendEnd::Deleted,
            "send_job_done: the row is not deleted"
        ),
        "send_job_acks" => assert!(
            send_acks(db, 2, 2, false) == store::SendEnd::Deleted,
            "send_job_acks: the row is not deleted"
        ),
        "send_give_up" => store::give_up_send_job(db, 1).expect("the give-up"),
        "send_prepare_evicting" => {
            let mut rows = send_rows(transmit(EVICT_LOCALS[2]), 1);
            rows.queued_at = EVICT_NOW;
            rows.csp_nonces = vec![[0x31; 32]];
            rows.d2d_nonces = vec![[0x32; 32]];
            let task = store::prepare_send_zone(db, &rows, &[2]).expect("the evicting prepare");
            assert!(task == 3, "send_prepare_evicting: the new row is task 3");
        }
        "send_sweep" => assert!(
            store::sweep_sends(db, SWEEP_NOW).expect("the sweep")
                == store::Swept {
                    done: 1,
                    transmit: 1,
                    jobs: 1
                },
            "send_sweep: the sweep did not take one row of each"
        ),
        other => panic!("unknown operation {other}"),
    }
}

/// Zone 1 complete: every table holds the fixture's count, the identity row is the phone's.
fn assert_zone_1_complete(dir: &Path, op: &str) {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    for (table, rows) in ZONE_1_TABLES {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count");
        assert!(n == rows, "{op}: {table} does not hold zone 1's rows");
    }
    let fixture = join_fixture(KILL_OWN);
    let (id, ck, dgk, cookie, slot, label, at): (
        String,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        String,
        String,
        i64,
    ) = conn
        .query_row(
            "SELECT threema_id, client_key, device_group_key, device_cookie, slot_state, \
               device_label, linked_at_ms FROM identity",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .expect("the identity row");
    assert!(
        id == KILL_OWN
            && ck == fixture.ck
            && dgk == fixture.dgk
            && cookie == fixture.cookie
            && slot == "new"
            && label == JOIN_LABEL
            && at == JOIN_NOW_MS,
        "{op}: the identity row is not the phone's with slot new"
    );
}

/// The state before an operation: written by the parent, in process, through the store's own API.
fn before(op: &str, db: &kourier_db::ZoneDb) {
    match op {
        // A fresh file: no device, nothing for the core.
        "link" | "import" => {}
        "ready" => kourier_threema::store::import_join(
            db,
            &join_fixture(KILL_OWN).import,
            JOIN_LABEL,
            JOIN_NOW_MS,
        )
        .expect("zone 1 before zone 2"),
        "nonces" => db
            .zone(|tx| {
                store::add_nonces(tx, store::NonceScope::Csp, &[hash(1)])?;
                store::add_nonces(tx, store::NonceScope::D2d, &[hash(2)])
            })
            .expect("the nonces before"),
        "deferred" | "remap" | "expire" => def_before(op, db),
        "remapped" => remapped_before(db),
        "split" => split_before(db),
        "cleared" => cleared_before(db),
        send if send.starts_with("send_") => send_before(op, db),
        other => panic!("unknown operation {other}"),
    }
}

/// The operation under test, run by the child.
fn operation(op: &str, db: &kourier_db::ZoneDb) {
    let wake = Rc::new(Notify::new());
    match op {
        "link" => store::record_link(db, &identity(), LINK_NOW_MS, &wake).expect("link"),
        "nonces" => store::add_nonce_batches(
            db,
            // One already stored (no error, still one row) and two new ones.
            &[hash(1), hash(3), hash(4)],
            &[hash(5), hash(6)],
        )
        .expect("nonces"),
        "import" => kourier_threema::store::import_join(
            db,
            &join_fixture(KILL_OWN).import,
            JOIN_LABEL,
            JOIN_NOW_MS,
        )
        .expect("zone 1"),
        "ready" => kourier_threema::store::ready_join(db, JOIN_NOW_MS + 1, &wake)
            .map(drop)
            .expect("zone 2"),
        "deferred" | "remap" | "expire" => def_operation(op, db),
        "remapped" => {
            let done = store::remap_with(db, 2, REMAPPED_NOW, &wake).expect("the re-map");
            assert!(
                done.zones == 1
                    && done.mapped == 1
                    && done.deferred_again == 1
                    && done.dropped == 1,
                "remapped: the re-map did not map one row, defer one again and drop one in one zone"
            );
        }
        "split" => {
            let done = store::remap_with(db, 2, SPLIT_NOW, &wake).expect("the re-map");
            assert!(
                done.zones == 1 && done.mapped == 1,
                "split: the re-map did not map the delta in one zone"
            );
        }
        "cleared" => {
            let conv = kourier_model::ConvKey::Direct(
                UserId::from_parts("threema", CLEAR_PEER).expect("a user"),
            );
            let deleted = store::clear_deferred(
                db,
                &conv,
                Some(CLEAR_BEFORE),
                &[],
                kourier_model::CmdId(CLEAR_CMD),
                CLEAR_NOW,
                &wake,
            )
            .expect("the clear");
            assert!(
                deleted == 2,
                "cleared: the clear did not delete the chat's two rows"
            );
        }
        send if send.starts_with("send_") => send_operation(op, db),
        other => panic!("unknown operation {other}"),
    }
}

/// What a state looks like to the comparison: every row the operation can touch, as text.
fn dump(dir: &Path, op: &str) -> Vec<String> {
    let conn: Connection = kourier_db::open(&db_path(dir), &key()).expect("reopen with the key");
    let query = match op {
        "link" => {
            "SELECT 'i:' || threema_id || hex(client_key) || server_group || hex(device_group_key) \
               || hex(device_cookie) || d2m_device_id || ',' || csp_device_id || slot_state \
               || device_label || linked_at_ms FROM identity \
             UNION ALL SELECT 'j:' || id || ',' || at FROM join_ready \
             UNION ALL SELECT 'o:' || seq || '=' || hex(envelope) || ',' || created_at FROM core_outbox \
             ORDER BY 1"
        }
        "nonces" => {
            "SELECT 'c:' || hex(hash) FROM csp_nonce \
             UNION ALL SELECT 'd:' || hex(hash) FROM d2d_nonce ORDER BY 1"
        }
        // Every row either zone can write, as text.
        "import" | "ready" => {
            "SELECT 'i:' || threema_id || hex(client_key) || server_group || hex(device_group_key) \
               || hex(device_cookie) || d2m_device_id || ',' || csp_device_id || slot_state \
               || device_label || linked_at_ms FROM identity \
             UNION ALL SELECT 'p:' || hex(sync) FROM own_profile \
             UNION ALL SELECT 's:' || hex(sync) FROM settings \
             UNION ALL SELECT 'c:' || identity || hex(sync) || ',' || coalesce(last_update_at, 'null') FROM contact \
             UNION ALL SELECT 'g:' || creator || group_id || hex(sync) || ',' || last_update_at FROM grp \
             UNION ALL SELECT 'l:' || id || hex(sync) || ',' || last_update_at FROM dist_list \
             UNION ALL SELECT 'b:' || hex(id) || hex(data) || ',' || received_at FROM held_blob \
             UNION ALL SELECT 'n:' || hex(hash) FROM csp_nonce \
             UNION ALL SELECT 'm:' || hex(hash) FROM d2d_nonce \
             UNION ALL SELECT 'j:' || id || ',' || at FROM join_ready \
             UNION ALL SELECT 'o:' || seq || '=' || hex(envelope) || ',' || created_at FROM core_outbox \
             ORDER BY 1"
        }
        // Every row of `deferred` and the kept sum, as text.
        "deferred" | "remap" | "expire" => {
            "SELECT 'r:' || id || ',' || source || ',' || kind || ',' || map_version || ',' \
               || server_at || ',' || flags || ',' || received_at || ',' || hex(envelope) FROM deferred \
             UNION ALL SELECT 't:' || row_count || ',' || byte_sum FROM deferred_total \
             UNION ALL SELECT 'o:' || seq FROM core_outbox \
             ORDER BY 1"
        }
        // The deferred rows, the kept sum, every core_outbox row WITH its events, and the send queue's rows (R5).
        "cleared" => {
            "SELECT 'r:' || id || ',' || source || ',' || kind || ',' || map_version || ',' \
               || server_at || ',' || flags || ',' || received_at || ',' || hex(envelope) FROM deferred \
             UNION ALL SELECT 't:' || row_count || ',' || byte_sum FROM deferred_total \
             UNION ALL SELECT 'o:' || seq || '=' || hex(envelope) || ',' || created_at FROM core_outbox \
             UNION ALL SELECT 'q:' || id || ',' || state FROM send_task \
             UNION ALL SELECT 'e:' || task || ',' || identity || ',' || state FROM send_receiver \
             ORDER BY 1"
        }
        // The deferred rows, the kept sum, the settings BLOB and every core_outbox row WITH its events.
        "split" => {
            "SELECT 'r:' || id || ',' || source || ',' || kind || ',' || map_version || ',' \
               || server_at || ',' || flags || ',' || received_at || ',' || hex(envelope) FROM deferred \
             UNION ALL SELECT 't:' || row_count || ',' || byte_sum FROM deferred_total \
             UNION ALL SELECT 's:' || hex(sync) FROM settings \
             UNION ALL SELECT 'o:' || seq || '=' || hex(envelope) || ',' || created_at FROM core_outbox \
             ORDER BY 1"
        }
        // The deferred rows, the kept sum, the contacts' BLOBs and every core_outbox row WITH its events.
        "remapped" => {
            "SELECT 'r:' || id || ',' || source || ',' || kind || ',' || map_version || ',' \
               || server_at || ',' || flags || ',' || received_at || ',' || hex(envelope) FROM deferred \
             UNION ALL SELECT 't:' || row_count || ',' || byte_sum FROM deferred_total \
             UNION ALL SELECT 'c:' || identity || hex(sync) FROM contact \
             UNION ALL SELECT 'o:' || seq || '=' || hex(envelope) || ',' || created_at FROM core_outbox \
             ORDER BY 1"
        }
        // Who the sweep leaves.
        "send_sweep" => {
            "SELECT 't:' || id || ',' || origin || ',' || state FROM send_task \
             UNION ALL SELECT 'r:' || task || ',' || identity || ',' || state FROM send_receiver \
             ORDER BY 1"
        }
        // Every column of both send tables and both nonce stores, as text (`hex(NULL)` is empty: no sealed
        // column is shorter than 40 bytes).
        send if send.starts_with("send_") => {
            "SELECT 't:' || id || ',' || origin || ',' || kind || ',' || ifnull(local, 'null') || ',' || message_id \
               || ',' || hex(conversation) || ',' || created_at || ',' || state || ',' || has_update || ',' \
               || ifnull(sent_at, 'null') || ',' || bytes || ',' || attempts || ',' || queued_at || ',' \
               || ifnull(finished_at, 'null') || ',' || hex(reflect_env) || ',' || hex(update_env) FROM send_task \
             UNION ALL SELECT 'r:' || task || ',' || identity || ',' || state || ',' || hex(payload) FROM send_receiver \
             UNION ALL SELECT 'c:' || hex(hash) FROM csp_nonce \
             UNION ALL SELECT 'd:' || hex(hash) FROM d2d_nonce \
             ORDER BY 1"
        }
        other => panic!("unknown operation {other}"),
    };
    let mut stmt = conn.prepare(query).expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The child: performs the operation named by the environment, or does nothing when it runs as an
/// ordinary test.
#[test]
fn child() {
    let (Ok(op), Ok(dir)) = (std::env::var(OP), std::env::var(DIR)) else {
        return;
    };
    let db = store::open(&db_path(Path::new(&dir)), &key()).expect("open");
    operation(&op, &db);
    drop(db);
}

/// Runs the child on `dir`; `point` arms `KOURIER_FAILPOINT`. Returns how it ended and its stderr.
fn run_child(dir: &Path, op: &str, point: Option<String>) -> (std::process::ExitStatus, String) {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.arg("--exact")
        .arg(support::child_filter(module_path!(), "child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(OP, op)
        .env(DIR, dir)
        .env_remove("KOURIER_FAILPOINT");
    if let Some(point) = point {
        cmd.env("KOURIER_FAILPOINT", point);
    }
    let out = cmd.output().expect("spawn the child");
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A fresh directory in the state before the operation, closed.
fn fresh(op: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = store::open(&db_path(dir.path()), &key()).expect("open");
    before(op, &db);
    drop(db);
    dir
}

#[derive(Clone, Copy)]
enum Expect {
    Before,
    After,
}

/// The loop: kills `op` at `point:1`, `point:2`, ... until the child ends normally. Every kill must
/// be an abort at that failpoint and leave exactly the state `expect` names. Returns the kills.
fn kill_at_every_hit(op: &str, point: &str, expect: Expect) -> u32 {
    let reference = fresh(op);
    assert_literal_state(op, reference.path(), false);
    let state_before = dump(reference.path(), op);
    let (status, _) = run_child(reference.path(), op, None);
    assert!(status.success(), "{op}: the operation itself failed");
    assert_literal_state(op, reference.path(), true);
    let state_after = dump(reference.path(), op);
    assert!(
        state_before != state_after,
        "{op}: the operation changed nothing"
    );

    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = fresh(op);
        let (status, said) = run_child(dir.path(), op, Some(format!("{point}:{n}")));
        let state = dump(dir.path(), op);
        if status.success() {
            assert!(
                state == state_after,
                "{point}:{n} was never reached, and the state is not the complete state after"
            );
            break;
        }
        assert_eq!(status.signal(), Some(6), "{point}:{n} must abort");
        assert!(
            said.contains("hit: aborting"),
            "{point}:{n}: the child did not die at the failpoint"
        );
        assert!(
            state == state_before || state == state_after,
            "{point}:{n}: the file holds neither the state before nor the complete state after"
        );
        match expect {
            Expect::Before => assert!(state == state_before, "{point}:{n} is before the commit"),
            Expect::After => assert!(state == state_after, "{point}:{n} is after the commit"),
        }
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

#[test]
fn the_link_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("link", fp::INSIDE_SAVE_IDENTITY, Expect::Before);
    kill_at_every_hit("link", fp::AFTER_SAVE_IDENTITY, Expect::After);
}

#[test]
fn a_batch_of_nonces_of_both_stores_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("nonces", fp::INSIDE_ADD_NONCES, Expect::Before);
    kill_at_every_hit("nonces", fp::AFTER_ADD_NONCES, Expect::After);
}

/// Zone 1 of a join is all or nothing: after a kill the file holds no identity, no row of any new
/// table and no `core_outbox` row, or ALL of zone 1 and still no `core_outbox` row.
#[test]
fn the_join_import_zone_1_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("import", fp::INSIDE_IMPORT, Expect::Before);
    kill_at_every_hit("import", fp::AFTER_IMPORT, Expect::After);
}

/// Zone 2 of a join (on a file where zone 1 is done) is all or nothing: after a kill `core_outbox` is
/// empty or holds ALL its rows, `AccountReady` first.
#[test]
fn the_join_ready_zone_2_is_all_or_nothing_at_every_kill_point() {
    kill_at_every_hit("ready", fp::INSIDE_READY, Expect::Before);
    kill_at_every_hit("ready", fp::AFTER_READY, Expect::After);
}

/// The batch zone of the reflection loop (TH5 A4: `INSIDE_REFLECTED` / `AFTER_REFLECTED`) with three
/// deferred inserts whose row bound drops the oldest row: after a kill the file holds the two rows and the
/// kept sum from before, or the four rows and the sum after, never a row without its sum.
#[test]
fn a_batch_of_deferred_rows_is_all_or_nothing_with_its_sum_at_every_kill_point() {
    kill_at_every_hit("deferred", fp::INSIDE_REFLECTED, Expect::Before);
    kill_at_every_hit("deferred", fp::AFTER_REFLECTED, Expect::After);
}

/// A re-map zone (`INSIDE_REMAP` / `AFTER_REMAP`): two rows deleted, one deferred again, all or nothing
/// with the kept sum.
#[test]
fn a_remap_zone_is_all_or_nothing_with_its_sum_at_every_kill_point() {
    kill_at_every_hit("remap", fp::INSIDE_REMAP, Expect::Before);
    kill_at_every_hit("remap", fp::AFTER_REMAP, Expect::After);
}

/// The start's age bound (`INSIDE_EXPIRE` / `AFTER_EXPIRE`): both old rows go with the kept sum, or none.
#[test]
fn the_age_bound_of_deferred_is_all_or_nothing_with_its_sum_at_every_kill_point() {
    kill_at_every_hit("expire", fp::INSIDE_EXPIRE, Expect::Before);
    kill_at_every_hit("expire", fp::AFTER_EXPIRE, Expect::After);
}

/// THE RE-MAP's zone (TH5 A8: `store::remap_with`, what the actor's start runs; `INSIDE_REMAP` /
/// `AFTER_REMAP`): a text mapped (its events and its delete), a poll vote deferred again, a type 0x00 dropped,
/// all or nothing: never the events without the delete, or the delete without the events.
#[test]
fn the_remap_of_a_page_writes_its_events_and_deletes_its_rows_all_or_nothing() {
    kill_at_every_hit("remapped", fp::INSIDE_REMAP, Expect::Before);
    kill_at_every_hit("remapped", fp::AFTER_REMAP, Expect::After);
}

/// THE ROWS OF AT MOST 500 EVENTS (TH5b S-L3; `INSIDE_REMAP` / `AFTER_REMAP`): a settings delta that unblocks 1,201
/// stored contacts states 1,202 events, which the zone writes as THREE `core_outbox` rows (500, 500, 202, in order,
/// `AccountSettings` first) with its settings BLOB and the row's delete, all or nothing: never one row of the three.
#[test]
fn a_delta_of_1202_events_is_three_rows_of_at_most_500_all_or_nothing() {
    kill_at_every_hit("split", fp::INSIDE_REMAP, Expect::Before);
    kill_at_every_hit("split", fp::AFTER_REMAP, Expect::After);
}

/// THE CLEAR's zone (TH5b R1: `store::clear_deferred`, the actor's arm of `Command::ClearConversation`;
/// `INSIDE_CLEAR` / `AFTER_CLEAR`): the chat's two rows deleted with the kept sum, its two unsent send rows with their
/// receivers (TH6a R5) and the command's `CommandPersisted` written, all or nothing: never the deletes without the
/// confirmation (the core would not send the clear again) or the confirmation without the deletes (the rows would outlive the
/// clear). A second zone for the send rows dies here.
#[test]
fn the_clear_deletes_its_chats_rows_and_confirms_the_command_all_or_nothing() {
    kill_at_every_hit("cleared", fp::INSIDE_CLEAR, Expect::Before);
    kill_at_every_hit("cleared", fp::AFTER_CLEAR, Expect::After);
}

/// ZONE PREPARE (`INSIDE_SEND_PREPARE` / `AFTER_SEND_PREPARE`): the task, its receivers (three boxes and a blocked one), the
/// CSP nonces and the reflect's D2D nonce, all or nothing: a nonce is durable before its first use, and nothing goes out for
/// a row that is not there.
#[test]
fn send_prepare_writes_the_task_its_receivers_and_both_nonce_stores_all_or_nothing() {
    kill_at_every_hit("send_prepare", fp::INSIDE_SEND_PREPARE, Expect::Before);
    kill_at_every_hit("send_prepare", fp::AFTER_SEND_PREPARE, Expect::After);
}

/// ZONE REFLECTED (`INSIDE_SEND_REFLECTED` / `AFTER_SEND_REFLECTED`): the state and the emptied reflect together.
#[test]
fn send_reflected_moves_the_state_and_empties_the_reflect_all_or_nothing() {
    kill_at_every_hit("send_reflected", fp::INSIDE_SEND_REFLECTED, Expect::Before);
    kill_at_every_hit("send_reflected", fp::AFTER_SEND_REFLECTED, Expect::After);
}

/// ZONE ACKS (`INSIDE_SEND_ACKS` / `AFTER_SEND_ACKS`): a batch of 3 of 5 acks, all or nothing; and the last pass, whose ack, state,
/// sealed update and D2D nonce are one zone: never a row that is `acked` without its update's nonce, or a last ack with no update.
#[test]
fn send_acks_a_batch_and_the_last_with_its_update_and_nonce_all_or_nothing() {
    kill_at_every_hit("send_acks_batch", fp::INSIDE_SEND_ACKS, Expect::Before);
    kill_at_every_hit("send_acks_batch", fp::AFTER_SEND_ACKS, Expect::After);
    kill_at_every_hit("send_acks_last", fp::INSIDE_SEND_ACKS, Expect::Before);
    kill_at_every_hit("send_acks_last", fp::AFTER_SEND_ACKS, Expect::After);
}

/// ZONE DONE (`INSIDE_SEND_DONE` / `AFTER_SEND_DONE`): `done`, its two times and the emptied update together.
#[test]
fn send_done_ends_the_row_all_or_nothing() {
    kill_at_every_hit("send_done", fp::INSIDE_SEND_DONE, Expect::Before);
    kill_at_every_hit("send_done", fp::AFTER_SEND_DONE, Expect::After);
}

/// A job's end, the delete with its cascade, at ZONE DONE and at the last ACKS zone of a job with no update: its row and every
/// receiver, or none of them; the nonces stay, they are permanent.
#[test]
fn send_job_done_deletes_the_row_with_its_cascade_all_or_nothing() {
    kill_at_every_hit("send_job_done", fp::INSIDE_SEND_DONE, Expect::Before);
    kill_at_every_hit("send_job_done", fp::AFTER_SEND_DONE, Expect::After);
    kill_at_every_hit("send_job_acks", fp::INSIDE_SEND_ACKS, Expect::Before);
    kill_at_every_hit("send_job_acks", fp::AFTER_SEND_ACKS, Expect::After);
}

/// A job's give-up (`INSIDE_SEND_GIVE_UP` / `AFTER_SEND_GIVE_UP`): the row and every receiver, or none of them.
#[test]
fn send_give_up_deletes_the_job_with_its_cascade_all_or_nothing() {
    kill_at_every_hit("send_give_up", fp::INSIDE_SEND_GIVE_UP, Expect::Before);
    kill_at_every_hit("send_give_up", fp::AFTER_SEND_GIVE_UP, Expect::After);
}

/// ZONE PREPARE that EVICTS (S-M1, `INSIDE_SEND_PREPARE` / `AFTER_SEND_PREPARE`): the budget is spent by an old row nobody
/// owns, so the same zone takes it with its receiver and writes the new row, its receiver and its nonces; a row as old that a
/// `Transmit` is attached to stays. A kill leaves the file as it was, or with the evicted row gone and the new one there
/// together: never a taken row without the new one, nor the new one beside a budget it did not fit.
#[test]
fn send_prepare_with_an_eviction_takes_the_old_row_and_writes_the_new_one_all_or_nothing() {
    kill_at_every_hit(
        "send_prepare_evicting",
        fp::INSIDE_SEND_PREPARE,
        Expect::Before,
    );
    kill_at_every_hit(
        "send_prepare_evicting",
        fp::AFTER_SEND_PREPARE,
        Expect::After,
    );
}

/// THE SWEEP (`INSIDE_SEND_SWEEP` / `AFTER_SEND_SWEEP`): one row of each age goes with its receiver, all or nothing.
#[test]
fn send_sweep_deletes_every_row_past_its_age_all_or_nothing() {
    kill_at_every_hit("send_sweep", fp::INSIDE_SEND_SWEEP, Expect::Before);
    kill_at_every_hit("send_sweep", fp::AFTER_SEND_SWEEP, Expect::After);
}
