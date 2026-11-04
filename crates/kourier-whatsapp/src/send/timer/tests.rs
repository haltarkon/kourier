//! The 1:1 disappearing timer set (W6a2, B7b, D22): what is accepted, the frame and its order against the gates, the
//! confirmation the core hears only after the ack, and what a refusal, a nack and a silence leave behind. Over a
//! [`FakeSession`] and a real store and event writer. Synthetic values only (the 555-01xx numbers; each tail of 8
//! digits is a substring of no other value here).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{AllocatedId, BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::{BackendError, Body, ConvKey, ErrorKind, Origin, Patch, SystemEvent, UserId};
use tokio::sync::Notify;

use super::{Set, request, set};
use crate::events::EventWriter;
use crate::features::{self, TIMER_CHOICES};
use crate::map::OwnIds;
use crate::send::acks::{Ack, Acks, Answer, Nack};
use crate::send::attempt::{ACK_WAIT, Files, Gate, HOLD_RETRY, Parts, START_ON_PHONE};
use crate::send::hold::Recheck;
use crate::send::started::{self, How};
use crate::send::stop::Transmits;
use crate::session::Session;
use crate::session::fake::{Call, FakeSession};
use crate::store::{KourierStore, StoreOwner};

const PEER_PN: &str = "15550162947308";
const STRANGER_PN: &str = "15550118375642";
const OWN_PN: &str = "15550173029584";
const OWN_LID: &str = "77700164820395";
const GROUP: &str = "120363000083920471";
const LOCAL: &str = "3EB0B7C61D2E48F95031";
/// The actor's clock in these tests (ms): the frame's version is its seconds.
const NOW: i64 = 1_761_500_000_000;
const DAY: u64 = 86_400;

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a synthetic user")
}

fn dm(pn: &str) -> ConvKey {
    ConvKey::Direct(user("pn", pn))
}

fn id() -> AllocatedId {
    AllocatedId {
        local: LOCAL.into(),
        sent_at: NOW - 500,
    }
}

fn change(conv: ConvKey, secs: u32) -> Set {
    Set {
        conv,
        id: id(),
        secs,
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    session: Arc<FakeSession>,
    acks: Arc<Acks>,
    writer: EventWriter,
    transmits: Transmits,
}

impl Rig {
    /// A store whose chat with [`PEER_PN`] the phone started.
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("dir");
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &DbKey::from([12; 32]),
            "wa-timer",
        )
        .expect("open");
        let peer = dm(PEER_PN);
        owner
            .store()
            .write("start", move |tx| started::note(tx, &peer, How::Peer))
            .await
            .expect("note");
        let writer = EventWriter::new(
            owner.store(),
            Arc::new(FakeClock::new(u64::try_from(NOW).expect("time"))),
            Arc::new(Notify::new()),
        );
        Self {
            _dir: dir,
            owner,
            session: Arc::new(FakeSession::new()),
            acks: Arc::default(),
            writer,
            transmits: Transmits::default(),
        }
    }

    fn store(&self) -> KourierStore {
        self.owner.store()
    }

    fn parts_with(&self, gate: Gate) -> Parts {
        let session: Arc<dyn Session> = self.session.clone();
        Parts {
            store: self.store(),
            session: Some(session),
            acks: self.acks.clone(),
            own: OwnIds {
                lid: Some(user("lid", OWN_LID)),
                pn: Some(user("pn", OWN_PN)),
            },
            now_ms: NOW,
            hold: Recheck::fixed(gate),
            linked_ms: None,
            stop: self.transmits.register_change(),
            files: Files::none(),
        }
    }

    /// The change, running; the handle ends with its answer.
    fn spawn(&self, set_: Set) -> tokio::task::JoinHandle<Result<(), BackendError>> {
        tokio::spawn(set(self.parts_with(Gate::Open), self.writer.clone(), set_))
    }

    async fn frames(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.session.sent().len() < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the frames are written");
    }

    fn answer(&self, answer: Answer) {
        self.acks.answer(Ack {
            id: LOCAL.into(),
            answer,
        });
    }

    /// How many rows `core_outbox` holds (statements included).
    async fn rows(&self) -> usize {
        self.store()
            .read(|conn| {
                Ok(outbox::peek(conn, 10_000)
                    .map_err(|_| crate::error::database("peek the outbox"))?
                    .len())
            })
            .await
            .expect("read the outbox")
    }

    /// Parks the store's writer until the returned sender is used: a write submitted meanwhile waits for it.
    async fn park(&self) -> std::sync::mpsc::Sender<()> {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let store = self.store();
        tokio::spawn(async move {
            let _ = store
                .raw(kourier_db::Priority::High, move |_| {
                    let _ = started_tx.send(());
                    let _ = wait.recv();
                    Ok(())
                })
                .await;
        });
        for _ in 0..500 {
            if started.try_recv().is_ok() {
                return release;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the writer did not park");
    }

    /// What the core was told, in order, without the features statements the writer's zone puts in front.
    async fn told(&self) -> Vec<BackendEvent> {
        self.store()
            .read(|conn| {
                let rows = outbox::peek(conn, 10_000)
                    .map_err(|_| crate::error::database("peek the outbox"))?;
                let mut told = Vec::new();
                for row in rows {
                    told.extend(
                        decode_outbox_row(&row.envelope)
                            .expect("decodes")
                            .into_iter()
                            .filter(|event| !features::is_statement(event)),
                    );
                }
                Ok(told)
            })
            .await
            .expect("read the outbox")
    }
}

fn failed_kind(result: &Result<(), BackendError>) -> ErrorKind {
    match result {
        Err(e) => e.kind.clone(),
        Ok(()) => panic!("the change did not fail"),
    }
}

/// D22: only a direct chat, with an id from the core, and only a timer WhatsApp's apps offer (or off) is accepted; the
/// apps ignore any other value. Kills "a group accepted", "no id needed", "any whole number of seconds" and "a fraction of
/// a second truncated".
#[test]
fn only_a_direct_chat_with_an_id_and_an_offered_timer_is_accepted() {
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let secs = |s| Some(Duration::from_secs(s));
    // The three offered timers and off.
    for choice in TIMER_CHOICES {
        let accepted = request(dm(PEER_PN), secs(u64::from(choice)), Some(id())).expect("offered");
        assert_eq!(accepted.secs, choice);
    }
    let off = request(dm(PEER_PN), None, Some(id())).expect("off");
    assert_eq!(off.secs, 0, "off is zero seconds");
    // What is not offered.
    for refused in [
        Some(Duration::ZERO),
        secs(3600),
        secs(DAY + 1),
        Some(Duration::from_millis(86_400_500)),
        secs(u64::from(u32::MAX) + 1),
    ] {
        let error = request(dm(PEER_PN), refused, Some(id())).expect_err("not offered");
        assert_eq!(error.kind, ErrorKind::InvalidInput, "{refused:?}");
    }
    assert_eq!(
        request(dm(PEER_PN), secs(DAY), None)
            .expect_err("no id")
            .kind,
        ErrorKind::InvalidInput
    );
    // A group's timer is an admin's group change (K8c).
    assert_eq!(
        request(group, secs(DAY), Some(id()))
            .expect_err("a group")
            .kind,
        ErrorKind::Unsupported
    );
}

/// The core is told the chat's timer with the frame's version (the receive map's scale) and the notice under the id the
/// core allocated, as ours (`Origin::OwnThisDevice`, `by: None`). Kills "no version", "the notice by someone", "the
/// notice of a peer's origin", "off stated as a timer".
#[test]
fn the_confirmation_is_the_chats_timer_with_its_version_and_our_own_notice() {
    let at_s = NOW / 1000;
    let events = super::confirmation(&change(dm(PEER_PN), 604_800), at_s, NOW);
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = events.as_slice()
    else {
        panic!("one upsert and one notice: {events:?}");
    };
    assert_eq!(patch.key, dm(PEER_PN));
    assert_eq!(patch.timer, Patch::Set(Duration::from_secs(604_800)));
    assert_eq!(patch.timer_version, u32::try_from(at_s).ok());
    assert_eq!(notice.conv, dm(PEER_PN));
    assert_eq!(&*notice.local, LOCAL, "the id the core allocated");
    assert_eq!(notice.sent_at, NOW - 500);
    assert!(matches!(notice.origin, Origin::OwnThisDevice));
    assert_eq!(
        notice.body,
        Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: Some(Duration::from_secs(604_800)),
        })
    );
    // Off clears the timer and says so in the notice.
    let events = super::confirmation(&change(dm(PEER_PN), 0), at_s, NOW);
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = events.as_slice()
    else {
        panic!("one upsert and one notice: {events:?}");
    };
    assert_eq!(patch.timer, Patch::Clear);
    assert_eq!(
        notice.body,
        Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: None,
        })
    );
}

/// The whole road: the frame is the protocol message of the setting (the timer in seconds and the frame's time as its
/// version, no `contextInfo` timer of its own) to the peer under the allocated id; the core hears NOTHING until the ack;
/// the command is answered `Ok` only after the confirmation is committed. Kills "the frame without the ephemeral
/// setting", "the confirmation before the ack", "the answer before the commit" and "another id".
#[tokio::test]
async fn a_timer_is_one_frame_and_is_confirmed_to_the_core_only_after_its_ack() {
    let rig = Rig::new().await;
    let running = rig.spawn(change(dm(PEER_PN), 604_800));
    rig.frames(1).await;
    let sent = rig.session.sent();
    assert_eq!(sent.len(), 1, "one frame");
    assert_eq!(sent[0].id, LOCAL, "the id the core allocated");
    let peer: wacore_binary::Jid = format!("{PEER_PN}@s.whatsapp.net").parse().expect("a jid");
    assert_eq!(sent[0].to, peer, "the peer's chat");
    let setting = sent[0]
        .message
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(setting.ephemeral_expiration, Some(604_800));
    assert_eq!(
        setting.ephemeral_setting_timestamp,
        Some(NOW / 1000),
        "the frame's time is the setting's version"
    );
    assert_eq!(sent[0].expiration, None, "no timer of the frame's own");
    // Not yet answered, and the core has heard nothing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!running.is_finished(), "the command waits for the ack");
    assert!(
        rig.told().await.is_empty(),
        "nothing is confirmed before the ack"
    );
    rig.answer(Answer::Acked {
        server_t: Some(NOW / 1000 + 1),
    });
    running
        .await
        .expect("ends")
        .expect("the change is answered ok");
    // The answer came after the commit: the rows are already there.
    let told = rig.told().await;
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = told.as_slice()
    else {
        panic!("the confirmation: {told:?}");
    };
    assert_eq!(patch.timer, Patch::Set(Duration::from_secs(604_800)));
    assert_eq!(&*notice.local, LOCAL);
    assert_eq!(rig.session.count(&Call::DropConnection), 0);
}

/// DUR-8, the answer waits for the commit: while the writer is parked the confirmation cannot be written, and the command is
/// not answered; released, it is answered `Ok` and the rows are there. (The test above reads the rows through the writer's own
/// queue, so it sees them whichever order the answer took.) Kills "the answer before the commit" (the confirmation written
/// by a task nobody waits for).
#[tokio::test]
async fn the_command_is_answered_only_after_the_confirmation_is_committed() {
    let rig = Rig::new().await;
    let running = rig.spawn(change(dm(PEER_PN), 604_800));
    rig.frames(1).await;
    let release = rig.park().await;
    rig.answer(Answer::Acked {
        server_t: Some(NOW / 1000 + 1),
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !running.is_finished(),
        "the command was answered before the confirmation was committed"
    );
    release.send(()).expect("release");
    running
        .await
        .expect("ends")
        .expect("answered once the commit is done");
    assert_eq!(
        rig.rows().await,
        1,
        "the confirmation is one row, in the outbox"
    );
    assert_eq!(rig.told().await.len(), 2, "the timer and the notice");
}

/// DUR-8, a confirmation that cannot be written: the frame was acked (WhatsApp has the new timer) and the zone that tells
/// the core FAILS (the seam of D3: a full disk, a closed store). The command fails `Internal` with its words, the core has
/// heard nothing and the outbox holds nothing (the zone rolled back). Kills "the write's result ignored" and "the answer
/// before the commit".
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]
async fn a_confirmation_that_cannot_be_committed_fails_the_change_and_tells_the_core_nothing() {
    let _serial = crate::fault::serial();
    let rig = Rig::new().await;
    let running = rig.spawn(change(dm(PEER_PN), 604_800));
    rig.frames(1).await;
    crate::fault::arm(crate::fault::EVENT_WRITE, 1);
    rig.answer(Answer::Acked {
        server_t: Some(NOW / 1000 + 1),
    });
    let result = running.await.expect("ends");
    crate::fault::disarm();
    assert_eq!(failed_kind(&result), ErrorKind::Internal);
    assert_eq!(rig.rows().await, 0, "a failed zone wrote");
    assert!(rig.told().await.is_empty(), "the core heard the timer");
    assert_eq!(
        rig.session.sent().len(),
        1,
        "the frame went once and is not resent"
    );
}

/// A nack fails the command with the send path's table and tells the core nothing; a 463 is a refusal for good. Kills
/// "a nack confirmed", "a 463 as a transient".
#[tokio::test]
async fn a_nack_fails_the_change_and_the_core_hears_nothing() {
    for (nack, wanted) in [
        (
            Nack::Code(429),
            ErrorKind::RateLimited {
                retry_after: Some(Duration::from_secs(300)),
            },
        ),
        (Nack::Code(463), ErrorKind::ReachOutRefused),
        (Nack::Code(479), ErrorKind::InvalidInput),
    ] {
        let rig = Rig::new().await;
        let running = rig.spawn(change(dm(PEER_PN), 86_400));
        rig.frames(1).await;
        rig.answer(Answer::Nacked(nack));
        let result = running.await.expect("ends");
        assert_eq!(failed_kind(&result), wanted, "{nack:?}");
        assert!(
            rig.told().await.is_empty(),
            "nothing confirmed after {nack:?}"
        );
    }
}

/// No ack within `ACK_WAIT`: transient, the connection is ended (as a send's), and the core is told nothing (the old
/// timer stays on screen with an error). Kills "the connection not dropped" and "a silence confirmed".
#[tokio::test]
async fn a_silent_server_ends_the_connection_and_confirms_nothing() {
    let rig = Rig::new().await;
    let running = rig.spawn(change(dm(PEER_PN), 86_400));
    rig.frames(1).await;
    let result = tokio::time::timeout(ACK_WAIT + Duration::from_secs(5), running)
        .await
        .expect("answered after ACK_WAIT")
        .expect("ends");
    assert_eq!(
        failed_kind(&result),
        ErrorKind::Transient { retry_after: None }
    );
    assert_eq!(rig.session.count(&Call::DropConnection), 1);
    assert!(rig.told().await.is_empty());
}

/// The same gates as a message, each BEFORE any frame: a chat the phone never started, the reach-out lock. Kills "the
/// started fact not read" and "the hold not asked".
#[tokio::test]
async fn the_gates_of_a_send_stop_a_timer_before_any_frame() {
    let rig = Rig::new().await;
    let stranger = rig.spawn(change(dm(STRANGER_PN), 86_400));
    let result = stranger.await.expect("ends");
    assert_eq!(failed_kind(&result), ErrorKind::Unsupported);
    assert_eq!(
        result.expect_err("refused").detail,
        START_ON_PHONE,
        "the words the person reads"
    );
    for gate in [Gate::Unknown, Gate::Held] {
        let held = set(
            rig.parts_with(gate),
            rig.writer.clone(),
            change(dm(PEER_PN), 86_400),
        )
        .await;
        assert_eq!(
            failed_kind(&held),
            ErrorKind::Transient {
                retry_after: Some(HOLD_RETRY)
            }
        );
    }
    assert!(rig.session.sent().is_empty(), "no frame went out");
    assert!(rig.told().await.is_empty());
}

/// The app's quit that comes first takes the change before its frame (like any attempt before its point of no return):
/// nothing is written, the answer is the quit's transient. Kills "the point of no return not taken".
#[tokio::test]
async fn a_quit_before_the_frame_ends_the_change_with_nothing_written() {
    let rig = Rig::new().await;
    let parts = rig.parts_with(Gate::Open);
    let (ended, kept) = rig.transmits.quit_all();
    assert_eq!(
        (ended, kept),
        (1, 0),
        "the change was registered and still open"
    );
    let result = set(parts, rig.writer.clone(), change(dm(PEER_PN), 86_400)).await;
    assert_eq!(
        failed_kind(&result),
        ErrorKind::Transient { retry_after: None }
    );
    assert!(rig.session.sent().is_empty());
    assert!(rig.told().await.is_empty());
}

/// A session that is not there is the send path's `NoClient` (transient), no frame.
#[tokio::test]
async fn no_session_is_a_transient_failure_with_no_frame() {
    let rig = Rig::new().await;
    let mut parts = rig.parts_with(Gate::Open);
    parts.session = None;
    let result = set(parts, rig.writer.clone(), change(dm(PEER_PN), 86_400)).await;
    assert_eq!(
        failed_kind(&result),
        ErrorKind::Transient { retry_after: None }
    );
    assert!(rig.told().await.is_empty());
}
