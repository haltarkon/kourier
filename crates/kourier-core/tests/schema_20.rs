//! core.db schema 20 (people pages P4, `0020_chat_media.sql`): the two flags of the gallery on
//! `messages`, their two PARTIAL indexes and the triggers that keep the flags, on a profile that
//! schema 19 really left behind, through the real migration runner.

use kourier_db::rusqlite::Connection;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// The flags `(media_visual, media_files)` of message `id`.
fn flags(conn: &Connection, id: i64) -> (i64, i64) {
    conn.query_row(
        "SELECT media_visual, media_files FROM messages WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("flags")
}

fn message(id: i64, body_kind: &str) -> String {
    format!(
        "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
             origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES ({id}, 1, 1, 0, 'aci:b', '{id}', 'Remote', {id}0, {id}0, {id}0, '{body_kind}', '{{}}');"
    )
}

/// A schema-19 file with a chat of texts, pictures, files, a voice note, a long text's file and a
/// sticker migrates to 20 with every row unchanged, the flags of the rows that have attachments
/// SET by the migration (the rows are converted: the new indexes would miss them otherwise), the
/// two indexes PARTIAL (a plain index over every message would walk the other kind's rows) and
/// holding the media rows of their own kind only (a sticker is no `Media` row).
#[test]
fn schema_20_sets_the_flags_of_the_rows_that_exist_and_builds_two_partial_indexes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v19 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..19]);
    kourier_db::migrate(&mut conn, &v19).expect("schema 19");
    conn.execute_batch(&format!(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');
         {}{}{}{}{}{}
         INSERT INTO attachments(message_id, idx, kind, state) VALUES
           (2, 0, 'Image', '\"Local\"'), (2, 1, 'File', '\"Local\"'),
           (3, 0, 'Voice', '\"Local\"'),
           (5, 0, 'Video', '\"Remote\"'),
           (6, 0, 'Image', '\"Local\"');
         INSERT INTO attachments(message_id, idx, kind, state, long_text)
           VALUES (4, 0, 'File', '\"Local\"', 1);",
        message(1, "Text"),
        message(2, "Media"),
        message(3, "Media"),
        message(4, "Media"),
        message(5, "Media"),
        message(6, "Sticker"),
    ))
    .expect("rows");

    let v20 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..20]);
    let out = kourier_db::migrate(&mut conn, &v20).expect("migrate");
    assert_eq!((out.from, out.to), (19, 20));

    assert_eq!(count(&conn, "SELECT count(*) FROM messages"), 6);
    assert_eq!(count(&conn, "SELECT count(*) FROM attachments"), 6);
    assert_eq!(flags(&conn, 1), (0, 0), "a text has none");
    assert_eq!(flags(&conn, 2), (1, 1), "a picture and a file: both");
    assert_eq!(flags(&conn, 3), (0, 0), "a voice note is neither");
    assert_eq!(
        flags(&conn, 4),
        (0, 0),
        "the rest of a long text is neither"
    );
    assert_eq!(flags(&conn, 5), (1, 0), "a video is visual");
    assert_eq!(
        flags(&conn, 6),
        (1, 0),
        "a sticker's picture sets the flag ..."
    );

    for (name, flag) in [
        ("messages_media_visual", "media_visual"),
        ("messages_media_files", "media_files"),
    ] {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [name],
                |r| r.get(0),
            )
            .expect("the index");
        let sql = sql.replace('\n', " ");
        assert!(
            sql.contains(&format!("WHERE body_kind = 'Media' AND {flag} = 1")),
            "a partial index over the media rows of its kind: {sql}"
        );
    }
    // ... but it is no row of an index: the indexes hold `Media` rows only.
    let held = |index: &str, flag: &str| {
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM messages INDEXED BY {index}
                 WHERE conversation_id = 1 AND body_kind = 'Media' AND {flag} = 1"
            ),
        )
    };
    assert_eq!(
        held("messages_media_visual", "media_visual"),
        2,
        "messages 2 and 5"
    );
    assert_eq!(held("messages_media_files", "media_files"), 1, "message 2");
}

/// The flags follow the attachments in the same transaction, whoever writes them: an insert, a
/// delete (also the cascade of a deleted message), a kind that changes, the rest of a long text
/// that becomes a file (`long_text` set or cleared) and an attachment that moves to another
/// message (a merge of two conversations).
#[test]
fn the_flags_follow_the_attachments_written_changed_moved_and_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    conn.execute_batch(&format!(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');
         {}{}",
        message(1, "Media"),
        message(2, "Media"),
    ))
    .expect("rows");
    assert_eq!(flags(&conn, 1), (0, 0), "no attachment yet");

    // An insert sets the flag of its kind.
    conn.execute(
        "INSERT INTO attachments(message_id, idx, kind, state) VALUES (1, 0, 'Image', '\"Local\"')",
        [],
    )
    .expect("a picture");
    assert_eq!(flags(&conn, 1), (1, 0));
    conn.execute(
        "INSERT INTO attachments(message_id, idx, kind, state) VALUES (1, 1, 'Audio', '\"Local\"')",
        [],
    )
    .expect("an audio file");
    assert_eq!(flags(&conn, 1), (1, 1), "an audio file is a file");

    // A kind that changes (the core sniffs a better one).
    conn.execute(
        "UPDATE attachments SET kind = 'File' WHERE message_id = 1 AND idx = 0",
        [],
    )
    .expect("kind");
    assert_eq!(
        flags(&conn, 1),
        (0, 1),
        "the picture became a file: no visual left"
    );

    // The rest of a long text is neither, and becomes one when the flag goes.
    conn.execute(
        "UPDATE attachments SET long_text = 1 WHERE message_id = 1",
        [],
    )
    .expect("long text");
    assert_eq!(flags(&conn, 1), (0, 0));
    conn.execute(
        "UPDATE attachments SET long_text = 0 WHERE message_id = 1 AND idx = 1",
        [],
    )
    .expect("not a long text");
    assert_eq!(flags(&conn, 1), (0, 1));

    // An attachment moves to another message: both flags follow.
    conn.execute(
        "UPDATE attachments SET message_id = 2, idx = 0 WHERE message_id = 1 AND idx = 1",
        [],
    )
    .expect("moved");
    assert_eq!(
        flags(&conn, 1),
        (0, 0),
        "what stays of message 1 is the rest of a long text: neither"
    );
    assert_eq!(flags(&conn, 2), (0, 1), "and message 2 has one now");

    // A delete clears the flag.
    conn.execute("DELETE FROM attachments WHERE message_id = 2", [])
        .expect("deleted");
    assert_eq!(flags(&conn, 2), (0, 0));
    // The cascade of a deleted message leaves nothing behind that a statement could meet.
    conn.execute("DELETE FROM messages WHERE id = 1", [])
        .expect("message deleted");
    assert_eq!(count(&conn, "SELECT count(*) FROM attachments"), 0);

    // The rest of a long text is written as a row of its own (the core's ingest): neither flag, whatever its kind.
    conn.execute_batch(&format!(
        "{}INSERT INTO attachments(message_id, idx, kind, long_text, state) VALUES (4, 0, 'File', 1, '\"Local\"');",
        message(4, "Media")
    ))
    .expect("rest of a long text");
    assert_eq!(flags(&conn, 4), (0, 0), "a long text's rest is no file");
    conn.execute(
        "INSERT INTO attachments(message_id, idx, kind, long_text, state) VALUES (4, 1, 'Image', 1, '\"Local\"')",
        [],
    )
    .expect("a flagged picture");
    assert_eq!(flags(&conn, 4), (0, 0), "nor is it a picture");

    // Only a `Media` message with the flag is in an index: a message of another kind with a picture is not.
    conn.execute_batch(&format!(
        "{}INSERT INTO attachments(message_id, idx, kind, state) VALUES (3, 0, 'Image', '\"Local\"');",
        message(3, "Sticker")
    ))
    .expect("sticker");
    assert_eq!(flags(&conn, 3), (1, 0));
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM messages INDEXED BY messages_media_visual
             WHERE conversation_id = 1 AND body_kind = 'Media' AND media_visual = 1"
        ),
        0,
        "a sticker is in no gallery index"
    );
}
