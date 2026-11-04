//! Protected chats, step S-P (architecture §3.4 "Protected chats", §8.7): a chat the user
//! protects on another device is listed and readable, and its messages show nowhere outside
//! the open chat. The core stores the backend's statement (core.db `conversations.protected`,
//! the migration found by its NAME, so the test does not depend on its number), changes it both ways, gives
//! the list row the flag and no preview, and still announces a new message (the notification
//! carries no text; the front-end shows the name and a count).

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{ConversationListQuery, ConversationRow, TimelineAnchor, TimelineQuery};
use kourier_core::testing::IngestHarness;
use kourier_db::rusqlite::Connection;
use kourier_fake::scripts::remote;
use kourier_model::{Body, ConvKey, ConversationPatch, MsgIdScope, NetworkKind, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

/// The step of this change, by the end of its name (its number changes at the landing).
const STEP: &str = "_protected_chats";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(uid(ALICE))
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([9; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn protect(h: &mut IngestHarness, state: Option<bool>) {
    let mut p = ConversationPatch::new(d_alice());
    p.protected = state;
    h.apply(vec![BackendEvent::ConversationUpsert(p)])
        .expect("upsert");
}

fn row(h: &IngestHarness) -> ConversationRow {
    let mut rows = h
        .conversations(&ConversationListQuery::default())
        .expect("list")
        .rows;
    assert_eq!(rows.len(), 1, "the one chat is listed");
    rows.remove(0)
}

fn stored(c: &Connection) -> i64 {
    c.query_row("SELECT protected FROM conversations", [], |r| r.get(0))
        .expect("protected")
}

/// The index of this step in `MIGRATIONS` (0-based), found by its name.
fn step_index() -> usize {
    kourier_core::MIGRATIONS
        .steps
        .iter()
        .position(|m| m.name.ends_with(STEP))
        .expect("the step is registered")
}

/// A file the step before left behind gets the column: its chat is not protected, a row written
/// without the column is not either, and the column takes only 0 and 1.
#[test]
fn the_migration_adds_the_flag_off_with_a_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([8; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let at = step_index();
    let steps: &'static [kourier_db::Migration] = kourier_core::MIGRATIONS.steps;
    let before = kourier_db::Migrations::new("core", &steps[..at]);
    kourier_db::migrate(&mut conn, &before).expect("the schema before the step");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:a', 'Direct');",
    )
    .expect("chat");
    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!(
        (out.from, out.to),
        (
            u32::try_from(at).expect("small"),
            kourier_core::MIGRATIONS.latest()
        )
    );
    assert_eq!(
        stored(&conn),
        0,
        "a chat stored before the step is not protected"
    );
    conn.execute(
        "INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (2, 1, 'aci:b', 'Direct')",
        [],
    )
    .expect("a chat without the column");
    let of_two: i64 = conn
        .query_row(
            "SELECT protected FROM conversations WHERE id = 2",
            [],
            |r| r.get(0),
        )
        .expect("protected");
    assert_eq!(of_two, 0, "the default");
    for bad in [2, -1] {
        let err = conn
            .execute(
                "UPDATE conversations SET protected = ?1 WHERE id = 1",
                [bad],
            )
            .expect_err("the check refuses it");
        assert!(err.to_string().contains("CHECK"), "{err}");
    }
    conn.execute("UPDATE conversations SET protected = 1 WHERE id = 1", [])
        .expect("1 is taken");
}

/// The backend's statement is stored, kept by a patch that says nothing, and lifted again; the
/// list row carries it, and a protected chat's row has NO preview (no text, author, kind or
/// time) while its timeline reads as any other.
#[test]
fn a_chat_is_protected_kept_and_lifted_and_its_row_has_no_preview() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![BackendEvent::Message(remote(
        &d_alice(),
        &uid(ALICE),
        "m1",
        1_700_000_000_000,
        "the plan",
    ))])
    .expect("a message");
    let open = row(&h);
    assert!(!open.protected && open.preview.is_some(), "not protected");

    protect(&mut h, Some(true));
    assert_eq!(stored(h.connection()), 1);
    let closed = row(&h);
    assert!(closed.protected, "the row says so");
    assert!(closed.preview.is_none(), "no preview leaves the core");
    assert_eq!(
        closed.last_activity, open.last_activity,
        "listed in its place"
    );
    let detail = h.conversation(closed.id).expect("query").expect("the chat");
    assert!(detail.row.protected && detail.row.preview.is_none());
    let page = h
        .timeline(&TimelineQuery {
            conv: closed.id,
            anchor: TimelineAnchor::Newest,
            limit: 10,
        })
        .expect("timeline");
    assert!(
        matches!(page.rows.as_slice(), [m] if matches!(&m.body, Body::Text(t) if t.body == "the plan")),
        "the open chat reads its messages"
    );

    protect(&mut h, None);
    assert!(row(&h).protected, "a patch that says nothing keeps it");
    // A patch about something else keeps it too (the full UPDATE writes the loaded value).
    let mut other = ConversationPatch::new(d_alice());
    other.archived = Some(false);
    h.apply(vec![BackendEvent::ConversationUpsert(other)])
        .expect("upsert");
    assert!(row(&h).protected, "another field's patch keeps it");

    protect(&mut h, Some(false));
    assert_eq!(stored(h.connection()), 0);
    let lifted = row(&h);
    assert!(
        !lifted.protected && lifted.preview.is_some(),
        "lifted: the preview is back"
    );
}

/// A new message of a protected chat is still announced (Threema Desktop notifies, without the
/// text): the core's announcement names the message only.
#[test]
fn a_protected_chat_still_announces_its_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    protect(&mut h, Some(true));
    let out = h
        .apply_announcing(vec![BackendEvent::Message(remote(
            &d_alice(),
            &uid(ALICE),
            "m1",
            1_700_000_000_000,
            "the plan",
        ))])
        .expect("a message");
    assert_eq!(out.notify.len(), 1, "announced");
}

/// Two chats of one person that turn out to be one (an identity merge): the joined chat is
/// protected when either was, so the messages of the protected side never show outside it; when
/// neither was, it is not, and keeps its preview.
#[test]
fn a_merge_keeps_the_chat_protected_when_either_side_was() {
    const ALICE_PNI: &str = "pni:00000000-0000-4000-8000-0000000a11ce";
    for protected_side in [Some(ALICE_PNI), Some(ALICE), None] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir);
        for (who, n) in [(ALICE_PNI, 1), (ALICE, 2)] {
            h.apply(vec![BackendEvent::Message(remote(
                &ConvKey::Direct(uid(who)),
                &uid(who),
                &format!("m{n}"),
                1_700_000_000_000 + n,
                "hello",
            ))])
            .expect("a message");
        }
        if let Some(side) = protected_side {
            let mut p = ConversationPatch::new(ConvKey::Direct(uid(side)));
            p.protected = Some(true);
            h.apply(vec![BackendEvent::ConversationUpsert(p)])
                .expect("upsert");
        }
        h.apply(vec![BackendEvent::IdentityMerged {
            from: uid(ALICE_PNI),
            into: uid(ALICE),
        }])
        .expect("merge");
        let joined = row(&h);
        let side = match protected_side {
            Some(ALICE) => "canonical",
            Some(_) => "alias",
            None => "neither",
        };
        assert_eq!(
            (joined.protected, joined.preview.is_none()),
            (protected_side.is_some(), protected_side.is_some()),
            "protected on the {side} side"
        );
    }
}
