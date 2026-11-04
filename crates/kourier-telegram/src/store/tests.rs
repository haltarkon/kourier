//! The store of schema 2: one transaction for rows, cursor, gaps and parked facts; the cursor
//! never moves down; the hash gates; the sweep; the run state; the frozen migration.
//!
//! Every value is synthetic (the names "Ada Test" / "Bo Peer", small ids).

use std::path::Path;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::DbKey;
use kourier_db::rusqlite::Connection;
use kourier_model::{ContactPatch, ConvKey, ConversationPatch, Patch, UserId};
use sha2::{Digest, Sha256};

use super::*;
use crate::db;

const CHAT: i64 = 5;
const SUPERGROUP: i64 = -1_000_000_000_402;
/// One server id step of a full TDLib message id.
const STEP: i64 = 1 << 20;
const NOW: i64 = 1_700_000_000_000;

fn open(dir: &Path) -> Connection {
    db::open(&dir.join("backend.db"), &DbKey::from_bytes([7; 32])).expect("open")
}

fn user(id: i64) -> UserId {
    UserId::parse(&format!("tg:{id}")).expect("tg id")
}

fn patch(title: &str) -> BackendEvent {
    let mut p = ConversationPatch::new(ConvKey::Direct(user(CHAT)));
    p.title = Patch::Set(title.to_owned());
    BackendEvent::ConversationUpsert(p)
}

fn contact(name: &str) -> BackendEvent {
    let mut p = ContactPatch::new(user(CHAT));
    p.profile_name = Patch::Set(name.to_owned());
    BackendEvent::ContactUpsert(p)
}

/// A row delivering `ids` of `chat` (incoming), carrying one ungated upsert as its event.
fn row(chat: i64, ids: &[i64]) -> Row {
    Row {
        events: vec![Event {
            event: patch("Ada Test"),
            gate: None,
        }],
        delivered: ids
            .iter()
            .map(|&msg_id| Delivered {
                chat_id: chat,
                msg_id,
                author: format!("tg:{CHAT}"),
                outgoing: false,
            })
            .collect(),
    }
}

fn rows(staged_rows: Vec<Row>) -> Staged {
    Staged {
        rows: staged_rows,
        ..Staged::default()
    }
}

fn outbox_len(conn: &Connection) -> u64 {
    kourier_db::outbox::len(conn).expect("outbox len")
}

/// Makes every later `core_outbox` insert of this connection fail.
fn refuse_appends(conn: &Connection) {
    conn.execute_batch(
        "CREATE TEMP TRIGGER refuse_appends BEFORE INSERT ON main.core_outbox
         BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
    )
    .expect("trigger");
}

fn cursor_of(conn: &Connection, chat_id: i64) -> i64 {
    chat(conn, chat_id)
        .expect("read")
        .map_or(0, |c| c.delivered_max_id)
}

#[test]
fn rows_cursor_and_gaps_commit_together() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let mut staged = rows(vec![row(SUPERGROUP, &[6 * STEP])]);
    staged.gaps.push(GapChange::Open {
        chat_id: SUPERGROUP,
        above: STEP,
        below: 6 * STEP,
        cause: GapCause::IdJump,
    });
    staged.parked.push(Parked::Delete {
        chat_id: SUPERGROUP,
        ids: vec![2 * STEP],
    });
    refuse_appends(&conn);
    assert!(
        commit_rows(&conn, &staged, NOW).is_err(),
        "a refused append was committed"
    );
    assert_eq!(cursor_of(&conn, SUPERGROUP), 0, "the cursor moved alone");
    assert!(
        delivered_author(&conn, SUPERGROUP, 6 * STEP)
            .expect("read")
            .is_none(),
        "a delivered row was kept without its outbox row"
    );
    assert!(
        gap_rows(&conn).expect("gaps").is_empty(),
        "the gap was kept alone"
    );
    assert!(
        parked_rows(&conn).expect("parked").is_empty(),
        "the delete was parked alone"
    );
    assert_eq!(outbox_len(&conn), 0);

    conn.execute_batch("DROP TRIGGER temp.refuse_appends")
        .expect("drop");
    let done = commit_rows(&conn, &staged, NOW).expect("commit");
    assert_eq!(done.seqs.len(), 1, "one row per Row");
    assert_eq!(outbox_len(&conn), 1);
    assert_eq!(cursor_of(&conn, SUPERGROUP), 6 * STEP);
    assert_eq!(
        gap_rows(&conn).expect("gaps"),
        vec![GapRow {
            chat_id: SUPERGROUP,
            above: STEP,
            below: 6 * STEP,
            cause: GapCause::IdJump,
            lost: false,
            pages: 0,
        }]
    );
    assert_eq!(parked_rows(&conn).expect("parked").len(), 1);
}

#[test]
fn the_cursor_never_moves_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    commit_rows(&conn, &rows(vec![row(CHAT, &[10 * STEP])]), NOW).expect("first");
    let first = chat(&conn, CHAT).expect("read").expect("row");
    assert_eq!(first.delivered_max_id, 10 * STEP);
    assert_eq!(first.low_id, 10 * STEP);
    assert_eq!(first.updated_ms, NOW);
    // An older message (a gap's fetch, a history batch) lowers `low_id` only.
    commit_rows(
        &conn,
        &rows(vec![row(CHAT, &[4 * STEP, 7 * STEP])]),
        NOW + 5,
    )
    .expect("older");
    let after = chat(&conn, CHAT).expect("read").expect("row");
    assert_eq!(after.delivered_max_id, 10 * STEP, "the cursor moved down");
    assert_eq!(
        after.low_id,
        4 * STEP,
        "low_id did not follow the older rows"
    );
    assert_eq!(after.updated_ms, NOW, "updated_ms moved without the cursor");
    commit_rows(&conn, &rows(vec![row(CHAT, &[11 * STEP])]), NOW + 9).expect("newer");
    let newer = chat(&conn, CHAT).expect("read").expect("row");
    assert_eq!(newer.delivered_max_id, 11 * STEP);
    assert_eq!(newer.updated_ms, NOW + 9);
    // Read positions and the verified mark only move up too.
    for read in [9 * STEP, 3 * STEP] {
        let staged = Staged {
            chats: vec![ChatFacts {
                chat_id: CHAT,
                read_inbox_max_id: Some(read),
                verified_max_id: Some(read),
                ..ChatFacts::default()
            }],
            ..Staged::default()
        };
        commit_rows(&conn, &staged, NOW).expect("facts");
    }
    let read = chat(&conn, CHAT).expect("read").expect("row");
    assert_eq!(read.read_inbox_max_id, 9 * STEP);
    assert_eq!(read.verified_max_id, 9 * STEP);
}

#[test]
fn a_second_identical_patch_is_not_stated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let gated = |event: BackendEvent, gate: Gate| Staged {
        rows: vec![Row {
            events: vec![Event {
                event,
                gate: Some(gate),
            }],
            delivered: Vec::new(),
        }],
        ..Staged::default()
    };
    let first = commit_rows(&conn, &gated(patch("Ada Test"), Gate::Chat(CHAT)), NOW).expect("1");
    assert_eq!((first.seqs.len(), first.gated), (1, 0));
    let again = commit_rows(&conn, &gated(patch("Ada Test"), Gate::Chat(CHAT)), NOW).expect("2");
    assert_eq!(
        (again.seqs.len(), again.gated),
        (0, 1),
        "the same patch was stated twice"
    );
    let changed = commit_rows(&conn, &gated(patch("Bo Peer"), Gate::Chat(CHAT)), NOW).expect("3");
    assert_eq!(changed.seqs.len(), 1, "a changed patch was gated");
    let user_first =
        commit_rows(&conn, &gated(contact("Bo Peer"), Gate::User(CHAT)), NOW).expect("4");
    let user_again =
        commit_rows(&conn, &gated(contact("Bo Peer"), Gate::User(CHAT)), NOW).expect("5");
    assert_eq!(
        (user_first.seqs.len(), user_again.gated),
        (1, 1),
        "the user gate"
    );
    assert_eq!(outbox_len(&conn), 3);

    // A gated event inside a message's row is left out, the message is not.
    let mut mixed = row(CHAT, &[STEP]);
    mixed.events = vec![
        Event {
            event: patch("Bo Peer"),
            gate: Some(Gate::Chat(CHAT)),
        },
        Event {
            event: contact("Ada Test"),
            gate: None,
        },
    ];
    let done = commit_rows(&conn, &rows(vec![mixed]), NOW).expect("mixed");
    assert_eq!((done.seqs.len(), done.gated), (1, 1));
    let stored = kourier_db::outbox::peek(&conn, 10).expect("peek");
    let last = decode_outbox_row(&stored.last().expect("row").envelope).expect("decode");
    assert_eq!(
        last.len(),
        1,
        "the gated patch was written beside the message"
    );

    // A reset gate states the same patch again (the core lost the conversation, critic L6).
    reset_chat_gate(&conn, CHAT).expect("reset");
    let restated = commit_rows(&conn, &gated(patch("Bo Peer"), Gate::Chat(CHAT)), NOW).expect("6");
    assert_eq!(restated.seqs.len(), 1, "a reset gate still gated");
}

#[test]
fn the_sweep_keeps_every_unread_row_and_the_newest_200() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let old = NOW - DELIVERED_KEEP_MS - 1;
    // 300 incoming messages, all older than the window; read up to the 50th.
    let ids: Vec<i64> = (1..=300).map(|n| n * STEP).collect();
    commit_rows(&conn, &rows(vec![row(CHAT, &ids)]), old).expect("old rows");
    let read = Staged {
        chats: vec![ChatFacts {
            chat_id: CHAT,
            read_inbox_max_id: Some(50 * STEP),
            ..ChatFacts::default()
        }],
        ..Staged::default()
    };
    commit_rows(&conn, &read, NOW).expect("read");
    assert_eq!(delivered_unread(&conn, CHAT).expect("unread"), 250);
    let swept = sweep(&conn, NOW).expect("sweep");
    assert_eq!(swept, 50, "only read rows outside the newest 200 go");
    assert_eq!(
        delivered_unread(&conn, CHAT).expect("unread"),
        250,
        "an unread row left delivered: the hint would count it twice"
    );
    assert!(
        delivered_author(&conn, CHAT, 51 * STEP)
            .expect("read")
            .is_some()
    );
    assert!(
        delivered_author(&conn, CHAT, 50 * STEP)
            .expect("read")
            .is_none()
    );

    // Once read, everything but the newest 200 goes; a recent row stays whatever its rank.
    let all_read = Staged {
        chats: vec![ChatFacts {
            chat_id: CHAT,
            read_inbox_max_id: Some(300 * STEP),
            ..ChatFacts::default()
        }],
        ..Staged::default()
    };
    commit_rows(&conn, &all_read, NOW).expect("read all");
    assert_eq!(sweep(&conn, NOW).expect("sweep"), 50);
    assert!(
        delivered_author(&conn, CHAT, 100 * STEP)
            .expect("read")
            .is_none()
    );
    assert!(
        delivered_author(&conn, CHAT, 101 * STEP)
            .expect("read")
            .is_some(),
        "the 200th newest went"
    );
    assert_eq!(
        delivered_ids(&conn, CHAT, i64::MAX, 500)
            .expect("ids")
            .len(),
        200
    );
    commit_rows(&conn, &rows(vec![row(CHAT, &[STEP / 2])]), NOW).expect("recent");
    assert_eq!(
        sweep(&conn, NOW).expect("sweep"),
        0,
        "a recent row was swept"
    );
}

#[test]
fn a_gap_closes_only_with_its_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let open = Staged {
        gaps: vec![GapChange::Open {
            chat_id: SUPERGROUP,
            above: STEP,
            below: 6 * STEP,
            cause: GapCause::IdJump,
        }],
        ..Staged::default()
    };
    commit_rows(&conn, &open, NOW).expect("open");
    let mut fill = rows(vec![row(
        SUPERGROUP,
        &[2 * STEP, 3 * STEP, 4 * STEP, 5 * STEP],
    )]);
    fill.gaps.push(GapChange::Close {
        chat_id: SUPERGROUP,
        above: STEP,
    });
    refuse_appends(&conn);
    assert!(commit_rows(&conn, &fill, NOW).is_err());
    assert_eq!(
        gap_rows(&conn).expect("gaps").len(),
        1,
        "the gap closed without its rows"
    );
    conn.execute_batch("DROP TRIGGER temp.refuse_appends")
        .expect("drop");
    commit_rows(&conn, &fill, NOW).expect("fill");
    assert!(gap_rows(&conn).expect("gaps").is_empty());

    // Paged and lost: kept for KT-3 with its pages, never removed by the cap.
    let mut paged = open.clone();
    paged.gaps.push(GapChange::Paged {
        chat_id: SUPERGROUP,
        above: STEP,
        below: 3 * STEP,
    });
    paged.gaps.push(GapChange::Lost {
        chat_id: SUPERGROUP,
        above: STEP,
    });
    commit_rows(&conn, &paged, NOW).expect("paged");
    let gaps = gap_rows(&conn).expect("gaps");
    assert_eq!(gaps.len(), 1);
    assert_eq!(
        (gaps[0].below, gaps[0].pages, gaps[0].lost),
        (3 * STEP, 1, true)
    );
}

/// TF10 (review dF10): a LOST gap is never reopened by an `Open` of the same key; an open one is
/// still widened. (Mutant: the upsert without its `WHERE gap.state = 'open'`.)
#[test]
fn a_lost_gap_is_never_opened_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let open_gap = |below: i64| Staged {
        gaps: vec![GapChange::Open {
            chat_id: SUPERGROUP,
            above: STEP,
            below,
            cause: GapCause::IdJump,
        }],
        ..Staged::default()
    };
    commit_rows(&conn, &open_gap(6 * STEP), NOW).expect("open");
    commit_rows(&conn, &open_gap(9 * STEP), NOW).expect("widen");
    let gaps = gap_rows(&conn).expect("gaps");
    assert_eq!(
        (gaps[0].below, gaps[0].lost),
        (9 * STEP, false),
        "an open gap is widened"
    );
    let lost = Staged {
        gaps: vec![
            GapChange::Paged {
                chat_id: SUPERGROUP,
                above: STEP,
                below: 3 * STEP,
            },
            GapChange::Lost {
                chat_id: SUPERGROUP,
                above: STEP,
            },
        ],
        ..Staged::default()
    };
    commit_rows(&conn, &lost, NOW).expect("lost");
    commit_rows(&conn, &open_gap(20 * STEP), NOW).expect("open again");
    let gaps = gap_rows(&conn).expect("gaps");
    assert_eq!(gaps.len(), 1);
    assert_eq!(
        (gaps[0].below, gaps[0].pages, gaps[0].lost),
        (3 * STEP, 1, true),
        "a lost gap was reopened or widened"
    );
}

#[test]
fn a_delete_of_an_id_never_delivered_is_parked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    commit_rows(&conn, &rows(vec![row(CHAT, &[STEP])]), NOW).expect("one delivered");
    let staged = Staged {
        parked: vec![
            Parked::Delete {
                chat_id: CHAT,
                ids: vec![STEP],
            },
            Parked::Delete {
                chat_id: SUPERGROUP,
                ids: vec![7 * STEP, 8 * STEP],
            },
        ],
        ..Staged::default()
    };
    let done = commit_rows(&conn, &staged, NOW).expect("park");
    assert!(done.seqs.is_empty(), "a delete created a row");
    let parked = parked_rows(&conn).expect("parked");
    assert_eq!(
        parked.len(),
        2,
        "a delete of ids never delivered was dropped"
    );
    assert_eq!(parked[1].chat_id, SUPERGROUP);
    assert_eq!(parked[1].kind, "delete");
    assert_eq!(parked[1].map_version, MAP_VERSION);
    let payload: serde_json::Value = serde_json::from_str(&parked[1].payload).expect("json");
    assert_eq!(payload["ids"], serde_json::json!([7 * STEP, 8 * STEP]));
    assert!(
        chat(&conn, SUPERGROUP).expect("read").is_none(),
        "a parked delete created a chat"
    );
}

#[test]
fn a_run_is_unclean_until_its_clean_end_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    assert_eq!(start_run(&conn).expect("first"), PreviousRun::None);
    assert_eq!(start_run(&conn).expect("crash"), PreviousRun::Unclean);
    let end = Staged {
        clean_end: true,
        ..Staged::default()
    };
    commit_rows(&conn, &end, NOW).expect("end");
    assert_eq!(
        start_run(&conn).expect("after a clean end"),
        PreviousRun::Clean {
            ended_ms: Some(NOW)
        }
    );
    assert_eq!(start_run(&conn).expect("again"), PreviousRun::Unclean);
}

#[test]
fn chat_facts_are_kept_and_photos_cleared() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let facts = ChatFacts {
        chat_id: SUPERGROUP,
        kind: Some(ChatKind::Channel),
        listed: Some(true),
        held: Some(Some(Held::Channel)),
        photo_small: Some(Some("synthetic_1".to_owned())),
        moved_to: Some(7),
        ..ChatFacts::default()
    };
    commit_rows(
        &conn,
        &Staged {
            chats: vec![facts],
            ..Staged::default()
        },
        NOW,
    )
    .expect("facts");
    let row = chat(&conn, SUPERGROUP).expect("read").expect("row");
    assert_eq!(
        (row.kind, row.listed, row.held, row.moved_to),
        (ChatKind::Channel, true, Some(Held::Channel), Some(7))
    );
    assert_eq!(row.photo_small.as_deref(), Some("synthetic_1"));
    let clear = ChatFacts {
        chat_id: SUPERGROUP,
        held: Some(None),
        photo_small: Some(None),
        ..ChatFacts::default()
    };
    commit_rows(
        &conn,
        &Staged {
            chats: vec![clear],
            ..Staged::default()
        },
        NOW,
    )
    .expect("clear");
    let row = chat(&conn, SUPERGROUP).expect("read").expect("row");
    assert_eq!(
        (row.kind, row.held, row.photo_small),
        (ChatKind::Channel, None, None)
    );
    // Debug of the staged items prints no author or event content.
    let printed = format!("{:?}", rows(vec![row_with_author("tg:999068204719")]));
    assert!(!printed.contains("999068204719") && !printed.contains("Ada Test"));
}

fn row_with_author(author: &str) -> Row {
    let mut r = row(CHAT, &[STEP]);
    r.delivered[0].author = author.to_owned();
    r
}

#[test]
fn a_schema_1_file_is_brought_to_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("backend.db");
    let key = DbKey::from_bytes([7; 32]);
    {
        let mut conn = kourier_db::open(&path, &key).expect("raw open");
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        let v1 = kourier_db::Migrations::new("telegram", &[kourier_db::migration!(1, "0001_init")]);
        kourier_db::migrate(&mut conn, &v1).expect("v1");
    }
    let conn = db::open(&path, &key).expect("open at 2");
    let version: i64 = conn
        .query_row(
            "SELECT version FROM schema_version WHERE component = 'telegram'",
            [],
            |r| r.get(0),
        )
        .expect("version");
    assert_eq!(version, 2);
    commit_rows(&conn, &rows(vec![row(CHAT, &[STEP])]), NOW).expect("usable");
}

/// The migration is FROZEN once it ran on a kept run profile; it is pinned from its first commit.
/// A change is a NEW migration (0003), never an edit of this file.
#[test]
fn migration_2_is_frozen() {
    let sql = include_str!("../../migrations/0002_chats.sql");
    let hex: String = Sha256::digest(sql.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex, "7a2d4399c9408ad458b920864b546817ab2316d7d2f73c4fc4c087b802b9870f",
        "migrations/0002_chats.sql changed: write a new migration instead"
    );
    assert_eq!(db::MIGRATIONS.latest(), 2);
}

/// Critic L6 (TG-3 C5a): a `Restate` patch is written although its hash equals the stated one, and
/// records its hash, so the next gated patch with the same content is left out again.
#[test]
fn a_restated_patch_is_written_and_gates_the_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    let one = |gate: Gate| Staged {
        rows: vec![Row {
            events: vec![Event {
                event: patch("Ada Test"),
                gate: Some(gate),
            }],
            delivered: Vec::new(),
        }],
        ..Staged::default()
    };
    let first = commit_rows(&conn, &one(Gate::Chat(CHAT)), NOW).expect("1");
    assert_eq!(first.seqs.len(), 1);
    let restated = commit_rows(&conn, &one(Gate::Restate(CHAT)), NOW).expect("2");
    assert_eq!(
        (restated.seqs.len(), restated.gated),
        (1, 0),
        "a restated patch was gated"
    );
    let gated = commit_rows(&conn, &one(Gate::Chat(CHAT)), NOW).expect("3");
    assert_eq!(gated.gated, 1, "the restated patch did not record its hash");
    assert_eq!(outbox_len(&conn), 2);
}

/// TG-3 C8: the run's seed holds every chat's cursor and every delivered row.
#[test]
fn the_seed_reads_every_chat_and_every_delivered_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open(dir.path());
    assert!(seed(&conn).expect("seed").chats.is_empty());
    let staged = rows(vec![
        row(CHAT, &[2 * STEP, 3 * STEP]),
        row(SUPERGROUP, &[STEP]),
    ]);
    commit_rows(&conn, &staged, NOW).expect("commit");
    let s = seed(&conn).expect("seed");
    let cursors: Vec<(i64, i64)> = s
        .chats
        .iter()
        .map(|c| (c.chat_id, c.delivered_max_id))
        .collect();
    let mut want = vec![(CHAT, 3 * STEP), (SUPERGROUP, STEP)];
    want.sort_unstable();
    assert_eq!(cursors, want);
    assert_eq!(s.delivered.len(), 3);
    assert!(s.delivered.iter().all(|d| !d.outgoing));
}
