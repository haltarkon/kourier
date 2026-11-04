//! core.db schema 18 (people pages P1, `0018_people.sql`): the contact columns for the about text
//! and the number verdict, on a profile that schema 17 really left behind, through the real
//! migration runner.

use kourier_db::rusqlite::{Connection, ErrorCode};

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// A schema-17 file with contacts migrates to 18 with those rows unchanged, the new columns NULL
/// (never stated: the number is not shown); the verdict takes 0 and 1 and refuses anything else.
#[test]
fn schema_18_adds_the_contact_columns_and_changes_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v17 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..17]);
    kourier_db::migrate(&mut conn, &v17).expect("schema 17");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO contacts(id, account_id, user_id, profile_name)
           VALUES (1, 1, 'aci:a', 'Alice'), (2, 1, 'aci:b', NULL);",
    )
    .expect("contacts");

    let v18 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..18]);
    let out = kourier_db::migrate(&mut conn, &v18).expect("migrate");
    assert_eq!((out.from, out.to), (17, 18));

    assert_eq!(count(&conn, "SELECT count(*) FROM contacts"), 2);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM contacts
             WHERE about IS NULL AND about_emoji IS NULL AND number_shown IS NULL"
        ),
        2,
        "no row is converted: nothing was said yet"
    );
    assert_eq!(
        conn.query_row("SELECT profile_name FROM contacts WHERE id = 1", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("name"),
        "Alice"
    );
    for v in [0, 1] {
        conn.execute("UPDATE contacts SET number_shown = ?1 WHERE id = 1", [v])
            .expect("a known verdict");
    }
    for v in [-1, 2, 7] {
        let refused = conn.execute("UPDATE contacts SET number_shown = ?1 WHERE id = 1", [v]);
        assert!(
            matches!(
                refused,
                Err(kourier_db::rusqlite::Error::SqliteFailure(e, _))
                    if e.code == ErrorCode::ConstraintViolation
            ),
            "verdict {v} is refused: {refused:?}"
        );
    }
}
