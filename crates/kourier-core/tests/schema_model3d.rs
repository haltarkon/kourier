//! The 3D viewer's core.db step (V2, `00NN_model3d.sql`): a model that was downloaded and set
//! aside before the step is handed back to the derive step, on a profile that the step before it
//! really left behind, through the real migration runner. Pictures, plain files and what has a
//! still stay. The step is found by its name, so its number can change at a landing.

/// The step's name ends so (`kourier_core::MIGRATIONS`).
const STEP: &str = "_model3d";

/// The index of the step in `MIGRATIONS` (0-based).
fn step_index() -> usize {
    kourier_core::MIGRATIONS
        .steps
        .iter()
        .position(|m| m.name.ends_with(STEP))
        .expect("the step is registered")
}

fn derived_at(conn: &kourier_db::rusqlite::Connection, id: i64) -> Option<i64> {
    conn.query_row(
        "SELECT derived_at FROM attachments WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
    .expect("row")
}

#[test]
fn the_model_step_hands_old_models_back_to_the_derive_step_and_leaves_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).expect("open");
    let before =
        kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..step_index()]);
    kourier_db::migrate(&mut conn, &before).expect("the schema before the step");
    let msg = |id: i64, kind: &str| {
        format!(
            "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
                 origin, sort_key, sent_at, received_at, body_kind, body)
               VALUES ({id}, 1, 1, 0, 'aci:b', '{id}', 'Remote', {id}0, {id}0, {id}0, '{kind}', '{{}}');"
        )
    };
    conn.execute_batch(&format!(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');
         INSERT INTO files(id, account_id, path, key, size, purpose) VALUES
           (1, 1, 'a', x'00', 1, 'attachment'), (2, 1, 'b', x'00', 1, 'thumb');
         {}{}
         INSERT INTO attachments(id, message_id, idx, kind, mime, file_name, state, file_id,
                                 thumb_file_id, derived_at, long_text) VALUES
           -- 1: a model by its name; 2: a model by its type; 3: by its type with parameters
           (1, 1, 0, 'File', 'application/octet-stream', 'Part.STL', '\"Local\"', 1, NULL, 5, 0),
           (2, 1, 1, 'File', 'model/3mf', NULL, '\"Local\"', 1, NULL, 5, 0),
           (3, 1, 2, 'File', 'Application/SLA; charset=x', 'x', '\"Local\"', 1, NULL, 5, 0),
           -- 4: a plain file; 5: a picture named .stl; 6: a sticker; 7: has a still already;
           -- 8: not downloaded; 9: a long text named .stl
           (4, 1, 3, 'File', 'text/plain', 'notes.txt', '\"Local\"', 1, NULL, 5, 0),
           (5, 1, 4, 'Image', 'image/jpeg', 'photo.stl', '\"Local\"', 1, 2, 5, 0),
           (6, 2, 0, 'File', NULL, 'sticker.stl', '\"Local\"', 1, NULL, 5, 0),
           (7, 1, 5, 'File', 'model/stl', 'a.stl', '\"Local\"', 1, 2, 5, 0),
           (8, 1, 6, 'File', 'model/stl', 'b.stl', '\"Remote\"', NULL, NULL, 5, 0),
           (9, 1, 7, 'File', 'model/stl', 'c.stl', '\"Local\"', 1, NULL, 5, 1);",
        msg(1, "Media"),
        msg(2, "Sticker"),
    ))
    .expect("old rows");

    kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");

    for id in 1..=3 {
        assert_eq!(derived_at(&conn, id), None, "model {id} is to be derived");
    }
    for id in 4..=9 {
        assert_eq!(derived_at(&conn, id), Some(5), "row {id} is left alone");
    }
    let no_facts: i64 = conn
        .query_row(
            "SELECT count(*) FROM attachments WHERE model_x_mm IS NOT NULL OR model_tint IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(no_facts, 0);
}
