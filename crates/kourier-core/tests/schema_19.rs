//! core.db schema 19 (people pages P2, `0019_group_page.sql`): the group's description column, on
//! a profile that schema 18 really left behind, through the real migration runner.

use kourier_db::rusqlite::Connection;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// A schema-18 file with a group and a direct chat migrates to 19 with the rows unchanged and the
/// new column NULL (never stated: the page shows no description until the backend says one).
#[test]
fn schema_19_adds_the_description_column_and_changes_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v18 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..18]);
    kourier_db::migrate(&mut conn, &v18).expect("schema 18");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind, title)
           VALUES (1, 1, 'gv2:abc', 'Group', 'Hiking'), (2, 1, 'aci:b', 'Direct', NULL);",
    )
    .expect("conversations");

    let v19 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..19]);
    let out = kourier_db::migrate(&mut conn, &v19).expect("migrate");
    assert_eq!((out.from, out.to), (18, 19));

    assert_eq!(count(&conn, "SELECT count(*) FROM conversations"), 2);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM conversations WHERE description IS NULL"
        ),
        2,
        "no row is converted: nothing was said yet"
    );
    assert_eq!(
        conn.query_row("SELECT title FROM conversations WHERE id = 1", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("title"),
        "Hiking",
        "and a row keeps what it had"
    );
    conn.execute(
        "UPDATE conversations SET description = 'Meet at six' WHERE id = 1",
        [],
    )
    .expect("the column takes text");
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM conversations WHERE description = 'Meet at six'"
        ),
        1
    );
}
