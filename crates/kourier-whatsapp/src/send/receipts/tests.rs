//! The receipts we send (W5a, chunk 11; D10, D7, D20): the job and its confirmation in one zone, the runner's
//! write-then-delete order, the calls of a group (one per author as sender, in the group's addressing mode), and
//! typing only into a chat the phone started. Over a [`FakeSession`] and a real store; synthetic ids only (the
//! fictitious 555-01xx range and made-up LIDs, each tail of 8 digits a substring of no other value).

use std::sync::Arc;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::DbKey;
use kourier_model::{CmdId, ConvKey, MsgTarget, ReceiptKind, TargetAuthor, UserId};
use wacore_binary::{Jid, Server};

use super::{
    Drained, JOB_KEEP_MS, Persisted, Privacy, confirm, confirm_read_marker, drain, persist, sweep,
    typing,
};
use crate::map::OwnIds;
use crate::send::address::Namespace;
use crate::send::attempt::Gate;
use crate::send::hold::Recheck;
use crate::send::started::{self, How};
use crate::session::fake::{Call, FakeSession};
use crate::session::{ReadKind, SendFailure, Session};
use crate::store::{KourierStore, StoreOwner, rows};

const PEER_PN: &str = "15550149263817";
const NEW_PN: &str = "15550186427395";
const AUTHOR_PN: &str = "15550127384956";
const AUTHOR_LID: &str = "77700146203958";
const OTHER_LID: &str = "77700135926048";
const OWN_PN: &str = "15550190573826";
const OWN_LID: &str = "77700158240739";
const GROUP: &str = "120363000064819527";
const MSG_A: &str = "3EB0D5A19C7E42F68013";
const MSG_B: &str = "3EB0F2C84B6A19D57024";
const MSG_C: &str = "3EB0C9E17D5A42B68035";
const MSG_OWN: &str = "3EB0A6B23F8C14E79046";
const NOW: i64 = 1_761_600_000_000;

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a synthetic user")
}

fn dm(pn: &str) -> ConvKey {
    ConvKey::Direct(user("pn", pn))
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: Some(user("pn", OWN_PN)),
    }
}

fn target(author: TargetAuthor, local: &str) -> MsgTarget {
    MsgTarget {
        conv: None,
        author,
        local: local.into(),
        alternates: Vec::new(),
    }
}

fn by(scheme: &str, value: &str) -> TargetAuthor {
    TargetAuthor::User(user(scheme, value))
}

struct Rig {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    session: Arc<FakeSession>,
    /// The runner's memory of the live connection's setting (fix F5).
    privacy: Privacy,
}

impl Rig {
    /// A store whose chat with [`PEER_PN`] the phone started.
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("dir");
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &DbKey::from([12; 32]),
            "wa-receipts",
        )
        .expect("open");
        let peer = dm(PEER_PN);
        owner
            .store()
            .write("start", move |tx| started::note(tx, &peer, How::Peer))
            .await
            .expect("note");
        Self {
            _dir: dir,
            owner,
            session: Arc::new(FakeSession::new()),
            privacy: Privacy::default(),
        }
    }

    fn store(&self) -> KourierStore {
        self.owner.store()
    }

    async fn jobs(&self) -> Vec<i64> {
        self.store()
            .read(|conn| {
                rows(
                    conn,
                    "test: jobs",
                    "SELECT cmd FROM receipt_jobs ORDER BY cmd",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .expect("jobs")
    }

    /// The commands `core_outbox` confirms, in order.
    async fn confirmed(&self) -> Vec<i64> {
        let found: Vec<Vec<u8>> = self
            .store()
            .read(|conn| {
                rows(
                    conn,
                    "test: outbox",
                    "SELECT envelope FROM core_outbox ORDER BY seq",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .expect("outbox");
        found
            .iter()
            .flat_map(|row| decode_outbox_row(row).expect("a row"))
            .filter_map(|event| match event {
                BackendEvent::CommandPersisted { cmd } => Some(cmd.0),
                _ => None,
            })
            .collect()
    }

    async fn drain(&self) -> Drained {
        drain(&self.store(), self.session.as_ref(), &own(), &self.privacy).await
    }
}

/// D10: a read is ONE zone with its job and its `CommandPersisted`; when the job cannot be written the command is
/// not confirmed either (the core keeps its row and sends it again). Mutant: the confirmation in a zone of its own
/// before the job's.
#[tokio::test]
async fn a_read_is_one_zone_with_its_job_and_its_confirmation() {
    let rig = Rig::new().await;
    let persisted = persist(
        &rig.store(),
        CmdId(51),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_A)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    assert_eq!(
        persisted,
        Persisted::Job,
        "a read of the peer's message is due"
    );
    assert_eq!(rig.jobs().await, vec![51], "the job is kept");
    assert_eq!(rig.confirmed().await, vec![51], "the command is confirmed");
    rig.store()
        .write("test: break the jobs", |tx| {
            tx.execute_batch("DROP TABLE receipt_jobs")
                .map_err(crate::error::sql("test: drop"))
        })
        .await
        .expect("dropped");
    let failed = persist(
        &rig.store(),
        CmdId(52),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_B)],
        ReceiptKind::Read,
        NOW,
    )
    .await;
    assert!(
        failed.is_err(),
        "a job that could not be written is an error"
    );
    assert_eq!(
        rig.confirmed().await,
        vec![51],
        "a command whose job was not written must not be confirmed"
    );
}

/// D10 and D7: a delivery (upstream's own), a read of our own messages only, and a read into a direct chat the
/// phone never started write NO job (nothing goes out) and are confirmed. Mutant: D7's look removed.
#[tokio::test]
async fn no_job_for_a_delivery_our_own_messages_or_a_chat_the_phone_never_started() {
    let rig = Rig::new().await;
    let store = rig.store();
    let delivered = persist(
        &store,
        CmdId(61),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_A)],
        ReceiptKind::Delivered,
        NOW,
    )
    .await
    .expect("persisted");
    let own_only = persist(
        &store,
        CmdId(62),
        dm(PEER_PN),
        vec![target(TargetAuthor::Own, MSG_OWN)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    let not_started = persist(
        &store,
        CmdId(63),
        dm(NEW_PN),
        vec![target(by("pn", NEW_PN), MSG_B)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    assert_eq!(delivered, Persisted::NothingDue);
    assert_eq!(own_only, Persisted::NothingDue);
    assert_eq!(
        not_started,
        Persisted::NotStarted,
        "a chat the phone never started gets no receipt (D7)"
    );
    assert!(rig.jobs().await.is_empty(), "no job was written");
    assert_eq!(rig.confirmed().await, vec![61, 62, 63], "each is confirmed");
    assert_eq!(rig.drain().await, Drained::All);
    assert!(rig.session.receipts().is_empty(), "nothing went out");
}

/// D10: the runner deletes a job only after its write returned Ok; a failed write keeps it for the next drain, which
/// sends it again (harmless). A 1:1 read has no `sender`. Mutant: the job deleted before the write.
#[tokio::test]
async fn a_job_is_deleted_only_after_its_write_returned_ok() {
    let rig = Rig::new().await;
    persist(
        &rig.store(),
        CmdId(71),
        dm(PEER_PN),
        vec![
            target(by("pn", PEER_PN), MSG_A),
            target(by("pn", PEER_PN), MSG_B),
        ],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    rig.session.script_receipt(Err(SendFailure::NotConnected));
    assert_eq!(rig.drain().await, Drained::Stopped, "the write failed");
    assert_eq!(
        rig.jobs().await,
        vec![71],
        "a job whose write failed is kept"
    );
    assert_eq!(rig.drain().await, Drained::All);
    assert!(rig.jobs().await.is_empty(), "a sent job is deleted");
    let receipts = rig.session.receipts();
    assert_eq!(receipts.len(), 2, "the kept job was sent again");
    let last = &receipts[1];
    assert_eq!(last.chat, Jid::new(PEER_PN, Server::Pn));
    assert_eq!(last.sender, None, "a 1:1 read names no sender");
    assert_eq!(last.ids, vec![MSG_A.to_owned(), MSG_B.to_owned()]);
    assert_eq!(last.kind, ReadKind::Read);
    assert_eq!(rig.drain().await, Drained::All);
    assert_eq!(
        rig.session.receipts().len(),
        2,
        "nothing is sent twice after Ok"
    );
}

/// Fix F5 (privacy LOW): before the FIRST 1:1 `read`/`played` write of a connection the runner has the session make
/// the phone's read-receipt setting known (upstream's `read`/`read-self` choice reads it); a setting that could not
/// be asked writes nothing and keeps the job; once known it is not asked again on that connection, and a new
/// connection asks again. Mutant: the receipt written without asking.
#[tokio::test]
async fn a_1_1_receipt_waits_until_the_connections_read_receipt_setting_is_known() {
    let rig = Rig::new().await;
    persist(
        &rig.store(),
        CmdId(71),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_A)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    rig.session.script_privacy(Err(SendFailure::IqTimeout));
    assert_eq!(
        rig.drain().await,
        Drained::Stopped,
        "the setting is unknown"
    );
    assert!(
        rig.session.receipts().is_empty(),
        "a receipt was written on a setting nobody knew"
    );
    assert_eq!(
        rig.jobs().await,
        vec![71],
        "the job waits for the next drain"
    );

    assert_eq!(rig.drain().await, Drained::All);
    assert_eq!(rig.session.receipts().len(), 1);
    let calls = rig.session.calls();
    let asked = calls
        .iter()
        .rposition(|call| *call == Call::PrivacyKnown)
        .expect("the setting was asked");
    let written = calls
        .iter()
        .position(|call| matches!(call, Call::MarkRead(..)))
        .expect("the receipt was written");
    assert!(
        asked < written,
        "the receipt was written before the setting was known"
    );
    assert_eq!(rig.session.count(&Call::PrivacyKnown), 2);

    // Known for the rest of the connection: a second job asks nothing.
    persist(
        &rig.store(),
        CmdId(72),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_B)],
        ReceiptKind::Viewed,
        NOW,
    )
    .await
    .expect("persisted");
    assert_eq!(rig.drain().await, Drained::All);
    assert_eq!(rig.session.receipts().len(), 2);
    assert_eq!(
        rig.session.count(&Call::PrivacyKnown),
        2,
        "the setting was asked again on the same connection"
    );

    // A new connection asks again.
    rig.privacy.forget();
    persist(
        &rig.store(),
        CmdId(73),
        dm(PEER_PN),
        vec![target(by("pn", PEER_PN), MSG_C)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    assert_eq!(rig.drain().await, Drained::All);
    assert_eq!(rig.session.count(&Call::PrivacyKnown), 3);
}

/// Fix F5: a group receipt is always `read` whatever the setting, so it does not wait for it (and a setting that
/// cannot be asked does not hold a group's receipt back). Mutant: the wait for every chat kind.
#[tokio::test]
async fn a_group_receipt_does_not_wait_for_the_read_receipt_setting() {
    let rig = Rig::new().await;
    persist(
        &rig.store(),
        CmdId(74),
        group(),
        vec![target(by("pn", AUTHOR_PN), MSG_A)],
        ReceiptKind::Read,
        NOW,
    )
    .await
    .expect("persisted");
    rig.session.script_privacy(Err(SendFailure::IqTimeout));
    rig.session.script_group_mode(Ok(Namespace::Pn));
    assert_eq!(rig.drain().await, Drained::All);
    assert_eq!(
        rig.session.receipts().len(),
        1,
        "the group receipt went out"
    );
    assert_eq!(
        rig.session.count(&Call::PrivacyKnown),
        0,
        "a group receipt asked for the 1:1 setting"
    );
}

/// D10, D20: a group's view is one `played` per AUTHOR with the author as `sender` in the group's addressing mode
/// (a number author with a known LID is written as the LID in a LID group); our own message gets none. Mutants: one
/// call for every author; the author written as the core holds it.
#[tokio::test]
async fn a_group_view_is_one_played_per_author_in_the_groups_mode() {
    let rig = Rig::new().await;
    rig.store()
        .write("test: a pair", |tx| {
            crate::store::exec(
                tx,
                "test: pair",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 1, 'test')",
                [AUTHOR_LID, AUTHOR_PN],
            )
        })
        .await
        .expect("pair");
    persist(
        &rig.store(),
        CmdId(81),
        group(),
        vec![
            target(by("pn", AUTHOR_PN), MSG_A),
            target(by("lid", OTHER_LID), MSG_B),
            target(by("pn", AUTHOR_PN), MSG_C),
            target(TargetAuthor::Own, MSG_OWN),
        ],
        ReceiptKind::Viewed,
        NOW,
    )
    .await
    .expect("persisted");
    rig.session.script_group_mode(Ok(Namespace::Lid));
    assert_eq!(rig.drain().await, Drained::All);
    let mut receipts = rig.session.receipts();
    receipts.sort_by_key(|r| r.ids.clone());
    assert_eq!(receipts.len(), 2, "one call per author: {}", receipts.len());
    for receipt in &receipts {
        assert_eq!(receipt.chat, Jid::group(GROUP));
        assert_eq!(receipt.kind, ReadKind::Played, "a view is played");
    }
    assert_eq!(receipts[0].ids, vec![MSG_A.to_owned(), MSG_C.to_owned()]);
    assert_eq!(
        receipts[0].sender,
        Some(Jid::new(AUTHOR_LID, Server::Lid)),
        "the number author is written in the group's LID mode"
    );
    assert_eq!(receipts[1].ids, vec![MSG_B.to_owned()]);
    assert_eq!(receipts[1].sender, Some(Jid::new(OTHER_LID, Server::Lid)));
    assert!(rig.jobs().await.is_empty());
}

/// D10: `SyncReadMarker` is noted for the history hint and confirmed in one zone; `DeleteAttachment` is confirmed.
#[tokio::test]
async fn a_read_marker_is_noted_and_confirmed_and_a_deleted_attachment_confirmed() {
    let rig = Rig::new().await;
    confirm_read_marker(&rig.store(), CmdId(91), &dm(PEER_PN), NOW)
        .await
        .expect("confirmed");
    confirm(&rig.store(), CmdId(92), NOW)
        .await
        .expect("confirmed");
    assert_eq!(rig.confirmed().await, vec![91, 92]);
    let key = dm(PEER_PN).encode();
    let noted: Vec<i64> = rig
        .store()
        .read(move |conn| {
            rows(
                conn,
                "test: read here",
                "SELECT read_here FROM history_chats WHERE conv = ?1",
                [key],
                |r| r.get(0),
            )
        })
        .await
        .expect("read");
    assert_eq!(noted, vec![1], "the read is noted for the history hint");
}

/// D7: a typing state goes to a chat the phone started and never to one it did not. Mutant: the look removed.
#[tokio::test]
async fn typing_goes_only_into_a_chat_the_phone_started() {
    let rig = Rig::new().await;
    let session: Arc<dyn Session> = rig.session.clone();
    typing(
        rig.store(),
        session.clone(),
        own(),
        dm(NEW_PN),
        true,
        Recheck::fixed(Gate::Open),
    )
    .await;
    assert!(
        rig.session.typed().is_empty(),
        "no typing into a chat the phone never started"
    );
    typing(
        rig.store(),
        session,
        own(),
        dm(PEER_PN),
        true,
        Recheck::fixed(Gate::Open),
    )
    .await;
    assert_eq!(
        rig.session.typed(),
        vec![(Jid::new(PEER_PN, Server::Pn), true)]
    );
}

/// Fix F6 (privacy LOW): the gate is asked AGAIN right before the typing state is written; a lock that is known by
/// then (pushed while the store and the account's mode were awaited) sends nothing. Mutant: the second look removed.
#[tokio::test]
async fn a_typing_state_is_dropped_when_the_lock_is_known_before_it_is_written() {
    let rig = Rig::new().await;
    let session: Arc<dyn Session> = rig.session.clone();
    typing(
        rig.store(),
        session.clone(),
        own(),
        dm(PEER_PN),
        true,
        Recheck::fixed(Gate::Held),
    )
    .await;
    assert!(
        rig.session.typed().is_empty(),
        "a typing state went out into a known lock"
    );
    typing(
        rig.store(),
        session,
        own(),
        dm(PEER_PN),
        true,
        Recheck::fixed(Gate::Unknown),
    )
    .await;
    assert!(rig.session.typed().is_empty(), "or into an unknown one");
}

async fn read_job(rig: &Rig, cmd: i64, conv: ConvKey, local: &str, author: TargetAuthor, at: i64) {
    persist(
        &rig.store(),
        CmdId(cmd),
        conv,
        vec![target(author, local)],
        ReceiptKind::Read,
        at,
    )
    .await
    .expect("persisted");
}

/// Fix F14 (robustness LOW-3): only a CONNECTIVITY failure stops the drain. A job that fails for a reason of its own
/// would fail at every drain and hold back the receipts of every chat for ever: it is dropped, and the jobs behind it
/// go out. Mutant: any failure stops.
#[tokio::test]
async fn a_job_that_can_never_be_sent_is_dropped_and_the_jobs_behind_it_go_out() {
    let rig = Rig::new().await;
    read_job(&rig, 71, dm(PEER_PN), MSG_A, by("pn", PEER_PN), NOW).await;
    read_job(&rig, 72, dm(PEER_PN), MSG_B, by("pn", PEER_PN), NOW).await;
    rig.session.script_receipt(Err(SendFailure::InvalidRequest));
    assert_eq!(
        rig.drain().await,
        Drained::All,
        "a failure of the job itself stopped the drain"
    );
    assert!(rig.jobs().await.is_empty(), "the failed job was kept");
    let receipts = rig.session.receipts();
    assert_eq!(
        receipts.len(),
        2,
        "the job behind the failed one was not sent"
    );
    assert_eq!(receipts[1].ids, vec![MSG_B.to_owned()]);
    for failure in [SendFailure::Internal, SendFailure::PrimaryDeviceRejected] {
        read_job(&rig, 73, dm(PEER_PN), MSG_C, by("pn", PEER_PN), NOW).await;
        rig.session.script_receipt(Err(failure));
        assert_eq!(rig.drain().await, Drained::All, "{failure:?}");
        assert!(rig.jobs().await.is_empty(), "{failure:?}: the job was kept");
    }
}

/// Fix F14: the failures about the connection keep the job and stop the drain, in order: the job and the ones behind
/// it wait for the next connection. Mutant: a connectivity failure drops the job.
#[tokio::test]
async fn a_connection_failure_keeps_the_job_and_stops_the_drain() {
    for failure in [
        SendFailure::NoClient,
        SendFailure::NotConnected,
        SendFailure::NotLoggedIn,
        SendFailure::Socket,
        SendFailure::IqTimeout,
    ] {
        let rig = Rig::new().await;
        read_job(&rig, 71, dm(PEER_PN), MSG_A, by("pn", PEER_PN), NOW).await;
        read_job(&rig, 72, dm(PEER_PN), MSG_B, by("pn", PEER_PN), NOW).await;
        rig.session.script_receipt(Err(failure));
        assert_eq!(rig.drain().await, Drained::Stopped, "{failure:?}");
        assert_eq!(
            rig.jobs().await,
            vec![71, 72],
            "{failure:?}: both jobs wait"
        );
        assert_eq!(
            rig.session.receipts().len(),
            1,
            "{failure:?}: stopped at the first"
        );
    }
}

/// Fix F14 (interop LOW-5): a group's addressing mode that could not be read is not planned with a guess. A failure of
/// the connection keeps the job (the receipt would name the author in the wrong form); a refusal of the group's info
/// for good drops it. Mutant: the failed mode is read as `None` and the receipt goes out.
#[tokio::test]
async fn a_group_receipt_waits_for_the_groups_addressing_mode() {
    let rig = Rig::new().await;
    read_job(&rig, 71, group(), MSG_A, by("lid", AUTHOR_LID), NOW).await;
    rig.session.script_group_mode(Err(SendFailure::IqTimeout));
    assert_eq!(rig.drain().await, Drained::Stopped, "the mode is unknown");
    assert!(
        rig.session.receipts().is_empty(),
        "a receipt was planned with a guessed mode"
    );
    assert_eq!(rig.jobs().await, vec![71], "the job waits");
    // The group's info refused for good (the account left it): the job could never go out.
    read_job(&rig, 72, group(), MSG_B, by("lid", AUTHOR_LID), NOW).await;
    // The server's own trouble (a 503) is no reason to give the receipt up.
    rig.session
        .script_group_mode(Err(SendFailure::IqRefused { code: 503 }));
    assert_eq!(rig.drain().await, Drained::Stopped, "a 503 of the server");
    assert_eq!(rig.jobs().await, vec![71, 72], "both jobs wait");
    rig.session.script_group_mode(Ok(Namespace::Lid));
    rig.session
        .script_group_mode(Err(SendFailure::IqRefused { code: 403 }));
    assert_eq!(rig.drain().await, Drained::All);
    assert!(rig.jobs().await.is_empty(), "both jobs are gone");
    assert_eq!(
        rig.session.receipts().len(),
        1,
        "the first job went out with its mode known, the refused group's did not"
    );
}

/// Fix F14: a job older than 14 days (by `at`) is forgotten at a start; one of exactly 14 days stays. Mutant: the
/// bound read from the wrong side.
#[tokio::test]
async fn receipt_jobs_older_than_fourteen_days_go_at_a_sweep() {
    let rig = Rig::new().await;
    read_job(
        &rig,
        71,
        dm(PEER_PN),
        MSG_A,
        by("pn", PEER_PN),
        NOW - JOB_KEEP_MS - 1,
    )
    .await;
    read_job(
        &rig,
        72,
        dm(PEER_PN),
        MSG_B,
        by("pn", PEER_PN),
        NOW - JOB_KEEP_MS,
    )
    .await;
    read_job(&rig, 73, dm(PEER_PN), MSG_C, by("pn", PEER_PN), NOW).await;
    let gone = rig
        .store()
        .write("sweep", |tx| sweep(tx, NOW))
        .await
        .expect("sweep");
    assert_eq!(gone, 1);
    assert_eq!(rig.jobs().await, vec![72, 73]);
}
