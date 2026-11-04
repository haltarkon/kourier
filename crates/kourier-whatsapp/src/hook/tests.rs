//! The hook without a client (W3a, chunk C2): `Hook::commit` is everything `on_messages` does except
//! ending the client. Hand-built batches, a recording `Notes`.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_backend_api::FakeClock;
use kourier_db::DbKey;
use wacore::types::events::InboundMessage;
use wacore::types::message::{MessageInfo, MessageSource};
use waproto::whatsapp as wa;

use super::{Hook, Notes};
use crate::error::sql;
use crate::fault;
use crate::inbox::{Committed, Inbox};
use crate::store::StoreOwner;

#[derive(Default)]
struct Recorder {
    committed: AtomicUsize,
    inserted: AtomicUsize,
    failed: AtomicUsize,
}

impl Notes for Recorder {
    fn committed(&self, done: &Committed) {
        self.committed.fetch_add(1, Ordering::SeqCst);
        self.inserted.fetch_add(done.inserted, Ordering::SeqCst);
    }

    fn failed(&self) {
        self.failed.fetch_add(1, Ordering::SeqCst);
    }
}

fn item(id: &str, offline: bool) -> InboundMessage {
    let mut message = wa::Message::default();
    message.conversation = Some(format!("synthetic text {id}"));
    InboundMessage::builder()
        .message(Arc::new(message))
        .info(Arc::new(MessageInfo {
            id: id.into(),
            source: MessageSource {
                chat: "100@g.us".parse().expect("jid"),
                sender: "200@lid".parse().expect("jid"),
                is_group: true,
                ..MessageSource::default()
            },
            is_offline: offline,
            ..MessageInfo::default()
        }))
        .build()
}

async fn rows(owner: &StoreOwner) -> i64 {
    owner
        .store()
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))
                .map_err(sql("count rows"))
        })
        .await
        .expect("count")
}

fn hook() -> (tempfile::TempDir, StoreOwner, Hook, Arc<Recorder>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([4; 32]),
        "wa-hook",
    )
    .expect("open");
    let notes = Arc::new(Recorder::default());
    let inbox = Inbox::new(owner.store(), Arc::new(FakeClock::new(1_000_000)));
    (dir, owner, Hook::new(inbox, notes.clone()), notes)
}

#[tokio::test]
async fn a_committed_batch_is_in_the_inbox_before_the_hook_returns_and_the_notes_hear_of_it() {
    let _serial = fault::serial();
    let (_dir, owner, hook, notes) = hook();
    let done = hook
        .commit(&[item("A", false), item("B", true)])
        .await
        .expect("committed");
    assert_eq!((done.total, done.inserted), (2, 2));
    assert_eq!(
        rows(&owner).await,
        2,
        "durable by the time the hook returned"
    );
    assert_eq!(notes.committed.load(Ordering::SeqCst), 1);
    assert_eq!(notes.inserted.load(Ordering::SeqCst), 2);
    assert_eq!(notes.failed.load(Ordering::SeqCst), 0);
    owner.close();
}

#[tokio::test]
async fn a_failed_commit_is_an_error_writes_nothing_and_is_told_to_the_notes() {
    let _serial = fault::serial();
    let (_dir, owner, hook, notes) = hook();
    fault::arm(fault::INBOX_COMMIT, 1);
    let failed = hook.commit(&[item("A", false)]).await;
    let error = failed.expect_err("the hook must report a failed commit as Err, never Ok");
    assert_eq!(
        error.to_string(),
        "the inbox commit failed",
        "no input in the error"
    );
    assert_eq!(rows(&owner).await, 0);
    assert_eq!(
        (
            notes.committed.load(Ordering::SeqCst),
            notes.failed.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
    // The server sends the batch again: it lands once.
    hook.commit(&[item("A", false)])
        .await
        .expect("the redelivery commits");
    assert_eq!(rows(&owner).await, 1);
    owner.close();
}
