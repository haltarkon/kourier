//! core.db schema 17 (upload progress, `0017_outbox_cancel.sql`): the durable mark of a stop on
//! the outbox rows, on a profile that schema 16 really left behind, through the real migration
//! runner.

use kourier_db::rusqlite::{Connection, ErrorCode};

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

fn insert_row(
    conn: &Connection,
    id: i64,
    message: i64,
    kind: &str,
    state: &str,
    attempts: i64,
) -> kourier_db::rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
           deadline_at) VALUES (?1, ?2, 1, ?3, '{\"x\":1}', ?4, ?5, 9000)",
        (id, message, kind, state, attempts),
    )
}

/// A schema-16 file with outbox rows of every state and kind migrates to 17 with those rows
/// unchanged and `cancel_state` 0 on all of them; the column takes 0, 1 and 2 and refuses
/// anything else, and a row written without it (every writer before the stop existed) is 0.
#[test]
fn schema_17_adds_the_stop_mark_and_changes_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v16 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..16]);
    kourier_db::migrate(&mut conn, &v16).expect("schema 16");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind)
           VALUES (1, 1, 'd:pn:1', 'Direct');",
    )
    .expect("account and chat");
    for message in 10..16_i64 {
        conn.execute(
            "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
               origin, sort_key, sent_at, received_at, body_kind, body, read_state, history, silent)
             VALUES (?1, 1, 1, 0, '', ?2, 'Own', 1000, 100, 100, 'Text', '{}', 'Read', 0, 0)",
            (message, message.to_string()),
        )
        .expect("message");
    }
    let rows = [
        (1, 10, "Message", "Queued", 0),
        (2, 11, "Message", "Staged", 0),
        (3, 12, "Message", "Transmitting", 1),
        (4, 13, "Message", "Failed", 3),
        (5, 14, "Edit", "Queued", 0),
        (6, 15, "Delete", "Transmitting", 2),
    ];
    for (id, message, kind, state, attempts) in rows {
        insert_row(&conn, id, message, kind, state, attempts).expect("outbox row");
    }
    let before: Vec<(i64, i64, String, String, String, i64)> = read_rows(&conn);
    assert_eq!(before.len(), 6);

    let v17 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..17]);
    let out = kourier_db::migrate(&mut conn, &v17).expect("migrate");
    assert_eq!((out.from, out.to), (16, 17));

    assert_eq!(read_rows(&conn), before, "no row is converted");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM outbox WHERE cancel_state = 0"),
        6,
        "and every row has no stop on record"
    );

    // The values the core writes, and nothing else.
    for mark in [0, 1, 2] {
        conn.execute("UPDATE outbox SET cancel_state = ?1 WHERE id = 3", [mark])
            .expect("a known mark");
    }
    for mark in [-1, 3, 9] {
        let refused = conn.execute("UPDATE outbox SET cancel_state = ?1 WHERE id = 3", [mark]);
        assert!(
            matches!(
                refused,
                Err(kourier_db::rusqlite::Error::SqliteFailure(e, _))
                    if e.code == ErrorCode::ConstraintViolation
            ),
            "mark {mark} is refused: {refused:?}"
        );
    }
    assert!(
        conn.execute("UPDATE outbox SET cancel_state = NULL WHERE id = 3", [])
            .is_err(),
        "NOT NULL"
    );
    // A writer that does not know the column (an older enqueue statement) gets 0.
    conn.execute(
        "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body, read_state, history, silent)
         VALUES (30, 1, 1, 0, '', '30', 'Own', 1000, 100, 100, 'Text', '{}', 'Read', 0, 0)",
        [],
    )
    .expect("a message");
    insert_row(&conn, 7, 30, "Message", "Queued", 0).expect("a row without the column");
    assert_eq!(
        count(&conn, "SELECT cancel_state FROM outbox WHERE id = 7"),
        0
    );
}

fn read_rows(conn: &Connection) -> Vec<(i64, i64, String, String, String, i64)> {
    let mut q = conn
        .prepare("SELECT id, message_id, kind, state, request, attempts FROM outbox ORDER BY id")
        .expect("prepare");
    q.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .expect("rows")
    .collect::<Result<_, _>>()
    .expect("collect")
}
