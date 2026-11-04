//! The delivery zone (W4 C13): one row of a chunk, in one transaction: the look at the cursor, the secrets,
//! the kept items, the chats' states, the `core_outbox` row, the entry and the cursor. Every id, number and
//! secret is synthetic.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::{
    Body as ModelBody, CallDirection, CallState, ContactPatch, ConversationPatch, Origin, Patch,
    SystemEvent, UserId,
};
use wa::call_log_record::CallResult;
use wa::web_message_info::StubType;
use wacore::history_sync::MAX_DECOMPRESSED;
use wacore_binary::Jid;
use waproto::buffa::{Message as _, MessageField};
use waproto::whatsapp as wa;

use super::{Appended, Outcome, Own, Source, TAKEN_UP, TakenUp, append, take_up, take_up_calls};
use crate::error::sql;
use crate::fault;
use crate::history::feed::{Entry, scan};
use crate::history::map::Item;
use crate::history::rows::{Body, Bounds, Cursor, Next, Phase, Reader, Row};
use crate::history::testing::{Chat, Chunk, Msg, Status, SyncType};
use crate::map::{Ctx, MAP_VERSION, OwnIds, conv_of};
use crate::store::chat_state::{self, Field, Put};
use crate::store::{KourierStore, StoreOwner};

const P1: &str = "15550100101@s.whatsapp.net";
const P2: &str = "15550100102@s.whatsapp.net";
const P3: &str = "15550100103@s.whatsapp.net";
const MEMBER: &str = "15550100104@s.whatsapp.net";
const GROUP: &str = "120363000000000001@g.us";
const OWN_PN: &str = "15550100999";
const OWN_LID: &str = "99900100999";
const OWN_PN_JID: &str = "15550100999@s.whatsapp.net";
const SECRET_1: [u8; 32] = [0x11; 32];
const SECRET_2: [u8; 32] = [0x22; 32];
const SECRET_3: [u8; 32] = [0x33; 32];
const NOW: i64 = 1_700_000_500_000;

struct World {
    _dir: tempfile::TempDir,
    _owner: StoreOwner,
    store: KourierStore,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([8; 32]),
        "wa-deliver",
    )
    .expect("open");
    let store = owner.store();
    World {
        _dir: dir,
        _owner: owner,
        store,
    }
}

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", OWN_LID).expect("lid")),
            pn: Some(UserId::from_parts("pn", OWN_PN).expect("pn")),
        },
    }
}

fn own() -> Own {
    Own {
        pn: Some(Jid::pn(OWN_PN)),
        lid: Some(Jid::lid(OWN_LID)),
    }
}

fn source(msg_id: &str) -> Source {
    Source {
        msg_id: msg_id.to_owned(),
        sync_type: Some(0),
        progress: Some(0.4),
    }
}

/// A chunk's bytes with its plan (the first pass is pure).
struct Planned {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
    ctx: Ctx,
}

fn planned(chunk: &Chunk) -> Planned {
    let bytes = chunk.blob();
    let entries = scan(&bytes, MAX_DECOMPRESSED).expect("scan").entries;
    Planned {
        bytes,
        entries,
        ctx: ctx(),
    }
}

/// The row that begins at `at`, read afresh (reading is a function of the chunk and the cursor, so the
/// same row comes again).
fn row_at(chunk: &Planned, at: Cursor) -> Row {
    let mut reader = Reader::new(&chunk.bytes, &chunk.entries, &chunk.ctx, NOW);
    match reader.next(at, Bounds::DEFAULT).expect("a row") {
        Next::Row(row) => row,
        Next::End => panic!("no row at {at:?}"),
    }
}

/// Every row of the chunk in order, each asked for at the previous one's end.
fn rows(chunk: &Planned, seq: i64) -> Vec<Row> {
    let mut reader = Reader::new(&chunk.bytes, &chunk.entries, &chunk.ctx, NOW);
    let mut here = Cursor::start(seq);
    let mut out = Vec::new();
    while let Next::Row(row) = reader.next(here, Bounds::DEFAULT).expect("a row") {
        here = row.after;
        out.push(row);
    }
    out
}

async fn appended(world: &World, msg_id: &str, row: Row) -> Appended {
    match append(&world.store, source(msg_id), own(), row, NOW)
        .await
        .expect("the zone")
    {
        Outcome::Appended(done) => done,
        Outcome::OutOfStep => panic!("the row was out of step"),
    }
}

async fn count(world: &World, table: &'static str) -> i64 {
    world
        .store
        .read(move |conn| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .map_err(sql("count"))
        })
        .await
        .expect("count")
}

async fn cursor(world: &World) -> (i64, i64, i64, i64) {
    world
        .store
        .read(|conn| {
            conn.query_row(
                "SELECT chunk, phase, conv, msg FROM history_cursor WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(sql("cursor"))
        })
        .await
        .expect("cursor")
}

fn at(cursor: Cursor) -> (i64, i64, i64, i64) {
    (
        cursor.chunk,
        cursor.phase.number(),
        i64::from(cursor.conv),
        i64::from(cursor.msg),
    )
}

/// Every event in `core_outbox`, in order.
async fn events(world: &World) -> Vec<BackendEvent> {
    world
        .store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                // The chats' features (W5a, D9) are read by a test of their own.
                out.extend(
                    decode_outbox_row(&row.envelope)
                        .expect("decodes")
                        .into_iter()
                        .filter(|e| !crate::features::is_statement(e)),
                );
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

/// (chat, sender, id) of every secret in the store, sorted.
async fn secrets(world: &World) -> Vec<(String, String, String)> {
    world
        .store
        .read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT chat, sender, msg_id FROM msg_secrets ORDER BY chat, sender, msg_id",
                )
                .map_err(sql("secrets"))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql("secrets"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql("secrets"))
        })
        .await
        .expect("secrets")
}

fn key(jid: &str) -> kourier_model::ConvKey {
    conv_of(&jid.parse().expect("jid")).expect("a conversation")
}

/// A chat of ours with a secret on each message, a group with a member's message, in one chunk.
fn chunk_with_secrets() -> Chunk {
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(
            Chat::new(P1)
                .unread(1)
                .active_at(1_700_000_200)
                .message(Msg::text(P1, "A2", 1_700_000_101, "two").secret(&SECRET_2))
                .message(Msg::text(P1, "A1", 1_700_000_100, "one").secret(&SECRET_1)),
        )
        .chat(
            Chat::new(GROUP)
                .name("A group")
                .active_at(1_700_000_150)
                .message(
                    Msg::text(GROUP, "G1", 1_700_000_120, "hello")
                        .from(MEMBER)
                        .secret(&SECRET_3),
                ),
        )
}

#[tokio::test]
async fn the_row_and_its_secrets_are_one_zone() {
    let _serial = fault::serial();
    let w = world();
    let chunk = planned(&chunk_with_secrets());
    let all = rows(&chunk, 1);
    // chats first, then the messages of the most recent chat (the 1:1 chat), then the group's.
    assert!(matches!(all[0].body, Body::Chats(_)), "{:?}", all[0].body);
    let first = all[0].before;
    assert_eq!(first, Cursor::start(1));
    let chats = appended(&w, "n1", row_at(&chunk, first)).await;
    assert!(chats.seq.is_some(), "the chats row is a row of core_outbox");
    let messages_at = all[0].after;
    assert_eq!(
        cursor(&w).await,
        at(messages_at),
        "the cursor moved with the chats row"
    );

    // The messages row, with its zone failing right before the commit: nothing of it is anywhere.
    fault::arm(fault::HISTORY_ROW, 1);
    let failed = append(
        &w.store,
        source("n1"),
        own(),
        row_at(&chunk, messages_at),
        NOW,
    )
    .await;
    fault::disarm();
    assert!(failed.is_err(), "the injected fault ends the zone");
    assert_eq!(count(&w, "core_outbox").await, 1, "no row of messages");
    assert!(
        secrets(&w).await.is_empty(),
        "the secrets are rolled back with the row they belong to"
    );
    assert_eq!(count(&w, "history_delivery").await, 1, "no entry");
    assert_eq!(cursor(&w).await, at(messages_at), "the cursor did not move");
    assert_eq!(
        count(&w, "history_counts WHERE delivered > 0").await,
        0,
        "nothing counted"
    );

    // The same row once more, without the fault: the row, the secrets, the entry and the cursor together.
    let row = row_at(&chunk, messages_at);
    let after = row.after;
    let done = appended(&w, "n1", row).await;
    assert_eq!(done.delivered, 2);
    assert_eq!(done.dropped, 0);
    let seq = done.seq.expect("the row was appended");
    let batches: Vec<_> = events(&w)
        .await
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more,
                progress,
            } => Some((conv, messages, has_more, progress)),
            _ => None,
        })
        .collect();
    assert_eq!(batches.len(), 1);
    let (conv, messages, has_more, progress) = &batches[0];
    assert_eq!(*conv, key(P1));
    assert_eq!(
        messages.iter().map(|m| &*m.local).collect::<Vec<_>>(),
        ["A1", "A2"],
        "oldest first"
    );
    assert!(*has_more);
    assert_eq!(*progress, Some(0.4));
    assert_eq!(
        secrets(&w).await,
        [
            (P1.to_owned(), P1.to_owned(), "A1".to_owned()),
            (P1.to_owned(), P1.to_owned(), "A2".to_owned()),
        ],
        "a secret for each message that went"
    );
    assert_eq!(cursor(&w).await, at(after));
    let entry: (i64, i64, i64, i64, i64, i64) = w
        .store
        .read(move |conn| {
            conn.query_row(
                "SELECT kind, place, first, end_phase, end_place, single \
                 FROM history_delivery WHERE seq = ?1",
                [seq],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .map_err(sql("entry"))
        })
        .await
        .expect("the entry of the row");
    assert_eq!(
        entry,
        (
            2,
            i64::from(messages_at.conv),
            i64::from(messages_at.msg),
            after.phase.number(),
            i64::from(after.conv),
            0
        )
    );
    assert_eq!(
        count(&w, "history_counts WHERE delivered = 2").await,
        1,
        "counted with the row"
    );
}

#[tokio::test]
async fn a_row_that_does_not_begin_at_the_cursor_writes_nothing() {
    let w = world();
    let chunk = planned(&chunk_with_secrets());
    let all = rows(&chunk, 1);
    assert_eq!(all.len(), 3, "chats, the 1:1 chat's messages, the group's");

    // A row of the middle of the chunk before the rows that lead to it.
    let early = row_at(&chunk, all[1].before);
    let refused = append(&w.store, source("n1"), own(), early, NOW)
        .await
        .expect("an answer");
    assert_eq!(refused, Outcome::OutOfStep);
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert!(secrets(&w).await.is_empty());
    assert_eq!(cursor(&w).await, (0, 0, 0, 0));

    // In order, each row once.
    for (n, row) in all.iter().enumerate() {
        let again = row_at(&chunk, row.before);
        appended(&w, "n1", again).await;
        // The same row a second time: the cursor has moved on.
        let twice = row_at(&chunk, row.before);
        let answer = append(&w.store, source("n1"), own(), twice, NOW)
            .await
            .expect("an answer");
        assert_eq!(answer, Outcome::OutOfStep, "row {n} is appended once");
    }
    assert_eq!(
        count(&w, "core_outbox").await,
        3,
        "one row of core_outbox each"
    );
    assert_eq!(
        secrets(&w).await.len(),
        3,
        "a secret for each of the three messages, once"
    );
    assert_eq!(
        count(&w, "history_counts WHERE delivered = 3").await,
        1,
        "counted once each"
    );
    assert_eq!(cursor(&w).await.1, Phase::End.number());

    // A chunk may begin at its start now the one before it is finished, and not before.
    let next = planned(&chunk_with_secrets());
    let start_of_2 = row_at(&next, Cursor::start(2));
    let began = append(&w.store, source("n2"), own(), start_of_2, NOW)
        .await
        .expect("an answer");
    assert!(matches!(began, Outcome::Appended(_)), "{began:?}");
    // Chunk 3 cannot start while chunk 2 stands at its first row.
    let jump = row_at(&next, Cursor::start(3));
    let refused = append(&w.store, source("n3"), own(), jump, NOW)
        .await
        .expect("an answer");
    assert_eq!(refused, Outcome::OutOfStep);
}

#[tokio::test]
async fn the_hint_is_decided_in_the_zone_from_what_the_store_holds() {
    let w = world();
    // Chunk 1 names P1 with 3 unread: the hint is stated.
    let first = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            .chat(Chat::new(P1).unread(3).active_at(1_700_000_300)),
    );
    for row in rows(&first, 1) {
        appended(&w, "n1", row).await;
    }
    // The store's own state, set as the rest of the build sets it: P3 was read on this computer, and two
    // live messages of P2 arrived that the phone's count of P2 includes.
    let p3 = key(P3).encode();
    let p2 = key(P2).encode();
    w.store
        .write("test setup", move |tx| {
            tx.execute(
                "INSERT INTO history_chats(conv, more, hint_stated, read_here, last_chunk) \
                 VALUES (?1, 0, 0, 1, 0)",
                [&p3],
            )
            .map_err(sql("setup"))?;
            for at in [1_700_000_100_000i64, 1_700_000_200_000] {
                tx.execute(
                    "INSERT INTO history_live(conv, sent_at) VALUES (?1, ?2)",
                    (&p2, at),
                )
                .map_err(sql("setup"))?;
            }
            Ok(())
        })
        .await
        .expect("setup");
    // Chunk 2 names P1 again (no second hint), P2 (3 - 2 live = 1) and P3 (read here: none).
    let second = planned(
        &Chunk::new(SyncType::RECENT, 2, 60)
            .chat(Chat::new(P1).unread(5).active_at(1_700_000_400))
            .chat(Chat::new(P2).unread(3).active_at(1_700_000_300))
            .chat(Chat::new(P3).unread(2).active_at(1_700_000_250)),
    );
    for row in rows(&second, 2) {
        appended(&w, "n2", row).await;
    }
    let hints: Vec<(String, Option<u32>)> = events(&w)
        .await
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) => {
                Some((patch.key.encode(), patch.unread_hint))
            }
            _ => None,
        })
        .collect();
    let hint_of = |jid: &str, nth: usize| {
        hints
            .iter()
            .filter(|(conv, _)| *conv == key(jid).encode())
            .nth(nth)
            .map(|(_, hint)| *hint)
    };
    assert_eq!(hint_of(P1, 0), Some(Some(3)), "stated once");
    assert_eq!(hint_of(P1, 1), Some(None), "never again for the chat");
    assert_eq!(
        hint_of(P2, 0),
        Some(Some(1)),
        "the live ones are not counted twice"
    );
    assert_eq!(hint_of(P3, 0), Some(None), "not after a read here");
    let stated: Vec<(String, i64, i64)> = w
        .store
        .read(|conn| {
            let mut stmt = conn
                .prepare("SELECT conv, hint_stated, last_chunk FROM history_chats ORDER BY conv")
                .map_err(sql("chats"))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql("chats"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql("chats"))
        })
        .await
        .expect("chats");
    let state_of = |jid: &str| {
        stated
            .iter()
            .find(|(conv, ..)| *conv == key(jid).encode())
            .map(|(_, hint, chunk)| (*hint, *chunk))
    };
    assert_eq!(
        state_of(P1),
        Some((1, 2)),
        "stated before, touched by chunk 2"
    );
    assert_eq!(state_of(P2), Some((1, 2)));
    assert_eq!(
        state_of(P3),
        Some((0, 2)),
        "nothing stated: it stays unstated"
    );
}

#[tokio::test]
async fn what_this_build_cannot_show_is_kept_once_and_counted() {
    let w = world();
    let mut stub = Msg::text(P1, "S1", 1_700_000_090, "");
    stub.info.message_stub_type = Some(StubType::GROUP_PARTICIPANT_ADD);
    let mut chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(
        Chat::new(P1)
            .unread(1)
            .active_at(1_700_000_200)
            .message(Msg::text(P1, "A1", 1_700_000_100, "one"))
            .message(stub),
    );
    chunk
        .sync
        .call_log_records
        .push(wa::CallLogRecord::default());
    let chunk = planned(&chunk);
    let mut total = Appended::default();
    for row in rows(&chunk, 1) {
        let done = appended(&w, "n1", row).await;
        total.deferred += done.deferred;
        total.delivered += done.delivered;
    }
    assert_eq!(total.deferred, 2, "the stub and the call record");
    assert_eq!(total.delivered, 1);
    let kept: Vec<(String, i64, i64)> = w
        .store
        .read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT chunk_msg_id, kind, map_version FROM history_deferred ORDER BY kind",
                )
                .map_err(sql("kept"))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql("kept"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql("kept"))
        })
        .await
        .expect("kept");
    assert_eq!(
        kept,
        [
            ("n1".to_owned(), 0, i64::from(crate::map::MAP_VERSION)),
            ("n1".to_owned(), 1, i64::from(crate::map::MAP_VERSION)),
        ]
    );
    assert_eq!(count(&w, "history_counts WHERE deferred = 2").await, 1);

    // The walk again from the start (the cursor put back, as a lost stage would): the items are not kept twice.
    w.store
        .write("test setup", |tx| {
            tx.execute(
                "UPDATE history_cursor SET chunk = 0, phase = 0, conv = 0, msg = 0",
                [],
            )
            .map_err(sql("setup"))
            .map(drop)
        })
        .await
        .expect("setup");
    let mut again = 0;
    for row in rows(&chunk, 1) {
        again += appended(&w, "n1", row).await.deferred;
    }
    assert_eq!(again, 0, "an item already kept is not kept again");
    assert_eq!(count(&w, "history_deferred").await, 2);
    assert_eq!(count(&w, "history_counts WHERE deferred = 2").await, 1);
}

#[tokio::test]
async fn a_message_the_contract_refuses_is_dropped_with_its_secret() {
    let w = world();
    let chunk = planned(&chunk_with_secrets());
    let all = rows(&chunk, 1);
    appended(&w, "n1", row_at(&chunk, all[0].before)).await;
    // Spoil one message of the 1:1 chat's row: it names another conversation than its batch.
    let mut row = row_at(&chunk, all[1].before);
    let Body::Messages { pieces, .. } = &mut row.body else {
        panic!("a messages row");
    };
    let spoiled = pieces
        .iter_mut()
        .find_map(|piece| match &mut piece.item {
            Item::Message(message) if &*message.local == "A2" => Some(message),
            _ => None,
        })
        .expect("message A2");
    spoiled.conv = key(P2);
    let done = appended(&w, "n1", row).await;
    assert_eq!((done.delivered, done.dropped), (1, 1));
    assert_eq!(
        secrets(&w).await,
        [(P1.to_owned(), P1.to_owned(), "A1".to_owned())],
        "the secret of a message that did not go is not kept"
    );
    assert_eq!(
        count(&w, "history_counts WHERE delivered = 1 AND dropped = 1").await,
        1
    );
    let delivered: Vec<String> = events(&w)
        .await
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::HistoryBatch { messages, .. } => Some(messages),
            _ => None,
        })
        .flatten()
        .map(|m| m.local.to_string())
        .collect();
    assert_eq!(delivered, ["A1"]);
}

/// Keeps `bytes` in `history_deferred` as a build whose map was at `version` would have.
async fn keep_as(
    w: &World,
    (msg_id, n): (&str, i64),
    conv: &str,
    bytes: Vec<u8>,
    kind: u8,
    version: u32,
) {
    let (msg_id, conv) = (msg_id.to_owned(), conv.to_owned());
    w.store
        .write("test setup", move |tx| {
            tx.execute(
                "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                kourier_db::rusqlite::params![msg_id, n, conv, bytes, version, kind],
            )
            .map_err(sql("setup"))
            .map(drop)
        })
        .await
        .expect("setup");
}

/// The passes of [`take_up`] until nothing is due (at most `limit`, so a broken pass fails the test, not hangs it).
async fn take_up_all(w: &World, limit: usize) -> Vec<TakenUp> {
    let mut passes = Vec::new();
    while let Some(done) = take_up(&w.store, own(), ctx(), NOW).await.expect("a pass") {
        passes.push(done);
        assert!(passes.len() <= limit, "the passes do not end: {passes:?}");
    }
    passes
}

/// (n, kind, version) of what `history_deferred` still holds.
async fn left_deferred(w: &World) -> Vec<(i64, i64, i64)> {
    w.store
        .read(|conn| {
            let mut stmt = conn
                .prepare("SELECT n, kind, map_version FROM history_deferred ORDER BY n")
                .map_err(sql("kept"))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql("kept"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql("kept"))
        })
        .await
        .expect("kept")
}

const OLD: u32 = MAP_VERSION - 1;

/// C19 (the critic's M6): what an earlier build's map deferred is delivered by a build that can show it, as
/// SILENT, READ history rows (`HistoryBatch`, never a live `Message`: that would notify and count), one row a
/// chat, in the zone that removes the items, with their secrets. What is not due stays: a current item, a part
/// that is no message. What this build cannot show either is judged by this build now (so the pass ends).
#[tokio::test]
async fn a_deferred_history_item_is_taken_up_as_a_silent_history_row() {
    let w = world();
    let text = |chat: &str, id: &str, at: u64, body: &str| {
        Msg::text(chat, id, at, body).info.encode_to_vec()
    };
    let mut stub = Msg::text(P1, "S1", 1_700_000_090, "").info;
    stub.message_stub_type = Some(StubType::GROUP_PARTICIPANT_ADD);
    let secret_a1 = Msg::text(P1, "A1", 1_700_000_100, "one")
        .secret(&SECRET_1)
        .info
        .encode_to_vec();
    keep_as(&w, ("n1", 0), P1, secret_a1, 0, OLD).await;
    keep_as(
        &w,
        ("n1", 1),
        P1,
        text(P1, "A2", 1_700_000_101, "two"),
        0,
        OLD,
    )
    .await;
    keep_as(
        &w,
        ("n1", 2),
        P2,
        text(P2, "B1", 1_700_000_102, "three"),
        0,
        OLD,
    )
    .await;
    keep_as(&w, ("n1", 3), P1, stub.encode_to_vec(), 0, OLD).await;
    keep_as(
        &w,
        ("n1", 4),
        P3,
        text(P3, "C1", 1_700_000_103, "four"),
        0,
        MAP_VERSION,
    )
    .await;
    keep_as(&w, ("n1", 5), P1, vec![1, 2, 3], 1, OLD).await;
    keep_as(
        &w,
        ("n1", 6),
        "status@broadcast",
        text("status@broadcast", "T1", 1_700_000_104, "x"),
        0,
        OLD,
    )
    .await;
    keep_as(&w, ("n1", 7), P3, vec![0x0a, 0xff], 0, OLD).await;

    let passes = take_up_all(&w, 6).await;
    let delivered: Vec<(u32, u32, u32, u32, u32)> = passes
        .iter()
        .map(|p| (p.delivered, p.dropped, p.kept, p.skipped, p.unread))
        .collect();
    assert_eq!(
        delivered,
        [
            (2, 0, 1, 0, 0),
            (1, 0, 0, 0, 0),
            (0, 0, 0, 1, 0),
            (0, 0, 0, 0, 1)
        ],
        "P1's two texts and its stub, P2's text, the status update, the unreadable item"
    );
    assert_eq!(
        passes.iter().map(|p| p.seq.is_some()).collect::<Vec<_>>(),
        [true, true, false, false],
        "only a pass with messages is a row of core_outbox"
    );

    let batches: Vec<_> = events(&w).await;
    assert_eq!(batches.len(), 2, "one row of events a chat: {batches:?}");
    let mut seen = Vec::new();
    for event in batches {
        let BackendEvent::HistoryBatch {
            conv,
            messages,
            has_more,
            progress,
        } = event
        else {
            panic!("a take-up is a history batch, never a live message: {event:?}");
        };
        assert!(!has_more, "a take-up asks for nothing more");
        assert_eq!(progress, None, "and states no progress");
        for m in &messages {
            assert!(m.silent && m.read, "{}: silent and read", m.local);
            assert!(!m.catch_up);
            seen.push((conv.clone(), m.local.to_string()));
        }
    }
    assert_eq!(
        seen,
        [
            (key(P1), "A1".to_owned()),
            (key(P1), "A2".to_owned()),
            (key(P2), "B1".to_owned()),
        ]
    );
    assert_eq!(
        left_deferred(&w).await,
        [
            (3, 0, i64::from(MAP_VERSION)),
            (4, 0, i64::from(MAP_VERSION)),
            (5, 1, i64::from(OLD)),
            (7, 0, i64::from(MAP_VERSION)),
        ],
        "the stub and the unreadable item are judged by this build; the current item and the call record stay as they were"
    );
    assert_eq!(
        secrets(&w).await,
        [(P1.to_owned(), P1.to_owned(), "A1".to_owned())],
        "the secret of a taken-up message is kept with it"
    );
    assert_eq!(
        count(
            &w,
            "history_counts WHERE delivered = 3 AND dropped = 0 AND bad = 1"
        )
        .await,
        1
    );
    assert_eq!(
        count(&w, "history_delivery").await,
        0,
        "no stage row, no entry"
    );
    assert_eq!(
        cursor(&w).await,
        (0, 0, 0, 0),
        "the cursor is the stage's and did not move"
    );

    // A pass over what is left finds nothing and writes nothing.
    assert!(take_up_all(&w, 1).await.is_empty());
    assert_eq!(count(&w, "core_outbox").await, 2);
}

/// The take-up is exactly-once: a zone that fails leaves the items and no row, and the next pass delivers them
/// once.
#[tokio::test]
async fn a_failed_take_up_leaves_the_items_and_the_next_pass_delivers_them_once() {
    let _serial = fault::serial();
    let w = world();
    let bytes = Msg::text(P1, "A1", 1_700_000_100, "one")
        .secret(&SECRET_1)
        .info
        .encode_to_vec();
    keep_as(&w, ("n1", 0), P1, bytes, 0, OLD).await;

    fault::arm(fault::HISTORY_ROW, 1);
    let failed = take_up(&w.store, own(), ctx(), NOW).await;
    fault::disarm();
    assert!(failed.is_err(), "the injected fault ends the zone");
    assert_eq!(
        left_deferred(&w).await,
        [(0, 0, i64::from(OLD))],
        "the item is still there, still due"
    );
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert!(
        secrets(&w).await.is_empty(),
        "the secret is rolled back with it"
    );

    let passes = take_up_all(&w, 2).await;
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0].delivered, 1);
    assert_eq!(count(&w, "core_outbox").await, 1);
    assert!(left_deferred(&w).await.is_empty());
}

/// A batch is cut at the delivery's bounds, and a message the contract check refuses is dropped and counted.
#[tokio::test]
async fn a_take_up_row_is_cut_at_the_bounds_of_a_row() {
    let w = world();
    let total = 3 * crate::history::rows::Bounds::DEFAULT.messages / 2;
    for i in 0..total {
        let bytes = Msg::text(P1, &format!("M{i:04}"), 1_700_000_000 + i as u64, "x")
            .info
            .encode_to_vec();
        keep_as(
            &w,
            ("n1", i64::try_from(i).expect("small")),
            P1,
            bytes,
            0,
            OLD,
        )
        .await;
    }
    let passes = take_up_all(&w, 3).await;
    assert_eq!(
        passes.iter().map(|p| p.delivered).collect::<Vec<_>>(),
        [500, 250],
        "500 messages are the most of a batch (the core refuses more)"
    );
    assert!(left_deferred(&w).await.is_empty());
}

// ----- what the live path named since the link (W4 fix F3) -----

fn live_title(conv: &kourier_model::ConvKey, title: &str) -> BackendEvent {
    let mut patch = ConversationPatch::new(conv.clone());
    patch.title = Patch::Set(title.to_owned());
    BackendEvent::ConversationUpsert(patch)
}

fn live_push_name(jid: &str, name: &str) -> BackendEvent {
    let jid: Jid = jid.parse().expect("jid");
    let mut patch = ContactPatch::new(crate::map::person_of(&jid).expect("a person"));
    patch.profile_name = Patch::Set(name.to_owned());
    BackendEvent::ContactUpsert(patch)
}

async fn noted_live(w: &World, events: Vec<BackendEvent>) {
    w.store
        .write("note", move |tx| {
            crate::history::named::record(tx, &crate::history::named::notes_of(&events), NOW)
        })
        .await
        .expect("note");
}

/// The title each chat's upsert states (None: left alone).
fn titles(events: &[BackendEvent]) -> Vec<(String, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) => Some((
                patch.key.encode(),
                match &patch.title {
                    Patch::Set(title) => Some(title.clone()),
                    _ => None,
                },
            )),
            _ => None,
        })
        .collect()
}

/// F3: a group renamed live since the link keeps its new title when an older chunk (a snapshot) says the old one; a
/// group the live path never renamed takes the chunk's.
/// Mutant for the re-check: the `named::titled` look removed from `chats_events`.
#[tokio::test]
async fn a_title_the_live_path_stated_is_not_overwritten_by_an_older_chunk() {
    let w = world();
    let other_group = "120363000000000002@g.us";
    noted_live(&w, vec![live_title(&key(GROUP), "Trip, final")]).await;
    let chunk = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
            .chat(Chat::new(GROUP).name("Trip 2026").active_at(1_700_000_300))
            .chat(
                Chat::new(other_group)
                    .name("Book club")
                    .active_at(1_700_000_200),
            ),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let titles = titles(&events(&w).await);
    let of = |jid: &str| {
        titles
            .iter()
            .find(|(conv, _)| *conv == key(jid).encode())
            .map(|(_, title)| title.clone())
    };
    assert_eq!(
        of(GROUP),
        Some(None),
        "the live title stands: the chunk says Unchanged"
    );
    assert_eq!(
        of(other_group),
        Some(Some("Book club".to_owned())),
        "a group nobody renamed since the link takes the chunk's name"
    );
}

/// F3: the same for a push name: the person whose push name the live path stated is left alone by the chunk's, the
/// others take it, and an upsert that would say nothing is not sent.
/// Mutant for the re-check: `leave_named_alone` returning its events unchanged.
#[tokio::test]
async fn a_push_name_the_live_path_stated_is_not_overwritten_by_an_older_chunk() {
    let w = world();
    // Both are people the core knows (a chunk states a push name for nobody else: F11).
    know_people(&w, &[P1, P2]).await;
    noted_live(&w, vec![live_push_name(P1, "Alex, new name")]).await;
    let chunk = planned(
        &Chunk::new(SyncType::PUSH_NAME, 0, 5)
            .push_name(P1, "Alex Old")
            .push_name(P2, "Blair Synthetic"),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let names: Vec<(String, String)> = events(&w)
        .await
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::ContactUpsert(patch) => match patch.profile_name {
                Patch::Set(name) => Some((patch.user.as_str().to_owned(), name)),
                other => panic!("an upsert that sets nothing was sent: {other:?}"),
            },
            _ => None,
        })
        .collect();
    let p1 = crate::map::person_of(&P1.parse().expect("jid")).expect("a person");
    let p2 = crate::map::person_of(&P2.parse().expect("jid")).expect("a person");
    assert!(
        names.iter().all(|(user, _)| user != p1.as_str()),
        "the live name stands, the chunk's is not sent: {names:?}"
    );
    assert_eq!(
        names,
        [(p2.as_str().to_owned(), "Blair Synthetic".to_owned())]
    );
}

/// The core knows these people (`persons_known`): they are in a chat or a group it was told of.
async fn know_people(w: &World, jids: &[&str]) {
    let users: Vec<String> = jids
        .iter()
        .map(|jid| {
            crate::map::person_of(&jid.parse().expect("jid"))
                .expect("a person")
                .as_str()
                .to_owned()
        })
        .collect();
    w.store
        .write("test: know", move |tx| {
            for user in &users {
                crate::store::book::know(tx, user)?;
            }
            Ok(())
        })
        .await
        .expect("know");
}

const P2_LID: &str = "99900100202@lid";
const P2_OTHER_LID: &str = "99900100299@lid";

/// `(kept pairs, kept names, stated pairs)`.
async fn kept_counts(w: &World) -> (i64, i64, i64) {
    (
        count(w, "kept_pairs").await,
        count(w, "kept_names").await,
        count(w, "merges_stated").await,
    )
}

/// The first message of `jid` in a direct chat with him.
fn first_message(jid: &str) -> BackendEvent {
    let user = crate::map::person_of(&jid.parse().expect("jid")).expect("a person");
    BackendEvent::Message(kourier_backend_api::IncomingMessage::new(
        kourier_model::ConvKey::Direct(user.clone()),
        "M0491",
        Origin::Remote {
            author: user,
            device: None,
        },
        ModelBody::text("hi"),
        1_000,
        1_000,
    ))
}

/// F11 (PRIV-5, N25 everywhere): a chunk's pair and push name of a person the core has no chat or group with are not stated: the
/// row carries nothing of him, backend.db keeps them, and the zone of his first message states them once. A person the core
/// knows is named in the row as before.
/// Mutants for the re-check: `strangers::scope` returning its events as they are; `book::states` not calling `strangers::release`.
#[tokio::test]
async fn a_strangers_pair_and_push_name_in_a_chunk_wait_until_he_is_known() {
    let w = world();
    know_people(&w, &[P3]).await;
    let chunk = planned(
        &Chunk::new(SyncType::PUSH_NAME, 0, 5)
            .pair(P2, P2_LID)
            .push_name(P2, "Blair Synthetic")
            .push_name(P3, "Casey Synthetic"),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let told = events(&w).await;
    let p2 = crate::map::person_of(&P2.parse().expect("jid")).expect("a person");
    let p3 = crate::map::person_of(&P3.parse().expect("jid")).expect("a person");
    assert!(
        told.iter().all(|event| match event {
            BackendEvent::IdentityMerged { .. } => false,
            BackendEvent::ContactUpsert(patch) => patch.user != p2,
            _ => true,
        }),
        "nothing of the stranger is in the row: {told:?}"
    );
    assert!(
        told.iter().any(|event| matches!(
            event,
            BackendEvent::ContactUpsert(patch) if patch.user == p3 && patch.profile_name == Patch::Set("Casey Synthetic".to_owned())
        )),
        "the known person is named"
    );
    assert_eq!(
        kept_counts(&w).await,
        (1, 1, 0),
        "kept, and not recorded as told"
    );

    // His first message: the zone states the pair, then the name, once.
    let message = first_message(P2);
    let stated = w
        .store
        .write("test: first message", move |tx| {
            crate::features::with_statements(tx, std::slice::from_ref(&message))
        })
        .await
        .expect("zone")
        .expect("statements");
    let pair = stated.iter().position(
        |event| matches!(event, BackendEvent::IdentityMerged { from, .. } if *from == p2),
    );
    let name = stated.iter().position(|event| {
        matches!(
            event,
            BackendEvent::ContactUpsert(patch)
                if patch.user == p2 && patch.profile_name == Patch::Set("Blair Synthetic".to_owned())
        )
    });
    let first_event = stated
        .iter()
        .position(|event| matches!(event, BackendEvent::Message(_)))
        .expect("the message");
    assert!(
        matches!((pair, name), (Some(pair), Some(name)) if pair < name && name < first_event),
        "the pair, the name, then the message: {pair:?} {name:?} {first_event:?}"
    );
    assert_eq!(
        kept_counts(&w).await,
        (0, 0, 1),
        "forgotten from the kept tables, recorded as told"
    );
    let second = w
        .store
        .write("test: second message", |tx| {
            crate::features::with_statements(tx, &[first_message(P2)])
        })
        .await
        .expect("zone")
        .unwrap_or_default();
    assert!(
        second.iter().all(|event| !matches!(
            event,
            BackendEvent::IdentityMerged { .. } | BackendEvent::ContactUpsert(_)
        )),
        "said once"
    );
}

/// F12 (PRIV-6, DUR-5): a pair a chunk states is recorded in the zone that states it, so the merge sweep never states a
/// second pair around the same number: here a store row names ANOTHER LID for the number a chunk paired.
/// Mutant for the re-check: `crate::merges::record` removed from the people row's zone.
#[tokio::test]
async fn a_pair_a_chunk_states_blocks_a_second_pair_of_the_sweep_around_the_number() {
    let w = world();
    know_people(&w, &[P2]).await;
    let chunk = planned(&Chunk::new(SyncType::PUSH_NAME, 0, 5).pair(P2, P2_LID));
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    assert!(
        events(&w)
            .await
            .iter()
            .any(|event| matches!(event, BackendEvent::IdentityMerged { .. })),
        "the known person's pair is stated"
    );
    // Later the server names another LID for the number (the newest row of the store).
    let (number, other) = (
        P2.trim_end_matches("@s.whatsapp.net").to_owned(),
        P2_OTHER_LID.trim_end_matches("@lid").to_owned(),
    );
    w.store
        .write("test: a newer mapping", move |tx| {
            crate::store::exec(
                tx,
                "test",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 9000, 'usync')",
                kourier_db::rusqlite::params![other, number],
            )
            .map(drop)
        })
        .await
        .expect("plant");
    let own = ctx().own;
    let (pairs, counts) = w
        .store
        .read(move |conn| crate::merges::gather(conn, &own))
        .await
        .expect("gather");
    assert!(
        pairs.is_empty(),
        "the sweep states nothing around the number"
    );
    assert_eq!(counts.crossing, 1, "it saw the chunk's pair and stood back");
}

/// F5: a delivery that resumes two days after the capture still subtracts the live messages that arrived before the
/// chunk was made: the notes are kept a day before the chunk that waits, not a day before now.
/// Mutant for the re-check: `live::horizon` ignoring the floor (the notes go at the start of the resumed delivery).
#[tokio::test]
async fn a_delivery_resumed_two_days_later_still_subtracts_the_live_messages() {
    let w = world();
    let captured = 1_700_000_300_000i64;
    let day = 24 * 60 * 60 * 1000i64;
    let p2 = key(P2).encode();
    w.store
        .write("test setup", move |tx| {
            for at in [captured - 3_600_000, captured - 1_800_000] {
                tx.execute(
                    "INSERT INTO history_live(conv, sent_at) VALUES (?1, ?2)",
                    (&p2, at),
                )
                .map_err(sql("setup"))?;
            }
            // The chunk waits in the stage: the driver keeps the floor.
            crate::history::live::set_floor(tx, Some(captured))
        })
        .await
        .expect("setup");
    // The resumed delivery's start forgets what is too old to matter, two days after the capture.
    w.store
        .write("tidy", move |tx| {
            crate::history::live::tidy(tx, captured + 2 * day)
        })
        .await
        .expect("tidy");
    let chunk = planned(
        &Chunk::new(SyncType::RECENT, 1, 60).chat(Chat::new(P2).unread(3).active_at(1_700_000_300)),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let hint = events(&w).await.into_iter().find_map(|event| match event {
        BackendEvent::ConversationUpsert(patch) => Some(patch.unread_hint),
        _ => None,
    });
    assert_eq!(
        hint,
        Some(Some(1)),
        "3 unread at the phone, 2 of them already live rows"
    );
}

/// A message of a chat that is the phone's record of a CALL (`callLogMesssage`), not something a person wrote.
fn call_record(chat: &str, id: &str, at: u64) -> Msg {
    let mut call = Msg::text(chat, id, at, "");
    let mut message = wa::Message::default();
    message.call_log_messsage = MessageField::some(wa::message::CallLogMessage::default());
    call.info.message = MessageField::some(message);
    call
}

/// A stub of a missed call (the phone's notice in the chat), with no message at all.
fn call_stub(chat: &str, id: &str, at: u64) -> Msg {
    let mut stub = Msg::text(chat, id, at, "");
    stub.info.message = MessageField::none();
    stub.info.message_stub_type = Some(StubType::CALL_MISSED_VOICE);
    stub
}

/// Whether the backend counts `jid` as a chat started on the phone.
async fn is_started(w: &World, jid: &str) -> bool {
    let conv = key(jid);
    w.store
        .read(move |conn| crate::send::started::holds(conn, &conv))
        .await
        .expect("look")
}

/// W5a (D7, AMENDED by fix F2: the privacy BLOCKER = the interop HIGH): a history chat is started on the phone ONLY
/// when its chunk carries at least one REAL message of the chat from either side (the rule `note_messages` applies
/// live). A chats-row entry alone (a time, an unread count, a name, a token) is no start, and neither is a chat whose
/// only items are the phone's record of a call (a stub, a call log message): a stranger's call makes such a chat. A
/// chat only read here is no start either. Mutant: the old unconditional note of every chat of a chats row.
#[tokio::test]
async fn a_history_chat_is_started_only_when_the_chunk_carries_a_real_message_of_it() {
    const P4: &str = "15550100105@s.whatsapp.net";
    const P5: &str = "15550100106@s.whatsapp.net";
    const P6: &str = "15550100107@s.whatsapp.net";
    let w = world();
    let p3 = key(P3).encode();
    w.store
        .write("test setup", move |tx| {
            crate::history::reads::note_in(tx, &p3)
        })
        .await
        .expect("setup");
    let chunk = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            // Only a time and an unread count.
            .chat(Chat::new(P1).unread(1).active_at(1_700_000_300))
            // Only the phone's notice of a missed call.
            .chat(Chat::new(P2).active_at(1_700_000_301).message(call_stub(
                P2,
                "C1",
                1_700_000_200,
            )))
            // Only the phone's call log message (an unsupported body, a record of a call).
            .chat(Chat::new(P4).active_at(1_700_000_302).message(call_record(
                P4,
                "C2",
                1_700_000_201,
            )))
            // One real message of the peer.
            .chat(
                Chat::new(P5)
                    .active_at(1_700_000_303)
                    .message(call_stub(P5, "C3", 1_700_000_202))
                    .message(Msg::text(P5, "T1", 1_700_000_203, "synthetic hello")),
            )
            // Only a message of ours typed on the phone.
            .chat(Chat::new(P6).active_at(1_700_000_304).message(Msg::mine(
                P6,
                "M1",
                1_700_000_204,
                "synthetic mine",
                Status::READ,
            ))),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    assert!(
        !is_started(&w, P1).await,
        "a chats-row entry alone is no start (F2)"
    );
    assert!(
        !is_started(&w, P2).await,
        "a chat whose only item is a missed-call stub is no start (F2)"
    );
    assert!(
        !is_started(&w, P4).await,
        "a chat whose only item is a call log message is no start (F2)"
    );
    assert!(
        is_started(&w, P5).await,
        "a real message of the peer starts it"
    );
    assert!(
        is_started(&w, P6).await,
        "a real message of ours from the phone starts it"
    );
    assert!(!is_started(&w, P3).await, "a chat read here is no start");
}

/// Fix F2: what the history says is what the send path does. Over the delivery zone and the real attempt (a
/// `FakeSession` as the socket): a Transmit into a history chat with only a call record, or with only a time, is
/// refused and writes NO frame; a Transmit into a history chat with one real message of the peer writes its frame.
/// Mutants: the old unconditional note of every chat of a chats row; the chat's `real` always true.
#[tokio::test]
async fn a_transmit_into_a_history_chat_with_no_real_message_writes_no_frame() {
    use std::sync::Arc;

    use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
    use kourier_model::{Body as Content, Text};

    use crate::send::acks::Acks;
    use crate::send::attempt::{Gate, Parts, transmit};
    use crate::send::hold::Recheck;
    use crate::session::Session;
    use crate::session::fake::FakeSession;

    let w = world();
    let chunk = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            .chat(Chat::new(P1).active_at(1_700_000_300))
            .chat(Chat::new(P2).active_at(1_700_000_301).message(call_stub(
                P2,
                "C1",
                1_700_000_200,
            )))
            .chat(Chat::new(P3).active_at(1_700_000_302).message(Msg::text(
                P3,
                "T1",
                1_700_000_203,
                "synthetic hello",
            ))),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let session = Arc::new(FakeSession::new());
    let acks = Arc::new(Acks::default());
    let send = |jid: &str, local: &str| {
        let session: Arc<dyn Session> = session.clone();
        let parts = Parts {
            store: w.store.clone(),
            session: Some(session),
            acks: acks.clone(),
            own: ctx().own,
            now_ms: NOW,
            hold: Recheck::fixed(Gate::Open),
            linked_ms: None,
            stop: crate::send::stop::Registered::alone(crate::send::stop::Stop::closed()),
            files: crate::send::attempt::Files::none(),
        };
        let req = OutgoingRequest {
            conv: key(jid),
            local: local.into(),
            sent_at: NOW - 1_000,
            kind: OutgoingKind::Message {
                body: Content::Text(Text::plain("synthetic reply")),
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        tokio::spawn(transmit(parts, req, reply));
        answer
    };
    for (jid, local) in [(P1, "3EB0A1"), (P2, "3EB0A2")] {
        let outcome = send(jid, local).await.expect("answered");
        assert!(
            matches!(&outcome, TransmitOutcome::Failed(e) if e.detail.contains("start this chat")),
            "a chat with no real message in the history is refused: {outcome:?}"
        );
    }
    assert!(
        session.sent().is_empty(),
        "no frame went into a chat the phone never started"
    );
    // The chat with a real message: its frame is written (the answer waits for the ack; it is not given here).
    let answer = send(P3, "3EB0A3");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while session.sent().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the frame of the started chat is written");
    assert_eq!(session.sent().len(), 1);
    drop(answer);
}

/// W5a, D9 (as amended by fix F2): a chats row of the history states each chat's features in the delivery zone, right
/// before the chat's own events and with the started fact the same zone noted: a chat whose chunk carries a real
/// message can send, one with only a time is not accepted. Mutant: the statement states the fact before the zone
/// noted it (the old order), or states every chat as started.
#[tokio::test]
async fn a_chats_row_states_the_features_of_its_chats_as_started() {
    let w = world();
    let chunk = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            .chat(Chat::new(P1).active_at(1_700_000_300).message(Msg::text(
                P1,
                "T1",
                1_700_000_200,
                "synthetic hello",
            )))
            .chat(Chat::new(P2).active_at(1_700_000_301)),
    );
    for row in rows(&chunk, 1) {
        appended(&w, "n1", row).await;
    }
    let all = w
        .store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                out.extend(decode_outbox_row(&row.envelope).expect("decodes"));
            }
            Ok(out)
        })
        .await
        .expect("read");
    for (jid, can_send) in [
        (P1, None),
        (P2, Some(kourier_model::CannotSend::NotAccepted)),
    ] {
        let conv = key(jid);
        let stated: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                crate::features::is_statement(e)
                    && matches!(e, BackendEvent::ConversationUpsert(p) if p.key == conv)
            })
            .map(|(at, _)| at)
            .collect();
        assert_eq!(stated.len(), 1, "stated once: {jid}");
        let at = stated[0];
        let BackendEvent::ConversationUpsert(patch) = &all[at] else {
            unreachable!("a statement is an upsert")
        };
        assert_eq!(
            patch.features.as_ref().map(|f| f.cannot_send.clone()),
            Some(can_send),
            "stated with the start the same zone noted (or not): {jid}"
        );
        let first_of_chat = all
            .iter()
            .position(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == conv && !crate::features::is_statement(e)))
            .expect("the chat's own upsert");
        assert!(
            at < first_of_chat,
            "the statement comes before the chat's events"
        );
    }
}

/// W5a, D12 (brief L11): a chats row keeps its one-to-one chats' trusted-contact tokens IN THE DELIVERY ZONE, under
/// upstream's key rule: the LID the chunk states, else the current LID of the number by the store's mapping, else the
/// number's user; with the sender time when stated; never for a group. A zone that fails keeps none of them.
/// Mutants: the token not kept; the chunk's LID ignored (keyed by the chat's id).
#[tokio::test]
async fn a_chunks_tokens_are_kept_in_the_delivery_zone() {
    const LID_STATED: &str = "77700152938461";
    const LID_MAPPED: &str = "77700163049572";
    let _serial = fault::serial();
    let w = world();
    w.store
        .write("a pair", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, '15550100102', 1, 1, 'other')",
                [LID_MAPPED],
            )
            .map_err(sql("pair"))
        })
        .await
        .expect("pair");
    let chunk = planned(
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            .chat(
                Chat::new(P1)
                    .lid(&format!("{LID_STATED}@lid"))
                    .active_at(1_700_000_300)
                    .token(&[0xA1, 0xA2], 1_700_000_010, Some(1_700_000_020)),
            )
            .chat(
                Chat::new(P2)
                    .active_at(1_700_000_301)
                    .token(&[0xB1], 1_700_000_011, None),
            )
            .chat(
                Chat::new(P3)
                    .active_at(1_700_000_302)
                    .token(&[0xC1], 1_700_000_012, None),
            )
            .chat(
                Chat::new(GROUP)
                    .name("synthetic group")
                    .active_at(1_700_000_303)
                    .token(&[0xD1], 1_700_000_013, None),
            ),
    );
    let all = rows(&chunk, 1);
    assert!(matches!(all[0].body, Body::Chats(_)), "{:?}", all[0].body);
    let first = all[0].before;

    fault::arm(fault::HISTORY_ROW, 1);
    let failed = append(&w.store, source("n1"), own(), row_at(&chunk, first), NOW).await;
    fault::disarm();
    assert!(failed.is_err(), "the injected fault ends the zone");
    assert_eq!(
        count(&w, "tc_tokens").await,
        0,
        "the tokens are rolled back with their row"
    );

    appended(&w, "n1", row_at(&chunk, first)).await;
    let tokens: Vec<(String, Vec<u8>, i64, Option<i64>)> = w
        .store
        .read(|conn| {
            let mut s = conn
                .prepare("SELECT jid, token, token_timestamp, sender_timestamp FROM tc_tokens ORDER BY jid")
                .map_err(sql("tokens"))?;
            let rows = s
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .and_then(|rows| rows.collect())
                .map_err(sql("tokens"))?;
            Ok(rows)
        })
        .await
        .expect("tokens");
    let expected = vec![
        ("15550100103".to_owned(), vec![0xC1], 1_700_000_012, None),
        (
            LID_STATED.to_owned(),
            vec![0xA1, 0xA2],
            1_700_000_010,
            Some(1_700_000_020),
        ),
        (LID_MAPPED.to_owned(), vec![0xB1], 1_700_000_011, None),
    ];
    assert!(
        tokens == expected,
        "the tokens are not keyed by upstream's rule, or a group's was kept"
    );
}

// ---- W6a1 A6b (D13): the call-log records the push kept, taken up once the push is over ----

const CALL_1: &str = "3EB0A4C7F5E21D90";
const CALL_2: &str = "3EB0B8D14A6C7F23";
const CALL_3: &str = "3EB0C9E25B7D8A34";
const CALL_4: &str = "3EB0DAF36C8E9B45";
const CALL_5: &str = "3EB0EB047D9FAC56";

/// A call-log record as the phone's history lists it: its creator, call id, start (seconds) and result.
fn log_record(id: &str, creator: &str, start: i64, result: CallResult) -> wa::CallLogRecord {
    let mut record = wa::CallLogRecord::default();
    record.call_id = Some(id.to_owned());
    record.call_creator_jid = Some(creator.to_owned());
    record.start_time = Some(start);
    record.call_result = Some(result);
    record
}

fn participant(jid: &str) -> wa::call_log_record::ParticipantInfo {
    let mut info = wa::call_log_record::ParticipantInfo::default();
    info.user_jid = Some(jid.to_owned());
    info
}

async fn keep_call(w: &World, n: i64, record: &wa::CallLogRecord, version: u32) {
    keep_as(w, ("n1", n), "", record.encode_to_vec(), 1, version).await;
}

/// The passes of [`take_up_calls`] until nothing is due (at most `limit`).
async fn take_up_calls_all(w: &World, limit: usize) -> Vec<super::TakenCalls> {
    let mut passes = Vec::new();
    while let Some(done) = take_up_calls(&w.store, ctx().own, NOW)
        .await
        .expect("a pass")
    {
        passes.push(done);
        assert!(passes.len() <= limit, "the passes do not end: {passes:?}");
    }
    passes
}

/// Every event of `core_outbox`, statements included.
async fn all_events(world: &World) -> Vec<BackendEvent> {
    world
        .store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                out.extend(decode_outbox_row(&row.envelope).expect("decodes"));
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

/// (chat, local, direction, state) of every call line delivered, each checked quiet and from our other device.
fn call_lines_of(
    events: &[BackendEvent],
) -> Vec<(kourier_model::ConvKey, String, CallDirection, CallState)> {
    let mut out = Vec::new();
    for event in events {
        let BackendEvent::HistoryBatch {
            conv,
            messages,
            has_more,
            progress,
        } = event
        else {
            continue;
        };
        assert!(
            !has_more && progress.is_none(),
            "a call line is its own, finished batch"
        );
        for m in messages {
            let ModelBody::System(SystemEvent::Call(line)) = &m.body else {
                panic!("a call line was expected: {}", m.local);
            };
            assert!(
                m.silent && m.read && !m.catch_up,
                "{}: quiet history, never announced, never counted unread",
                m.local
            );
            assert!(
                matches!(m.origin, Origin::OwnOtherDevice { device: None }),
                "{}: the only origin that may say answered",
                m.local
            );
            out.push((
                conv.clone(),
                m.local.to_string(),
                line.direction,
                line.state,
            ));
        }
    }
    out
}

/// D13: the kept call-log records become quiet lines in the chats of their calls, in ONE row, in the zone that marks
/// them; the chat is ADDED (stated) when the core has none; a record that gives no line is marked too; the records stay
/// (kept, never deleted) and a second pass finds nothing; other kinds and kind 0 are not touched.
#[tokio::test]
async fn kept_call_records_become_quiet_lines_once_and_stay_marked() {
    let w = world();
    let own_jid = format!("{OWN_LID}@lid");
    // n0: an incoming call P1 placed, missed (kept by an OLDER map); n1: a call we placed from the phone, answered, with
    // P2 listed beside us; n2: a group call, declined; n3: no creator; n4: a result that says nothing.
    keep_call(
        &w,
        0,
        &log_record(CALL_1, P1, 1_700_000_100, CallResult::MISSED),
        OLD,
    )
    .await;
    let mut out = log_record(CALL_2, &own_jid, 1_700_000_200, CallResult::CONNECTED);
    out.participants = vec![participant(&own_jid), participant(P2)];
    keep_call(&w, 1, &out, MAP_VERSION).await;
    let mut group = log_record(CALL_3, P3, 1_700_000_300, CallResult::REJECTED);
    group.group_jid = Some(GROUP.to_owned());
    keep_call(&w, 2, &group, MAP_VERSION).await;
    let mut nobody = log_record(CALL_4, P1, 1_700_000_400, CallResult::MISSED);
    nobody.call_creator_jid = None;
    keep_call(&w, 3, &nobody, MAP_VERSION).await;
    keep_call(
        &w,
        4,
        &log_record(CALL_5, P1, 1_700_000_500, CallResult::INVALID),
        MAP_VERSION,
    )
    .await;
    keep_as(&w, ("n1", 5), "", vec![1, 2, 3], 2, MAP_VERSION).await;
    keep_as(
        &w,
        ("n1", 6),
        P1,
        Msg::text(P1, "A1", 1_700_000_100, "one")
            .info
            .encode_to_vec(),
        0,
        MAP_VERSION,
    )
    .await;

    let passes = take_up_calls_all(&w, 2).await;
    assert_eq!(passes.len(), 1, "five records are one pass: {passes:?}");
    let done = passes[0];
    assert_eq!(
        (
            done.counts.lines,
            done.counts.silent,
            done.counts.unplaced,
            done.counts.refused,
            done.unread
        ),
        (3, 1, 0, 0, 1),
        "the missed call, ours, the group's; the INVALID result; the record with no creator"
    );
    assert!(done.seq.is_some());
    assert_eq!(count(&w, "core_outbox").await, 1, "one row for the pass");

    let events = all_events(&w).await;
    assert_eq!(
        call_lines_of(&events),
        [
            (
                key(P1),
                "call:3EB0A4C7F5E21D90".to_owned(),
                CallDirection::Incoming,
                CallState::Missed
            ),
            (
                key(P2),
                "call:3EB0B8D14A6C7F23".to_owned(),
                CallDirection::Outgoing,
                CallState::Answered
            ),
            (
                key(GROUP),
                "gcall:3EB0C9E25B7D8A34".to_owned(),
                CallDirection::Incoming,
                CallState::Declined
            ),
        ],
        "each record in the chat of its call, quiet; ours to the one other person, the group's to its group"
    );
    // The chats were ADDED: each is stated (a ConversationUpsert with its features) before its line.
    assert_eq!(
        count(&w, "features_stated WHERE version > 0").await,
        3,
        "the core is told of the three chats"
    );
    let statements = events
        .iter()
        .filter(|e| crate::features::is_statement(e))
        .count();
    assert_eq!(statements, 3, "one statement a chat added, in the same row");
    assert_eq!(
        count(&w, "call_lines").await,
        3,
        "the interceptor's lookup knows the three calls"
    );

    assert_eq!(
        left_deferred(&w).await,
        [
            (0, 1, TAKEN_UP),
            (1, 1, TAKEN_UP),
            (2, 1, TAKEN_UP),
            (3, 1, TAKEN_UP),
            (4, 1, TAKEN_UP),
            (5, 2, i64::from(MAP_VERSION)),
            (6, 0, i64::from(MAP_VERSION)),
        ],
        "every record is KEPT and marked; former members and a deferred message are not touched"
    );
    // A second pass finds nothing and writes nothing; a later map version does not take them up again.
    assert!(take_up_calls_all(&w, 1).await.is_empty());
    assert_eq!(count(&w, "core_outbox").await, 1);
}

/// F13 (INTEROP-3, his answer "Only calls after link"): a kept record of a call from BEFORE the link adds no chat. The
/// push's records are the phone's older call log: one for a person with no chat is counted `unplaced` and marked taken up
/// (never retried), one for a chat the backend holds lands in it, one after the link adds its chat. Kills "the bound
/// ignored" (the old record adds its chat) and "the bound by now" (the new one is old).
#[tokio::test]
async fn a_kept_record_from_before_the_link_adds_no_chat() {
    let w = world();
    const LINK_S: i64 = 1_700_000_250;
    w.store
        .write("test: link", |tx| {
            crate::store::resyncs::set_linked(tx, LINK_S * 1000)?;
            crate::features::state(tx, &key(P2)).map(drop)
        })
        .await
        .expect("a link and a chat");
    // n0: before the link, P1 has no chat; n1: before the link, P2's chat exists; n2: after the link, P3 has no chat.
    keep_call(
        &w,
        0,
        &log_record(CALL_1, P1, 1_700_000_100, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    keep_call(
        &w,
        1,
        &log_record(CALL_2, P2, 1_700_000_200, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    keep_call(
        &w,
        2,
        &log_record(CALL_3, P3, LINK_S + 50, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;

    let passes = take_up_calls_all(&w, 2).await;
    assert_eq!(passes.len(), 1, "{passes:?}");
    let counts = passes[0].counts;
    assert_eq!(
        (counts.lines, counts.unplaced, counts.silent, counts.refused),
        (2, 1, 0, 0),
        "the old call of a person with no chat is counted, not placed"
    );
    let events = all_events(&w).await;
    assert_eq!(
        call_lines_of(&events)
            .into_iter()
            .map(|(conv, local, _, _)| (conv, local))
            .collect::<Vec<_>>(),
        [
            (key(P2), format!("call:{CALL_2}")),
            (key(P3), format!("call:{CALL_3}")),
        ],
        "the existing chat's line and the line of the chat a later call added"
    );
    assert_eq!(
        count(&w, "features_stated WHERE version > 0").await,
        2,
        "P2 and P3; never P1"
    );
    assert_eq!(
        count(&w, "call_lines").await,
        2,
        "the unplaced call is not filed"
    );
    assert_eq!(
        left_deferred(&w).await,
        [(0, 1, TAKEN_UP), (1, 1, TAKEN_UP), (2, 1, TAKEN_UP)],
        "the unplaced record is marked too: it is not asked again"
    );
    assert!(take_up_calls_all(&w, 1).await.is_empty());
}

/// The direction of a record is its creator compared with our ids, never the record's `isIncoming` (which is wrong in
/// both directions): a call we placed lands in the peer's chat, a call the peer placed in his own, whatever the flag.
#[tokio::test]
async fn a_history_records_direction_is_its_creator_never_is_incoming() {
    let w = world();
    let mut ours = log_record(CALL_1, OWN_PN_JID, 1_700_000_100, CallResult::CONNECTED);
    ours.is_incoming = Some(true);
    ours.participants = vec![participant(P1)];
    keep_call(&w, 0, &ours, MAP_VERSION).await;
    let mut theirs = log_record(CALL_2, P2, 1_700_000_200, CallResult::CONNECTED);
    theirs.is_incoming = Some(false);
    keep_call(&w, 1, &theirs, MAP_VERSION).await;
    take_up_calls_all(&w, 2).await;
    assert_eq!(
        call_lines_of(&all_events(&w).await)
            .into_iter()
            .map(|(conv, _, direction, state)| (conv, direction, state))
            .collect::<Vec<_>>(),
        [
            (key(P1), CallDirection::Outgoing, CallState::Answered),
            (key(P2), CallDirection::Incoming, CallState::Answered),
        ]
    );
}

/// The line is at the CALL's time (the record's start, seconds -> ms), not at the take-up.
#[tokio::test]
async fn a_history_call_line_is_at_the_calls_own_time() {
    let w = world();
    keep_call(
        &w,
        0,
        &log_record(CALL_1, P1, 1_700_000_123, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    take_up_calls_all(&w, 2).await;
    let at: Vec<i64> = all_events(&w)
        .await
        .into_iter()
        .filter_map(|e| match e {
            BackendEvent::HistoryBatch { messages, .. } => Some(messages),
            _ => None,
        })
        .flatten()
        .map(|m| m.sent_at)
        .collect();
    assert_eq!(at, [1_700_000_123_000]);
}

/// Exactly once: a zone that fails leaves the records due and no row; the next pass places them once.
#[tokio::test]
async fn a_failed_call_take_up_leaves_the_records_due_and_the_next_pass_places_them_once() {
    let _serial = fault::serial();
    let w = world();
    keep_call(
        &w,
        0,
        &log_record(CALL_1, P1, 1_700_000_100, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;

    fault::arm(fault::HISTORY_ROW, 1);
    let failed = take_up_calls(&w.store, ctx().own, NOW).await;
    fault::disarm();
    assert!(failed.is_err(), "the injected fault ends the zone");
    assert_eq!(
        left_deferred(&w).await,
        [(0, 1, i64::from(MAP_VERSION))],
        "the record is still there, still due"
    );
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert_eq!(
        count(&w, "call_lines").await,
        0,
        "and the call is not noted"
    );

    let passes = take_up_calls_all(&w, 2).await;
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0].counts.lines, 1);
    assert_eq!(count(&w, "core_outbox").await, 1);
}

/// A record's direction needs BOTH of our ids: with one unknown the records wait (kept, due), so a call of ours is never
/// filed under our own number.
#[tokio::test]
async fn the_call_records_wait_until_both_of_our_ids_are_known() {
    let w = world();
    keep_call(
        &w,
        0,
        &log_record(CALL_1, OWN_PN_JID, 1_700_000_100, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    let mut half = ctx().own;
    half.lid = None;
    assert!(
        take_up_calls(&w.store, half, NOW)
            .await
            .expect("a pass")
            .is_none(),
        "nothing is taken up while an id is unknown"
    );
    assert_eq!(left_deferred(&w).await, [(0, 1, i64::from(MAP_VERSION))]);
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert_eq!(
        take_up_calls_all(&w, 2).await.len(),
        1,
        "with both ids it goes"
    );
}

/// The phone archives `chat` at `archive_ms` (stated to the core) and its setting says a new message brings a chat back.
async fn archived_while_unarchiving(w: &World, chat: &kourier_model::ConvKey, archive_ms: i64) {
    let chat = chat.clone();
    w.store
        .write("test setup", move |tx| {
            chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, true)?;
            chat_state::put(
                tx,
                &Put {
                    chat: &chat,
                    field: Field::Archived,
                    value: 1,
                    action_ms: archive_ms,
                    generation: 0,
                    collection: "regular_low",
                    stated: true,
                },
            )
            .map(drop)
        })
        .await
        .expect("setup");
}

/// The `archived` words of the chat upserts among `events`, with the index of each.
fn archived_words(events: &[BackendEvent]) -> Vec<(usize, Option<bool>)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| match event {
            BackendEvent::ConversationUpsert(patch) if patch.archived.is_some() => {
                Some((i, patch.archived))
            }
            _ => None,
        })
        .collect()
}

fn batch_index(events: &[BackendEvent], chat: &kourier_model::ConvKey) -> usize {
    events
        .iter()
        .position(|event| matches!(event, BackendEvent::HistoryBatch { conv, messages, .. } if conv == chat && !messages.is_empty()))
        .expect("a batch of the chat")
}

/// The mirror's `(value, time)` of `chat`'s archive.
async fn archive_row(w: &World, chat: &kourier_model::ConvKey) -> Option<(i64, i64)> {
    let chat = chat.clone();
    w.store
        .read(move |conn| chat_state::get(conn, &chat, Field::Archived))
        .await
        .expect("get")
        .map(|row| (row.value, row.action_ms))
}

/// F1 (INTEROP-1): a pushed messages row notes the newest message of its chat in the zone that delivers it, and the chat
/// the core holds archived with an archive OLDER than it (the phone's setting moved it out, and the app-state sync came
/// first) is brought back in front of the batch.
#[tokio::test]
async fn a_pushed_row_takes_back_an_archive_older_than_its_newest_message() {
    let _serial = fault::serial();
    let w = world();
    let p1 = key(P1);
    let chunk = planned(&chunk_with_secrets());
    let all = rows(&chunk, 1);
    appended(&w, "n1", row_at(&chunk, all[0].before)).await;
    // P1's newest message is at 1_700_000_101 s; the archive is from before it.
    archived_while_unarchiving(&w, &p1, 1_700_000_050_000).await;
    appended(&w, "n1", row_at(&chunk, all[0].after)).await;
    let pushed = events(&w).await;
    let words = archived_words(&pushed);
    assert_eq!(words.len(), 1, "one word: {words:?}");
    assert_eq!(words[0].1, Some(false));
    assert_eq!(
        words[0].0 + 1,
        batch_index(&pushed, &p1),
        "right in front of the batch of the chat"
    );
    assert_eq!(archive_row(&w, &p1).await, Some((0, 1_700_000_050_000)));
    assert_eq!(
        w.store
            .read({
                let p1 = p1.clone();
                move |conn| chat_state::newest_counted(conn, &p1)
            })
            .await
            .expect("newest"),
        Some(1_700_000_101_000)
    );
}

/// H6 (DUR-5): the call-log lines of the take-up never bring an archived chat back, in either form: the history row (no
/// cut known) and the live row of a call after the chat's cut (INTEROP-4). The phone's setting unarchives, the chats are
/// archived and stated, the calls are newer than the archives: no `archived: false` in the outbox and the mirror still
/// holds both archives. Mutant: the take-up's zone routes its events through `zone::prepare`.
#[tokio::test]
async fn taken_up_call_lines_never_unarchive_a_chat_in_either_form() {
    let w = world();
    let (p1, p2) = (key(P1), key(P2));
    archived_while_unarchiving(&w, &p1, 1_700_000_050_000).await;
    archived_while_unarchiving(&w, &p2, 1_700_000_050_000).await;
    // P2 was cleared on the phone before its call: the call is a live row.
    let cleared = p2.clone();
    w.store
        .write("test: cut", move |tx| {
            chat_state::raise_cut(tx, &cleared, 1_700_000_100_000)
        })
        .await
        .expect("cut");
    keep_call(
        &w,
        0,
        &log_record(CALL_1, P1, 1_700_000_200, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    keep_call(
        &w,
        1,
        &log_record(CALL_2, P2, 1_700_000_300, CallResult::MISSED),
        MAP_VERSION,
    )
    .await;
    let passes = take_up_calls_all(&w, 2).await;
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0].counts.lines, 2);
    let taken = all_events(&w).await;
    assert!(
        taken
            .iter()
            .any(|event| matches!(event, BackendEvent::HistoryBatch { conv, .. } if *conv == p1)),
        "P1's line is a history row"
    );
    let live: Vec<_> = taken
        .iter()
        .filter_map(|event| match event {
            BackendEvent::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(live.len(), 1, "P2's line is a live row");
    assert!(live[0].conv == p2 && live[0].silent && live[0].read);
    assert!(
        archived_words(&taken).is_empty(),
        "a call-log line brought a chat back: {taken:?}"
    );
    assert_eq!(archive_row(&w, &p1).await.map(|row| row.0), Some(1));
    assert_eq!(archive_row(&w, &p2).await.map(|row| row.0), Some(1));
}

/// The same rule for a deferred item an earlier build kept and a later one takes up; a chat whose message is older than
/// its archive keeps the archive.
#[tokio::test]
async fn a_taken_up_row_takes_back_an_archive_older_than_its_message_and_keeps_a_newer_one() {
    let w = world();
    let (p2, p3) = (key(P2), key(P3));
    archived_while_unarchiving(&w, &p2, 1_700_000_050_000).await;
    keep_as(
        &w,
        ("n2", 0),
        P2,
        Msg::text(P2, "B1", 1_700_000_102, "three")
            .info
            .encode_to_vec(),
        0,
        OLD,
    )
    .await;
    take_up_all(&w, 3).await;
    let taken = events(&w).await;
    let words = archived_words(&taken);
    assert_eq!(words.len(), 1, "one word: {words:?}");
    assert_eq!(words[0].1, Some(false));
    assert_eq!(words[0].0 + 1, batch_index(&taken, &p2));
    assert_eq!(archive_row(&w, &p2).await.map(|row| row.0), Some(0));

    // Older than its archive: nothing is taken back.
    archived_while_unarchiving(&w, &p3, 1_700_000_900_000).await;
    keep_as(
        &w,
        ("n3", 0),
        P3,
        Msg::text(P3, "C1", 1_700_000_103, "four")
            .info
            .encode_to_vec(),
        0,
        OLD,
    )
    .await;
    let before = taken.len();
    take_up_all(&w, 3).await;
    let later: Vec<BackendEvent> = events(&w).await.split_off(before);
    assert!(
        archived_words(&later).is_empty(),
        "a message older than the archive: {later:?}"
    );
    assert_eq!(archive_row(&w, &p3).await.map(|row| row.0), Some(1));
}
