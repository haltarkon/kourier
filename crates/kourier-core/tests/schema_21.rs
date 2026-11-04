//! core.db schema 21 (P5a Verify, `0021_verified_notice.sql`): the quiet column learns the
//! verification notice, on a profile that schema 20 really left behind, through the real
//! migration runner.

use kourier_db::rusqlite::Connection;
use kourier_model::{Body, SystemEvent, UserId};

fn quiet_of(conn: &Connection, id: i64) -> i64 {
    conn.query_row("SELECT quiet FROM messages WHERE id = ?1", [id], |r| {
        r.get(0)
    })
    .expect("quiet")
}

fn insert(conn: &Connection, id: i64, body_kind: &str, body: &str) {
    conn.execute(
        "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
             origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (?1, 1, 1, 0, 'aci:b', ?1, 'Remote', ?1 * 10, ?1 * 10, ?1 * 10, ?2, ?3)",
        (id, body_kind, body),
    )
    .expect("message");
}

fn system(event: SystemEvent) -> String {
    serde_json::to_string(&Body::System(event)).expect("json")
}

fn verified(other_device: bool) -> SystemEvent {
    SystemEvent::VerifiedChanged {
        user: UserId::parse("aci:00000000-0000-4000-8000-0000000000b0").expect("user id"),
        verified: !other_device,
        other_device,
    }
}

/// A schema-20 file whose chat holds a verification notice (written by a build of this step
/// before its migration ran: a body the old column does not know), a safety-number notice and a
/// text migrates to 21: the verification notice is quiet now, the other rows keep their value,
/// no row is lost, and the column is still a VIRTUAL generated one (no stored copy to keep in
/// step). A row written after the migration is quiet too, in both device forms.
#[test]
fn the_quiet_column_learns_the_verified_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v20 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..20]);
    kourier_db::migrate(&mut conn, &v20).expect("schema 20");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');",
    )
    .expect("chat");
    let key_changed = system(SystemEvent::IdentityKeyChanged {
        user: UserId::parse("aci:00000000-0000-4000-8000-0000000000b0").expect("user id"),
    });
    insert(&conn, 1, "Text", r#"{"Text":{"body":"hi"}}"#);
    insert(&conn, 2, "System", &key_changed);
    insert(&conn, 3, "System", &system(verified(false)));
    assert_eq!(
        quiet_of(&conn, 3),
        0,
        "schema 20 does not know the kind (the test must see the change)"
    );

    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!((out.from, out.to), (20, kourier_core::MIGRATIONS.latest()));

    let n: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 3, "no row is lost");
    assert_eq!(quiet_of(&conn, 1), 0, "a text is not quiet");
    assert_eq!(
        quiet_of(&conn, 2),
        1,
        "the safety-number notice stays quiet"
    );
    assert_eq!(
        quiet_of(&conn, 3),
        1,
        "the verification notice is quiet after the migration"
    );
    insert(&conn, 4, "System", &system(verified(true)));
    assert_eq!(
        quiet_of(&conn, 4),
        1,
        "a notice from another device is quiet"
    );

    // `table_xinfo`'s `hidden` is 2 for a VIRTUAL generated column, 3 for a STORED one.
    let hidden: i64 = conn
        .query_row(
            "SELECT hidden FROM pragma_table_xinfo('messages') WHERE name = 'quiet'",
            [],
            |r| r.get(0),
        )
        .expect("the column");
    assert_eq!(hidden, 2, "quiet must stay a VIRTUAL generated column");
}
