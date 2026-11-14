//! 3D models (V2): `UiCommand::OpenModel` against the fake's synthetic CDN (an ASCII STL
//! downloads as a text pointer), the model mark of the timeline row, the refusals, and the
//! `ModelMesh` Debug.

use crate::support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{ModelMesh, Refusal, UiCommand, UiEvent};
use kourier_fake::attachments::{Pointer, synthetic_file};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script};
use kourier_model::{Attachment, AttachmentId, Body};
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{Env, add_linked};

/// A pyramid of four triangles, 20 × 10 × 5 mm.
const STL: &str = "solid t
facet normal 0 0 0
outer loop
vertex 0 0 0
vertex 20 0 0
vertex 0 10 0
endloop
endfacet
facet normal 0 0 0
outer loop
vertex 0 0 0
vertex 20 0 0
vertex 0 0 5
endloop
endfacet
facet normal 0 0 0
outer loop
vertex 20 0 0
vertex 0 10 0
vertex 0 0 5
endloop
endfacet
facet normal 0 0 0
outer loop
vertex 0 10 0
vertex 0 0 0
vertex 0 0 5
endloop
endfacet
endsolid t
";

fn model_attachment(name: &str, mime: &str, text: &str) -> Attachment {
    Attachment {
        size: text.len() as u64,
        remote: Some(
            Pointer::Text {
                text: text.to_owned(),
            }
            .to_blob(),
        ),
        ..synthetic_file(name, mime, text.len() as u64, 1)
    }
}

fn script(att: Attachment) -> Script {
    let mut m = remote(&d_alice(), &alice(), &(T0 + 1).to_string(), T0 + 1, "");
    m.body = Body::Media {
        attachments: vec![att],
        caption: None,
    };
    Script::new("model3d", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(vec![BackendEvent::Message(m)])
}

/// The attachment once it is downloaded and derived.
async fn derived(env: &Env) -> AttachmentId {
    let deadline = tokio::time::Instant::now() + support::WAIT * 3;
    loop {
        let row: Option<(i64, bool)> = env.peek(|c| {
            c.query_row(
                "SELECT id, file_id IS NOT NULL AND derived_at IS NOT NULL FROM attachments",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
        });
        if let Some((id, true)) = row {
            return AttachmentId(id);
        }
        assert!(tokio::time::Instant::now() < deadline, "no derived file");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn started(att: Attachment) -> (Env, AttachmentId) {
    let mut env = Env::new(Flavor::SignalLike, script(att)).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    let id = derived(&env).await;
    (env, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_model_answers_with_the_mesh_and_the_row_carries_the_mark() {
    let (mut env, att) = started(model_attachment("part.stl", "model/stl", STL)).await;
    // The still and the facts are in the row: a shade picture (no colours of its own).
    let (thumb, x, z, tris, tint): (bool, f64, f64, i64, i64) = env.peek(|c| {
        c.query_row(
            "SELECT thumb_file_id IS NOT NULL, model_x_mm, model_z_mm, model_tris, model_tint
             FROM attachments",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("row")
    });
    assert!(thumb);
    assert_eq!((x, z, tris, tint), (20.0, 5.0, 4, 1));
    // The timeline row carries the facts.
    let conv = env.peek(|c| {
        c.query_row("SELECT id FROM conversations", [], |r| {
            r.get(0).map(kourier_model::ConversationId)
        })
        .expect("conversation")
    });
    let rows = env.timeline(conv).await;
    let facts = rows
        .iter()
        .find_map(|r| match &r.body {
            Body::Media { attachments, .. } => attachments[0].model,
            _ => None,
        })
        .expect("model facts on the row");
    assert_eq!((facts.tris, facts.tint), (4, true));
    assert!((facts.y_mm - 10.0).abs() < 1e-3);

    env.handle
        .send(UiCommand::OpenModel { attachment: att })
        .expect("send");
    let mesh = env
        .event("the model opens", |e| match e {
            UiEvent::ModelOpened { attachment, mesh } if *attachment == att => Some(mesh.clone()),
            _ => None,
        })
        .await;
    assert_eq!(mesh.triangles, 4);
    assert_eq!(mesh.positions.len(), 36);
    assert!(mesh.colors.is_none());
    assert_eq!((mesh.min, mesh.max), ([0.0; 3], [20.0, 10.0, 5.0]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_a_hidden_message_or_a_plain_file_is_refused() {
    let (mut env, att) = started(model_attachment("part.stl", "model/stl", STL)).await;
    // The sweeper hid the message on an unconfirmed clock.
    {
        let db = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(support::CORE_KEY))
            .expect("open");
        db.execute("UPDATE messages SET expiry_hidden = 1", [])
            .expect("hide");
    }
    env.handle
        .send(UiCommand::OpenModel { attachment: att })
        .expect("send");
    let refusal = env
        .event("a refusal", |e| match e {
            UiEvent::ActionFailed { action, error, .. } if *action == "OpenModel" => {
                Some(error.refusal())
            }
            _ => None,
        })
        .await;
    assert_eq!(refusal, Some(Refusal::Gone));

    // A file that is no model is refused too.
    let (mut env, att) = started(model_attachment("notes.txt", "text/plain", "hello")).await;
    env.handle
        .send(UiCommand::OpenModel { attachment: att })
        .expect("send");
    env.event("a refusal", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "OpenModel" => Some(()),
        _ => None,
    })
    .await;
}

#[test]
fn the_model_mesh_debug_prints_counts_and_no_coordinate() {
    let mesh = ModelMesh {
        positions: vec![
            123.456, 7.25, 9.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, //
        ],
        colors: Some(vec![0xAABB_CCDD]),
        min: [123.456, 1.0, 2.0],
        max: [4.0, 5.0, 6.0],
        triangles: 1,
    };
    let mesh = std::sync::Arc::new(mesh);
    let text = format!(
        "{mesh:?} {:?}",
        UiEvent::ModelOpened {
            attachment: AttachmentId(1),
            mesh: mesh.clone(),
        }
    );
    assert!(text.contains("triangles: 1"), "{text}");
    for leak in ["123.456", "7.25", "9.5", "AABBCCDD", "2864434397"] {
        assert!(!text.contains(leak), "{text}");
    }
}
