//! The upload record (W5b, G4): read back, replaced by a later upload, reused only within 6 hours, swept after
//! 30 days, the attempt's one read, and the kill points of its zone (a child of this test binary aborted inside and
//! after the commit). Synthetic values only (tails of 8+ digits that are substrings of no other value).

use std::path::Path;

use kourier_backend_api::{OutgoingKind, OutgoingRequest};
use kourier_db::DbKey;
use kourier_db::rusqlite::params;
use kourier_model::{Body, ConvKey, MsgTarget, TargetAuthor, Text, UserId};

use super::{KEEP_MS, REUSE_MS, Stored, commit, key_of, look, put, sweep};
use crate::media::MediaPointer;
use crate::store::{KourierStore, StoreOwner};

const PEER_LID: &str = "77700163940285";
const NOW: i64 = 1_760_000_000_000;
const KEY: [u8; 32] = [23; 32];

fn conv() -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", PEER_LID).expect("a user"))
}

fn pointer(tail: &str, length: u64) -> MediaPointer {
    MediaPointer {
        media_type: "image".into(),
        direct_path: format!("/v/t62.7118-24/synthetic-path-{tail}"),
        media_key: format!("{tail:0>64}"),
        file_sha256: Some(format!("{tail:a>64}")),
        file_enc_sha256: Some(format!("{tail:b>64}")),
        file_length: length,
        media_key_timestamp: Some(1_760_000_123),
        static_url: None,
        thumb_jpeg: None,
        ..crate::media::MediaPointer::default()
    }
}

fn open(dir: &Path) -> StoreOwner {
    StoreOwner::open(&dir.join("backend.db"), &DbKey::from(KEY), "wa-record").expect("open")
}

async fn looked(store: &KourierStore, local: &str) -> Option<Stored> {
    let local = local.to_owned();
    store
        .read(move |conn| look(conn, &local))
        .await
        .expect("look")
}

async fn plant(store: &KourierStore, local: &str, pointer: MediaPointer, at: i64) {
    let local = local.to_owned();
    store
        .write("test: plant", move |tx| {
            put(tx, &conv(), &local, &pointer, at)
        })
        .await
        .expect("plant");
}

#[tokio::test]
async fn a_record_is_read_back_and_a_later_upload_of_the_same_message_replaces_it() {
    let dir = tempfile::tempdir().expect("dir");
    let owner = open(dir.path());
    let store = owner.store();
    assert_eq!(looked(&store, "3EB0C1D2E3F40571936284").await, None);
    commit(
        &store,
        conv(),
        "3EB0C1D2E3F40571936284".into(),
        pointer("40817263", 54_321),
        NOW,
    )
    .await
    .expect("commit");
    assert_eq!(
        looked(&store, "3EB0C1D2E3F40571936284").await,
        Some(Stored {
            pointer: pointer("40817263", 54_321),
            at_ms: NOW
        })
    );
    // A resend after six hours uploaded again: the new upload is the record, under the same local.
    commit(
        &store,
        conv(),
        "3EB0C1D2E3F40571936284".into(),
        pointer("59302847", 65_432),
        NOW + REUSE_MS,
    )
    .await
    .expect("commit again");
    assert_eq!(
        looked(&store, "3EB0C1D2E3F40571936284").await,
        Some(Stored {
            pointer: pointer("59302847", 65_432),
            at_ms: NOW + REUSE_MS
        })
    );
    assert_eq!(
        looked(&store, "3EB0FFFF000083920174").await,
        None,
        "another local"
    );
    // A row whose pointer does not read is no record: the file goes up again.
    store
        .write("test: spoil", |tx| {
            tx.execute(
                "UPDATE media_uploads SET pointer = x'7b7b' WHERE local = ?1",
                params!["3EB0C1D2E3F40571936284"],
            )
            .map_err(crate::error::sql("test: spoil"))
            .map(drop)
        })
        .await
        .expect("spoil");
    assert_eq!(looked(&store, "3EB0C1D2E3F40571936284").await, None);
    owner.close();
}

#[test]
fn a_resend_sends_a_record_again_only_within_six_hours_of_its_upload() {
    let stored = Stored {
        pointer: pointer("71829304", 1),
        at_ms: NOW,
    };
    assert!(stored.reusable(NOW), "at once");
    assert!(stored.reusable(NOW + REUSE_MS - 1), "just inside");
    assert!(!stored.reusable(NOW + REUSE_MS), "six hours: upload again");
    assert!(!stored.reusable(NOW + KEEP_MS), "a month later");
    assert!(!stored.reusable(NOW - 1), "a clock set back: upload again");
    assert_eq!(REUSE_MS, 6 * 60 * 60 * 1000);
}

#[tokio::test]
async fn records_older_than_thirty_days_go_at_a_sweep_and_younger_ones_stay() {
    let dir = tempfile::tempdir().expect("dir");
    let owner = open(dir.path());
    let store = owner.store();
    let day = 24 * 60 * 60 * 1000;
    for (local, at) in [
        ("3EB0OLD000000062918374", NOW - KEEP_MS - 1),
        ("3EB0EDGE00000073029485", NOW - KEEP_MS),
        ("3EB0MONTH0000084130596", NOW - 29 * day),
        ("3EB0DAYS00000095241607", NOW - 3 * day),
    ] {
        plant(&store, local, pointer("18293047", 7), at).await;
    }
    let gone = store
        .write("test: sweep", |tx| sweep(tx, NOW))
        .await
        .expect("sweep");
    assert_eq!(gone, 1);
    assert_eq!(looked(&store, "3EB0OLD000000062918374").await, None);
    for kept in [
        "3EB0EDGE00000073029485",
        "3EB0MONTH0000084130596",
        "3EB0DAYS00000095241607",
    ] {
        assert!(looked(&store, kept).await.is_some(), "{kept} is kept");
    }
    assert_eq!(KEEP_MS, 30 * day);
    owner.close();
}

fn req(local: &str, kind: OutgoingKind) -> OutgoingRequest {
    OutgoingRequest {
        conv: conv(),
        local: local.into(),
        sent_at: NOW,
        kind,
        only: None,
        is_recipient_update: false,
    }
}

fn target(local: &str) -> MsgTarget {
    MsgTarget {
        conv: Some(conv()),
        author: TargetAuthor::Own,
        local: local.into(),
        alternates: Vec::new(),
    }
}

fn files() -> Body {
    Body::Media {
        attachments: Vec::new(),
        caption: Some(Text::plain("synthetic caption")),
    }
}

#[test]
fn the_attempt_reads_a_file_messages_own_record_and_a_caption_edits_targets() {
    let message = |body| OutgoingKind::Message {
        body,
        reply_to: None,
        expiry: None,
    };
    assert_eq!(
        key_of(&req("3EB0F11E00000029384756", message(files()))),
        Some("3EB0F11E00000029384756".to_owned()),
        "a resend of a file message reads its own record"
    );
    assert_eq!(
        key_of(&req(
            "3EB0ED1700000038475610",
            OutgoingKind::Edit {
                target: target("3EB0F11E00000029384756"),
                body: files(),
            }
        )),
        Some("3EB0F11E00000029384756".to_owned()),
        "a caption edit reads the record of the file it edits"
    );
    for kind in [
        message(Body::Text(Text::plain("synthetic text"))),
        OutgoingKind::Edit {
            target: target("3EB0F11E00000029384756"),
            body: Body::Text(Text::plain("synthetic edited text")),
        },
        OutgoingKind::Delete {
            target: target("3EB0F11E00000029384756"),
            for_everyone: true,
        },
        OutgoingKind::React {
            target: target("3EB0F11E00000029384756"),
            emoji: "x".into(),
            remove: false,
        },
    ] {
        assert_eq!(key_of(&req("3EB0AAAA00000047561029", kind)), None);
    }
}

/// The kill points of the zone: a child of this test binary commits a record with a point armed and is aborted.
mod kill {
    use super::*;

    const CHILD: &str = "WA_RECORD_KILL_CHILD";

    /// The child: commits one record into the store in the directory it is given.
    #[tokio::test]
    async fn commit_child() {
        let Some(dir) = std::env::var_os(CHILD) else {
            return;
        };
        let owner = open(Path::new(&dir));
        commit(
            &owner.store(),
            conv(),
            "3EB0K1110000056172839".into(),
            pointer("26374859", 99),
            NOW,
        )
        .await
        .expect("commit");
        owner.close();
    }

    /// Runs the child with `point` armed; whether it was aborted.
    fn run_child(dir: &Path, point: &str) -> bool {
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "send::record::tests::kill::commit_child",
                "--nocapture",
            ])
            .env(CHILD, dir)
            .env("KOURIER_FAILPOINT", point)
            .output()
            .expect("spawn the child");
        !out.status.success()
    }

    fn rows(dir: &Path) -> i64 {
        let conn = kourier_db::open(&dir.join("backend.db"), &DbKey::from(KEY)).expect("open");
        conn.query_row("SELECT count(*) FROM media_uploads", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn a_kill_inside_the_zone_leaves_no_record_and_one_after_it_leaves_the_record() {
        for (point, left) in [
            (crate::failpoints::INSIDE_MEDIA_UPLOAD, 0),
            (crate::failpoints::AFTER_MEDIA_UPLOAD, 1),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            open(dir.path()).close();
            assert!(
                run_child(dir.path(), point),
                "{point} did not abort the child"
            );
            assert_eq!(rows(dir.path()), left, "{point}");
        }
    }
}
